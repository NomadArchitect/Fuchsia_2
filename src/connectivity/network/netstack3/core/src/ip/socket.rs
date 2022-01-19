// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! IPv4 and IPv6 sockets.

use core::cmp::Ordering;
use core::marker::PhantomData;
use core::num::NonZeroU8;

use net_types::ip::{Ip, Ipv4, Ipv4Addr, Ipv6, Ipv6Addr};
use net_types::{SpecifiedAddr, UnicastAddr};
use packet::{BufferMut, Serializer};
use packet_formats::ip::{Ipv4Proto, Ipv6Proto};
use packet_formats::{ipv4::Ipv4PacketBuilder, ipv6::Ipv6PacketBuilder};
use rand::Rng;
use thiserror::Error;

use crate::device::{AddressEntry, DeviceId};
use crate::ip::{forwarding::ForwardingTable, IpExt, Ipv6SocketData};
use crate::socket::Socket;
use crate::{BufferDispatcher, Ctx, EventDispatcher};

/// A socket identifying a connection between a local and remote IP host.
pub(crate) trait IpSocket<I: Ip>: Socket<UpdateError = IpSockCreationError> {
    /// Get the local IP address.
    fn local_ip(&self) -> &SpecifiedAddr<I::Addr>;

    /// Get the remote IP address.
    fn remote_ip(&self) -> &SpecifiedAddr<I::Addr>;
}

/// An execution context defining a type of IP socket.
pub(crate) trait IpSocketContext<I: IpExt> {
    // TODO(joshlf): Remove `Clone` bound once we're no longer cloning sockets.
    type IpSocket: IpSocket<I> + Clone;

    /// A builder carrying optional parameters passed to [`new_ip_socket`].
    ///
    /// [`new_ip_socket`]: crate::ip::socket::IpSocketContext::new_ip_socket
    type Builder: Default;

    /// Constructs a new [`Self::IpSocket`].
    ///
    /// `new_ip_socket` constructs a new `Self::IpSocket` to the given remote IP
    /// address from the given local IP address with the given IP protocol. If
    /// no local IP address is given, one will be chosen automatically.
    ///
    /// `new_ip_socket` returns an error if no route to the remote was found in
    /// the forwarding table, if the given local IP address is not valid for the
    /// found route, or if the remote is a loopback address (which is not
    /// currently supported, but will be in the future). Currently, this is true
    /// regardless of the value of `unroutable_behavior`. Eventually, this will
    /// be changed.
    ///
    /// The builder may be used to override certain default parameters. Passing
    /// `None` for the `builder` parameter is equivalent to passing
    /// `Some(Default::default())`.
    fn new_ip_socket(
        &mut self,
        local_ip: Option<SpecifiedAddr<I::Addr>>,
        remote_ip: SpecifiedAddr<I::Addr>,
        proto: I::Proto,
        unroutable_behavior: UnroutableBehavior,
        builder: Option<Self::Builder>,
    ) -> Result<Self::IpSocket, IpSockCreationError>;
}

/// An error in sending a packet on an IP socket.
#[derive(Error, Copy, Clone, Debug, Eq, PartialEq)]
pub enum IpSockSendError {
    /// An MTU was exceeded.
    ///
    /// This could be caused by an MTU at any layer of the stack, including both
    /// device MTUs and packet format body size limits.
    #[error("a maximum transmission unit (MTU) was exceeded")]
    Mtu,
    /// The socket is currently unroutable.
    #[error("the socket is currently unroutable: {}", _0)]
    Unroutable(#[from] IpSockUnroutableError),
}

/// An error in sending a packet on a temporary IP socket.
#[derive(Error, Copy, Clone, Debug)]
pub enum IpSockCreateAndSendError {
    /// An MTU was exceeded.
    ///
    /// This could be caused by an MTU at any layer of the stack, including both
    /// device MTUs and packet format body size limits.
    #[error("a maximum transmission unit (MTU) was exceeded")]
    Mtu,
    /// The temporary socket could not be created.
    #[error("the temporary socket could not be created: {}", _0)]
    Create(#[from] IpSockCreationError),
}

pub(crate) trait BufferIpSocketContext<I: IpExt, B: BufferMut>: IpSocketContext<I> {
    /// Send an IP packet on a socket.
    ///
    /// The generated packet has its metadata initialized from `socket`,
    /// including the source and destination addresses, the Time To Live/Hop
    /// Limit, and the Protocol/Next Header. The outbound device is also chosen
    /// based on information stored in the socket.
    ///
    /// If the socket is currently unroutable, an error is returned.
    fn send_ip_packet<S: Serializer<Buffer = B>>(
        &mut self,
        socket: &Self::IpSocket,
        body: S,
    ) -> Result<(), (S, IpSockSendError)>;

    /// Creates a temporary IP socket and sends a single packet on it.
    ///
    /// `local_ip`, `remote_ip`, `proto`, and `builder` are passed directly to
    /// [`IpSocketContext::new_ip_socket`]. `get_body_from_src_ip` is given the
    /// source IP address for the packet - which may have been chosen
    /// automatically if `local_ip` is `None` - and returns the body to be
    /// encapsulated. This is provided in case the body's contents depend on the
    /// chosen source IP address.
    ///
    /// # Errors
    ///
    /// If an error is encountered while sending the packet, the body returned
    /// from `get_body_from_src_ip` will be returned along with the error. If an
    /// error is encountered while constructing the temporary IP socket,
    /// `get_body_from_src_ip` will be called on an arbitrary IP address in
    /// order to obtain a body to return. In the case where a buffer was passed
    /// by ownership to `get_body_from_src_ip`, this allows the caller to
    /// recover that buffer.
    fn send_oneshot_ip_packet<S: Serializer<Buffer = B>, F: FnOnce(SpecifiedAddr<I::Addr>) -> S>(
        &mut self,
        local_ip: Option<SpecifiedAddr<I::Addr>>,
        remote_ip: SpecifiedAddr<I::Addr>,
        proto: I::Proto,
        builder: Option<Self::Builder>,
        get_body_from_src_ip: F,
    ) -> Result<(), (S, IpSockCreateAndSendError)> {
        // We use a `match` instead of `map_err` because `map_err` would require passing a closure
        // which takes ownership of `get_body_from_src_ip`, which we also use in the success case.
        match self.new_ip_socket(local_ip, remote_ip, proto, UnroutableBehavior::Close, builder) {
            Err(err) => Err((get_body_from_src_ip(I::LOOPBACK_ADDRESS), err.into())),
            Ok(tmp) => self.send_ip_packet(&tmp, get_body_from_src_ip(*tmp.local_ip())).map_err(
                |(body, err)| match err {
                    IpSockSendError::Mtu => (body, IpSockCreateAndSendError::Mtu),
                    IpSockSendError::Unroutable(_) => {
                        unreachable!("socket which was just created should still be routable")
                    }
                },
            ),
        }
    }
}

/// What should a socket do when it becomes unroutable?
///
/// `UnroutableBehavior` describes how a socket is configured to behave if it
/// becomes unroutable. In particular, this affects the behavior of
/// [`Socket::apply_update`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum UnroutableBehavior {
    /// The socket should stay open.
    ///
    /// When a call to [`Socket::apply_update`] results in the socket becoming
    /// unroutable, the socket will remain open, and `apply_update` will return
    /// `Ok`.
    ///
    /// So long as the socket is unroutable, attempting to send packets will
    /// fail on a per-packet basis. If the socket later becomes routable again,
    /// these operations will succeed again.
    #[allow(dead_code)]
    StayOpen,
    /// The socket should close.
    ///
    /// When a call to [`Socket::apply_update`] results in the socket becoming
    /// unroutable, the socket will be closed - `apply_update` will return
    /// `Err`.
    Close,
}

/// An error encountered when creating an IP socket.
#[derive(Error, Copy, Clone, Debug, Eq, PartialEq)]
pub enum IpSockCreationError {
    /// The specified local IP address is not a unicast address in its subnet.
    ///
    /// For IPv4, this means that the address is a member of a subnet to which
    /// we are attached, but in that subnet, it is not a unicast address. For
    /// IPv6, whether or not an address is unicast is a property of the address
    /// and does not depend on what subnets we're attached to.
    #[error("the specified local IP address is not a unicast address in its subnet")]
    LocalAddrNotUnicast,
    /// No local IP address was specified, and one could not be automatically
    /// selected.
    #[error("a local IP address could not be automatically selected")]
    NoLocalAddrAvailable,
    /// The socket is unroutable.
    #[error("the socket is unroutable: {}", _0)]
    Unroutable(#[from] IpSockUnroutableError),
}

/// An error encountered when attempting to compute the routing information on
/// an IP socket.
///
/// An `IpSockUnroutableError` can occur when creating a socket or when updating
/// an existing socket in response to changes to the forwarding table or to the
/// set of IP addresses assigned to devices.
#[derive(Error, Copy, Clone, Debug, Eq, PartialEq)]
pub enum IpSockUnroutableError {
    /// The specified local IP address is not one of our assigned addresses.
    ///
    /// For IPv6, this error will also be returned if the specified local IP
    /// address exists on one of our devices, but it is in the "temporary"
    /// state.
    #[error("the specified local IP address is not one of our assigned addresses")]
    LocalAddrNotAssigned,
    /// No route exists to the specified remote IP address.
    #[error("no route exists to the remote IP address")]
    NoRouteToRemoteAddr,
}

/// A builder for IPv4 sockets.
///
/// [`IpSocketContext::new_ip_socket`] accepts optional configuration in the
/// form of a `SocketBuilder`. All configurations have default values that are
/// used if a custom value is not provided.
#[derive(Default)]
pub(crate) struct Ipv4SocketBuilder {
    // NOTE(joshlf): These fields are `Option`s rather than being set to a
    // default value in `Default::default` because global defaults may be set
    // per-stack at runtime, meaning that default values cannot be known at
    // compile time.
    ttl: Option<NonZeroU8>,
}

impl Ipv4SocketBuilder {
    /// Set the Time to Live (TTL) field that will be set on outbound IPv4
    /// packets.
    ///
    /// The TTL must be non-zero. Per [RFC 1122 Section 3.2.1.7] and [RFC 1812
    /// Section 4.2.2.9], hosts and routers (respectively) must not originate
    /// IPv4 packets with a TTL of zero.
    ///
    /// [RFC 1122 Section 3.2.1.7]: https://tools.ietf.org/html/rfc1122#section-3.2.1.7
    /// [RFC 1812 Section 4.2.2.9]: https://tools.ietf.org/html/rfc1812#section-4.2.2.9
    #[allow(dead_code)] // TODO(joshlf): Remove once this is used
    pub(crate) fn ttl(&mut self, ttl: NonZeroU8) -> &mut Ipv4SocketBuilder {
        self.ttl = Some(ttl);
        self
    }
}

/// A builder for IPv6 sockets.
///
/// [`IpSocketContext::new_ip_socket`] accepts optional configuration in the
/// form of a `SocketBuilder`. All configurations have default values that are
/// used if a custom value is not provided.
#[derive(Default)]
pub struct Ipv6SocketBuilder {
    // NOTE(joshlf): These fields are `Option`s rather than being set to a
    // default value in `Default::default` because global defaults may be set
    // per-stack at runtime, meaning that default values cannot be known at
    // compile time.
    hop_limit: Option<u8>,
}

impl Ipv6SocketBuilder {
    /// Set the Hop Limit field that will be set on outbound IPv6 packets.
    #[allow(dead_code)] // TODO(joshlf): Remove once this is used
    pub(crate) fn hop_limit(&mut self, hop_limit: u8) -> &mut Ipv6SocketBuilder {
        self.hop_limit = Some(hop_limit);
        self
    }
}

/// The production implementation of the [`IpSocket`] trait.
#[derive(Clone)]
#[cfg_attr(test, derive(Debug, PartialEq))]
pub struct IpSock<I: IpExt, D> {
    // TODO(joshlf): This struct is larger than it needs to be. `remote_ip`,
    // `local_ip`, and `proto` are all stored in `cached`'s `builder` when it's
    // `Routable`. Instead, we could move these into the `Unroutable` variant of
    // `CachedInfo`.
    defn: IpSockDefinition<I>,
    // This is `Ok` if the socket is currently routable and `Err` otherwise. If
    // `unroutable_behavior` is `Close`, then this is guaranteed to be `Ok`.
    cached: Result<CachedInfo<I, D>, IpSockUnroutableError>,
}

/// The definition of an IP socket.
///
/// These values are part of the socket's definition, and never change.
#[derive(Clone)]
#[cfg_attr(test, derive(Debug, PartialEq))]
struct IpSockDefinition<I: IpExt> {
    remote_ip: SpecifiedAddr<I::Addr>,
    // Guaranteed to be unicast in its subnet since it's always equal to an
    // address assigned to the local device. We can't use the `UnicastAddr`
    // witness type since `Ipv4Addr` doesn't implement `UnicastAddress`.
    //
    // TODO(joshlf): Support unnumbered interfaces. Once we do that, a few
    // issues arise: A) Does the unicast restriction still apply, and is that
    // even well-defined for IPv4 in the absence of a subnet? B) Presumably we
    // have to always bind to a particular interface?
    local_ip: SpecifiedAddr<I::Addr>,
    hop_limit: u8,
    proto: I::Proto,
    #[cfg_attr(not(test), allow(unused))]
    unroutable_behavior: UnroutableBehavior,
    per_proto_data: I::SocketData,
}

/// Information which is cached inside an [`IpSock`].
///
/// This information is cached as an optimization, but is not part of the
/// definition of the socket.
#[derive(Clone)]
#[cfg_attr(test, derive(Debug, PartialEq))]
#[allow(unused)]
struct CachedInfo<I: IpExt, D> {
    builder: I::PacketBuilder,
    device: D,
    next_hop: SpecifiedAddr<I::Addr>,
}

/// An update to the information cached in an [`IpSock`].
///
/// Whenever IP-layer information changes that might affect `IpSock`s, an
/// `IpSockUpdate` is emitted, and clients are responsible for applying this
/// update to all `IpSock`s that they are responsible for.
pub struct IpSockUpdate<I: IpExt> {
    // Currently, `IpSockUpdate`s only represent a single type of update: that
    // the forwarding table or assignment of IP addresses to devices have
    // changed in some way. Thus, this is a ZST.
    _marker: PhantomData<I>,
}

impl<I: IpExt> IpSockUpdate<I> {
    /// Constructs a new `IpSocketUpdate`.
    pub(crate) fn new() -> IpSockUpdate<I> {
        IpSockUpdate { _marker: PhantomData }
    }
}

impl<I: IpExt, D> Socket for IpSock<I, D> {
    type Update = IpSockUpdate<I>;
    type UpdateMeta = ForwardingTable<I, D>;
    type UpdateError = IpSockCreationError;

    fn apply_update(
        &mut self,
        _update: &IpSockUpdate<I>,
        _meta: &ForwardingTable<I, D>,
    ) -> Result<(), IpSockCreationError> {
        // NOTE(joshlf): Currently, with this unimplemented, we will panic if we
        // ever try to update the forwarding table or the assignment of IP
        // addresses to devices while sockets are installed. However, updates
        // that happen while no sockets are installed will still succeed. This
        // should allow us to continue testing Netstack3 until we implement this
        // method.
        unimplemented!()
    }
}

impl<I: IpExt, D> IpSocket<I> for IpSock<I, D> {
    fn local_ip(&self) -> &SpecifiedAddr<I::Addr> {
        &self.defn.local_ip
    }

    fn remote_ip(&self) -> &SpecifiedAddr<I::Addr> {
        &self.defn.remote_ip
    }
}

/// Apply an update to all IPv4 sockets.
///
/// `update_all_ipv4_sockets` applies the given socket update to all IPv4
/// sockets in existence. It does this by delegating to every module that is
/// responsible for storing IPv4 sockets.
pub(crate) fn update_all_ipv4_sockets<D: EventDispatcher>(
    ctx: &mut Ctx<D>,
    update: IpSockUpdate<Ipv4>,
) {
    crate::ip::icmp::update_all_ipv4_sockets(ctx, update);
}

/// Apply an update to all IPv6 sockets.
///
/// `update_all_ipv6_sockets` applies the given socket update to all IPv6
/// sockets in existence. It does this by delegating to every module that is
/// responsible for storing IPv6 sockets.
pub(crate) fn update_all_ipv6_sockets<D: EventDispatcher>(
    ctx: &mut Ctx<D>,
    update: IpSockUpdate<Ipv6>,
) {
    crate::ip::icmp::update_all_ipv6_sockets(ctx, update);
}

// TODO(joshlf): Once we support configuring transport-layer protocols using
// type parameters, use that to ensure that `proto` is the right protocol for
// the caller. We will still need to have a separate enforcement mechanism for
// raw IP sockets once we support those.

/// Computes the [`CachedInfo`] for an IPv4 socket based on its definition.
///
/// Returns an error if the socket is currently unroutable.
fn compute_ipv4_cached_info<D: EventDispatcher>(
    ctx: &Ctx<D>,
    defn: &IpSockDefinition<Ipv4>,
) -> Result<CachedInfo<Ipv4, DeviceId>, IpSockUnroutableError> {
    super::lookup_route(ctx, defn.remote_ip)
        .ok_or(IpSockUnroutableError::NoRouteToRemoteAddr)
        .and_then(|dst| {
            if !crate::device::get_assigned_ip_addr_subnets::<_, Ipv4Addr>(ctx, dst.device)
                .any(|addr_sub| defn.local_ip == addr_sub.addr())
            {
                return Err(IpSockUnroutableError::LocalAddrNotAssigned);
            }

            Ok(CachedInfo {
                builder: Ipv4PacketBuilder::new(
                    defn.local_ip,
                    defn.remote_ip,
                    defn.hop_limit,
                    defn.proto,
                ),
                device: dst.device,
                next_hop: dst.next_hop,
            })
        })
}

/// Computes the [`CachedInfo`] for an IPv6 socket based on its definition.
///
/// Returns an error if the socket is currently unroutable.
fn compute_ipv6_cached_info<D: EventDispatcher>(
    ctx: &Ctx<D>,
    defn: &IpSockDefinition<Ipv6>,
) -> Result<CachedInfo<Ipv6, DeviceId>, IpSockUnroutableError> {
    super::lookup_route(ctx, defn.remote_ip)
        .ok_or(IpSockUnroutableError::NoRouteToRemoteAddr)
        .and_then(|dst| {
            // TODO(joshlf):
            // - Allow the specified local IP to be the local IP of a
            //   different device so long as we're operating in the weak
            //   host model.
            // - What about when the socket is bound to a device? How does
            //   that affect things?
            if crate::device::get_ip_addr_state(ctx, dst.device, &defn.local_ip)
                .map_or(true, |state| state.is_tentative())
            {
                return Err(IpSockUnroutableError::LocalAddrNotAssigned);
            }

            let mut builder =
                Ipv6PacketBuilder::new(defn.local_ip, defn.remote_ip, defn.hop_limit, defn.proto);
            builder.flowlabel(defn.per_proto_data.flow_label);

            Ok(CachedInfo { builder, device: dst.device, next_hop: dst.next_hop })
        })
}

impl<D: EventDispatcher> IpSocketContext<Ipv4> for Ctx<D> {
    type IpSocket = IpSock<Ipv4, DeviceId>;

    type Builder = Ipv4SocketBuilder;

    fn new_ip_socket(
        &mut self,
        local_ip: Option<SpecifiedAddr<Ipv4Addr>>,
        remote_ip: SpecifiedAddr<Ipv4Addr>,
        proto: Ipv4Proto,
        unroutable_behavior: UnroutableBehavior,
        builder: Option<Ipv4SocketBuilder>,
    ) -> Result<IpSock<Ipv4, DeviceId>, IpSockCreationError> {
        let builder = builder.unwrap_or_default();

        if Ipv4::LOOPBACK_SUBNET.contains(&remote_ip) {
            return Err(IpSockUnroutableError::NoRouteToRemoteAddr.into());
        }

        let local_ip = super::lookup_route(self, remote_ip)
            .ok_or(IpSockUnroutableError::NoRouteToRemoteAddr)
            .and_then(|dst| {
                if let Some(local_ip) = local_ip {
                    if crate::device::get_assigned_ip_addr_subnets::<_, Ipv4Addr>(self, dst.device)
                        .any(|addr_sub| local_ip == addr_sub.addr())
                    {
                        Ok(local_ip)
                    } else {
                        return Err(IpSockUnroutableError::LocalAddrNotAssigned);
                    }
                } else {
                    // TODO(joshlf): Are we sure that a device route can never
                    // be set for a device without an IP address? At the least,
                    // this is not currently enforced anywhere, and is a DoS
                    // vector.
                    Ok(crate::device::get_ip_addr_subnet(self, dst.device)
                        .expect("IP device route set for device without IP address")
                        .addr())
                }
            })?;

        let defn = IpSockDefinition {
            local_ip,
            remote_ip,
            hop_limit: builder.ttl.unwrap_or(super::DEFAULT_TTL).get(),
            proto,
            unroutable_behavior,
            per_proto_data: (),
        };
        let cached = compute_ipv4_cached_info(self, &defn)?;
        Ok(IpSock { defn, cached: Ok(cached) })
    }
}

impl<D: EventDispatcher> IpSocketContext<Ipv6> for Ctx<D> {
    type IpSocket = IpSock<Ipv6, DeviceId>;

    type Builder = Ipv6SocketBuilder;

    fn new_ip_socket(
        &mut self,
        local_ip: Option<SpecifiedAddr<Ipv6Addr>>,
        remote_ip: SpecifiedAddr<Ipv6Addr>,
        proto: Ipv6Proto,
        unroutable_behavior: UnroutableBehavior,
        builder: Option<Ipv6SocketBuilder>,
    ) -> Result<IpSock<Ipv6, DeviceId>, IpSockCreationError> {
        let builder = builder.unwrap_or_default();

        if Ipv6::LOOPBACK_SUBNET.contains(&remote_ip) {
            return Err(IpSockUnroutableError::NoRouteToRemoteAddr.into());
        }

        let local_ip = super::lookup_route(self, remote_ip)
            .ok_or(IpSockUnroutableError::NoRouteToRemoteAddr.into())
            .and_then(|dst| {
                if let Some(local_ip) = local_ip {
                    // TODO(joshlf):
                    // - Allow the specified local IP to be the local IP of a
                    //   different device so long as we're operating in the weak
                    //   host model.
                    // - What about when the socket is bound to a device? How does
                    //   that affect things?
                    //
                    // TODO(fxbug.dev/69196): Give `local_ip` the type
                    // `Option<UnicastAddr<Ipv6Addr>>` instead of doing this dynamic
                    // check here.
                    let local_ip = UnicastAddr::from_witness(local_ip)
                        .ok_or(IpSockCreationError::LocalAddrNotUnicast)?;
                    if crate::device::get_ip_addr_state(
                        self,
                        dst.device,
                        &local_ip.into_specified(),
                    )
                    .map_or(true, |state| state.is_tentative())
                    {
                        return Err(IpSockUnroutableError::LocalAddrNotAssigned.into());
                    }
                    Ok(local_ip)
                } else {
                    // TODO(joshlf):
                    // - If device binding is used, then we should only consider the
                    //   addresses of the device being bound to.
                    // - If we are operating in the strong host model, then perhaps
                    //   we should restrict ourselves to addresses associated with
                    //   the device found by looking up the remote in the forwarding
                    //   table? This I'm less sure of.

                    ipv6_source_address_selection::select_ipv6_source_address(
                        remote_ip,
                        dst.device,
                        crate::device::list_devices(self)
                            .map(|device_id| {
                                crate::device::get_ipv6_addr_subnets(self, device_id)
                                    .map(move |a| (a, device_id))
                            })
                            .flatten(),
                    )
                    .ok_or(IpSockCreationError::NoLocalAddrAvailable)
                }
            })?;

        let defn = IpSockDefinition {
            local_ip: local_ip.into_specified(),
            remote_ip,
            hop_limit: builder.hop_limit.unwrap_or(super::DEFAULT_TTL.get()),
            proto,
            unroutable_behavior,
            per_proto_data: Ipv6SocketData {
                flow_label: gen_ipv6_flowlabel(self.dispatcher.rng_mut()),
            },
        };
        let cached = compute_ipv6_cached_info(self, &defn)?;
        Ok(IpSock { defn, cached: Ok(cached) })
    }
}

/// Generates a new IPv6 flow label using the provided random number generator.
///
/// As specified by [RFC 6437 Section 2], flow labels should be non-zero,
/// 20 bits in length, and generated from a discrete uniform distribution.
///
/// [RFC 6437 Section 2]: https://tools.ietf.org/html/rfc6437#section-2
fn gen_ipv6_flowlabel<R: Rng>(rng: &mut R) -> u32 {
    rng.gen_range(1..1 << Ipv6::FLOW_LABEL_BITS)
}

impl<B: BufferMut, D: BufferDispatcher<B>> BufferIpSocketContext<Ipv4, B> for Ctx<D> {
    fn send_ip_packet<S: Serializer<Buffer = B>>(
        &mut self,
        socket: &IpSock<Ipv4, DeviceId>,
        body: S,
    ) -> Result<(), (S, IpSockSendError)> {
        // TODO(joshlf): Call `trace!` with relevant fields from the socket.
        increment_counter!(self, "send_ipv4_packet");
        // TODO(joshlf): Handle loopback sockets.
        assert!(!Ipv4::LOOPBACK_SUBNET.contains(&socket.defn.remote_ip));
        // If the remote IP is non-loopback but the local IP is loopback, that
        // implies a bug elsewhere - when we resolve the local IP in
        // `new_ipv4_socket`, if the remote IP is not a loopback IP, then we
        // should never choose a loopback IP as our local IP.
        assert!(!Ipv4::LOOPBACK_SUBNET.contains(&socket.defn.local_ip));

        match &socket.cached {
            Ok(CachedInfo { builder, device, next_hop }) => {
                // Tentative addresses are not considered bound to an interface
                // in the traditional sense. Therefore, no packet should have a
                // source IP set to a tentative address. This should be enforced
                // by the `IpSock` being kept up to date.
                debug_assert!(!crate::device::is_addr_tentative_on_device(
                    self,
                    &socket.defn.local_ip,
                    *device
                ));

                let mut builder = builder.clone();
                builder.id(self.state.ipv4.gen_next_packet_id());

                crate::device::send_ip_frame(self, *device, *next_hop, body.encapsulate(builder))
                    .map_err(|ser| (ser.into_inner(), IpSockSendError::Mtu))
            }
            Err(err) => Err((body, (*err).into())),
        }
    }
}

impl<B: BufferMut, D: BufferDispatcher<B>> BufferIpSocketContext<Ipv6, B> for Ctx<D> {
    fn send_ip_packet<S: Serializer<Buffer = B>>(
        &mut self,
        socket: &IpSock<Ipv6, DeviceId>,
        body: S,
    ) -> Result<(), (S, IpSockSendError)> {
        // TODO(joshlf): Call `trace!` with relevant fields from the socket.
        increment_counter!(self, "send_ipv6_packet");
        // TODO(joshlf): Handle loopback sockets.
        assert!(!Ipv6::LOOPBACK_SUBNET.contains(&socket.defn.remote_ip));
        // If the remote IP is non-loopback but the local IP is loopback, that
        // implies a bug elsewhere - when we resolve the local IP in
        // `new_ipv6_socket`, if the remote IP is not a loopback IP, then we
        // should never choose a loopback IP as our local IP.
        assert!(!Ipv6::LOOPBACK_SUBNET.contains(&socket.defn.local_ip));

        match &socket.cached {
            Ok(CachedInfo { builder, device, next_hop }) => {
                // Tentative addresses are not considered bound to an interface
                // in the traditional sense. Therefore, no packet should have a
                // source IP set to a tentative address. This should be enforced
                // by the `IpSock` being kept up to date.
                debug_assert!(!crate::device::is_addr_tentative_on_device(
                    self,
                    &socket.defn.local_ip,
                    *device
                ));

                crate::device::send_ip_frame(self, *device, *next_hop, body.encapsulate(builder))
                    .map_err(|ser| (ser.into_inner(), IpSockSendError::Mtu))
            }
            Err(err) => Err((body, (*err).into())),
        }
    }
}

/// IPv6 source address selection as defined in [RFC 6724 Section 5].
mod ipv6_source_address_selection {
    use net_types::ip::IpAddress as _;

    use super::*;
    use crate::device::AddressState;

    /// Selects the source address for an IPv6 socket using the algorithm
    /// defined in [RFC 6724 Section 5].
    ///
    /// This algorithm is only applicable when the user has not explicitly
    /// specified a source address.
    ///
    /// `remote_ip` is the remote IP address of the socket, `outbound_device` is
    /// the device over which outbound traffic to `remote_ip` is sent (according
    /// to the forwarding table), and `addresses` is an iterator of all
    /// addresses on all devices. The algorithm works by iterating over
    /// `addresses` and selecting the address which is most preferred according
    /// to a set of selection criteria.
    pub(super) fn select_ipv6_source_address<
        'a,
        Instant: 'a,
        I: Iterator<Item = (&'a AddressEntry<Ipv6Addr, Instant, UnicastAddr<Ipv6Addr>>, DeviceId)>,
    >(
        remote_ip: SpecifiedAddr<Ipv6Addr>,
        outbound_device: DeviceId,
        addresses: I,
    ) -> Option<UnicastAddr<Ipv6Addr>> {
        // Source address selection as defined in RFC 6724 Section 5.
        //
        // The algorithm operates by defining a partial ordering on available
        // source addresses, and choosing one of the best address as defined by
        // that ordering (given multiple best addresses, the choice from among
        // those is implementation-defined). The partial order is defined in
        // terms of a sequence of rules. If a given rule defines an order
        // between two addresses, then that is their order. Otherwise, the next
        // rule must be consulted, and so on until all of the rules are
        // exhausted.

        addresses
            // Tentative addresses are not considered available to the source
            // selection algorithm.
            .filter(|(a, _)| !a.state.is_tentative())
            .max_by(|(a, a_device), (b, b_device)| {
                select_ipv6_source_address_cmp(
                    remote_ip,
                    outbound_device,
                    a,
                    *a_device,
                    b,
                    *b_device,
                )
            })
            .map(|(addr, _device)| addr.addr_sub().addr())
    }

    /// Comparison operator used by `select_ipv6_source_address_cmp`.
    fn select_ipv6_source_address_cmp<Instant>(
        remote_ip: SpecifiedAddr<Ipv6Addr>,
        outbound_device: DeviceId,
        a: &AddressEntry<Ipv6Addr, Instant, UnicastAddr<Ipv6Addr>>,
        a_device: DeviceId,
        b: &AddressEntry<Ipv6Addr, Instant, UnicastAddr<Ipv6Addr>>,
        b_device: DeviceId,
    ) -> Ordering {
        // TODO(fxbug.dev/46822): Implement rules 2, 4, 5.5, 6, and 7.

        let a_state = a.state;
        let a_addr = a.addr_sub().addr().into_specified();
        let b_state = b.state;
        let b_addr = b.addr_sub().addr().into_specified();

        // Assertions required in order for this implementation to be valid.

        // Required by the implementation of Rule 1.
        debug_assert!(!(a_addr == remote_ip && b_addr == remote_ip));

        // Required by the implementation of Rule 3.
        debug_assert!(!a_state.is_tentative());
        debug_assert!(!b_state.is_tentative());

        rule_1(remote_ip, a_addr, b_addr)
            .then_with(|| rule_3(a_state, b_state))
            .then_with(|| rule_5(outbound_device, a_device, b_device))
            .then_with(|| rule_8(remote_ip, a, b))
    }

    // Assumes that `a` and `b` are not both equal to `remote_ip`.
    fn rule_1(
        remote_ip: SpecifiedAddr<Ipv6Addr>,
        a: SpecifiedAddr<Ipv6Addr>,
        b: SpecifiedAddr<Ipv6Addr>,
    ) -> Ordering {
        if (a == remote_ip) != (b == remote_ip) {
            // Rule 1: Prefer same address.
            //
            // Note that both `a` and `b` cannot be equal to `remote_ip` since
            // that would imply that we had added the same address twice to the
            // same device.
            //
            // If `(a == remote_ip) != (b == remote_ip)`, then exactly one of
            // them is equal. If this inequality does not hold, then they must
            // both be unequal to `remote_ip`. In the first case, we have a tie,
            // and in the second case, the rule doesn't apply. In either case,
            // we move onto the next rule.
            if a == remote_ip {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        } else {
            Ordering::Equal
        }
    }

    // Assumes that neither state is tentative.
    fn rule_3(a_state: AddressState, b_state: AddressState) -> Ordering {
        if a_state != b_state {
            // Rule 3: Avoid deprecated addresses.
            //
            // Note that, since we've already filtered out tentative addresses,
            // the only two possible states are deprecated and assigned. Thus,
            // `a_state != b_state` and `a_state.is_deprecated()` together imply
            // that `b_state` is assigned. Conversely, `a_state != b_state` and
            // `!a_state.is_deprecated()` together imply that `b_state` is
            // deprecated.
            if a_state.is_deprecated() {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        } else {
            Ordering::Equal
        }
    }

    fn rule_5(outbound_device: DeviceId, a_device: DeviceId, b_device: DeviceId) -> Ordering {
        if (a_device == outbound_device) != (b_device == outbound_device) {
            // Rule 5: Prefer outgoing interface.
            if a_device == outbound_device {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        } else {
            Ordering::Equal
        }
    }

    fn rule_8<Instant>(
        remote_ip: SpecifiedAddr<Ipv6Addr>,
        a: &AddressEntry<Ipv6Addr, Instant, UnicastAddr<Ipv6Addr>>,
        b: &AddressEntry<Ipv6Addr, Instant, UnicastAddr<Ipv6Addr>>,
    ) -> Ordering {
        // Per RFC 6724 Section 2.2:
        //
        //   We define the common prefix length CommonPrefixLen(S, D) of a
        //   source address S and a destination address D as the length of the
        //   longest prefix (looking at the most significant, or leftmost, bits)
        //   that the two addresses have in common, up to the length of S's
        //   prefix (i.e., the portion of the address not including the
        //   interface ID).  For example, CommonPrefixLen(fe80::1, fe80::2) is
        //   64.
        fn common_prefix_len<Instant>(
            src: &AddressEntry<Ipv6Addr, Instant, UnicastAddr<Ipv6Addr>>,
            dst: SpecifiedAddr<Ipv6Addr>,
        ) -> u8 {
            core::cmp::min(
                src.addr_sub().addr().common_prefix_len(&dst),
                src.addr_sub().subnet().prefix(),
            )
        }

        // Rule 8: Use longest matching prefix.
        //
        // Note that, per RFC 6724 Section 5:
        //
        //   Rule 8 MAY be superseded if the implementation has other means of
        //   choosing among source addresses.  For example, if the
        //   implementation somehow knows which source address will result in
        //   the "best" communications performance.
        //
        // We don't currently make use of this option, but it's an option for
        // the future.
        common_prefix_len(a, remote_ip).cmp(&common_prefix_len(b, remote_ip))
    }

    #[cfg(test)]
    mod tests {
        use net_types::ip::AddrSubnet;

        use super::*;
        use crate::device::AddrConfig;

        #[test]
        fn test_select_ipv6_source_address() {
            use AddressState::*;

            // Test the comparison operator used by `select_ipv6_source_address`
            // by separately testing each comparison condition.

            let remote = SpecifiedAddr::new(Ipv6Addr::from_bytes([
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 192, 168, 0, 1,
            ]))
            .unwrap();
            let local0 = SpecifiedAddr::new(Ipv6Addr::from_bytes([
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 192, 168, 0, 2,
            ]))
            .unwrap();
            let local1 = SpecifiedAddr::new(Ipv6Addr::from_bytes([
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 192, 168, 0, 3,
            ]))
            .unwrap();
            let dev0 = DeviceId::new_ethernet(0);
            let dev1 = DeviceId::new_ethernet(1);
            let dev2 = DeviceId::new_ethernet(2);

            // Rule 1: Prefer same address
            assert_eq!(rule_1(remote, remote, local0), Ordering::Greater);
            assert_eq!(rule_1(remote, local0, remote), Ordering::Less);
            assert_eq!(rule_1(remote, local0, local1), Ordering::Equal);

            // Rule 3: Avoid deprecated states
            assert_eq!(rule_3(Assigned, Deprecated), Ordering::Greater);
            assert_eq!(rule_3(Deprecated, Assigned), Ordering::Less);
            assert_eq!(rule_3(Assigned, Assigned), Ordering::Equal);
            assert_eq!(rule_3(Deprecated, Deprecated), Ordering::Equal);

            // Rule 5: Prefer outgoing interface
            assert_eq!(rule_5(dev0, dev0, dev2), Ordering::Greater);
            assert_eq!(rule_5(dev0, dev2, dev0), Ordering::Less);
            assert_eq!(rule_5(dev0, dev0, dev0), Ordering::Equal);
            assert_eq!(rule_5(dev0, dev2, dev2), Ordering::Equal);

            // Rule 8: Use longest matching prefix.
            {
                let new_addr_entry = |bytes, prefix_len| {
                    AddressEntry::<_, (), _>::new(
                        AddrSubnet::new(Ipv6Addr::from_bytes(bytes), prefix_len).unwrap(),
                        AddressState::Assigned,
                        AddrConfig::Manual,
                    )
                };

                // First, test that the longest prefix match is preferred when
                // using addresses whose common prefix length is shorter than
                // the subnet prefix length.

                // 4 leading 0x01 bytes.
                let remote = SpecifiedAddr::new(Ipv6Addr::from_bytes([
                    1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                ]))
                .unwrap();
                // 3 leading 0x01 bytes.
                let local0 = new_addr_entry([1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 64);
                // 2 leading 0x01 bytes.
                let local1 = new_addr_entry([1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 64);

                assert_eq!(rule_8(remote, &local0, &local1), Ordering::Greater);
                assert_eq!(rule_8(remote, &local1, &local0), Ordering::Less);
                assert_eq!(rule_8(remote, &local0, &local0), Ordering::Equal);
                assert_eq!(rule_8(remote, &local1, &local1), Ordering::Equal);

                // Second, test that the common prefix length is capped at the
                // subnet prefix length.

                // 3 leading 0x01 bytes, but a subnet prefix length of 8 (1 byte).
                let local0 = new_addr_entry([1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 8);
                // 2 leading 0x01 bytes, but a subnet prefix length of 8 (1 byte).
                let local1 = new_addr_entry([1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 8);

                assert_eq!(rule_8(remote, &local0, &local1), Ordering::Equal);
                assert_eq!(rule_8(remote, &local1, &local0), Ordering::Equal);
                assert_eq!(rule_8(remote, &local0, &local0), Ordering::Equal);
                assert_eq!(rule_8(remote, &local1, &local1), Ordering::Equal);
            }

            {
                let new_addr_entry = |addr| {
                    AddressEntry::<_, (), _>::new(
                        AddrSubnet::new(addr, 128).unwrap(),
                        Assigned,
                        AddrConfig::Manual,
                    )
                };

                // If no rules apply, then the two address entries are equal.
                assert_eq!(
                    select_ipv6_source_address_cmp(
                        remote,
                        dev0,
                        &new_addr_entry(*local0),
                        dev1,
                        &new_addr_entry(*local1),
                        dev2
                    ),
                    Ordering::Equal
                );
            }
        }
    }
}

/// Test mock implementations of the traits defined in the `socket` module.
#[cfg(test)]
pub(crate) mod testutil {
    use alloc::vec::Vec;

    use net_types::Witness;
    use packet_formats::ip::IpPacketBuilder;

    use super::*;
    use crate::context::testutil::DummyCtx;
    use crate::context::FrameContext;

    /// A dummy implementation of [`IpSocket`].
    #[derive(Clone)]
    pub(crate) struct DummyIpSock<I: IpExt> {
        defn: IpSockDefinition<I>,
        // Guaranteed to be `true` if `unroutable_behavior` is `Close`.
        routable: bool,
    }

    /// An update to a [`DummyIpSocket`].
    pub(crate) struct DummyIpSocketUpdate<I: Ip> {
        // The value to set the socket's `routable` field to.
        routable: bool,
        _marker: PhantomData<I>,
    }

    impl<I: IpExt> Socket for DummyIpSock<I> {
        type Update = DummyIpSocketUpdate<I>;
        type UpdateMeta = ();
        type UpdateError = IpSockCreationError;

        fn apply_update(
            &mut self,
            update: &DummyIpSocketUpdate<I>,
            _meta: &(),
        ) -> Result<(), IpSockCreationError> {
            if !update.routable && self.defn.unroutable_behavior == UnroutableBehavior::Close {
                return Err(IpSockUnroutableError::NoRouteToRemoteAddr.into());
            }
            self.routable = update.routable;
            Ok(())
        }
    }

    impl<I: IpExt> IpSocket<I> for DummyIpSock<I> {
        fn local_ip(&self) -> &SpecifiedAddr<I::Addr> {
            &self.defn.local_ip
        }

        fn remote_ip(&self) -> &SpecifiedAddr<I::Addr> {
            &self.defn.remote_ip
        }
    }

    /// A dummy implementation of [`IpSocketContext`].
    ///
    /// `IpSocketContext` is implemented for any `DummyCtx<S>` where `S`
    /// implements `AsRef` and `AsMut` for `DummyIpSocketCtx`.
    pub(crate) struct DummyIpSocketCtx<I: Ip> {
        /// List of local IP addresses. Calls to `new_ip_socket` which provide a
        /// local address not in this list will fail.
        local_ips: Vec<SpecifiedAddr<I::Addr>>,
        /// List of all routable remote IP addresses.
        routable_remote_ips: Vec<SpecifiedAddr<I::Addr>>,
        // Whether or not calls to `new_ip_socket` should succeed.
        routable_at_creation: bool,
    }

    impl<I: Ip> DummyIpSocketCtx<I> {
        /// Creates a new `DummyIpSocketCtx`.
        ///
        /// `local_ips` is the list of local IP addresses. Calls to
        /// `new_ip_socket` which provide a local address not in this list will
        /// fail.
        ///
        /// `routable_at_creation` controls whether or not calls to
        /// `new_ip_socket` should succeed.
        pub(crate) fn new(
            local_ips: Vec<SpecifiedAddr<I::Addr>>,
            routable_remote_ips: Vec<SpecifiedAddr<I::Addr>>,
            routable_at_creation: bool,
        ) -> DummyIpSocketCtx<I> {
            DummyIpSocketCtx { local_ips, routable_remote_ips, routable_at_creation }
        }
    }

    #[derive(Default)]
    pub(crate) struct DummyIpSocketBuilder {
        hop_limit: Option<u8>,
    }

    pub(crate) trait SocketTestIpExt: IpExt {
        const DUMMY_SOCKET_DATA: Self::SocketData;
    }

    impl SocketTestIpExt for Ipv4 {
        const DUMMY_SOCKET_DATA: Self::SocketData = ();
    }

    impl SocketTestIpExt for Ipv6 {
        const DUMMY_SOCKET_DATA: Self::SocketData = Ipv6SocketData { flow_label: 0 };
    }

    impl<
            I: IpExt + SocketTestIpExt,
            S: AsRef<DummyIpSocketCtx<I>> + AsMut<DummyIpSocketCtx<I>>,
            Id,
            Meta,
        > IpSocketContext<I> for DummyCtx<S, Id, Meta>
    {
        type IpSocket = DummyIpSock<I>;

        type Builder = DummyIpSocketBuilder;

        fn new_ip_socket(
            &mut self,
            local_ip: Option<SpecifiedAddr<I::Addr>>,
            remote_ip: SpecifiedAddr<I::Addr>,
            proto: I::Proto,
            unroutable_behavior: UnroutableBehavior,
            builder: Option<DummyIpSocketBuilder>,
        ) -> Result<DummyIpSock<I>, IpSockCreationError> {
            let builder = builder.unwrap_or_default();

            let ctx = self.get_ref().as_ref();

            if !ctx.routable_at_creation || !ctx.routable_remote_ips.contains(&remote_ip) {
                return Err(IpSockUnroutableError::NoRouteToRemoteAddr.into());
            }

            let local_ip = local_ip
                .or_else(|| ctx.local_ips.get(0).cloned())
                .ok_or(IpSockCreationError::NoLocalAddrAvailable)?;
            if !ctx.local_ips.contains(&local_ip) {
                return Err(IpSockUnroutableError::LocalAddrNotAssigned.into());
            }

            Ok(DummyIpSock {
                defn: IpSockDefinition {
                    local_ip,
                    remote_ip,
                    proto,
                    hop_limit: builder.hop_limit.unwrap_or(crate::ip::DEFAULT_TTL.get()),
                    unroutable_behavior,
                    per_proto_data: I::DUMMY_SOCKET_DATA,
                },
                routable: true,
            })
        }
    }

    impl<
            I: IpExt + SocketTestIpExt,
            B: BufferMut,
            S: AsRef<DummyIpSocketCtx<I>> + AsMut<DummyIpSocketCtx<I>>,
            Id,
        > BufferIpSocketContext<I, B> for DummyCtx<S, Id, I::PacketBuilder>
    {
        fn send_ip_packet<SS: Serializer<Buffer = B>>(
            &mut self,
            sock: &DummyIpSock<I>,
            body: SS,
        ) -> Result<(), (SS, IpSockSendError)> {
            if !sock.routable {
                unimplemented!()
            }
            self.send_frame(
                I::PacketBuilder::new(
                    sock.defn.local_ip.get(),
                    sock.defn.remote_ip.get(),
                    sock.defn.hop_limit,
                    sock.defn.proto,
                ),
                body,
            )
            .map_err(|body| (body, IpSockSendError::Mtu))
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use net_types::Witness;
    use packet::{InnerPacketBuilder, ParseBuffer};
    use packet_formats::{
        ip::IpPacket,
        ipv4::{Ipv4OnlyMeta, Ipv4Packet},
        testutil::{parse_ethernet_frame, parse_ip_packet_in_ethernet_frame},
    };
    use specialize_ip_macro::ip_test;

    use super::*;
    use crate::ip::specialize_ip;
    use crate::testutil::*;

    #[test]
    fn test_new() {
        // Test that `new_ip_socket` works with various edge cases.

        // IPv4

        let mut ctx = DummyEventDispatcherBuilder::from_config(DUMMY_CONFIG_V4)
            .build::<DummyEventDispatcher>();

        // A template socket that we can use to more concisely define sockets in
        // various test cases.
        let template = IpSock {
            defn: IpSockDefinition {
                remote_ip: DUMMY_CONFIG_V4.remote_ip,
                local_ip: DUMMY_CONFIG_V4.local_ip,
                proto: Ipv4Proto::Icmp,
                hop_limit: crate::ip::DEFAULT_TTL.get(),
                unroutable_behavior: UnroutableBehavior::Close,
                per_proto_data: (),
            },
            cached: Ok(CachedInfo {
                builder: Ipv4PacketBuilder::new(
                    DUMMY_CONFIG_V4.local_ip,
                    DUMMY_CONFIG_V4.remote_ip,
                    crate::ip::DEFAULT_TTL.get(),
                    Ipv4Proto::Icmp,
                ),
                device: DeviceId::new_ethernet(0),
                next_hop: DUMMY_CONFIG_V4.remote_ip,
            }),
        };

        // All optional fields are `None`.
        assert_eq!(
            IpSocketContext::<Ipv4>::new_ip_socket(
                &mut ctx,
                None,
                DUMMY_CONFIG_V4.remote_ip,
                Ipv4Proto::Icmp,
                UnroutableBehavior::Close,
                None,
            ),
            Ok(template.clone())
        );

        // TTL is specified.
        let mut builder = Ipv4SocketBuilder::default();
        let _: &mut Ipv4SocketBuilder = builder.ttl(NonZeroU8::new(1).unwrap());
        assert_eq!(
            IpSocketContext::<Ipv4>::new_ip_socket(
                &mut ctx,
                None,
                DUMMY_CONFIG_V4.remote_ip,
                Ipv4Proto::Icmp,
                UnroutableBehavior::Close,
                Some(builder),
            ),
            {
                // The template socket, but with the TTL set to 1.
                let mut x = template.clone();
                x.defn.hop_limit = 1;
                x.cached.as_mut().unwrap().builder = Ipv4PacketBuilder::new(
                    DUMMY_CONFIG_V4.local_ip,
                    DUMMY_CONFIG_V4.remote_ip,
                    1,
                    Ipv4Proto::Icmp,
                );
                Ok(x)
            }
        );

        // Local address is specified, and is a valid local address.
        assert_eq!(
            IpSocketContext::<Ipv4>::new_ip_socket(
                &mut ctx,
                Some(DUMMY_CONFIG_V4.local_ip),
                DUMMY_CONFIG_V4.remote_ip,
                Ipv4Proto::Icmp,
                UnroutableBehavior::Close,
                None,
            ),
            Ok(template.clone())
        );

        // Local address is specified, and is an invalid local address.
        assert_eq!(
            IpSocketContext::<Ipv4>::new_ip_socket(
                &mut ctx,
                Some(DUMMY_CONFIG_V4.remote_ip),
                DUMMY_CONFIG_V4.remote_ip,
                Ipv4Proto::Icmp,
                UnroutableBehavior::Close,
                None,
            ),
            Err(IpSockUnroutableError::LocalAddrNotAssigned.into())
        );

        // Loopback sockets are not yet supported.
        assert_eq!(
            IpSocketContext::<Ipv4>::new_ip_socket(
                &mut ctx,
                None,
                Ipv4::LOOPBACK_ADDRESS,
                Ipv4Proto::Icmp,
                UnroutableBehavior::Close,
                None,
            ),
            Err(IpSockUnroutableError::NoRouteToRemoteAddr.into())
        );

        // IPv6

        let mut ctx = DummyEventDispatcherBuilder::from_config(DUMMY_CONFIG_V6)
            .build::<DummyEventDispatcher>();

        // Since the dispatcher's random number generator is deterministic, we can
        // use a clone to assert on the sequence of flow labels it will produce.
        let mut rng = ctx.dispatcher.rng().clone();

        // A template socket that we can use to more concisely define sockets in
        // various test cases.
        let template = IpSock {
            defn: IpSockDefinition {
                remote_ip: DUMMY_CONFIG_V6.remote_ip,
                local_ip: DUMMY_CONFIG_V6.local_ip,
                proto: Ipv6Proto::Icmpv6,
                hop_limit: crate::ip::DEFAULT_TTL.get(),
                unroutable_behavior: UnroutableBehavior::Close,
                per_proto_data: Ipv6SocketData { flow_label: 0 },
            },
            cached: Ok(CachedInfo {
                builder: Ipv6PacketBuilder::new(
                    DUMMY_CONFIG_V6.local_ip,
                    DUMMY_CONFIG_V6.remote_ip,
                    crate::ip::DEFAULT_TTL.get(),
                    Ipv6Proto::Icmpv6,
                ),
                device: DeviceId::new_ethernet(0),
                next_hop: DUMMY_CONFIG_V6.remote_ip,
            }),
        };

        let with_flow_label =
            |mut template: IpSock<Ipv6, DeviceId>, flow_label| -> IpSock<Ipv6, DeviceId> {
                template.defn.per_proto_data = Ipv6SocketData { flow_label };
                template.cached.as_mut().unwrap().builder.flowlabel(flow_label);
                template
            };

        // All optional fields are `None`.
        assert_eq!(
            IpSocketContext::<Ipv6>::new_ip_socket(
                &mut ctx,
                None,
                DUMMY_CONFIG_V6.remote_ip,
                Ipv6Proto::Icmpv6,
                UnroutableBehavior::Close,
                None,
            ),
            Ok(with_flow_label(template.clone(), gen_ipv6_flowlabel(&mut rng)))
        );

        // Hop Limit is specified.
        const SPECIFIED_HOP_LIMIT: u8 = 1;
        let mut builder = Ipv6SocketBuilder::default();
        let _: &mut Ipv6SocketBuilder = builder.hop_limit(SPECIFIED_HOP_LIMIT);
        assert_eq!(
            IpSocketContext::<Ipv6>::new_ip_socket(
                &mut ctx,
                None,
                DUMMY_CONFIG_V6.remote_ip,
                Ipv6Proto::Icmpv6,
                UnroutableBehavior::Close,
                Some(builder),
            ),
            {
                let mut template_with_hop_limit = template.clone();
                template_with_hop_limit.defn.hop_limit = SPECIFIED_HOP_LIMIT;
                let builder = Ipv6PacketBuilder::new(
                    DUMMY_CONFIG_V6.local_ip,
                    DUMMY_CONFIG_V6.remote_ip,
                    SPECIFIED_HOP_LIMIT,
                    Ipv6Proto::Icmpv6,
                );
                template_with_hop_limit.cached.as_mut().unwrap().builder = builder;
                Ok(with_flow_label(template_with_hop_limit, gen_ipv6_flowlabel(&mut rng)))
            }
        );

        // Local address is specified, and is a valid local address.
        assert_eq!(
            IpSocketContext::<Ipv6>::new_ip_socket(
                &mut ctx,
                Some(DUMMY_CONFIG_V6.local_ip),
                DUMMY_CONFIG_V6.remote_ip,
                Ipv6Proto::Icmpv6,
                UnroutableBehavior::Close,
                None,
            ),
            Ok(with_flow_label(template, gen_ipv6_flowlabel(&mut rng)))
        );

        // Local address is specified, and is an invalid local address.
        assert_eq!(
            IpSocketContext::<Ipv6>::new_ip_socket(
                &mut ctx,
                Some(DUMMY_CONFIG_V6.remote_ip),
                DUMMY_CONFIG_V6.remote_ip,
                Ipv6Proto::Icmpv6,
                UnroutableBehavior::Close,
                None,
            ),
            Err(IpSockUnroutableError::LocalAddrNotAssigned.into())
        );

        // Loopback sockets are not yet supported.
        assert_eq!(
            IpSocketContext::<Ipv6>::new_ip_socket(
                &mut ctx,
                None,
                Ipv6::LOOPBACK_ADDRESS,
                Ipv6Proto::Icmpv6,
                UnroutableBehavior::Close,
                None,
            ),
            Err(IpSockUnroutableError::NoRouteToRemoteAddr.into())
        );
    }

    #[ip_test]
    #[specialize_ip]
    fn test_send<I: Ip>() {
        // Test various edge cases of the
        // `BufferIpSocketContext::send_ip_packet` method.

        #[ipv4]
        let (cfg, socket_builder, proto) = {
            let mut builder = Ipv4SocketBuilder::default();
            let _: &mut Ipv4SocketBuilder = builder.ttl(NonZeroU8::new(1).unwrap());
            (DUMMY_CONFIG_V4, builder, Ipv4Proto::Icmp)
        };

        #[ipv6]
        let (cfg, socket_builder, proto) = {
            let mut builder = Ipv6SocketBuilder::default();
            let _: &mut Ipv6SocketBuilder = builder.hop_limit(1);
            (DUMMY_CONFIG_V6, builder, Ipv6Proto::Icmpv6)
        };

        let DummyEventDispatcherConfig::<_> {
            local_mac,
            remote_mac,
            local_ip,
            remote_ip,
            subnet: _,
        } = cfg;

        let mut ctx =
            DummyEventDispatcherBuilder::from_config(cfg.clone()).build::<DummyEventDispatcher>();

        // Create a normal, routable socket.
        let mut sock = IpSocketContext::<I>::new_ip_socket(
            &mut ctx,
            None,
            remote_ip,
            proto,
            UnroutableBehavior::Close,
            Some(socket_builder),
        )
        .unwrap();

        #[ipv4]
        let curr_id = ctx.state.ipv4.gen_next_packet_id();

        // Send a packet on the socket and make sure that the right contents
        // are sent.
        BufferIpSocketContext::<I, _>::send_ip_packet(
            &mut ctx,
            &sock,
            (&[0u8][..]).into_serializer(),
        )
        .unwrap();

        assert_eq!(ctx.dispatcher.frames_sent().len(), 1);

        let (dev, frame) = &ctx.dispatcher.frames_sent()[0];
        assert_eq!(dev, &DeviceId::new_ethernet(0));

        #[ipv4]
        {
            let (mut body, src_mac, dst_mac, _ethertype) = parse_ethernet_frame(&frame).unwrap();
            let packet = (&mut body).parse::<Ipv4Packet<&[u8]>>().unwrap();
            assert_eq!(src_mac, local_mac.get());
            assert_eq!(dst_mac, remote_mac.get());
            assert_eq!(packet.src_ip(), local_ip.get());
            assert_eq!(packet.dst_ip(), remote_ip.get());
            assert_eq!(packet.proto(), proto);
            assert_eq!(packet.ttl(), 1);
            let Ipv4OnlyMeta { id } = packet.version_specific_meta();
            assert_eq!(id, curr_id + 1);
            assert_eq!(body, [0]);
        }

        #[ipv6]
        {
            let (body, src_mac, dst_mac, src_ip, dst_ip, proto, ttl) =
                parse_ip_packet_in_ethernet_frame::<Ipv6>(&frame).unwrap();
            assert_eq!(body, [0]);
            assert_eq!(src_mac, local_mac.get());
            assert_eq!(dst_mac, remote_mac.get());
            assert_eq!(src_ip, local_ip.get());
            assert_eq!(dst_ip, remote_ip.get());
            assert_eq!(proto, proto);
            assert_eq!(ttl, 1);
        }

        // Try sending a packet which will be larger than the device's MTU,
        // and make sure it fails.
        let body = vec![0u8; crate::ip::Ipv6::MINIMUM_LINK_MTU as usize];
        let res = BufferIpSocketContext::<I, _>::send_ip_packet(
            &mut ctx,
            &sock,
            (&body[..]).into_serializer(),
        );
        matches::assert_matches!(res, Err((_, IpSockSendError::Mtu)));

        // Make sure that sending on an unroutable socket fails.
        sock.cached = Err(IpSockUnroutableError::NoRouteToRemoteAddr);
        let res = BufferIpSocketContext::<I, _>::send_ip_packet(
            &mut ctx,
            &sock,
            (&body[..]).into_serializer(),
        );
        matches::assert_matches!(
            res,
            Err((_, IpSockSendError::Unroutable(IpSockUnroutableError::NoRouteToRemoteAddr)))
        );
    }
}
