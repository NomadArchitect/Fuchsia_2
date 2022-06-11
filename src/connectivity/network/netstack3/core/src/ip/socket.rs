// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! IPv4 and IPv6 sockets.

use alloc::vec::Vec;
use core::cmp::Ordering;
use core::convert::Infallible;
use core::num::NonZeroU8;

use net_types::ip::{Ip, Ipv4, Ipv4Addr, Ipv6, Ipv6Addr};
use net_types::{SpecifiedAddr, UnicastAddr};
use packet::{Buf, BufferMut, SerializeError, Serializer};
use packet_formats::ip::{Ipv4Proto, Ipv6Proto};
use thiserror::Error;

use crate::{
    context::{CounterContext, InstantContext, RngContext},
    ip::{
        device::state::{IpDeviceStateIpExt, Ipv6AddressEntry},
        forwarding::Destination,
        IpDeviceIdContext, IpExt, SendIpPacketMeta,
    },
};

/// A socket identifying a connection between a local and remote IP host.
pub(crate) trait IpSocket<I: Ip> {
    /// Get the local IP address.
    fn local_ip(&self) -> &SpecifiedAddr<I::Addr>;

    /// Get the remote IP address.
    fn remote_ip(&self) -> &SpecifiedAddr<I::Addr>;
}

/// An execution context defining a type of IP socket.
pub trait IpSocketHandler<I: IpExt, C>: IpDeviceIdContext<I> {
    /// A builder carrying optional parameters passed to [`new_ip_socket`].
    ///
    /// [`new_ip_socket`]: crate::ip::socket::IpSocketHandler::new_ip_socket
    type Builder: Default;

    /// Constructs a new [`Self::IpSocket`].
    ///
    /// `new_ip_socket` constructs a new `Self::IpSocket` to the given remote IP
    /// address from the given local IP address with the given IP protocol. If
    /// no local IP address is given, one will be chosen automatically. If
    /// `device` is `Some`, the socket will be bound to the given device - only
    /// routes which egress over the device will be used. If no route is
    /// available which egresses over the device - even if routes are available
    /// which egress over other devices - the socket will be considered
    /// unroutable.
    ///
    /// `new_ip_socket` returns an error if no route to the remote was found in
    /// the forwarding table or if the given local IP address is not valid for
    /// the found route.
    ///
    /// The builder may be used to override certain default parameters. Passing
    /// `None` for the `builder` parameter is equivalent to passing
    /// `Some(Default::default())`.
    fn new_ip_socket(
        &mut self,
        ctx: &mut C,
        device: Option<Self::DeviceId>,
        local_ip: Option<SpecifiedAddr<I::Addr>>,
        remote_ip: SpecifiedAddr<I::Addr>,
        proto: I::Proto,
        builder: Option<Self::Builder>,
    ) -> Result<IpSock<I, Self::DeviceId>, IpSockCreationError>;
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

impl From<SerializeError<Infallible>> for IpSockSendError {
    fn from(err: SerializeError<Infallible>) -> IpSockSendError {
        match err {
            SerializeError::Alloc(err) => match err {},
            SerializeError::Mtu => IpSockSendError::Mtu,
        }
    }
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

/// An extension of [`IpSocketHandler`] adding the ability to send packets on an
/// IP socket.
pub trait BufferIpSocketHandler<I: IpExt, C, B: BufferMut>: IpSocketHandler<I, C> {
    /// Sends an IP packet on a socket.
    ///
    /// The generated packet has its metadata initialized from `socket`,
    /// including the source and destination addresses, the Time To Live/Hop
    /// Limit, and the Protocol/Next Header. The outbound device is also chosen
    /// based on information stored in the socket.
    ///
    /// `mtu` may be used to optionally impose an MTU on the outgoing packet.
    /// Note that the device's MTU will still be imposed on the packet. That is,
    /// the smaller of `mtu` and the device's MTU will be imposed on the packet.
    ///
    /// If the socket is currently unroutable, an error is returned.
    fn send_ip_packet<S: Serializer<Buffer = B>>(
        &mut self,
        ctx: &mut C,
        socket: &IpSock<I, Self::DeviceId>,
        body: S,
        mtu: Option<u32>,
    ) -> Result<(), (S, IpSockSendError)>;

    /// Creates a temporary IP socket and sends a single packet on it.
    ///
    /// `local_ip`, `remote_ip`, `proto`, and `builder` are passed directly to
    /// [`IpSocketHandler::new_ip_socket`]. `get_body_from_src_ip` is given the
    /// source IP address for the packet - which may have been chosen
    /// automatically if `local_ip` is `None` - and returns the body to be
    /// encapsulated. This is provided in case the body's contents depend on the
    /// chosen source IP address.
    ///
    /// If `device` is specified, the available routes are limited to those that
    /// egress over the device.
    ///
    /// `mtu` may be used to optionally impose an MTU on the outgoing packet.
    /// Note that the device's MTU will still be imposed on the packet. That is,
    /// the smaller of `mtu` and the device's MTU will be imposed on the packet.
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
        ctx: &mut C,
        device: Option<Self::DeviceId>,
        local_ip: Option<SpecifiedAddr<I::Addr>>,
        remote_ip: SpecifiedAddr<I::Addr>,
        proto: I::Proto,
        builder: Option<Self::Builder>,
        get_body_from_src_ip: F,
        mtu: Option<u32>,
    ) -> Result<(), (S, IpSockCreateAndSendError)> {
        // We use a `match` instead of `map_err` because `map_err` would require passing a closure
        // which takes ownership of `get_body_from_src_ip`, which we also use in the success case.
        match self.new_ip_socket(ctx, device, local_ip, remote_ip, proto, builder) {
            Err(err) => Err((get_body_from_src_ip(I::LOOPBACK_ADDRESS), err.into())),
            Ok(tmp) => self
                .send_ip_packet(ctx, &tmp, get_body_from_src_ip(*tmp.local_ip()), mtu)
                .map_err(|(body, err)| match err {
                    IpSockSendError::Mtu => (body, IpSockCreateAndSendError::Mtu),
                    IpSockSendError::Unroutable(_) => {
                        unreachable!("socket which was just created should still be routable")
                    }
                }),
        }
    }
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
    /// An error occurred while looking up a route.
    #[error("a route cannot be determined: {}", _0)]
    Route(#[from] IpSockRouteError),
}

/// An error encountered when looking up a route for an IP socket.
#[derive(Error, Copy, Clone, Debug, Eq, PartialEq)]
pub enum IpSockRouteError {
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
pub struct Ipv4SocketBuilder {
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
    hop_limit: Option<NonZeroU8>,
}

impl Ipv6SocketBuilder {
    /// Sets the Hop Limit field that will be set on outbound IPv6 packets.
    #[allow(dead_code)] // TODO(joshlf): Remove once this is used
    pub(crate) fn hop_limit(&mut self, hop_limit: NonZeroU8) -> &mut Ipv6SocketBuilder {
        self.hop_limit = Some(hop_limit);
        self
    }
}

/// The production implementation of the [`IpSocket`] trait.
#[derive(Clone)]
#[cfg_attr(test, derive(Debug, PartialEq))]
pub struct IpSock<I: IpExt, D> {
    defn: IpSockDefinition<I, D>,
}

/// The definition of an IP socket.
///
/// These values are part of the socket's definition, and never change.
#[derive(Clone)]
#[cfg_attr(test, derive(Debug, PartialEq))]
struct IpSockDefinition<I: IpExt, D> {
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
    #[cfg_attr(not(test), allow(unused))]
    device: Option<D>,
    hop_limit: Option<NonZeroU8>,
    proto: I::Proto,
}

impl<I: IpExt, D> IpSocket<I> for IpSock<I, D> {
    fn local_ip(&self) -> &SpecifiedAddr<I::Addr> {
        &self.defn.local_ip
    }

    fn remote_ip(&self) -> &SpecifiedAddr<I::Addr> {
        &self.defn.remote_ip
    }
}

// TODO(joshlf): Once we support configuring transport-layer protocols using
// type parameters, use that to ensure that `proto` is the right protocol for
// the caller. We will still need to have a separate enforcement mechanism for
// raw IP sockets once we support those.

/// The route for a socket.
pub(super) struct IpSockRoute<I: Ip, D> {
    /// The local IP to use for the socket.
    pub(super) local_ip: SpecifiedAddr<I::Addr>,

    /// The destination for packets originating from the socket.
    pub(super) destination: Destination<I::Addr, D>,
}

/// The context required in order to implement [`IpSocketHandler`].
///
/// Blanket impls of `IpSocketHandler` are provided in terms of
/// `IpSocketContext`.
pub(super) trait IpSocketContext<I, C>:
    IpDeviceIdContext<I> + InstantContext + CounterContext + RngContext
where
    I: IpDeviceStateIpExt<<Self as InstantContext>::Instant>,
{
    /// Returns a route for a socket.
    ///
    /// If `device` is specified, the available routes are limited to those that
    /// egress over the device.
    fn lookup_route(
        &self,
        ctx: &mut C,
        device: Option<Self::DeviceId>,
        src_ip: Option<SpecifiedAddr<I::Addr>>,
        dst_ip: SpecifiedAddr<I::Addr>,
    ) -> Result<IpSockRoute<I, Self::DeviceId>, IpSockRouteError>;
}

/// The context required in order to implement [`BufferIpSocketHandler`].
///
/// Blanket impls of `BufferIpSocketHandler` are provided in terms of
/// `BufferIpSocketContext`.
pub(super) trait BufferIpSocketContext<I, C, B: BufferMut>: IpSocketContext<I, C>
where
    I: IpDeviceStateIpExt<<Self as InstantContext>::Instant> + packet_formats::ip::IpExt,
{
    /// Send an IP packet to the next-hop node.
    fn send_ip_packet<S: Serializer<Buffer = B>>(
        &mut self,
        ctx: &mut C,
        meta: SendIpPacketMeta<I, Self::DeviceId, SpecifiedAddr<I::Addr>>,
        body: S,
    ) -> Result<(), S>;
}

impl<C, SC: IpSocketContext<Ipv4, C>> IpSocketHandler<Ipv4, C> for SC {
    type Builder = Ipv4SocketBuilder;

    fn new_ip_socket(
        &mut self,
        ctx: &mut C,
        device: Option<SC::DeviceId>,
        local_ip: Option<SpecifiedAddr<Ipv4Addr>>,
        remote_ip: SpecifiedAddr<Ipv4Addr>,
        proto: Ipv4Proto,
        builder: Option<Ipv4SocketBuilder>,
    ) -> Result<IpSock<Ipv4, SC::DeviceId>, IpSockCreationError> {
        // Make sure the remote is routable with a local address before creating
        // the socket. We do not care about the actual destination here because
        // we will recalculate it when we send a packet so that the best route
        // available at the time is used for each outgoing packet.
        let IpSockRoute { local_ip, destination: _ } =
            self.lookup_route(ctx, device, local_ip, remote_ip)?;

        let Ipv4SocketBuilder { ttl } = builder.unwrap_or_default();

        let defn = IpSockDefinition { local_ip, remote_ip, device, proto, hop_limit: ttl };
        Ok(IpSock { defn })
    }
}

impl<C, SC: IpSocketContext<Ipv6, C>> IpSocketHandler<Ipv6, C> for SC {
    type Builder = Ipv6SocketBuilder;

    fn new_ip_socket(
        &mut self,
        ctx: &mut C,
        device: Option<SC::DeviceId>,
        local_ip: Option<SpecifiedAddr<Ipv6Addr>>,
        remote_ip: SpecifiedAddr<Ipv6Addr>,
        proto: Ipv6Proto,
        builder: Option<Ipv6SocketBuilder>,
    ) -> Result<IpSock<Ipv6, SC::DeviceId>, IpSockCreationError> {
        // Make sure the remote is routable with a local address before creating
        // the socket. We do not care about the actual destination here because
        // we will recalculate it when we send a packet so that the best route
        // available at the time is used for each outgoing packet.
        let IpSockRoute { local_ip, destination: _ } =
            self.lookup_route(ctx, device, local_ip, remote_ip)?;

        let Ipv6SocketBuilder { hop_limit } = builder.unwrap_or_default();

        let defn = IpSockDefinition { local_ip, remote_ip, device, proto, hop_limit };
        Ok(IpSock { defn })
    }
}

fn send_ip_packet<
    I: IpExt + IpDeviceStateIpExt<SC::Instant> + packet_formats::ip::IpExt,
    B: BufferMut,
    S: Serializer<Buffer = B>,
    C,
    SC: BufferIpSocketContext<I, C, B>
        + BufferIpSocketContext<I, C, Buf<Vec<u8>>>
        + IpSocketContext<I, C>,
>(
    sync_ctx: &mut SC,
    ctx: &mut C,
    IpSock { defn: IpSockDefinition { remote_ip, local_ip, device, hop_limit, proto } }: &IpSock<
        I,
        SC::DeviceId,
    >,
    body: S,
    mtu: Option<u32>,
) -> Result<(), (S, IpSockSendError)> {
    let IpSockRoute { local_ip: got_local_ip, destination: Destination { device, next_hop } } =
        match sync_ctx.lookup_route(ctx, *device, Some(*local_ip), *remote_ip) {
            Ok(o) => o,
            Err(IpSockRouteError::NoLocalAddrAvailable) => {
                unreachable!("local IP {} was specified", local_ip)
            }
            Err(IpSockRouteError::Unroutable(e)) => {
                return Err((body, IpSockSendError::Unroutable(e)))
            }
        };

    assert_eq!(*local_ip, got_local_ip);

    BufferIpSocketContext::send_ip_packet(
        sync_ctx,
        ctx,
        SendIpPacketMeta {
            device,
            src_ip: *local_ip,
            dst_ip: *remote_ip,
            next_hop,
            ttl: *hop_limit,
            proto: *proto,
            mtu,
        },
        body,
    )
    .map_err(|s| (s, IpSockSendError::Mtu))
}

impl<
        B: BufferMut,
        C,
        SC: BufferIpSocketContext<Ipv4, C, B>
            + BufferIpSocketContext<Ipv4, C, Buf<Vec<u8>>>
            + IpSocketContext<Ipv4, C>,
    > BufferIpSocketHandler<Ipv4, C, B> for SC
{
    fn send_ip_packet<S: Serializer<Buffer = B>>(
        &mut self,
        ctx: &mut C,
        ip_sock: &IpSock<Ipv4, SC::DeviceId>,
        body: S,
        mtu: Option<u32>,
    ) -> Result<(), (S, IpSockSendError)> {
        // TODO(joshlf): Call `trace!` with relevant fields from the socket.
        self.increment_counter("send_ipv4_packet");

        send_ip_packet(self, ctx, ip_sock, body, mtu)
    }
}

impl<
        B: BufferMut,
        C,
        SC: BufferIpSocketContext<Ipv6, C, B> + BufferIpSocketContext<Ipv6, C, Buf<Vec<u8>>>,
    > BufferIpSocketHandler<Ipv6, C, B> for SC
{
    fn send_ip_packet<S: Serializer<Buffer = B>>(
        &mut self,
        ctx: &mut C,
        ip_sock: &IpSock<Ipv6, SC::DeviceId>,
        body: S,
        mtu: Option<u32>,
    ) -> Result<(), (S, IpSockSendError)> {
        // TODO(joshlf): Call `trace!` with relevant fields from the socket.
        self.increment_counter("send_ipv6_packet");

        send_ip_packet(self, ctx, ip_sock, body, mtu)
    }
}

/// IPv6 source address selection as defined in [RFC 6724 Section 5].
pub(super) mod ipv6_source_address_selection {
    use net_types::ip::IpAddress as _;

    use super::*;

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
    pub(crate) fn select_ipv6_source_address<
        'a,
        D: Copy + PartialEq,
        Instant: 'a,
        I: Iterator<Item = (&'a Ipv6AddressEntry<Instant>, D)>,
    >(
        remote_ip: SpecifiedAddr<Ipv6Addr>,
        outbound_device: D,
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

    /// Comparison operator used by `select_ipv6_source_address`.
    fn select_ipv6_source_address_cmp<Instant, D: Copy + PartialEq>(
        remote_ip: SpecifiedAddr<Ipv6Addr>,
        outbound_device: D,
        a: &Ipv6AddressEntry<Instant>,
        a_device: D,
        b: &Ipv6AddressEntry<Instant>,
        b_device: D,
    ) -> Ordering {
        // TODO(fxbug.dev/46822): Implement rules 2, 4, 5.5, 6, and 7.

        let a_addr = a.addr_sub().addr().into_specified();
        let b_addr = b.addr_sub().addr().into_specified();

        // Assertions required in order for this implementation to be valid.

        // Required by the implementation of Rule 1.
        debug_assert!(!(a_addr == remote_ip && b_addr == remote_ip));

        // Tentative addresses are not valid source addresses since they are
        // not considered assigned.
        debug_assert!(!a.state.is_tentative());
        debug_assert!(!b.state.is_tentative());

        rule_1(remote_ip, a_addr, b_addr)
            .then_with(|| rule_3(a.deprecated, b.deprecated))
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

    fn rule_3(a_deprecated: bool, b_deprecated: bool) -> Ordering {
        match (a_deprecated, b_deprecated) {
            (true, false) => Ordering::Less,
            (true, true) | (false, false) => Ordering::Equal,
            (false, true) => Ordering::Greater,
        }
    }

    fn rule_5<D: PartialEq>(outbound_device: D, a_device: D, b_device: D) -> Ordering {
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
        a: &Ipv6AddressEntry<Instant>,
        b: &Ipv6AddressEntry<Instant>,
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
            src: &Ipv6AddressEntry<Instant>,
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
        use crate::{
            device::DeviceId,
            ip::device::state::{AddrConfig, AddressState},
        };

        #[test]
        fn test_select_ipv6_source_address() {
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
            assert_eq!(rule_3(false, true), Ordering::Greater);
            assert_eq!(rule_3(true, false), Ordering::Less);
            assert_eq!(rule_3(true, true), Ordering::Equal);
            assert_eq!(rule_3(false, false), Ordering::Equal);

            // Rule 5: Prefer outgoing interface
            assert_eq!(rule_5(dev0, dev0, dev2), Ordering::Greater);
            assert_eq!(rule_5(dev0, dev2, dev0), Ordering::Less);
            assert_eq!(rule_5(dev0, dev0, dev0), Ordering::Equal);
            assert_eq!(rule_5(dev0, dev2, dev2), Ordering::Equal);

            // Rule 8: Use longest matching prefix.
            {
                let new_addr_entry = |bytes, prefix_len| {
                    Ipv6AddressEntry::<()>::new(
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
                    Ipv6AddressEntry::<()>::new(
                        AddrSubnet::new(addr, 128).unwrap(),
                        AddressState::Assigned,
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
    use alloc::{collections::HashMap, vec::Vec};
    use core::fmt::Debug;

    use net_types::{
        ip::{AddrSubnet, IpAddress, Subnet},
        Witness,
    };

    use super::*;
    use crate::{
        context::{
            testutil::{DummyInstant, DummyNonSyncCtx, DummySyncCtx},
            FrameContext,
        },
        ip::{
            device::state::{AddrConfig, AddressState, AssignedAddress as _, IpDeviceState},
            forwarding::ForwardingTable,
            DummyDeviceId, IpDeviceId, SendIpPacketMeta,
        },
    };

    /// A dummy implementation of [`IpSocketContext`].
    ///
    /// `IpSocketContext` is implemented for any `DummyCtx<S>` where `S`
    /// implements `AsRef` and `AsMut` for `DummyIpSocketCtx`.
    pub(crate) struct DummyIpSocketCtx<I: IpDeviceStateIpExt<DummyInstant>, D> {
        pub(crate) table: ForwardingTable<I, D>,
        device_state: HashMap<D, IpDeviceState<DummyInstant, I>>,
    }

    impl<
            I: IpDeviceStateIpExt<DummyInstant>,
            S: AsRef<DummyIpSocketCtx<I, DeviceId>> + AsMut<DummyIpSocketCtx<I, DeviceId>>,
            Id,
            Meta,
            Event: Debug,
            DeviceId: IpDeviceId + 'static,
        > IpSocketContext<I, DummyNonSyncCtx> for DummySyncCtx<S, Id, Meta, Event, DeviceId>
    {
        fn lookup_route(
            &self,
            _ctx: &mut DummyNonSyncCtx,
            device: Option<Self::DeviceId>,
            local_ip: Option<SpecifiedAddr<I::Addr>>,
            addr: SpecifiedAddr<I::Addr>,
        ) -> Result<IpSockRoute<I, Self::DeviceId>, IpSockRouteError> {
            let destination = self
                .get_ref()
                .as_ref()
                .table
                .lookup(device, addr)
                .ok_or(IpSockUnroutableError::NoRouteToRemoteAddr)?;

            let Destination { device, next_hop: _ } = &destination;
            local_ip
                .map_or_else(
                    || {
                        self.get_ref()
                            .as_ref()
                            .device_state
                            .get(&device)
                            .unwrap()
                            .iter_addrs()
                            .map(|e| e.addr())
                            .next()
                            .ok_or(IpSockRouteError::NoLocalAddrAvailable)
                    },
                    |local_ip| {
                        self.get_ref()
                            .as_ref()
                            .device_state
                            .get(&device)
                            .unwrap()
                            .iter_addrs()
                            .any(|e| e.addr() == local_ip)
                            .then(|| local_ip)
                            .ok_or(IpSockUnroutableError::LocalAddrNotAssigned.into())
                    },
                )
                .map(|local_ip| IpSockRoute { local_ip, destination })
        }
    }

    impl<
            I: IpDeviceStateIpExt<DummyInstant> + packet_formats::ip::IpExt,
            B: BufferMut,
            S: AsRef<DummyIpSocketCtx<I, DeviceId>> + AsMut<DummyIpSocketCtx<I, DeviceId>>,
            Id,
            Meta,
            Event: Debug,
            DeviceId,
        > BufferIpSocketContext<I, DummyNonSyncCtx, B>
        for DummySyncCtx<S, Id, Meta, Event, DeviceId>
    where
        DummySyncCtx<S, Id, Meta, Event, DeviceId>: FrameContext<
                DummyNonSyncCtx,
                B,
                SendIpPacketMeta<I, Self::DeviceId, SpecifiedAddr<I::Addr>>,
            > + IpSocketContext<I, DummyNonSyncCtx>
            + InstantContext<Instant = DummyInstant>,
    {
        fn send_ip_packet<SS: Serializer<Buffer = B>>(
            &mut self,
            ctx: &mut DummyNonSyncCtx,
            meta: SendIpPacketMeta<I, Self::DeviceId, SpecifiedAddr<I::Addr>>,
            body: SS,
        ) -> Result<(), SS> {
            self.send_frame(ctx, meta, body)
        }
    }

    impl<I: IpDeviceStateIpExt<DummyInstant>, D: IpDeviceId> DummyIpSocketCtx<I, D> {
        pub(crate) fn with_devices_state(
            devices: impl IntoIterator<
                Item = (D, IpDeviceState<DummyInstant, I>, Vec<SpecifiedAddr<I::Addr>>),
            >,
        ) -> Self {
            let mut table = ForwardingTable::default();
            let mut device_state = HashMap::default();
            for (device, state, addrs) in devices {
                for ip in addrs {
                    assert_eq!(
                        table.add_device_route(
                            Subnet::new(ip.get(), <I::Addr as IpAddress>::BYTES * 8).unwrap(),
                            device,
                        ),
                        Ok(())
                    );
                }
                assert!(
                    device_state.insert(device, state).is_none(),
                    "duplicate entries for {}",
                    device
                );
            }

            DummyIpSocketCtx { table, device_state }
        }
    }

    pub(crate) struct DummyDeviceConfig<D, A: IpAddress> {
        pub(crate) device: D,
        pub(crate) local_ips: Vec<SpecifiedAddr<A>>,
        pub(crate) remote_ips: Vec<SpecifiedAddr<A>>,
    }

    impl<D: IpDeviceId> DummyIpSocketCtx<Ipv4, D> {
        /// Creates a new `DummyIpSocketCtx<Ipv4>` with the given device
        /// configs.
        pub(crate) fn new_ipv4(
            devices: impl IntoIterator<Item = DummyDeviceConfig<D, Ipv4Addr>>,
        ) -> Self {
            DummyIpSocketCtx::with_devices_state(devices.into_iter().map(
                |DummyDeviceConfig { device, local_ips, remote_ips }| {
                    let mut device_state = IpDeviceState::default();
                    for ip in local_ips {
                        // Users of this utility don't care about subnet prefix length,
                        // so just pick a reasonable one.
                        device_state
                            .add_addr(AddrSubnet::new(ip.get(), 32).unwrap())
                            .expect("add address");
                    }
                    (device, device_state, remote_ips)
                },
            ))
        }
    }

    impl DummyIpSocketCtx<Ipv4, DummyDeviceId> {
        /// Creates a new `DummyIpSocketCtx<Ipv4>`.
        pub(crate) fn new_dummy_ipv4(
            local_ips: Vec<SpecifiedAddr<Ipv4Addr>>,
            remote_ips: Vec<SpecifiedAddr<Ipv4Addr>>,
        ) -> Self {
            Self::new_ipv4([DummyDeviceConfig { device: DummyDeviceId, local_ips, remote_ips }])
        }
    }

    impl<D: IpDeviceId> DummyIpSocketCtx<Ipv6, D> {
        /// Creates a new `DummyIpSocketCtx<Ipv6>` with the given device
        /// configs.
        pub(crate) fn new_ipv6(
            devices: impl IntoIterator<Item = DummyDeviceConfig<D, Ipv6Addr>>,
        ) -> Self {
            DummyIpSocketCtx::with_devices_state(devices.into_iter().map(
                |DummyDeviceConfig { device, local_ips, remote_ips }| {
                    let mut device_state = IpDeviceState::default();
                    for ip in local_ips {
                        // Users of this utility don't care about subnet prefix length,
                        // so just pick a reasonable one.
                        device_state
                            .add_addr(Ipv6AddressEntry::new(
                                // Users of this utility don't care about subnet prefix
                                // length, so just pick a reasonable one.
                                AddrSubnet::new(ip.get(), 128).unwrap(),
                                AddressState::Assigned,
                                AddrConfig::Manual,
                            ))
                            .expect("add address");
                    }
                    (device, device_state, remote_ips)
                },
            ))
        }
    }

    impl DummyIpSocketCtx<Ipv6, DummyDeviceId> {
        /// Creates a new `DummyIpSocketCtx<Ipv6>`.
        pub(crate) fn new_dummy_ipv6(
            local_ips: Vec<SpecifiedAddr<Ipv6Addr>>,
            remote_ips: Vec<SpecifiedAddr<Ipv6Addr>>,
        ) -> Self {
            Self::new_ipv6([DummyDeviceConfig { device: DummyDeviceId, local_ips, remote_ips }])
        }
    }
}

#[cfg(test)]
mod tests {
    use net_types::{
        ip::{AddrSubnet, SubnetEither},
        Witness,
    };
    use packet::{Buf, InnerPacketBuilder, ParseBuffer};
    use packet_formats::{
        ip::IpPacket,
        ipv4::{Ipv4OnlyMeta, Ipv4Packet, Ipv4PacketBuilder},
        ipv6::Ipv6PacketBuilder,
        testutil::{parse_ethernet_frame, parse_ip_packet_in_ethernet_frame},
    };
    use specialize_ip_macro::{ip_test, specialize_ip};

    use super::*;
    use crate::{device::DeviceId, testutil::*, Ctx, SyncCtx};

    enum AddressType {
        LocallyOwned,
        Remote,
        Unspecified {
            // Indicates whether or not it should be possible for the stack to
            // select an address when the client fails to specify one.
            can_select: bool,
        },
        Unroutable,
    }

    enum DeviceType {
        Unspecified,
        OtherDevice,
        LocalDevice,
    }

    struct NewSocketTestCase {
        local_ip_type: AddressType,
        remote_ip_type: AddressType,
        device_type: DeviceType,
        expected_result: Result<(), IpSockCreationError>,
    }

    #[specialize_ip]
    fn test_new<I: Ip>(test_case: NewSocketTestCase) {
        #[ipv4]
        let (cfg, proto) = (DUMMY_CONFIG_V4, Ipv4Proto::Icmp);

        #[ipv6]
        let (cfg, proto) = (DUMMY_CONFIG_V6, Ipv6Proto::Icmpv6);

        let DummyEventDispatcherConfig { local_ip, remote_ip, subnet, local_mac: _, remote_mac: _ } =
            cfg;
        let Ctx { mut sync_ctx, mut non_sync_ctx } =
            DummyEventDispatcherBuilder::from_config(cfg).build();
        let loopback_device_id = crate::add_loopback_device(&mut sync_ctx, u16::MAX.into())
            .expect("create the loopback interface");
        crate::device::testutil::enable_device(
            &mut sync_ctx,
            &mut non_sync_ctx,
            loopback_device_id,
        );

        let NewSocketTestCase { local_ip_type, remote_ip_type, expected_result, device_type } =
            test_case;

        #[ipv4]
        let remove_all_local_addrs = |sync_ctx: &mut crate::testutil::DummySyncCtx,
                                      ctx: &mut ()| {
            let devices = crate::ip::device::iter_ipv4_devices(sync_ctx)
                .map(|(device, _state)| device)
                .collect::<Vec<_>>();
            for device in devices {
                let subnets = crate::ip::device::get_assigned_ipv4_addr_subnets(sync_ctx, device)
                    .collect::<Vec<_>>();
                for subnet in subnets {
                    crate::device::del_ip_addr(sync_ctx, ctx, device, &subnet.addr())
                        .expect("failed to remove addr from device");
                }
            }
        };

        #[ipv6]
        let remove_all_local_addrs = |sync_ctx: &mut crate::testutil::DummySyncCtx,
                                      ctx: &mut ()| {
            let devices = crate::ip::device::iter_ipv6_devices(sync_ctx)
                .map(|(device, _state)| device)
                .collect::<Vec<_>>();
            for device in devices {
                let subnets = crate::ip::device::get_assigned_ipv6_addr_subnets(sync_ctx, device)
                    .collect::<Vec<_>>();
                for subnet in subnets {
                    crate::device::del_ip_addr(sync_ctx, ctx, device, &subnet.addr())
                        .expect("failed to remove addr from device");
                }
            }
        };

        const LOCAL_DEVICE: DeviceId = DeviceId::new_ethernet(0);
        const OTHER_DEVICE: DeviceId = DeviceId::new_ethernet(1);
        let local_device = match device_type {
            DeviceType::Unspecified => None,
            DeviceType::LocalDevice => Some(LOCAL_DEVICE),
            DeviceType::OtherDevice => Some(OTHER_DEVICE),
        };

        let (expected_from_ip, from_ip) = match local_ip_type {
            AddressType::LocallyOwned => (local_ip, Some(local_ip)),
            AddressType::Remote => (remote_ip, Some(remote_ip)),
            AddressType::Unspecified { can_select } => {
                if !can_select {
                    remove_all_local_addrs(&mut sync_ctx, &mut non_sync_ctx);
                }
                (local_ip, None)
            }
            AddressType::Unroutable => {
                remove_all_local_addrs(&mut sync_ctx, &mut non_sync_ctx);
                (local_ip, Some(local_ip))
            }
        };

        let (to_ip, device) = match remote_ip_type {
            AddressType::LocallyOwned => (
                local_ip,
                IpDeviceIdContext::<I>::loopback_id(&sync_ctx)
                    .expect("local test should have loopback device"),
            ),
            AddressType::Remote => (remote_ip, LOCAL_DEVICE),
            AddressType::Unspecified { can_select: _ } => {
                panic!("remote_ip_type cannot be unspecified")
            }
            AddressType::Unroutable => {
                match subnet.into() {
                    SubnetEither::V4(subnet) => {
                        crate::ip::del_route::<Ipv4, _, _>(&mut sync_ctx, &mut non_sync_ctx, subnet)
                            .expect("failed to delete IPv4 device route")
                    }
                    SubnetEither::V6(subnet) => {
                        crate::ip::del_route::<Ipv6, _, _>(&mut sync_ctx, &mut non_sync_ctx, subnet)
                            .expect("failed to delete IPv6 device route")
                    }
                }

                (remote_ip, LOCAL_DEVICE)
            }
        };

        #[ipv4]
        let builder = Ipv4PacketBuilder::new(
            expected_from_ip,
            to_ip,
            crate::ip::DEFAULT_TTL.get(),
            Ipv4Proto::Icmp,
        );

        #[ipv6]
        let builder = Ipv6PacketBuilder::new(
            expected_from_ip,
            to_ip,
            crate::ip::DEFAULT_TTL.get(),
            Ipv6Proto::Icmpv6,
        );

        let get_expected_result = |template| expected_result.map(|()| template);

        let template = IpSock {
            defn: IpSockDefinition {
                remote_ip: to_ip,
                local_ip: expected_from_ip,
                device: local_device,
                proto,
                hop_limit: None,
            },
        };

        let res = IpSocketHandler::<I, _>::new_ip_socket(
            &mut sync_ctx,
            &mut non_sync_ctx,
            local_device,
            from_ip,
            to_ip,
            proto,
            None,
        );
        assert_eq!(res, get_expected_result(template.clone()));

        #[ipv4]
        {
            // TTL is specified.
            let mut builder = Ipv4SocketBuilder::default();
            let _: &mut Ipv4SocketBuilder = builder.ttl(NonZeroU8::new(1).unwrap());
            assert_eq!(
                IpSocketHandler::<Ipv4, _>::new_ip_socket(
                    &mut sync_ctx,
                    &mut non_sync_ctx,
                    local_device,
                    from_ip,
                    to_ip,
                    proto,
                    Some(builder),
                ),
                {
                    // The template socket, but with the TTL set to 1.
                    let mut x = template.clone();
                    let IpSock::<Ipv4, DeviceId> { defn } = &mut x;
                    defn.hop_limit = NonZeroU8::new(1);
                    get_expected_result(x)
                }
            );
        }

        #[ipv6]
        {
            // Hop Limit is specified.
            const SPECIFIED_HOP_LIMIT: u8 = 1;
            let mut builder = Ipv6SocketBuilder::default();
            let _: &mut Ipv6SocketBuilder =
                builder.hop_limit(NonZeroU8::new(SPECIFIED_HOP_LIMIT).unwrap());
            assert_eq!(
                IpSocketHandler::<Ipv6, _>::new_ip_socket(
                    &mut sync_ctx,
                    &mut non_sync_ctx,
                    local_device,
                    from_ip,
                    to_ip,
                    proto,
                    Some(builder),
                ),
                {
                    let mut template_with_hop_limit = template.clone();
                    let IpSock::<Ipv6, DeviceId> { defn } = &mut template_with_hop_limit;
                    defn.hop_limit = NonZeroU8::new(SPECIFIED_HOP_LIMIT);
                    let builder =
                        Ipv6PacketBuilder::new(expected_from_ip, to_ip, SPECIFIED_HOP_LIMIT, proto);
                    get_expected_result(template_with_hop_limit)
                }
            );
        }
    }

    #[ip_test]
    fn test_new_unroutable_local_to_remote<I: Ip>() {
        test_new::<I>(NewSocketTestCase {
            local_ip_type: AddressType::Unroutable,
            remote_ip_type: AddressType::Remote,
            device_type: DeviceType::Unspecified,
            expected_result: Err(IpSockRouteError::Unroutable(
                IpSockUnroutableError::LocalAddrNotAssigned,
            )
            .into()),
        });
    }

    #[ip_test]
    fn test_new_local_to_unroutable_remote<I: Ip>() {
        test_new::<I>(NewSocketTestCase {
            local_ip_type: AddressType::LocallyOwned,
            remote_ip_type: AddressType::Unroutable,
            device_type: DeviceType::Unspecified,
            expected_result: Err(IpSockRouteError::Unroutable(
                IpSockUnroutableError::NoRouteToRemoteAddr,
            )
            .into()),
        });
    }

    #[ip_test]
    fn test_new_local_to_remote<I: Ip>() {
        test_new::<I>(NewSocketTestCase {
            local_ip_type: AddressType::LocallyOwned,
            remote_ip_type: AddressType::Remote,
            device_type: DeviceType::Unspecified,
            expected_result: Ok(()),
        });
    }

    #[ip_test]
    fn test_new_unspecified_to_remote<I: Ip>() {
        test_new::<I>(NewSocketTestCase {
            local_ip_type: AddressType::Unspecified { can_select: true },
            remote_ip_type: AddressType::Remote,
            device_type: DeviceType::Unspecified,
            expected_result: Ok(()),
        });
    }

    #[ip_test]
    fn test_new_unspecified_to_remote_through_local_device<I: Ip>() {
        test_new::<I>(NewSocketTestCase {
            local_ip_type: AddressType::Unspecified { can_select: true },
            remote_ip_type: AddressType::Remote,
            device_type: DeviceType::LocalDevice,
            expected_result: Ok(()),
        });
    }

    #[ip_test]
    fn test_new_unspecified_to_remote_through_other_device<I: Ip>() {
        test_new::<I>(NewSocketTestCase {
            local_ip_type: AddressType::Unspecified { can_select: true },
            remote_ip_type: AddressType::Remote,
            device_type: DeviceType::OtherDevice,
            expected_result: Err(IpSockRouteError::Unroutable(
                IpSockUnroutableError::NoRouteToRemoteAddr,
            )
            .into()),
        });
    }

    #[ip_test]
    fn test_new_unspecified_to_remote_cant_select<I: Ip>() {
        test_new::<I>(NewSocketTestCase {
            local_ip_type: AddressType::Unspecified { can_select: false },
            remote_ip_type: AddressType::Remote,
            device_type: DeviceType::Unspecified,
            expected_result: Err(IpSockRouteError::NoLocalAddrAvailable.into()),
        });
    }

    #[ip_test]
    fn test_new_remote_to_remote<I: Ip>() {
        test_new::<I>(NewSocketTestCase {
            local_ip_type: AddressType::Remote,
            remote_ip_type: AddressType::Remote,
            device_type: DeviceType::Unspecified,
            expected_result: Err(IpSockRouteError::Unroutable(
                IpSockUnroutableError::LocalAddrNotAssigned,
            )
            .into()),
        });
    }

    #[ip_test]
    fn test_new_local_to_local<I: Ip>() {
        test_new::<I>(NewSocketTestCase {
            local_ip_type: AddressType::LocallyOwned,
            remote_ip_type: AddressType::LocallyOwned,
            device_type: DeviceType::Unspecified,
            expected_result: Ok(()),
        });
    }

    #[ip_test]
    fn test_new_unspecified_to_local<I: Ip>() {
        test_new::<I>(NewSocketTestCase {
            local_ip_type: AddressType::Unspecified { can_select: true },
            remote_ip_type: AddressType::LocallyOwned,
            device_type: DeviceType::Unspecified,
            expected_result: Ok(()),
        });
    }

    #[ip_test]
    fn test_new_remote_to_local<I: Ip>() {
        test_new::<I>(NewSocketTestCase {
            local_ip_type: AddressType::Remote,
            remote_ip_type: AddressType::LocallyOwned,
            device_type: DeviceType::Unspecified,
            expected_result: Err(IpSockRouteError::Unroutable(
                IpSockUnroutableError::LocalAddrNotAssigned,
            )
            .into()),
        });
    }

    #[specialize_ip]
    fn test_send_local<I: Ip>(from_addr_type: AddressType, to_addr_type: AddressType) {
        set_logger_for_test();

        use packet_formats::icmp::{IcmpEchoRequest, IcmpPacketBuilder, IcmpUnusedCode};

        #[ipv4]
        let (subnet, local_ip, remote_ip, local_mac, proto, socket_builder) = {
            let DummyEventDispatcherConfig::<Ipv4Addr> {
                subnet,
                local_ip,
                remote_ip,
                local_mac,
                remote_mac: _,
            } = DUMMY_CONFIG_V4;

            (subnet, local_ip, remote_ip, local_mac, Ipv4Proto::Icmp, Ipv4SocketBuilder::default())
        };

        #[ipv6]
        let (subnet, local_ip, remote_ip, local_mac, proto, socket_builder) = {
            let DummyEventDispatcherConfig::<Ipv6Addr> {
                subnet,
                local_ip,
                remote_ip,
                local_mac,
                remote_mac: _,
            } = DUMMY_CONFIG_V6;

            (
                subnet,
                local_ip,
                remote_ip,
                local_mac,
                Ipv6Proto::Icmpv6,
                Ipv6SocketBuilder::default(),
            )
        };

        let mut builder = DummyEventDispatcherBuilder::default();
        let device_id = DeviceId::new_ethernet(builder.add_device(local_mac));
        let Ctx { mut sync_ctx, mut non_sync_ctx } = builder.build();
        crate::device::add_ip_addr_subnet(
            &mut sync_ctx,
            &mut non_sync_ctx,
            device_id,
            AddrSubnet::new(local_ip.get(), 16).unwrap(),
        )
        .unwrap();
        crate::device::add_ip_addr_subnet(
            &mut sync_ctx,
            &mut non_sync_ctx,
            device_id,
            AddrSubnet::new(remote_ip.get(), 16).unwrap(),
        )
        .unwrap();
        match subnet.into() {
            SubnetEither::V4(subnet) => crate::ip::add_device_route::<Ipv4, _, _>(
                &mut sync_ctx,
                &mut non_sync_ctx,
                subnet,
                device_id,
            )
            .expect("install IPv4 device route on a fresh stack without routes"),
            SubnetEither::V6(subnet) => crate::ip::add_device_route::<Ipv6, _, _>(
                &mut sync_ctx,
                &mut non_sync_ctx,
                subnet,
                device_id,
            )
            .expect("install IPv6 device route on a fresh stack without routes"),
        }

        let loopback_device_id = crate::add_loopback_device(&mut sync_ctx, u16::MAX.into())
            .expect("create the loopback interface");
        crate::device::testutil::enable_device(
            &mut sync_ctx,
            &mut non_sync_ctx,
            loopback_device_id,
        );

        let (expected_from_ip, from_ip) = match from_addr_type {
            AddressType::LocallyOwned => (local_ip, Some(local_ip)),
            AddressType::Remote => panic!("from_addr_type cannot be remote"),
            AddressType::Unspecified { can_select: _ } => (local_ip, None),
            AddressType::Unroutable => panic!("from_addr_type cannot be unroutable"),
        };

        let to_ip = match to_addr_type {
            AddressType::LocallyOwned => local_ip,
            AddressType::Remote => remote_ip,
            AddressType::Unspecified { can_select: _ } => {
                panic!("to_addr_type cannot be unspecified")
            }
            AddressType::Unroutable => panic!("to_addr_type cannot be unroutable"),
        };

        let sock = IpSocketHandler::<I, _>::new_ip_socket(
            &mut sync_ctx,
            &mut non_sync_ctx,
            None,
            from_ip,
            to_ip,
            proto,
            Some(socket_builder),
        )
        .unwrap();

        let reply = IcmpEchoRequest::new(0, 0).reply();
        let body = &[1, 2, 3, 4];
        let buffer = Buf::new(body.to_vec(), ..)
            .encapsulate(IcmpPacketBuilder::<I, &[u8], _>::new(
                expected_from_ip,
                to_ip,
                IcmpUnusedCode,
                reply,
            ))
            .serialize_vec_outer()
            .unwrap();

        // Send an echo packet on the socket and validate that the packet is
        // delivered locally.
        BufferIpSocketHandler::<I, _, _>::send_ip_packet(
            &mut sync_ctx,
            &mut non_sync_ctx,
            &sock,
            buffer.into_inner().buffer_view().as_ref().into_serializer(),
            None,
        )
        .unwrap();

        assert_eq!(sync_ctx.dispatcher.frames_sent().len(), 0);

        #[ipv4]
        assert_eq!(get_counter_val(&mut sync_ctx, "dispatch_receive_ipv4_packet"), 1);

        #[ipv6]
        assert_eq!(get_counter_val(&mut sync_ctx, "dispatch_receive_ipv6_packet"), 1);
    }

    #[ip_test]
    fn test_send_local_to_local<I: Ip>() {
        test_send_local::<I>(AddressType::LocallyOwned, AddressType::LocallyOwned);
    }

    #[ip_test]
    fn test_send_unspecified_to_local<I: Ip>() {
        test_send_local::<I>(
            AddressType::Unspecified { can_select: true },
            AddressType::LocallyOwned,
        );
    }

    #[ip_test]
    fn test_send_local_to_remote<I: Ip>() {
        test_send_local::<I>(AddressType::LocallyOwned, AddressType::Remote);
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
            let _: &mut Ipv6SocketBuilder = builder.hop_limit(NonZeroU8::new(1).unwrap());
            (DUMMY_CONFIG_V6, builder, Ipv6Proto::Icmpv6)
        };

        let DummyEventDispatcherConfig::<_> { local_mac, remote_mac, local_ip, remote_ip, subnet } =
            cfg;

        let Ctx { mut sync_ctx, mut non_sync_ctx } =
            DummyEventDispatcherBuilder::from_config(cfg.clone()).build();

        // Create a normal, routable socket.
        let sock = IpSocketHandler::<I, _>::new_ip_socket(
            &mut sync_ctx,
            &mut non_sync_ctx,
            None,
            None,
            remote_ip,
            proto,
            Some(socket_builder),
        )
        .unwrap();

        #[ipv4]
        let curr_id = crate::ip::gen_ipv4_packet_id(&mut sync_ctx);

        #[ipv4]
        let check_frame = move |frame: &[u8], packet_count| {
            let (mut body, src_mac, dst_mac, _ethertype) = parse_ethernet_frame(frame).unwrap();
            let packet = (&mut body).parse::<Ipv4Packet<&[u8]>>().unwrap();
            assert_eq!(src_mac, local_mac.get());
            assert_eq!(dst_mac, remote_mac.get());
            assert_eq!(packet.src_ip(), local_ip.get());
            assert_eq!(packet.dst_ip(), remote_ip.get());
            assert_eq!(packet.proto(), proto);
            assert_eq!(packet.ttl(), 1);
            let Ipv4OnlyMeta { id } = packet.version_specific_meta();
            assert_eq!(usize::from(id), usize::from(curr_id) + packet_count);
            assert_eq!(body, [0]);
        };

        #[ipv6]
        let check_frame = move |frame: &[u8], _packet_count| {
            let (body, src_mac, dst_mac, src_ip, dst_ip, ip_proto, ttl) =
                parse_ip_packet_in_ethernet_frame::<Ipv6>(frame).unwrap();
            assert_eq!(body, [0]);
            assert_eq!(src_mac, local_mac.get());
            assert_eq!(dst_mac, remote_mac.get());
            assert_eq!(src_ip, local_ip.get());
            assert_eq!(dst_ip, remote_ip.get());
            assert_eq!(ip_proto, proto);
            assert_eq!(ttl, 1);
        };
        let mut packet_count = 0;
        assert_eq!(sync_ctx.dispatcher.frames_sent().len(), packet_count);

        // Send a packet on the socket and make sure that the right contents
        // are sent.
        BufferIpSocketHandler::<I, _, _>::send_ip_packet(
            &mut sync_ctx,
            &mut non_sync_ctx,
            &sock,
            (&[0u8][..]).into_serializer(),
            None,
        )
        .unwrap();
        let mut check_sent_frame = |sync_ctx: &SyncCtx<DummyEventDispatcher, _, _>| {
            packet_count += 1;
            assert_eq!(sync_ctx.dispatcher.frames_sent().len(), packet_count);
            let (dev, frame) = &sync_ctx.dispatcher.frames_sent()[packet_count - 1];
            assert_eq!(dev, &DeviceId::new_ethernet(0));
            check_frame(&frame, packet_count);
        };
        check_sent_frame(&sync_ctx);

        // Send a packet while imposing an MTU that is large enough to fit the
        // packet.
        let small_body = [0; 1];
        let small_body_serializer = (&small_body).into_serializer();
        let res = BufferIpSocketHandler::<I, _, _>::send_ip_packet(
            &mut sync_ctx,
            &mut non_sync_ctx,
            &sock,
            small_body_serializer,
            Some(Ipv6::MINIMUM_LINK_MTU.into()),
        );
        assert_matches::assert_matches!(res, Ok(()));
        check_sent_frame(&sync_ctx);

        // Send a packet on the socket while imposing an MTU which will not
        // allow a packet to be sent.
        let res = BufferIpSocketHandler::<I, _, _>::send_ip_packet(
            &mut sync_ctx,
            &mut non_sync_ctx,
            &sock,
            small_body_serializer,
            Some(1), // mtu
        );
        assert_matches::assert_matches!(res, Err((_, IpSockSendError::Mtu)));

        assert_eq!(sync_ctx.dispatcher.frames_sent().len(), packet_count);
        // Try sending a packet which will be larger than the device's MTU,
        // and make sure it fails.
        let res = BufferIpSocketHandler::<I, _, _>::send_ip_packet(
            &mut sync_ctx,
            &mut non_sync_ctx,
            &sock,
            (&[0; crate::ip::Ipv6::MINIMUM_LINK_MTU as usize][..]).into_serializer(),
            None,
        );
        assert_matches::assert_matches!(res, Err((_, IpSockSendError::Mtu)));

        // Make sure that sending on an unroutable socket fails.
        crate::ip::del_route::<I, _, _>(&mut sync_ctx, &mut non_sync_ctx, subnet).unwrap();
        let res = BufferIpSocketHandler::<I, _, _>::send_ip_packet(
            &mut sync_ctx,
            &mut non_sync_ctx,
            &sock,
            small_body_serializer,
            None,
        );
        assert_matches::assert_matches!(
            res,
            Err((_, IpSockSendError::Unroutable(IpSockUnroutableError::NoRouteToRemoteAddr)))
        );
    }
}
