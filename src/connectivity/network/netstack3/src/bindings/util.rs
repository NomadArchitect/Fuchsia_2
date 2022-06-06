// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::{convert::Infallible as Never, num::NonZeroU64};

use fidl_fuchsia_net as fidl_net;
use fidl_fuchsia_net_stack as fidl_net_stack;
use fidl_fuchsia_posix as fposix;
use net_types::ip::{
    AddrSubnetEither, AddrSubnetError, IpAddr, Ipv4Addr, Ipv6Addr, SubnetEither, SubnetError,
};
use net_types::{SpecifiedAddr, Witness};
use netstack3_core::{AddRouteError, AddableEntryEither, DeviceId, EntryEither, NetstackError};

use crate::bindings::socket::IntoErrno;

/// A core type which can be fallibly converted from the FIDL type `F`.
///
/// For all `C: TryFromFidl<F>`, we provide a blanket impl of [`F:
/// TryIntoCore<C>`].
///
/// [`F: TryIntoCore<C>`]: crate::bindings::util::TryIntoCore
pub(crate) trait TryFromFidl<F>: Sized {
    /// The type of error returned from [`try_from_fidl`].
    ///
    /// [`try_from_fidl`]: crate::bindings::util::TryFromFidl::try_from_fidl
    type Error;

    /// Attempt to convert from `fidl` into an instance of `Self`.
    fn try_from_fidl(fidl: F) -> Result<Self, Self::Error>;
}

/// A core type which can be fallibly converted to the FIDL type `F`.
///
/// For all `C: TryIntoFidl<F>`, we provide a blanket impl of [`F:
/// TryFromCore<C>`].
///
/// [`F: TryFromCore<C>`]: crate::bindings::util::TryFromCore
pub(crate) trait TryIntoFidl<F>: Sized {
    /// The type of error returned from [`try_into_fidl`].
    ///
    /// [`try_into_fidl`]: crate::bindings::util::TryIntoFidl::try_into_fidl
    type Error;

    /// Attempt to convert `self` into an instance of `F`.
    fn try_into_fidl(self) -> Result<F, Self::Error>;
}

/// A core type which can be infallibly converted into the FIDL type `F`.
///
/// `IntoFidl<F>` extends [`TryIntoFidl<F, Error = Never>`], and provides the
/// infallible conversion method [`into_fidl`].
///
/// [`TryIntoFidl<F, Error = Never>`]: crate::bindings::util::TryIntoFidl
/// [`into_fidl`]: crate::bindings::util::IntoFidl::into_fidl
pub(crate) trait IntoFidl<F>: TryIntoFidl<F, Error = Never> {
    /// Infallibly convert `self` into an instance of `F`.
    fn into_fidl(self) -> F {
        match self.try_into_fidl() {
            Ok(f) => f,
            Err(never) => match never {},
        }
    }
}

impl<F, T: TryIntoFidl<F, Error = Never>> IntoFidl<F> for T {}

/// A FIDL type which can be fallibly converted from the core type `C`.
///
/// `TryFromCore<C>` is automatically implemented for all `F` where [`C:
/// TryIntoFidl<F>`].
///
/// [`C: TryIntoFidl<F>`]: crate::bindings::util::TryIntoFidl
pub(crate) trait TryFromCore<C>: Sized
where
    C: TryIntoFidl<Self>,
{
    /// Attempt to convert from `core` into an instance of `Self`.
    fn try_from_core(core: C) -> Result<Self, C::Error> {
        core.try_into_fidl()
    }
}

impl<F, C: TryIntoFidl<F>> TryFromCore<C> for F {}

/// A FIDL type which can be fallibly converted into the core type `C`.
///
/// `TryIntoCore<C>` is automatically implemented for all `F` where [`C:
/// TryFromFidl<F>`].
///
/// [`C: TryFromFidl<F>`]: crate::bindings::util::TryFromFidl
pub(crate) trait TryIntoCore<C>: Sized
where
    C: TryFromFidl<Self>,
{
    /// Attempt to convert from `self` into an instance of `C`.
    ///
    /// This is equivalent to [`C::try_from_fidl(self)`].
    ///
    /// [`C::try_from_fidl(self)`]: crate::bindings::util::TryFromFidl::try_from_fidl
    fn try_into_core(self) -> Result<C, C::Error> {
        C::try_from_fidl(self)
    }
}

impl<F, C: TryFromFidl<F>> TryIntoCore<C> for F {}

/// A FIDL type which can be infallibly converted into the core type `C`.
///
/// `IntoCore<C>` extends [`TryIntoCore<C>`] where `<C as TryFromFidl<_>>::Error
/// = Never`, and provides the infallible conversion method [`into_core`].
///
/// [`TryIntoCore<C>`]: crate::bindings::util::TryIntoCore
/// [`into_fidl`]: crate::bindings::util::IntoCore::into_core
pub(crate) trait IntoCore<C>: TryIntoCore<C>
where
    C: TryFromFidl<Self, Error = Never>,
{
    /// Infallibly convert `self` into an instance of `C`.
    fn into_core(self) -> C {
        match self.try_into_core() {
            Ok(c) => c,
            Err(never) => match never {},
        }
    }
}

impl<F, C: TryFromFidl<F, Error = Never>> IntoCore<C> for F {}

impl<T> TryIntoFidl<T> for Never {
    type Error = Never;

    fn try_into_fidl(self) -> Result<T, Never> {
        match self {}
    }
}

impl TryIntoFidl<fidl_net_stack::Error> for SubnetError {
    type Error = Never;

    fn try_into_fidl(self) -> Result<fidl_net_stack::Error, Never> {
        Ok(fidl_net_stack::Error::InvalidArgs)
    }
}

impl TryIntoFidl<fidl_net_stack::Error> for AddrSubnetError {
    type Error = Never;

    fn try_into_fidl(self) -> Result<fidl_net_stack::Error, Never> {
        Ok(fidl_net_stack::Error::InvalidArgs)
    }
}

impl TryIntoFidl<fidl_net_stack::Error> for NetstackError {
    type Error = Never;

    fn try_into_fidl(self) -> Result<fidl_net_stack::Error, Never> {
        match self {
            NetstackError::Exists => Ok(fidl_net_stack::Error::AlreadyExists),
            NetstackError::NotFound => Ok(fidl_net_stack::Error::NotFound),
            _ => Ok(fidl_net_stack::Error::Internal),
        }
    }
}

impl TryIntoFidl<fidl_net_stack::Error> for AddRouteError {
    type Error = Never;

    fn try_into_fidl(self) -> Result<fidl_net_stack::Error, Never> {
        match self {
            AddRouteError::AlreadyExists => Ok(fidl_net_stack::Error::AlreadyExists),
            AddRouteError::GatewayNotNeighbor => Ok(fidl_net_stack::Error::BadState),
        }
    }
}

impl TryFromFidl<fidl_net::IpAddress> for IpAddr {
    type Error = Never;

    fn try_from_fidl(addr: fidl_net::IpAddress) -> Result<IpAddr, Never> {
        match addr {
            fidl_net::IpAddress::Ipv4(v4) => Ok(IpAddr::V4(v4.into_core())),
            fidl_net::IpAddress::Ipv6(v6) => Ok(IpAddr::V6(v6.into_core())),
        }
    }
}

impl TryIntoFidl<fidl_net::IpAddress> for IpAddr {
    type Error = Never;

    fn try_into_fidl(self) -> Result<fidl_net::IpAddress, Never> {
        match self {
            IpAddr::V4(addr) => Ok(fidl_net::IpAddress::Ipv4(addr.into_fidl())),
            IpAddr::V6(addr) => Ok(fidl_net::IpAddress::Ipv6(addr.into_fidl())),
        }
    }
}

impl TryFromFidl<fidl_net::Ipv4Address> for Ipv4Addr {
    type Error = Never;

    fn try_from_fidl(addr: fidl_net::Ipv4Address) -> Result<Ipv4Addr, Never> {
        Ok(addr.addr.into())
    }
}

impl TryIntoFidl<fidl_net::Ipv4Address> for Ipv4Addr {
    type Error = Never;

    fn try_into_fidl(self) -> Result<fidl_net::Ipv4Address, Never> {
        Ok(fidl_net::Ipv4Address { addr: self.ipv4_bytes() })
    }
}

impl TryFromFidl<fidl_net::Ipv6Address> for Ipv6Addr {
    type Error = Never;

    fn try_from_fidl(addr: fidl_net::Ipv6Address) -> Result<Ipv6Addr, Never> {
        Ok(addr.addr.into())
    }
}

impl TryIntoFidl<fidl_net::Ipv6Address> for Ipv6Addr {
    type Error = Never;

    fn try_into_fidl(self) -> Result<fidl_net::Ipv6Address, Never> {
        Ok(fidl_net::Ipv6Address { addr: self.ipv6_bytes() })
    }
}

/// An error indicating that an address was a member of the wrong class (for
/// example, a unicast address used where a multicast address is required).
pub struct AddrClassError;

// TODO(joshlf): Introduce a separate variant to `fidl_net_stack::Error` for
// `AddrClassError`?
impl TryIntoFidl<fidl_net_stack::Error> for AddrClassError {
    type Error = Never;

    fn try_into_fidl(self) -> Result<fidl_net_stack::Error, Never> {
        Ok(fidl_net_stack::Error::InvalidArgs)
    }
}

impl TryFromFidl<fidl_net::IpAddress> for SpecifiedAddr<IpAddr> {
    type Error = AddrClassError;

    fn try_from_fidl(fidl: fidl_net::IpAddress) -> Result<SpecifiedAddr<IpAddr>, AddrClassError> {
        SpecifiedAddr::new(fidl.into_core()).ok_or(AddrClassError)
    }
}

impl TryIntoFidl<fidl_net::IpAddress> for SpecifiedAddr<IpAddr> {
    type Error = Never;

    fn try_into_fidl(self) -> Result<fidl_net::IpAddress, Never> {
        Ok(self.get().into_fidl())
    }
}

impl TryFromFidl<fidl_net::Subnet> for AddrSubnetEither {
    type Error = AddrSubnetError;

    fn try_from_fidl(fidl: fidl_net::Subnet) -> Result<AddrSubnetEither, AddrSubnetError> {
        AddrSubnetEither::new(fidl.addr.into_core(), fidl.prefix_len)
    }
}

impl TryIntoFidl<fidl_net::Subnet> for AddrSubnetEither {
    type Error = Never;

    fn try_into_fidl(self) -> Result<fidl_net::Subnet, Never> {
        let (addr, prefix) = self.addr_prefix();
        Ok(fidl_net::Subnet { addr: addr.into_fidl(), prefix_len: prefix })
    }
}

impl TryFromFidl<fidl_net::Subnet> for SubnetEither {
    type Error = SubnetError;

    fn try_from_fidl(fidl: fidl_net::Subnet) -> Result<SubnetEither, SubnetError> {
        SubnetEither::new(fidl.addr.into_core(), fidl.prefix_len)
    }
}

impl TryIntoFidl<fidl_net::Subnet> for SubnetEither {
    type Error = Never;

    fn try_into_fidl(self) -> Result<fidl_net::Subnet, Never> {
        let (net, prefix) = self.net_prefix();
        Ok(fidl_net::Subnet { addr: net.into_fidl(), prefix_len: prefix })
    }
}

/// Provides a stateful context for operations that require state-keeping to be
/// completed.
///
/// `ConversionContext` is used by conversion functions in
/// [`TryFromFidlWithContext`] and [`TryFromCoreWithContext`].
pub(crate) trait ConversionContext {
    /// Converts a binding identifier (exposed in FIDL as `u64`) to a core
    /// identifier `DeviceId`.
    ///
    /// Returns `None` if there is no core mapping equivalent for `binding_id`.
    fn get_core_id(&self, binding_id: u64) -> Option<DeviceId>;
    /// Converts a core identifier `DeviceId` to a FIDL-compatible `u64`
    /// identifier.
    ///
    /// Returns `None` if there is no FIDL mapping equivalent for `core_id`.
    fn get_binding_id(&self, core_id: DeviceId) -> Option<u64>;
}

/// A core type which can be fallibly converted from the FIDL type `F` given a
/// context that implements [`ConversionContext`].
///
/// For all `C: TryFromFidlWithContext<F>`, we provide a blanket impl of [`F:
/// TryIntoCoreWithContext<C>`].
///
/// [`F: TryIntoCoreWithContext<C>`]: crate::bindings::util::TryIntoCoreWithContext
pub(crate) trait TryFromFidlWithContext<F>: Sized {
    /// The type of error returned from [`try_from_fidl_with_ctx`].
    ///
    /// [`try_from_fidl_with_ctx`]: crate::bindings::util::TryFromFidlWithContext::try_from_fidl_with_ctx
    type Error;

    /// Attempt to convert from `fidl` into an instance of `Self`.
    fn try_from_fidl_with_ctx<C: ConversionContext>(ctx: &C, fidl: F) -> Result<Self, Self::Error>;
}

/// A core type which can be fallibly converted to the FIDL type `F` given a
/// context that implements [`ConversionContext`].
///
/// For all `C: TryIntoFidlWithContext<F>`, we provide a blanket impl of [`F:
/// TryFromCoreWithContext<C>`].
///
/// [`F: TryFromCoreWithContext<C>`]: crate::bindings::util::TryFromCoreWithContext
pub(crate) trait TryIntoFidlWithContext<F>: Sized {
    /// The type of error returned from [`try_into_fidl_with_ctx`].
    ///
    /// [`try_into_fidl_with_ctx`]: crate::bindings::util::TryIntoFidlWithContext::try_from_fidl_with_ctx
    type Error;

    /// Attempt to convert from `self` into an instance of `F`.
    fn try_into_fidl_with_ctx<C: ConversionContext>(self, ctx: &C) -> Result<F, Self::Error>;
}

/// A FIDL type which can be fallibly converted from the core type `C` given a
/// context that implements [`ConversionContext`].
///
/// `TryFromCoreWithContext<C>` is automatically implemented for all `F` where
/// [`C: TryIntoFidlWithContext<F>`].
///
/// [`C: TryIntoFidlWithContext<F>`]: crate::bindings::util::TryIntoFidlWithContext
pub(crate) trait TryFromCoreWithContext<C>: Sized
where
    C: TryIntoFidlWithContext<Self>,
{
    /// Attempt to convert from `core` into an instance of `Self`.
    fn try_from_core_with_ctx<X: ConversionContext>(ctx: &X, core: C) -> Result<Self, C::Error> {
        core.try_into_fidl_with_ctx(ctx)
    }
}

impl<F, C: TryIntoFidlWithContext<F>> TryFromCoreWithContext<C> for F {}

/// A FIDL type which can be fallibly converted into the core type `C` given a
/// context that implements [`ConversionContext`].
///
/// `TryIntoCoreWithContext<C>` is automatically implemented for all `F` where
/// [`C: TryFromFidlWithContext<F>`].
///
/// [`C: TryFromFidlWithContext<F>`]: crate::bindings::util::TryFromFidlWithContext
pub(crate) trait TryIntoCoreWithContext<C>: Sized
where
    C: TryFromFidlWithContext<Self>,
{
    /// Attempt to convert from `self` into an instance of `C`.
    fn try_into_core_with_ctx<X: ConversionContext>(self, ctx: &X) -> Result<C, C::Error> {
        C::try_from_fidl_with_ctx(ctx, self)
    }
}

impl<F, C: TryFromFidlWithContext<F>> TryIntoCoreWithContext<C> for F {}

#[derive(Debug)]
pub struct DeviceNotFoundError;

impl TryFromFidlWithContext<u64> for DeviceId {
    type Error = DeviceNotFoundError;

    fn try_from_fidl_with_ctx<C: ConversionContext>(
        ctx: &C,
        fidl: u64,
    ) -> Result<DeviceId, DeviceNotFoundError> {
        ctx.get_core_id(fidl).ok_or(DeviceNotFoundError)
    }
}

impl TryIntoFidlWithContext<u64> for DeviceId {
    type Error = DeviceNotFoundError;

    fn try_into_fidl_with_ctx<C: ConversionContext>(
        self,
        ctx: &C,
    ) -> Result<u64, DeviceNotFoundError> {
        ctx.get_binding_id(self).ok_or(DeviceNotFoundError)
    }
}

impl IntoErrno for DeviceNotFoundError {
    fn into_errno(self) -> fposix::Errno {
        fposix::Errno::Enodev
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum ForwardingConversionError {
    DeviceNotFound,
    TypeMismatch,
    Subnet(SubnetError),
    AddrClassError,
}

impl From<DeviceNotFoundError> for ForwardingConversionError {
    fn from(_: DeviceNotFoundError) -> Self {
        ForwardingConversionError::DeviceNotFound
    }
}

impl From<SubnetError> for ForwardingConversionError {
    fn from(err: SubnetError) -> Self {
        ForwardingConversionError::Subnet(err)
    }
}

impl From<AddrClassError> for ForwardingConversionError {
    fn from(_: AddrClassError) -> Self {
        ForwardingConversionError::AddrClassError
    }
}

impl From<ForwardingConversionError> for fidl_net_stack::Error {
    fn from(fwd_error: ForwardingConversionError) -> Self {
        match fwd_error {
            ForwardingConversionError::DeviceNotFound => fidl_net_stack::Error::NotFound,
            ForwardingConversionError::TypeMismatch
            | ForwardingConversionError::Subnet(_)
            | ForwardingConversionError::AddrClassError => fidl_net_stack::Error::InvalidArgs,
        }
    }
}

impl TryFromFidlWithContext<fidl_net_stack::ForwardingEntry> for AddableEntryEither<DeviceId> {
    type Error = ForwardingConversionError;

    fn try_from_fidl_with_ctx<C: ConversionContext>(
        ctx: &C,
        fidl: fidl_net_stack::ForwardingEntry,
    ) -> Result<AddableEntryEither<DeviceId>, ForwardingConversionError> {
        let fidl_net_stack::ForwardingEntry { subnet, device_id, next_hop, metric: _ } = fidl;
        let subnet = subnet.try_into_core()?;
        let device =
            NonZeroU64::new(device_id).map(|d| d.get().try_into_core_with_ctx(ctx)).transpose()?;
        let next_hop: Option<SpecifiedAddr<IpAddr>> =
            next_hop.map(|next_hop| (*next_hop).try_into_core()).transpose()?;
        AddableEntryEither::new(subnet, device, next_hop.map(Into::into))
            .ok_or(ForwardingConversionError::TypeMismatch)
    }
}

impl TryIntoFidlWithContext<fidl_net_stack::ForwardingEntry> for EntryEither<DeviceId> {
    type Error = DeviceNotFoundError;

    fn try_into_fidl_with_ctx<C: ConversionContext>(
        self,
        ctx: &C,
    ) -> Result<fidl_net_stack::ForwardingEntry, DeviceNotFoundError> {
        let (subnet, device, gateway) = self.into_subnet_device_gateway();
        let device_id = device.try_into_fidl_with_ctx(ctx)?;
        let next_hop = gateway.map(|next_hop| {
            let next_hop: SpecifiedAddr<IpAddr> = next_hop.into();
            Box::new(next_hop.get().into_fidl())
        });
        Ok(fidl_net_stack::ForwardingEntry {
            subnet: subnet.into_fidl(),
            device_id,
            next_hop,
            metric: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use fidl_fuchsia_net as fidl_net;
    use net_types::ip::{Ipv4Addr, Ipv6Addr};

    use super::*;

    struct FakeConversionContext {
        binding: u64,
        core: DeviceId,
    }

    impl ConversionContext for FakeConversionContext {
        fn get_core_id(&self, binding_id: u64) -> Option<DeviceId> {
            if binding_id == self.binding {
                Some(self.core)
            } else {
                None
            }
        }

        fn get_binding_id(&self, core_id: DeviceId) -> Option<u64> {
            if self.core == core_id {
                Some(self.binding)
            } else {
                None
            }
        }
    }

    struct EmptyFakeConversionContext;
    impl ConversionContext for EmptyFakeConversionContext {
        fn get_core_id(&self, _binding_id: u64) -> Option<DeviceId> {
            None
        }

        fn get_binding_id(&self, _core_id: DeviceId) -> Option<u64> {
            None
        }
    }

    fn create_addr_v4(bytes: [u8; 4]) -> (IpAddr, fidl_net::IpAddress) {
        let core = IpAddr::V4(Ipv4Addr::from(bytes));
        let fidl = fidl_net::IpAddress::Ipv4(fidl_net::Ipv4Address { addr: bytes });
        (core, fidl)
    }

    fn create_addr_v6(bytes: [u8; 16]) -> (IpAddr, fidl_net::IpAddress) {
        let core = IpAddr::V6(Ipv6Addr::from(bytes));
        let fidl = fidl_net::IpAddress::Ipv6(fidl_net::Ipv6Address { addr: bytes });
        (core, fidl)
    }

    fn create_subnet(
        subnet: (IpAddr, fidl_net::IpAddress),
        prefix: u8,
    ) -> (SubnetEither, fidl_net::Subnet) {
        let (core, fidl) = subnet;
        (
            SubnetEither::new(core, prefix).unwrap(),
            fidl_net::Subnet { addr: fidl, prefix_len: prefix },
        )
    }

    fn create_addr_subnet(
        addr: (IpAddr, fidl_net::IpAddress),
        prefix: u8,
    ) -> (AddrSubnetEither, fidl_net::Subnet) {
        let (core, fidl) = addr;
        (
            AddrSubnetEither::new(core, prefix).unwrap(),
            fidl_net::Subnet { addr: fidl, prefix_len: prefix },
        )
    }

    #[test]
    fn test_addr_v4() {
        let bytes = [192, 168, 0, 1];
        let (core, fidl) = create_addr_v4(bytes);

        assert_eq!(core, fidl.into_core());
        assert_eq!(fidl, core.into_fidl());
    }

    #[test]
    fn test_addr_v6() {
        let bytes = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        let (core, fidl) = create_addr_v6(bytes);

        assert_eq!(core, fidl.into_core());
        assert_eq!(fidl, core.into_fidl());
    }

    #[test]
    fn test_addr_subnet_v4() {
        let bytes = [192, 168, 0, 1];
        let prefix = 24;
        let (core, fidl) = create_addr_subnet(create_addr_v4(bytes), prefix);

        assert_eq!(fidl, core.into_fidl());
        assert_eq!(core, fidl.try_into_core().unwrap());
    }

    #[test]
    fn test_addr_subnet_v6() {
        let bytes = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        let prefix = 64;
        let (core, fidl) = create_addr_subnet(create_addr_v6(bytes), prefix);

        assert_eq!(fidl, core.into_fidl());
        assert_eq!(core, fidl.try_into_core().unwrap());
    }

    #[test]
    fn test_subnet_v4() {
        let bytes = [192, 168, 0, 0];
        let prefix = 24;
        let (core, fidl) = create_subnet(create_addr_v4(bytes), prefix);

        assert_eq!(fidl, core.into_fidl());
        assert_eq!(core, fidl.try_into_core().unwrap());
    }

    #[test]
    fn test_subnet_v6() {
        let bytes = [1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0];
        let prefix = 64;
        let (core, fidl) = create_subnet(create_addr_v6(bytes), prefix);

        assert_eq!(fidl, core.into_fidl());
        assert_eq!(core, fidl.try_into_core().unwrap());
    }
}
