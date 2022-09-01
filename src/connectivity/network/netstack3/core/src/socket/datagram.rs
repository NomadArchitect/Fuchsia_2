// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Shared code for implementing datagram sockets.

use alloc::collections::HashSet;
use core::{
    fmt::Debug,
    hash::Hash,
    num::{NonZeroU16, NonZeroU8},
};

use derivative::Derivative;
use net_types::{ip::IpAddress, MulticastAddr, MulticastAddress as _, SpecifiedAddr, ZonedAddr};

use crate::{
    algorithm::ProtocolFlowId,
    data_structures::{
        id_map::{Entry as IdMapEntry, IdMap, OccupiedEntry as IdMapOccupied},
        socketmap::Tagged,
    },
    error::{LocalAddressError, ZonedAddressError},
    ip::{
        socket::{IpSock, IpSockDefinition, SendOptions},
        HopLimits, IpDeviceId, IpExt,
    },
    socket::{
        self,
        address::{ConnAddr, ConnIpAddr, ListenerIpAddr},
        AddrVec, Bound, BoundSocketMap, ExistsError, InsertError, ListenerAddr, SocketMapAddrSpec,
        SocketMapAddrStateSpec, SocketMapConflictPolicy, SocketMapStateSpec, SocketTypeState as _,
        SocketTypeStateMut as _,
    },
};

/// Datagram socket storage.
#[derive(Derivative)]
#[derivative(Default(bound = ""))]
pub(crate) struct DatagramSockets<A: SocketMapAddrSpec, S: DatagramSocketStateSpec>
where
    Bound<S>: Tagged<AddrVec<A>>,
{
    pub(crate) bound: BoundSocketMap<A, S>,
    pub(crate) unbound: IdMap<UnboundSocketState<A::IpAddr, A::DeviceId, S::UnboundSharingState>>,
}

#[derive(Debug, Derivative)]
#[derivative(Default(bound = "S: Default"))]
pub(crate) struct UnboundSocketState<A: IpAddress, D, S> {
    pub(crate) device: Option<D>,
    pub(crate) sharing: S,
    pub(crate) ip_options: IpOptions<A, D>,
}

#[derive(Debug)]
pub(crate) struct ListenerState<A: Eq + Hash, D: Hash + Eq> {
    pub(crate) ip_options: IpOptions<A, D>,
}

#[derive(Debug)]
pub(crate) struct ConnState<I: IpExt, D: Eq + Hash> {
    pub(crate) socket: IpSock<I, D, IpOptions<I::Addr, D>>,
}

#[derive(Clone, Debug, Derivative)]
#[derivative(Default(bound = ""))]
pub(crate) struct IpOptions<A, D> {
    multicast_memberships: MulticastMemberships<A, D>,
    hop_limits: SocketHopLimits,
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct SocketHopLimits {
    unicast: Option<NonZeroU8>,
    // TODO(https://fxbug.dev/108323): Make this an Option<u8> to allow sending
    // multicast packets destined only for the local machine.
    multicast: Option<NonZeroU8>,
}

impl SocketHopLimits {
    pub(crate) fn set_unicast(value: Option<NonZeroU8>) -> impl FnOnce(&mut Self) {
        move |limits| limits.unicast = value
    }

    pub(crate) fn set_multicast(value: Option<NonZeroU8>) -> impl FnOnce(&mut Self) {
        move |limits| limits.multicast = value
    }

    fn get_limits_with_defaults(&self, defaults: &HopLimits) -> HopLimits {
        let Self { unicast, multicast } = self;
        HopLimits {
            unicast: unicast.unwrap_or(defaults.unicast),
            multicast: multicast.unwrap_or(defaults.multicast),
        }
    }
}

impl<A: IpAddress, D> SendOptions<A::Version> for IpOptions<A, D> {
    fn hop_limit(&self, destination: &SpecifiedAddr<A>) -> Option<NonZeroU8> {
        if destination.is_multicast() {
            self.hop_limits.multicast
        } else {
            self.hop_limits.unicast
        }
    }
}

#[derive(Clone, Debug, Derivative)]
#[derivative(Default(bound = ""))]
pub(crate) struct MulticastMemberships<A, D>(HashSet<(MulticastAddr<A>, D)>);

pub(crate) enum MulticastMembershipChange {
    Join,
    Leave,
}

impl<A: Eq + Hash, D: Eq + Hash> MulticastMemberships<A, D> {
    pub(crate) fn apply_membership_change(
        &mut self,
        address: MulticastAddr<A>,
        device: D,
        want_membership: bool,
    ) -> Option<MulticastMembershipChange> {
        let Self(map) = self;
        if want_membership {
            map.insert((address, device)).then(|| MulticastMembershipChange::Join)
        } else {
            map.remove(&(address, device)).then(|| MulticastMembershipChange::Leave)
        }
    }
}

impl<A: Eq + Hash, D: Eq + Hash> IntoIterator for MulticastMemberships<A, D> {
    type Item = (MulticastAddr<A>, D);
    type IntoIter = <HashSet<(MulticastAddr<A>, D)> as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        let Self(memberships) = self;
        memberships.into_iter()
    }
}

impl<A: IpAddress, D: IpDeviceId> ConnAddr<A, D, NonZeroU16, NonZeroU16> {
    pub(crate) fn from_protocol_flow_and_local_port(
        id: &ProtocolFlowId<A>,
        local_port: NonZeroU16,
    ) -> Self {
        Self {
            ip: ConnIpAddr {
                local: (*id.local_addr(), local_port),
                remote: (*id.remote_addr(), id.remote_port()),
            },
            device: None,
        }
    }
}

fn leave_all_joined_groups<
    A: SocketMapAddrSpec,
    S: DatagramSocketStateSpec,
    C: DatagramStateNonSyncContext<A>,
    SC: DatagramStateContext<A, C, S>,
>(
    sync_ctx: &mut SC,
    ctx: &mut C,
    memberships: MulticastMemberships<A::IpAddr, A::DeviceId>,
) {
    for (addr, device) in memberships {
        sync_ctx.leave_multicast_group(ctx, device, addr)
    }
}

pub(crate) trait DatagramStateContext<A: SocketMapAddrSpec, C, S> {
    /// Requests that the specified device join the given multicast group.
    ///
    /// If this method is called multiple times with the same device and
    /// address, the device will remain joined to the multicast group until
    /// [`UdpContext::leave_multicast_group`] has been called the same number of times.
    fn join_multicast_group(
        &mut self,
        ctx: &mut C,
        device: A::DeviceId,
        addr: MulticastAddr<A::IpAddr>,
    );

    /// Requests that the specified device leave the given multicast group.
    ///
    /// Each call to this method must correspond to an earlier call to
    /// [`UdpContext::join_multicast_group`]. The device remains a member of the
    /// multicast group so long as some call to `join_multicast_group` has been
    /// made without a corresponding call to `leave_multicast_group`.
    fn leave_multicast_group(
        &mut self,
        ctx: &mut C,
        device: A::DeviceId,
        addr: MulticastAddr<A::IpAddr>,
    );

    /// Gets the ID for the device that has the given address assigned.
    ///
    /// Returns `None` if no such device exists.
    fn get_device_with_assigned_addr(&self, addr: SpecifiedAddr<A::IpAddr>) -> Option<A::DeviceId>;

    /// Calls the function with an immutable reference to the datagram sockets.
    fn with_sockets<O, F: FnOnce(&DatagramSockets<A, S>) -> O>(&self, cb: F) -> O;

    /// Calls the function with a mutable reference to the datagram sockets.
    fn with_sockets_mut<O, F: FnOnce(&mut DatagramSockets<A, S>) -> O>(&mut self, cb: F) -> O;

    fn get_default_hop_limits(&self, device: Option<A::DeviceId>) -> HopLimits;
}

pub(crate) trait DatagramStateNonSyncContext<A: SocketMapAddrSpec> {
    /// Attempts to allocate an identifier for a listener.
    ///
    /// `is_available` checks whether the provided address could be used without
    /// conflicting with any existing entries in state context's socket map,
    /// returning an error otherwise.
    fn try_alloc_listen_identifier(
        &mut self,
        is_available: impl Fn(A::LocalIdentifier) -> Result<(), InUseError>,
    ) -> Option<A::LocalIdentifier>;
}

pub(crate) trait DatagramSocketStateSpec: SocketMapStateSpec {
    type UnboundId: Clone + From<usize> + Into<usize> + Debug;
    type UnboundSharingState: Default;
}

pub(crate) trait DatagramSocketSpec<A: SocketMapAddrSpec>:
    DatagramSocketStateSpec<
        ListenerState = ListenerState<A::IpAddr, A::DeviceId>,
        ConnState = ConnState<A::IpVersion, A::DeviceId>,
    > + SocketMapConflictPolicy<
        ListenerAddr<A::IpAddr, A::DeviceId, A::LocalIdentifier>,
        <Self as SocketMapStateSpec>::ListenerSharingState,
        A,
    > + SocketMapConflictPolicy<
        ConnAddr<A::IpAddr, A::DeviceId, A::LocalIdentifier, A::RemoteIdentifier>,
        <Self as SocketMapStateSpec>::ConnSharingState,
        A,
    >
{
}

impl<A, SS> DatagramSocketSpec<A> for SS
where
    A: SocketMapAddrSpec,
    SS: DatagramSocketStateSpec<
            ListenerState = ListenerState<A::IpAddr, A::DeviceId>,
            ConnState = ConnState<A::IpVersion, A::DeviceId>,
        > + SocketMapConflictPolicy<
            ListenerAddr<A::IpAddr, A::DeviceId, A::LocalIdentifier>,
            <Self as SocketMapStateSpec>::ListenerSharingState,
            A,
        > + SocketMapConflictPolicy<
            ConnAddr<A::IpAddr, A::DeviceId, A::LocalIdentifier, A::RemoteIdentifier>,
            <Self as SocketMapStateSpec>::ConnSharingState,
            A,
        >,
{
}

pub(crate) struct InUseError;

pub(crate) fn create_unbound<
    A: SocketMapAddrSpec,
    S: DatagramSocketStateSpec,
    C,
    SC: DatagramStateContext<A, C, S>,
>(
    sync_ctx: &mut SC,
) -> S::UnboundId
where
    Bound<S>: Tagged<AddrVec<A>>,
{
    sync_ctx.with_sockets_mut(|DatagramSockets { unbound, bound: _ }| {
        unbound.push(UnboundSocketState::default()).into()
    })
}

pub(crate) fn remove_unbound<
    A: SocketMapAddrSpec,
    S: DatagramSocketStateSpec,
    C: DatagramStateNonSyncContext<A>,
    SC: DatagramStateContext<A, C, S>,
>(
    sync_ctx: &mut SC,
    ctx: &mut C,
    id: S::UnboundId,
) where
    Bound<S>: Tagged<AddrVec<A>>,
{
    let UnboundSocketState { device: _, sharing: _, ip_options } =
        sync_ctx.with_sockets_mut(|state| {
            let DatagramSockets { unbound, bound: _ } = state;
            unbound.remove(id.into()).expect("invalid UDP unbound ID")
        });
    let IpOptions { multicast_memberships, hop_limits: _ } = ip_options;

    leave_all_joined_groups(sync_ctx, ctx, multicast_memberships);
}

pub(crate) fn remove_listener<
    A: SocketMapAddrSpec,
    C: DatagramStateNonSyncContext<A>,
    SC: DatagramStateContext<A, C, S>,
    S: DatagramSocketSpec<A>,
>(
    sync_ctx: &mut SC,
    ctx: &mut C,
    id: S::ListenerId,
) -> ListenerAddr<A::IpAddr, A::DeviceId, A::LocalIdentifier>
where
    Bound<S>: Tagged<AddrVec<A>>,
    S::ListenerAddrState:
        SocketMapAddrStateSpec<Id = S::ListenerId, SharingState = S::ListenerSharingState>,
{
    let (ListenerState { ip_options }, addr) = sync_ctx.with_sockets_mut(|state| {
        let DatagramSockets { bound, unbound: _ } = state;
        let (state, _, addr): (_, S::ListenerSharingState, _) =
            bound.listeners_mut().remove(&id).expect("Invalid UDP listener ID");
        (state, addr)
    });
    let IpOptions { multicast_memberships, hop_limits: _ } = ip_options;

    leave_all_joined_groups(sync_ctx, ctx, multicast_memberships);
    addr
}

pub(crate) fn remove_conn<
    A: SocketMapAddrSpec,
    C: DatagramStateNonSyncContext<A>,
    SC: DatagramStateContext<A, C, S>,
    S: DatagramSocketSpec<A>,
>(
    sync_ctx: &mut SC,
    ctx: &mut C,
    id: S::ConnId,
) -> ConnAddr<A::IpAddr, A::DeviceId, A::LocalIdentifier, A::RemoteIdentifier>
where
    Bound<S>: Tagged<AddrVec<A>>,
    S::ConnAddrState: SocketMapAddrStateSpec<Id = S::ConnId, SharingState = S::ConnSharingState>,
    A::IpVersion: IpExt,
{
    let (ConnState { socket }, addr) = sync_ctx.with_sockets_mut(|state| {
        let DatagramSockets { bound, unbound: _ } = state;
        let (state, _sharing, addr): (_, S::ConnSharingState, _) =
            bound.conns_mut().remove(&id).expect("UDP connection not found");
        (state, addr)
    });
    let (_, IpOptions { multicast_memberships, hop_limits: _ }): (IpSockDefinition<_, _>, _) =
        socket.into_definition_options();

    leave_all_joined_groups(sync_ctx, ctx, multicast_memberships);
    addr
}

/// Returns the address and device that should be used for a socket.
///
/// Given an address for a socket and an optional device that the socket is
/// already bound on, returns the address and device that should be used
/// for the socket. If `addr` and `device` require inconsistent devices,
/// or if `addr` requires a zone but there is none specified (by `addr` or
/// `device`), an error is returned.
pub(crate) fn resolve_addr_with_device<A: IpAddress, D: PartialEq>(
    addr: ZonedAddr<A, D>,
    device: Option<D>,
) -> Result<(SpecifiedAddr<A>, Option<D>), ZonedAddressError> {
    let (addr, zone) = addr.into_addr_zone();
    let device = match (zone, device) {
        (Some(zone), Some(device)) => {
            if zone != device {
                return Err(ZonedAddressError::DeviceZoneMismatch);
            }
            Some(device)
        }
        (Some(zone), None) => Some(zone),
        (None, Some(device)) => Some(device),
        (None, None) => {
            if socket::must_have_zone(&addr) {
                return Err(ZonedAddressError::RequiredZoneNotProvided);
            } else {
                None
            }
        }
    };
    Ok((addr, device))
}

pub(crate) fn listen<
    A: SocketMapAddrSpec,
    C: DatagramStateNonSyncContext<A>,
    SC: DatagramStateContext<A, C, S>,
    S: DatagramSocketSpec<A>,
>(
    sync_ctx: &mut SC,
    ctx: &mut C,
    id: S::UnboundId,
    addr: Option<ZonedAddr<A::IpAddr, A::DeviceId>>,
    local_id: Option<A::LocalIdentifier>,
) -> Result<S::ListenerId, LocalAddressError>
where
    Bound<S>: Tagged<AddrVec<A>>,
    S::ListenerAddrState:
        SocketMapAddrStateSpec<Id = S::ListenerId, SharingState = S::ListenerSharingState>,
    S::UnboundSharingState: Clone + Into<S::ListenerSharingState>,
    S::ListenerSharingState: Default,
{
    // Borrow the state immutably so that sync_ctx can be used to make other
    // calls inside the closure.
    let (addr, device, identifier) =
        sync_ctx.with_sockets(|DatagramSockets { bound, unbound }| {
            let identifier = match local_id {
                Some(local_id) => Ok(local_id),
                None => {
                    let addr = addr.map(|addr| addr.into_addr_zone().0);
                    let sharing_options = Default::default();
                    ctx.try_alloc_listen_identifier(|identifier| {
                        let check_addr =
                            ListenerAddr { device: None, ip: ListenerIpAddr { identifier, addr } };
                        bound.listeners().could_insert(&check_addr, &sharing_options).map_err(|e| {
                            match e {
                                InsertError::Exists
                                | InsertError::IndirectConflict
                                | InsertError::ShadowAddrExists
                                | InsertError::ShadowerExists => InUseError,
                            }
                        })
                    })
                }
                .ok_or(LocalAddressError::FailedToAllocateLocalPort),
            }?;

            let UnboundSocketState { device, sharing: _, ip_options: _ } = unbound
                .get(id.clone().into())
                .unwrap_or_else(|| panic!("unbound ID {:?} is invalid", id));
            Ok(match addr {
                Some(addr) => {
                    // Extract the specified address and the device. The device
                    // is either the one from the address or the one to which
                    // the socket was previously bound.
                    let (addr, device) = resolve_addr_with_device(addr, *device)?;

                    // Binding to multicast addresses is allowed regardless.
                    // Other addresses can only be bound to if they are assigned
                    // to the device.
                    if !addr.is_multicast() {
                        let assigned_to = sync_ctx
                            .get_device_with_assigned_addr(addr)
                            .ok_or(LocalAddressError::CannotBindToAddress)?;
                        if device.map_or(false, |device| assigned_to != device) {
                            return Err(LocalAddressError::AddressMismatch);
                        }
                    }
                    (Some(addr), device, identifier)
                }
                None => (None, *device, identifier),
            })
        })?;

    // Re-borrow the state mutably.
    // TODO(https://fxbug.dev/108008): Combine this with the above when
    // with_sockets_mut provides an additional context argument.
    sync_ctx.with_sockets_mut(
        |DatagramSockets { unbound, bound,  }| {
            let unbound_entry = match unbound.entry(id.clone().into()) {
                IdMapEntry::Vacant(_) => panic!("unbound ID {:?} is invalid", id),
                IdMapEntry::Occupied(o) => o,
            };

            /// Wrapper for an occupied entry that implements Into::into by
            /// removing from the entry.
            struct TakeMemberships<'a, A: IpAddress, D, S>(IdMapOccupied<'a, usize, UnboundSocketState<A, D, S>>, );

            impl<'a, A: IpAddress, D: Hash + Eq, S> From<TakeMemberships<'a, A, D, S>> for ListenerState<A, D> {
                fn from(TakeMemberships(entry): TakeMemberships<'a, A, D, S>) -> Self {
                    let UnboundSocketState {device: _, sharing: _, ip_options} = entry.remove();
                    ListenerState {ip_options}
                }
            }

            let UnboundSocketState { device: _, sharing, ip_options: _ } =
                unbound_entry.get();
                let sharing = sharing.clone();
            match bound
                .listeners_mut()
                .try_insert(
                    ListenerAddr { ip: ListenerIpAddr { addr, identifier }, device },
                    // Passing TakeMemberships defers removal of unbound_entry
                    // until try_insert is known to be able to succeed.
                    TakeMemberships(unbound_entry),
                    sharing.into(),
                )
                .map_err(|(e, state, _sharing): (_, _, S::ListenerSharingState)| (e, state))
            {
                Ok(listener) => Ok(listener),
                Err((e, TakeMemberships(entry))) => {
                    // Drop the occupied entry, leaving it in the unbound socket
                    // IdMap.
                    let _: (InsertError, IdMapOccupied<'_, _, _>) = (e, entry);
                    Err(LocalAddressError::AddressInUse)
                }
            }
        },
    )
}

#[derive(Derivative)]
#[derivative(Clone(bound = ""), Debug(bound = ""))]
pub(crate) enum DatagramBoundId<S: DatagramSocketStateSpec> {
    Listener(S::ListenerId),
    Connected(S::ConnId),
}

pub(crate) fn set_bound_device<
    A: SocketMapAddrSpec,
    C: DatagramStateNonSyncContext<A>,
    SC: DatagramStateContext<A, C, S>,
    S: DatagramSocketSpec<A>,
>(
    sync_ctx: &mut SC,
    _ctx: &mut C,
    id: impl Into<DatagramBoundId<S>>,
    device_id: Option<A::DeviceId>,
) -> Result<(), LocalAddressError>
where
    Bound<S>: Tagged<AddrVec<A>>,
    S::ListenerAddrState:
        SocketMapAddrStateSpec<Id = S::ListenerId, SharingState = S::ListenerSharingState>,
    S::ConnAddrState: SocketMapAddrStateSpec<Id = S::ConnId, SharingState = S::ConnSharingState>,
{
    sync_ctx.with_sockets_mut(|state| {
        let DatagramSockets { bound, unbound: _ } = state;
        let id = id.into();

        // Don't allow changing the device if one of the IP addresses in the socket
        // address vector requires a zone (scope ID).
        let (device, must_have_zone) = match &id {
            DatagramBoundId::Listener(id) => {
                let (_, _, addr): &(S::ListenerState, S::ListenerSharingState, _) = bound
                    .listeners()
                    .get_by_id(&id)
                    .unwrap_or_else(|| panic!("invalid listener ID {:?}", id));
                let ListenerAddr { device, ip: ListenerIpAddr { addr, identifier: _ } } = addr;
                (device, addr.as_ref().map_or(false, socket::must_have_zone))
            }
            DatagramBoundId::Connected(id) => {
                let (_, _, addr): &(S::ConnState, S::ConnSharingState, _) = bound
                    .conns()
                    .get_by_id(&id)
                    .unwrap_or_else(|| panic!("invalid conn ID {:?}", id));
                let ConnAddr {
                    device,
                    ip: ConnIpAddr { local: (local_ip, _), remote: (remote_ip, _) },
                } = addr;
                (device, [local_ip, remote_ip].into_iter().any(|a| socket::must_have_zone(a)))
            }
        };

        if device != &device_id && must_have_zone {
            return Err(LocalAddressError::Zone(ZonedAddressError::DeviceZoneMismatch));
        }

        match id {
            DatagramBoundId::Listener(id) => bound
                .listeners_mut()
                .try_update_addr(&id, |ListenerAddr { ip, device: _ }| ListenerAddr {
                    ip,
                    device: device_id,
                })
                .map_err(|ExistsError {}| LocalAddressError::AddressInUse),
            DatagramBoundId::Connected(id) => bound
                .conns_mut()
                .try_update_addr(&id, |ConnAddr { ip, device: _ }| ConnAddr {
                    ip,
                    device: device_id,
                })
                .map_err(|ExistsError| LocalAddressError::AddressInUse),
        }
    })
}

/// Error resulting from attempting to change multicast membership settings for
/// a socket.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SetMulticastMembershipError {
    /// The provided address does not match the provided device.
    AddressNotAvailable,
    /// The provided address does not match any address on the host.
    NoDeviceWithAddress,
    /// No device or address was specified and there is no device with a route
    /// to the multicast address.
    NoDeviceAvailable,
    /// The requested membership change had no effect (tried to leave a group
    /// without joining, or to join a group again).
    NoMembershipChange,
    /// The socket is bound to a device that doesn't match the one specified.
    WrongDevice,
}

fn pick_matching_interface<
    A: SocketMapAddrSpec,
    S,
    C: DatagramStateNonSyncContext<A>,
    SC: DatagramStateContext<A, C, S>,
>(
    sync_ctx: &SC,
    interface: MulticastInterfaceSelector<A::IpAddr, A::DeviceId>,
) -> Result<Option<A::DeviceId>, SetMulticastMembershipError> {
    match interface {
        MulticastInterfaceSelector::Interface(device) => Ok(Some(device)),
        MulticastInterfaceSelector::AnyInterfaceWithRoute => Ok(None),
        MulticastInterfaceSelector::LocalAddress(addr) => sync_ctx
            .get_device_with_assigned_addr(addr)
            .ok_or(SetMulticastMembershipError::NoDeviceWithAddress)
            .map(Some),
    }
}

fn pick_interface_for_addr<
    A: SocketMapAddrSpec,
    S,
    C: DatagramStateNonSyncContext<A>,
    SC: DatagramStateContext<A, C, S>,
>(
    _sync_ctx: &SC,
    _addr: MulticastAddr<A::IpAddr>,
) -> Option<A::DeviceId> {
    log_unimplemented!(
        (),
        "https://fxbug.dev/39479: Implement this by passing `_addr` through the routing table."
    );
    None
}

#[derive(Derivative)]
#[derivative(Clone(bound = "S::UnboundId: Clone, S::ListenerId: Clone, S::ConnId: Clone"))]
pub(crate) enum DatagramSocketId<S: DatagramSocketStateSpec> {
    Unbound(S::UnboundId),
    Bound(DatagramBoundId<S>),
}

/// Selector for the device to affect when changing multicast membership
/// settings.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MulticastInterfaceSelector<A, D> {
    /// Use the device with the assigned address.
    LocalAddress(SpecifiedAddr<A>),
    /// Use the device with the specified identifier.
    Interface(D),
    /// Pick any device with a route to the multicast target address.
    AnyInterfaceWithRoute,
}

/// Sets the specified socket's membership status for the given group.
///
/// If `id` is unbound, the membership state will take effect when it is bound.
/// An error is returned if the membership change request is invalid (e.g.
/// leaving a group that was not joined, or joining a group multiple times) or
/// if the device to use to join is unspecified or conflicts with the existing
/// socket state.
pub(crate) fn set_multicast_membership<
    A: SocketMapAddrSpec,
    C: DatagramStateNonSyncContext<A>,
    SC: DatagramStateContext<A, C, S>,
    S: DatagramSocketSpec<A>,
>(
    sync_ctx: &mut SC,
    ctx: &mut C,
    id: impl Into<DatagramSocketId<S>>,
    multicast_group: MulticastAddr<A::IpAddr>,
    interface: MulticastInterfaceSelector<A::IpAddr, A::DeviceId>,
    want_membership: bool,
) -> Result<(), SetMulticastMembershipError>
where
    Bound<S>: Tagged<AddrVec<A>>,
    S::ListenerAddrState:
        SocketMapAddrStateSpec<Id = S::ListenerId, SharingState = S::ListenerSharingState>,
    S::ConnAddrState: SocketMapAddrStateSpec<Id = S::ConnId, SharingState = S::ConnSharingState>,
{
    let id = id.into();
    let interface = pick_matching_interface(sync_ctx, interface)?;

    let interface = sync_ctx.with_sockets(|state| {
        let DatagramSockets { bound, unbound } = state;
        let bound_device = match id.clone() {
            DatagramSocketId::Unbound(id) => {
                let UnboundSocketState { device, sharing: _, ip_options: _ } =
                    unbound.get(id.into()).expect("unbound UDP socket not found");
                device
            }
            DatagramSocketId::Bound(DatagramBoundId::Listener(id)) => {
                let (_, _, ListenerAddr { ip: _, device }): &(
                    ListenerState<_, _>,
                    S::ListenerSharingState,
                    _,
                ) = bound.listeners().get_by_id(&id).expect("Listening socket not found");
                device
            }
            DatagramSocketId::Bound(DatagramBoundId::Connected(id)) => {
                let (_, _, ConnAddr { ip: _, device }): &(ConnState<_, _>, S::ConnSharingState, _) =
                    bound.conns().get_by_id(&id).expect("Connected socket not found");
                device
            }
        };

        // If the socket is bound to a device, check that against the provided
        // interface ID. If none was provided, use the bound device. If there is
        // none, try to pick a device using the provided address.
        match (bound_device, interface) {
            (Some(bound_device), None) => Ok(*bound_device),
            (None, Some(interface)) => Ok(interface),
            (Some(bound_device), Some(interface)) => (*bound_device == interface)
                .then(|| interface)
                .ok_or(SetMulticastMembershipError::WrongDevice),
            (None, None) => pick_interface_for_addr(sync_ctx, multicast_group)
                .ok_or(SetMulticastMembershipError::NoDeviceAvailable),
        }
    })?;

    // Re-borrow the state mutably here now that we have picked an interface.
    // This can be folded into the above if we can teach our context traits that
    // the UDP state can be borrowed while the interface picking code runs.
    let change = sync_ctx.with_sockets_mut(|state| {
        let DatagramSockets { bound, unbound } = state;
        let ip_options = match id {
            DatagramSocketId::Unbound(id) => {
                let UnboundSocketState { device: _, sharing: _, ip_options } =
                    unbound.get_mut(id.into()).expect("unbound UDP socket not found");
                ip_options
            }

            DatagramSocketId::Bound(DatagramBoundId::Listener(id)) => {
                let (ListenerState { ip_options }, _, _): (
                    _,
                    &S::ListenerSharingState,
                    &ListenerAddr<_, _, _>,
                ) = bound.listeners_mut().get_by_id_mut(&id).expect("Listening socket not found");
                ip_options
            }
            DatagramSocketId::Bound(DatagramBoundId::Connected(id)) => {
                let (ConnState { socket }, _, _): (_, &S::ConnSharingState, &ConnAddr<_, _, _, _>) =
                    bound.conns_mut().get_by_id_mut(&id).expect("Connected socket not found");
                socket.options_mut()
            }
        };

        let IpOptions { multicast_memberships, hop_limits: _ } = ip_options;
        multicast_memberships
            .apply_membership_change(multicast_group, interface, want_membership)
            .ok_or(SetMulticastMembershipError::NoMembershipChange)
    })?;

    match change {
        MulticastMembershipChange::Join => {
            sync_ctx.join_multicast_group(ctx, interface, multicast_group)
        }
        MulticastMembershipChange::Leave => {
            sync_ctx.leave_multicast_group(ctx, interface, multicast_group)
        }
    }

    Ok(())
}

fn get_options_device<A: SocketMapAddrSpec, S: DatagramSocketSpec<A>>(
    DatagramSockets { bound, unbound }: &DatagramSockets<A, S>,
    id: DatagramSocketId<S>,
) -> (&IpOptions<A::IpAddr, A::DeviceId>, &Option<A::DeviceId>)
where
    Bound<S>: Tagged<AddrVec<A>>,
    S::ListenerAddrState:
        SocketMapAddrStateSpec<Id = S::ListenerId, SharingState = S::ListenerSharingState>,
    S::ConnAddrState: SocketMapAddrStateSpec<Id = S::ConnId, SharingState = S::ConnSharingState>,
    A::IpVersion: IpExt,
{
    match id {
        DatagramSocketId::Unbound(id) => {
            let UnboundSocketState { ip_options, device, sharing: _ } =
                unbound.get(id.into()).expect("unbound UDP socket not found");
            (ip_options, device)
        }
        DatagramSocketId::Bound(DatagramBoundId::Listener(id)) => {
            let (ListenerState { ip_options }, _, ListenerAddr { device, ip: _ }): &(
                _,
                S::ListenerSharingState,
                _,
            ) = bound.listeners().get_by_id(&id).expect("listening socket not found");
            (ip_options, device)
        }
        DatagramSocketId::Bound(DatagramBoundId::Connected(id)) => {
            let (ConnState { socket }, _, ConnAddr { device, ip: _ }): &(
                _,
                S::ConnSharingState,
                _,
            ) = bound.conns().get_by_id(&id).expect("connected socket not found");
            (socket.options(), device)
        }
    }
}

fn get_options_mut<A: SocketMapAddrSpec, S: DatagramSocketSpec<A>>(
    DatagramSockets { bound, unbound }: &mut DatagramSockets<A, S>,
    id: DatagramSocketId<S>,
) -> &mut IpOptions<A::IpAddr, A::DeviceId>
where
    Bound<S>: Tagged<AddrVec<A>>,
    S::ListenerAddrState:
        SocketMapAddrStateSpec<Id = S::ListenerId, SharingState = S::ListenerSharingState>,
    S::ConnAddrState: SocketMapAddrStateSpec<Id = S::ConnId, SharingState = S::ConnSharingState>,
    A::IpVersion: IpExt,
{
    match id {
        DatagramSocketId::Unbound(id) => {
            let UnboundSocketState { ip_options, device: _, sharing: _ } =
                unbound.get_mut(id.into()).expect("unbound UDP socket not found");
            ip_options
        }
        DatagramSocketId::Bound(DatagramBoundId::Listener(id)) => {
            let (ListenerState { ip_options }, _, _): (
                _,
                &S::ListenerSharingState,
                &ListenerAddr<_, _, _>,
            ) = bound.listeners_mut().get_by_id_mut(&id).expect("listening socket not found");
            ip_options
        }
        DatagramSocketId::Bound(DatagramBoundId::Connected(id)) => {
            let (ConnState { socket }, _, _): (_, &S::ConnSharingState, &ConnAddr<_, _, _, _>) =
                bound.conns_mut().get_by_id_mut(&id).expect("connected socket not found");
            socket.options_mut()
        }
    }
}

pub(crate) fn update_ip_hop_limit<
    A: SocketMapAddrSpec,
    SC: DatagramStateContext<A, C, S>,
    C: DatagramStateNonSyncContext<A>,
    S: DatagramSocketSpec<A>,
>(
    sync_ctx: &mut SC,
    _ctx: &mut C,
    id: impl Into<DatagramSocketId<S>>,
    update: impl FnOnce(&mut SocketHopLimits),
) where
    Bound<S>: Tagged<AddrVec<A>>,
    S::ListenerAddrState:
        SocketMapAddrStateSpec<Id = S::ListenerId, SharingState = S::ListenerSharingState>,
    S::ConnAddrState: SocketMapAddrStateSpec<Id = S::ConnId, SharingState = S::ConnSharingState>,
    A::IpVersion: IpExt,
{
    sync_ctx.with_sockets_mut(|sockets| {
        let options = get_options_mut(sockets, id.into());

        update(&mut options.hop_limits)
    })
}

pub(crate) fn get_ip_hop_limits<
    A: SocketMapAddrSpec,
    SC: DatagramStateContext<A, C, S>,
    C: DatagramStateNonSyncContext<A>,
    S: DatagramSocketSpec<A>,
>(
    sync_ctx: &SC,
    _ctx: &C,
    id: impl Into<DatagramSocketId<S>>,
) -> HopLimits
where
    Bound<S>: Tagged<AddrVec<A>>,
    S::ListenerAddrState:
        SocketMapAddrStateSpec<Id = S::ListenerId, SharingState = S::ListenerSharingState>,
    S::ConnAddrState: SocketMapAddrStateSpec<Id = S::ConnId, SharingState = S::ConnSharingState>,
    A::IpVersion: IpExt,
{
    sync_ctx.with_sockets(|sockets| {
        let (options, device) = get_options_device(sockets, id.into());
        let IpOptions { hop_limits, multicast_memberships: _ } = options;
        hop_limits.get_limits_with_defaults(&sync_ctx.get_default_hop_limits(*device))
    })
}

#[cfg(test)]
mod test {
    use alloc::vec::Vec;
    use core::{convert::Infallible as Never, marker::PhantomData};

    use derivative::Derivative;
    use ip_test_macro::ip_test;
    use net_types::ip::{Ip, Ipv4, Ipv6};
    use nonzero_ext::nonzero;

    use crate::{
        context::testutil::DummyInstant,
        data_structures::socketmap::SocketMap,
        ip::{
            device::state::IpDeviceStateIpExt, socket::testutil::DummyIpSocketCtx, DummyDeviceId,
        },
        socket::{IncompatibleError, InsertError, RemoveResult},
        testutil::DummyNonSyncCtx,
    };

    use super::*;

    trait DatagramIpExt: Ip + IpExt + IpDeviceStateIpExt<DummyInstant> {}

    impl DatagramIpExt for Ipv4 {}
    impl DatagramIpExt for Ipv6 {}

    struct DummyAddrSpec<I, D>(Never, PhantomData<(I, D)>);

    impl<I: IpExt, D: IpDeviceId> SocketMapAddrSpec for DummyAddrSpec<I, D> {
        type DeviceId = D;
        type IpAddr = I::Addr;
        type IpVersion = I;
        type LocalIdentifier = u8;
        type RemoteIdentifier = char;
    }

    struct DummyStateSpec<I, D>(Never, PhantomData<(I, D)>);

    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    struct Tag;

    #[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
    struct Sharing;

    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    struct Id<T>(usize, PhantomData<T>);

    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    struct Conn;
    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    struct Listen;
    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    struct Unbound;

    impl<S> From<usize> for Id<S> {
        fn from(u: usize) -> Self {
            Self(u, PhantomData)
        }
    }

    impl<S> From<Id<S>> for usize {
        fn from(Id(u, _): Id<S>) -> Self {
            u
        }
    }

    impl<I: DatagramIpExt, D: IpDeviceId> SocketMapStateSpec for DummyStateSpec<I, D> {
        type AddrVecTag = Tag;
        type ConnAddrState = Id<Conn>;
        type ConnId = Id<Conn>;
        type ConnSharingState = Sharing;
        type ConnState = ConnState<I, D>;
        type ListenerAddrState = Id<Listen>;
        type ListenerId = Id<Listen>;
        type ListenerSharingState = Sharing;
        type ListenerState = ListenerState<I::Addr, D>;
    }

    impl<A, S> Tagged<A> for Id<S> {
        type Tag = Tag;
        fn tag(&self, _address: &A) -> Self::Tag {
            Tag
        }
    }

    impl<I: DatagramIpExt, D: IpDeviceId> From<Id<Conn>> for DatagramSocketId<DummyStateSpec<I, D>> {
        fn from(u: Id<Conn>) -> Self {
            DatagramSocketId::Bound(DatagramBoundId::Connected(u))
        }
    }

    impl<I: DatagramIpExt, D: IpDeviceId> From<Id<Listen>> for DatagramSocketId<DummyStateSpec<I, D>> {
        fn from(u: Id<Listen>) -> Self {
            DatagramSocketId::Bound(DatagramBoundId::Listener(u))
        }
    }

    impl<I: DatagramIpExt, D: IpDeviceId> From<Id<Unbound>> for DatagramSocketId<DummyStateSpec<I, D>> {
        fn from(u: Id<Unbound>) -> Self {
            DatagramSocketId::Unbound(u)
        }
    }

    impl<I: DatagramIpExt, D: IpDeviceId> DatagramSocketStateSpec for DummyStateSpec<I, D> {
        type UnboundId = Id<Unbound>;
        type UnboundSharingState = Sharing;
    }

    impl<A, I: DatagramIpExt, D: IpDeviceId>
        SocketMapConflictPolicy<A, Sharing, DummyAddrSpec<I, D>> for DummyStateSpec<I, D>
    {
        fn check_for_conflicts(
            _new_sharing_state: &Sharing,
            _addr: &A,
            _socketmap: &SocketMap<AddrVec<DummyAddrSpec<I, D>>, Bound<Self>>,
        ) -> Result<(), InsertError>
        where
            Bound<Self>: Tagged<AddrVec<DummyAddrSpec<I, D>>>,
        {
            // Addresses are completely independent and shadowing doesn't cause
            // conflicts.
            Ok(())
        }
    }

    impl<S> SocketMapAddrStateSpec for Id<S> {
        type Id = Self;
        type SharingState = Sharing;
        fn new(_sharing: &Self::SharingState, id: Self) -> Self {
            id
        }
        fn try_get_dest<'a, 'b>(
            &'b mut self,
            _new_sharing_state: &'a Self::SharingState,
        ) -> Result<&'b mut Vec<Self::Id>, IncompatibleError> {
            Err(IncompatibleError)
        }
        fn remove_by_id(&mut self, _id: Self::Id) -> RemoveResult {
            RemoveResult::IsLast
        }
    }

    #[derive(Derivative)]
    #[derivative(Default(bound = ""))]
    struct DummyDatagramState<I: DatagramIpExt, D: IpDeviceId> {
        sockets: DatagramSockets<DummyAddrSpec<I, D>, DummyStateSpec<I, D>>,
        state: DummyIpSocketCtx<I, D>,
    }

    const DEFAULT_HOP_LIMITS: HopLimits =
        HopLimits { unicast: nonzero!(34u8), multicast: nonzero!(99u8) };

    impl<I: DatagramIpExt, D: IpDeviceId>
        DatagramStateContext<DummyAddrSpec<I, D>, DummyNonSyncCtx, DummyStateSpec<I, D>>
        for DummyDatagramState<I, D>
    {
        fn join_multicast_group(
            &mut self,
            _ctx: &mut DummyNonSyncCtx,
            _device: <DummyAddrSpec<I, D> as SocketMapAddrSpec>::DeviceId,
            _addr: MulticastAddr<<DummyAddrSpec<I, D> as SocketMapAddrSpec>::IpAddr>,
        ) {
            unimplemented!("not required for any existing tests")
        }

        fn leave_multicast_group(
            &mut self,
            _ctx: &mut DummyNonSyncCtx,
            _device: <DummyAddrSpec<I, D> as SocketMapAddrSpec>::DeviceId,
            _addr: MulticastAddr<<DummyAddrSpec<I, D> as SocketMapAddrSpec>::IpAddr>,
        ) {
            unimplemented!("not required for any existing tests")
        }

        fn get_device_with_assigned_addr(
            &self,
            addr: SpecifiedAddr<<DummyAddrSpec<I, D> as SocketMapAddrSpec>::IpAddr>,
        ) -> Option<<DummyAddrSpec<I, D> as SocketMapAddrSpec>::DeviceId> {
            let Self { state, sockets: _ } = self;
            state.find_device_with_addr(addr)
        }

        fn get_default_hop_limits(
            &self,
            _device: Option<<DummyAddrSpec<I, D> as SocketMapAddrSpec>::DeviceId>,
        ) -> HopLimits {
            DEFAULT_HOP_LIMITS
        }

        fn with_sockets<
            O,
            F: FnOnce(&DatagramSockets<DummyAddrSpec<I, D>, DummyStateSpec<I, D>>) -> O,
        >(
            &self,
            cb: F,
        ) -> O {
            let Self { sockets, state: _ } = self;
            cb(sockets)
        }

        fn with_sockets_mut<
            O,
            F: FnOnce(&mut DatagramSockets<DummyAddrSpec<I, D>, DummyStateSpec<I, D>>) -> O,
        >(
            &mut self,
            cb: F,
        ) -> O {
            let Self { sockets, state: _ } = self;
            cb(sockets)
        }
    }

    impl<I: IpExt> DatagramStateNonSyncContext<DummyAddrSpec<I, DummyDeviceId>> for DummyNonSyncCtx {
        fn try_alloc_listen_identifier(
            &mut self,
            _is_available: impl Fn(u8) -> Result<(), InUseError>,
        ) -> Option<<DummyAddrSpec<I, DummyDeviceId> as SocketMapAddrSpec>::LocalIdentifier>
        {
            unimplemented!("not required for any existing tests")
        }
    }

    #[ip_test]
    fn set_get_hop_limits<I: Ip + DatagramIpExt>() {
        let mut sync_ctx = DummyDatagramState::<I, DummyDeviceId>::default();
        let mut non_sync_ctx = DummyNonSyncCtx::default();

        let unbound = create_unbound(&mut sync_ctx);
        const EXPECTED_HOP_LIMITS: HopLimits =
            HopLimits { unicast: nonzero!(45u8), multicast: nonzero!(23u8) };

        update_ip_hop_limit(&mut sync_ctx, &mut non_sync_ctx, unbound, |limits| {
            *limits = SocketHopLimits {
                unicast: Some(EXPECTED_HOP_LIMITS.unicast),
                multicast: Some(EXPECTED_HOP_LIMITS.multicast),
            }
        });

        assert_eq!(get_ip_hop_limits(&sync_ctx, &non_sync_ctx, unbound), EXPECTED_HOP_LIMITS);
    }

    #[ip_test]
    fn default_hop_limits<I: Ip + DatagramIpExt>() {
        let mut sync_ctx = DummyDatagramState::<I, DummyDeviceId>::default();
        let mut non_sync_ctx = DummyNonSyncCtx::default();

        let unbound = create_unbound(&mut sync_ctx);
        assert_eq!(get_ip_hop_limits(&sync_ctx, &non_sync_ctx, unbound), DEFAULT_HOP_LIMITS);

        update_ip_hop_limit(&mut sync_ctx, &mut non_sync_ctx, unbound, |limits| {
            *limits =
                SocketHopLimits { unicast: Some(nonzero!(1u8)), multicast: Some(nonzero!(1u8)) }
        });

        // The limits no longer match the default.
        assert_ne!(get_ip_hop_limits(&sync_ctx, &non_sync_ctx, unbound), DEFAULT_HOP_LIMITS);

        // Clear the hop limits set on the socket.
        update_ip_hop_limit(&mut sync_ctx, &mut non_sync_ctx, unbound, |limits| {
            *limits = Default::default()
        });

        // The values should be back at the defaults.
        assert_eq!(get_ip_hop_limits(&sync_ctx, &non_sync_ctx, unbound), DEFAULT_HOP_LIMITS);
    }
}
