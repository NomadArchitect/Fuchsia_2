// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The Ethernet protocol.

use alloc::boxed::Box;
use alloc::collections::HashMap;
use alloc::collections::VecDeque;
use alloc::vec::{IntoIter, Vec};
use core::fmt::Debug;
use core::num::NonZeroU8;
use core::slice::Iter;

use log::{debug, trace};
use net_types::ethernet::Mac;
use net_types::ip::{
    AddrSubnet, Ip, IpAddr, IpAddress, IpVersion, Ipv4, Ipv4Addr, Ipv6, Ipv6Addr,
    UnicastOrMulticastIpv6Addr,
};
use net_types::{
    BroadcastAddress, LinkLocalAddress, LinkLocalUnicastAddr, MulticastAddr, MulticastAddress,
    SpecifiedAddr, UnicastAddr, UnicastAddress, Witness,
};
use packet::{Buf, BufferMut, EmptyBuf, Nested, Serializer};
use packet_formats::arp::{peek_arp_types, ArpHardwareType, ArpNetworkType};
use packet_formats::ethernet::{
    EtherType, EthernetFrame, EthernetFrameBuilder, EthernetFrameLengthCheck, EthernetIpExt,
};
use specialize_ip_macro::specialize_ip_address;

use crate::context::{DualStateContext, FrameContext, InstantContext, StateContext, TimerHandler};
use crate::data_structures::ref_counted_hash_map::{InsertResult, RefCountedHashSet, RemoveResult};
use crate::device::arp::{
    self, ArpContext, ArpDeviceIdContext, ArpFrameMetadata, ArpState, ArpTimerId,
};
use crate::device::link::LinkDevice;
use crate::device::ndp::{self, NdpContext, NdpHandler, NdpState, NdpTimerId};
use crate::device::{
    AddrConfigType, AddressEntry, AddressError, AddressState, BufferIpDeviceContext,
    DeviceIdContext, FrameDestination, IpDeviceContext, RecvIpFrameMeta, Tentative,
};
use crate::ip::gmp::igmp::{IgmpContext, IgmpGroupState, IgmpPacketMetadata, IgmpTimerId};
use crate::ip::gmp::mld::{MldContext, MldFrameMetadata, MldGroupState, MldReportDelay};
use crate::ip::gmp::{GmpHandler, GroupJoinResult, GroupLeaveResult, MulticastGroupSet};
#[cfg(test)]
use crate::Ctx;
use crate::Instant;

const ETHERNET_MAX_PENDING_FRAMES: usize = 10;

impl From<Mac> for FrameDestination {
    fn from(mac: Mac) -> FrameDestination {
        if mac.is_broadcast() {
            FrameDestination::Broadcast
        } else if mac.is_multicast() {
            FrameDestination::Multicast
        } else {
            debug_assert!(mac.is_unicast());
            FrameDestination::Unicast
        }
    }
}

/// A shorthand for `IpDeviceContext` with all of the appropriate type arguments
/// fixed to their Ethernet values.
pub(crate) trait EthernetIpDeviceContext:
    IpDeviceContext<
    EthernetLinkDevice,
    EthernetTimerId<<Self as DeviceIdContext<EthernetLinkDevice>>::DeviceId>,
    EthernetDeviceState<<Self as InstantContext>::Instant>,
>
{
}

impl<
        C: IpDeviceContext<
            EthernetLinkDevice,
            EthernetTimerId<<C as DeviceIdContext<EthernetLinkDevice>>::DeviceId>,
            EthernetDeviceState<<C as InstantContext>::Instant>,
        >,
    > EthernetIpDeviceContext for C
{
}

/// A shorthand for `BufferIpDeviceContext` with all of the appropriate type
/// arguments fixed to their Ethernet values.
pub(super) trait BufferEthernetIpDeviceContext<B: BufferMut>:
    BufferIpDeviceContext<
    EthernetLinkDevice,
    EthernetTimerId<<Self as DeviceIdContext<EthernetLinkDevice>>::DeviceId>,
    EthernetDeviceState<<Self as InstantContext>::Instant>,
    B,
>
{
}

impl<
        B: BufferMut,
        C: BufferIpDeviceContext<
            EthernetLinkDevice,
            EthernetTimerId<<C as DeviceIdContext<EthernetLinkDevice>>::DeviceId>,
            EthernetDeviceState<<C as InstantContext>::Instant>,
            B,
        >,
    > BufferEthernetIpDeviceContext<B> for C
{
}

impl<C: EthernetIpDeviceContext>
    DualStateContext<MulticastGroupSet<Ipv4Addr, IgmpGroupState<C::Instant>>, C::Rng, C::DeviceId>
    for C
{
    fn get_states_with(
        &self,
        device: C::DeviceId,
        _id1: (),
    ) -> (&MulticastGroupSet<Ipv4Addr, IgmpGroupState<C::Instant>>, &C::Rng) {
        let (state, rng) = self.get_states_with(device, ());
        (&state.ip().ipv4_multicast_groups, rng)
    }

    fn get_states_mut_with(
        &mut self,
        device: C::DeviceId,
        _id1: (),
    ) -> (&mut MulticastGroupSet<Ipv4Addr, IgmpGroupState<C::Instant>>, &mut C::Rng) {
        let (state, rng) = self.get_states_mut_with(device, ());
        (&mut state.ip_mut().ipv4_multicast_groups, rng)
    }
}

impl<C: EthernetIpDeviceContext>
    DualStateContext<MulticastGroupSet<Ipv6Addr, MldGroupState<C::Instant>>, C::Rng, C::DeviceId>
    for C
{
    fn get_states_with(
        &self,
        device: C::DeviceId,
        _id1: (),
    ) -> (&MulticastGroupSet<Ipv6Addr, MldGroupState<C::Instant>>, &C::Rng) {
        let (state, rng) = self.get_states_with(device, ());
        (&state.ip().ipv6_multicast_groups, rng)
    }

    fn get_states_mut_with(
        &mut self,
        device: C::DeviceId,
        _id1: (),
    ) -> (&mut MulticastGroupSet<Ipv6Addr, MldGroupState<C::Instant>>, &mut C::Rng) {
        let (state, rng) = self.get_states_mut_with(device, ());
        (&mut state.ip_mut().ipv6_multicast_groups, rng)
    }
}

impl<C: EthernetIpDeviceContext> FrameContext<EmptyBuf, IgmpPacketMetadata<C::DeviceId>> for C {
    fn send_frame<S: Serializer<Buffer = EmptyBuf>>(
        &mut self,
        meta: IgmpPacketMetadata<C::DeviceId>,
        body: S,
    ) -> Result<(), S> {
        send_ip_frame(self, meta.device, meta.dst_ip.into_specified(), body)
    }
}

impl<C: EthernetIpDeviceContext> FrameContext<EmptyBuf, MldFrameMetadata<C::DeviceId>> for C {
    fn send_frame<S: Serializer<Buffer = EmptyBuf>>(
        &mut self,
        meta: MldFrameMetadata<C::DeviceId>,
        body: S,
    ) -> Result<(), S> {
        send_ip_frame(self, meta.device, meta.dst_ip.into_specified(), body)
    }
}

impl<C: EthernetIpDeviceContext> IgmpContext<EthernetLinkDevice> for C {
    fn get_ip_addr_subnet(&self, device: C::DeviceId) -> Option<AddrSubnet<Ipv4Addr>> {
        get_ip_addr_subnet(self, device)
    }

    fn igmp_enabled(&self, device: C::DeviceId) -> bool {
        self.get_state_with(device).ip().igmp_enabled
    }
}

impl<C: EthernetIpDeviceContext> MldContext<EthernetLinkDevice> for C {
    fn get_ipv6_link_local_addr(
        &self,
        device: C::DeviceId,
    ) -> Option<LinkLocalUnicastAddr<Ipv6Addr>> {
        get_ipv6_link_local_addr(self, device)
    }

    fn mld_enabled(&self, device: C::DeviceId) -> bool {
        self.get_state_with(device).ip().mld_enabled
    }
}

/// Builder for [`EthernetDeviceState`].
pub(crate) struct EthernetDeviceStateBuilder {
    mac: Mac,
    mtu: u32,
    ndp_configs: ndp::NdpConfigurations,
}

impl EthernetDeviceStateBuilder {
    /// Create a new `EthernetDeviceStateBuilder`.
    pub(crate) fn new(mac: Mac, mtu: u32) -> Self {
        // TODO(joshlf): Add a minimum MTU for all Ethernet devices such that
        //  you cannot create an `EthernetDeviceState` with an MTU smaller than
        //  the minimum. The absolute minimum needs to be at least the minimum
        //  body size of an Ethernet frame. For IPv6-capable devices, the
        //  minimum needs to be higher - the IPv6 minimum MTU. The easy path is
        //  to simply use the IPv6 minimum MTU as the minimum in all cases,
        //  although we may at some point want to figure out how to configure
        //  devices which don't support IPv6, and allow smaller MTUs for those
        //  devices.
        //
        //  A few questions:
        //  - How do we wire error information back up the call stack? Should
        //    this just return a Result or something?
        Self { mac, mtu, ndp_configs: ndp::NdpConfigurations::default() }
    }

    /// Update the NDP configurations that will be set on the ethernet device.
    pub(crate) fn set_ndp_configs(&mut self, v: ndp::NdpConfigurations) {
        self.ndp_configs = v;
    }

    /// Build the `EthernetDeviceState` from this builder.
    pub(super) fn build<I: Instant>(self) -> EthernetDeviceState<I> {
        EthernetDeviceState {
            mac: self.mac,
            mtu: self.mtu,
            hw_mtu: self.mtu,
            link_multicast_groups: RefCountedHashSet::default(),
            ipv4_arp: ArpState::default(),
            ndp: NdpState::new(self.ndp_configs),
            pending_frames: HashMap::new(),
            promiscuous_mode: false,
        }
    }
}

/// The state associated with an Ethernet device.
pub(crate) struct EthernetDeviceState<I: Instant> {
    /// Mac address of the device this state is for.
    mac: Mac,

    /// The value this netstack assumes as the device's current MTU.
    mtu: u32,

    /// The maximum MTU allowed by the hardware.
    ///
    /// `mtu` MUST NEVER be greater than `hw_mtu`.
    hw_mtu: u32,

    /// Link multicast groups this device has joined.
    link_multicast_groups: RefCountedHashSet<MulticastAddr<Mac>>,

    /// IPv4 ARP state.
    ipv4_arp: ArpState<EthernetLinkDevice, Ipv4Addr>,

    /// (IPv6) NDP state.
    ndp: ndp::NdpState<EthernetLinkDevice, I>,

    // pending_frames stores a list of serialized frames indexed by their
    // destination IP addresses. The frames contain an entire EthernetFrame
    // body and the MTU check is performed before queueing them here.
    pending_frames: HashMap<IpAddr, VecDeque<Buf<Vec<u8>>>>,

    /// A flag indicating whether the device will accept all ethernet frames
    /// that it receives, regardless of the ethernet frame's destination MAC
    /// address.
    promiscuous_mode: bool,
}

impl<I: Instant> EthernetDeviceState<I> {
    /// Adds a pending frame `frame` associated with `local_addr` to the list of
    /// pending frames in the current device state.
    ///
    /// If an older frame had to be dropped because it exceeds the maximum
    /// allowed number of pending frames, it is returned.
    fn add_pending_frame(
        &mut self,
        local_addr: IpAddr,
        frame: Buf<Vec<u8>>,
    ) -> Option<Buf<Vec<u8>>> {
        let buff = self.pending_frames.entry(local_addr).or_insert_with(Default::default);
        buff.push_back(frame);
        if buff.len() > ETHERNET_MAX_PENDING_FRAMES {
            buff.pop_front()
        } else {
            None
        }
    }

    /// Takes all pending frames associated with address `local_addr`.
    fn take_pending_frames(
        &mut self,
        local_addr: IpAddr,
    ) -> Option<impl Iterator<Item = Buf<Vec<u8>>>> {
        match self.pending_frames.remove(&local_addr) {
            Some(buff) => Some(buff.into_iter()),
            None => None,
        }
    }

    /// Is a packet with a destination MAC address, `dst`, destined for this
    /// device?
    ///
    /// Returns `true` if this device is has `dst_mac` as its assigned MAC
    /// address, `dst_mac` is the broadcast MAC address, or it is one of the
    /// multicast MAC addresses the device has joined.
    fn should_accept(&self, dst_mac: &Mac) -> bool {
        (self.mac == *dst_mac)
            || dst_mac.is_broadcast()
            || (MulticastAddr::new(*dst_mac)
                .map(|a| self.link_multicast_groups.contains(&a))
                .unwrap_or(false))
    }

    /// Should a packet with destination MAC address, `dst`, be accepted by this
    /// device?
    ///
    /// Returns `true` if this device is in promiscuous mode or the frame is
    /// destined for this device.
    fn should_deliver(&self, dst_mac: &Mac) -> bool {
        self.promiscuous_mode || self.should_accept(dst_mac)
    }
}

/// A timer ID for Ethernet devices.
///
/// `D` is the type of device ID that identifies different Ethernet devices.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Hash)]
pub(crate) enum EthernetTimerId<D> {
    Arp(ArpTimerId<EthernetLinkDevice, Ipv4Addr, D>),
    Ndp(NdpTimerId<EthernetLinkDevice, D>),
    Igmp(IgmpTimerId<EthernetLinkDevice, D>),
    Mld(MldReportDelay<EthernetLinkDevice, D>),
}

impl<D> From<ArpTimerId<EthernetLinkDevice, Ipv4Addr, D>> for EthernetTimerId<D> {
    fn from(id: ArpTimerId<EthernetLinkDevice, Ipv4Addr, D>) -> EthernetTimerId<D> {
        EthernetTimerId::Arp(id)
    }
}

impl<D> From<NdpTimerId<EthernetLinkDevice, D>> for EthernetTimerId<D> {
    fn from(id: NdpTimerId<EthernetLinkDevice, D>) -> EthernetTimerId<D> {
        EthernetTimerId::Ndp(id)
    }
}

impl<D> From<IgmpTimerId<EthernetLinkDevice, D>> for EthernetTimerId<D> {
    fn from(id: IgmpTimerId<EthernetLinkDevice, D>) -> EthernetTimerId<D> {
        EthernetTimerId::Igmp(id)
    }
}

impl<D> From<MldReportDelay<EthernetLinkDevice, D>> for EthernetTimerId<D> {
    fn from(id: MldReportDelay<EthernetLinkDevice, D>) -> EthernetTimerId<D> {
        EthernetTimerId::Mld(id)
    }
}

/// Handle an Ethernet timer firing.
pub(super) fn handle_timer<C: EthernetIpDeviceContext>(
    ctx: &mut C,
    id: EthernetTimerId<C::DeviceId>,
) {
    match id {
        EthernetTimerId::Arp(id) => arp::handle_timer(ctx, id.into()),
        EthernetTimerId::Ndp(id) => <C as NdpHandler<EthernetLinkDevice>>::handle_timer(ctx, id),
        EthernetTimerId::Igmp(id) => TimerHandler::handle_timer(ctx, id),
        EthernetTimerId::Mld(id) => TimerHandler::handle_timer(ctx, id),
    }
}

// If we are provided with an impl of `TimerContext<EthernetTimerId<_>>`, then
// we can in turn provide impls of `TimerContext` for ARP, NDP, IGMP, and MLD
// timers.
impl_timer_context!(
    DeviceIdContext<EthernetLinkDevice>,
    EthernetTimerId<<C as DeviceIdContext<EthernetLinkDevice>>::DeviceId>,
    NdpTimerId<EthernetLinkDevice, <C as DeviceIdContext<EthernetLinkDevice>>::DeviceId>,
    EthernetTimerId::Ndp(id),
    id
);
impl_timer_context!(
    DeviceIdContext<EthernetLinkDevice>,
    EthernetTimerId<<C as DeviceIdContext<EthernetLinkDevice>>::DeviceId>,
    ArpTimerId<EthernetLinkDevice, Ipv4Addr, <C as DeviceIdContext<EthernetLinkDevice>>::DeviceId>,
    EthernetTimerId::Arp(id),
    id
);
impl_timer_context!(
    DeviceIdContext<EthernetLinkDevice>,
    EthernetTimerId<<C as DeviceIdContext<EthernetLinkDevice>>::DeviceId>,
    IgmpTimerId<EthernetLinkDevice, <C as DeviceIdContext<EthernetLinkDevice>>::DeviceId>,
    EthernetTimerId::Igmp(id),
    id
);
impl_timer_context!(
    DeviceIdContext<EthernetLinkDevice>,
    EthernetTimerId<<C as DeviceIdContext<EthernetLinkDevice>>::DeviceId>,
    MldReportDelay<EthernetLinkDevice, <C as DeviceIdContext<EthernetLinkDevice>>::DeviceId>,
    EthernetTimerId::Mld(id),
    id
);

/// Initialize a device.
///
/// `initialize_device` sets the link-local address for `device_id` and performs
/// DAD on it.
///
/// `device_id` MUST be ready to send packets before `initialize_device` is
/// called.
pub(super) fn initialize_device<C: EthernetIpDeviceContext>(ctx: &mut C, device_id: C::DeviceId) {
    // Assign a link-local address.

    let state = ctx.get_state_with(device_id);

    // There should be no way to add addresses to a device before it's
    // initialized.
    assert!(state.ip().ipv6_addr_sub.is_empty());

    // Join the MAC-derived link-local address. Mark it as configured by SLAAC
    // and not set to expire.
    let addr_sub = state.link().mac.to_ipv6_link_local().into_witness();
    add_ip_addr_subnet_inner(ctx, device_id, addr_sub, AddrConfigType::Slaac, None).expect(
        "internal invariant violated: uninitialized device already had IP address assigned",
    );
}

/// Send an IP packet in an Ethernet frame.
///
/// `send_ip_frame` accepts a device ID, a local IP address, and a
/// `SerializationRequest`. It computes the routing information and serializes
/// the request in a new Ethernet frame and sends it.
#[specialize_ip_address]
pub(super) fn send_ip_frame<
    B: BufferMut,
    C: EthernetIpDeviceContext + FrameContext<B, <C as DeviceIdContext<EthernetLinkDevice>>::DeviceId>,
    A: IpAddress,
    S: Serializer<Buffer = B>,
>(
    ctx: &mut C,
    device_id: C::DeviceId,
    local_addr: SpecifiedAddr<A>,
    body: S,
) -> Result<(), S> {
    trace!("ethernet::send_ip_frame: local_addr = {:?}; device = {:?}", local_addr, device_id);

    let state = ctx.get_state_mut_with(device_id).link_mut();
    let (local_mac, mtu) = (state.mac, state.mtu);

    #[ipv4addr]
    let dst_mac = match MulticastAddr::from_witness(local_addr) {
        Some(multicast) => Ok(Mac::from(&multicast)),
        None => {
            arp::lookup(ctx, device_id, local_mac, local_addr.get()).ok_or(IpAddr::V4(local_addr))
        }
    };
    #[ipv6addr]
    let dst_mac = match UnicastOrMulticastIpv6Addr::from_specified(local_addr) {
        UnicastOrMulticastIpv6Addr::Multicast(addr) => Ok(Mac::from(&addr)),
        UnicastOrMulticastIpv6Addr::Unicast(addr) => {
            <C as NdpHandler<_>>::lookup(ctx, device_id, addr).ok_or(IpAddr::V6(local_addr))
        }
    };

    match dst_mac {
        Ok(dst_mac) => ctx
            .send_frame(
                device_id.into(),
                body.with_mtu(mtu as usize).encapsulate(EthernetFrameBuilder::new(
                    local_mac,
                    dst_mac,
                    A::Version::ETHER_TYPE,
                )),
            )
            .map_err(|ser| ser.into_inner().into_inner()),
        Err(local_addr) => {
            let state = ctx.get_state_mut_with(device_id).link_mut();
            // The `serialize_vec_outer` call returns an `Either<B,
            // Buf<Vec<u8>>`. We could naively call `.as_ref().to_vec()` on it,
            // but if it were the `Buf<Vec<u8>>` variant, we'd be unnecessarily
            // allocating a new `Vec` when we already have one. Instead, we
            // leave the `Buf<Vec<u8>>` variant as it is, and only convert the
            // `B` variant by calling `map_a`. That gives us an
            // `Either<Buf<Vec<u8>>, Buf<Vec<u8>>`, which we call `into_inner`
            // on to get a `Buf<Vec<u8>>`.
            let frame = body
                .with_mtu(mtu as usize)
                .serialize_vec_outer()
                .map_err(|ser| ser.1.into_inner())?
                .map_a(|buffer| Buf::new(buffer.as_ref().to_vec(), ..))
                .into_inner();
            let dropped = state
                .add_pending_frame(local_addr.transpose::<SpecifiedAddr<IpAddr>>().get(), frame);
            if let Some(dropped) = dropped {
                // TODO(brunodalbo): Is it ok to silently just let this drop? Or
                //  should the IP layer be notified in any way?
                log_unimplemented!((), "Ethernet dropped frame because ran out of allowable space");
            }
            Ok(())
        }
    }
}

/// Receive an Ethernet frame from the network.
pub(super) fn receive_frame<B: BufferMut, C: BufferEthernetIpDeviceContext<B>>(
    ctx: &mut C,
    device_id: C::DeviceId,
    mut buffer: B,
) {
    trace!("ethernet::receive_frame: device_id = {:?}", device_id);
    // NOTE(joshlf): We do not currently validate that the Ethernet frame
    // satisfies the minimum length requirement. We expect that if this
    // requirement is necessary (due to requirements of the physical medium),
    // the driver or hardware will have checked it, and that if this requirement
    // is not necessary, it is acceptable for us to operate on a smaller
    // Ethernet frame. If this becomes insufficient in the future, we may want
    // to consider making this behavior configurable (at compile time, at
    // runtime on a global basis, or at runtime on a per-device basis).
    let frame = if let Ok(frame) =
        buffer.parse_with::<_, EthernetFrame<_>>(EthernetFrameLengthCheck::NoCheck)
    {
        frame
    } else {
        trace!("ethernet::receive_frame: failed to parse ethernet frame");
        // TODO(joshlf): Do something else?
        return;
    };

    let (_, dst) = (frame.src_mac(), frame.dst_mac());

    if !ctx.get_state_with(device_id).link().should_deliver(&dst) {
        trace!("ethernet::receive_frame: destination mac {:?} not for device {:?}", dst, device_id);
        return;
    }

    let frame_dst = FrameDestination::from(dst);

    match frame.ethertype() {
        Some(EtherType::Arp) => {
            let types = if let Ok(types) = peek_arp_types(buffer.as_ref()) {
                types
            } else {
                // TODO(joshlf): Do something else here?
                return;
            };
            match types {
                (ArpHardwareType::Ethernet, ArpNetworkType::Ipv4) => {
                    arp::receive_arp_packet(ctx, device_id, buffer)
                }
            }
        }
        Some(EtherType::Ipv4) => {
            ctx.receive_frame(RecvIpFrameMeta::<_, Ipv4>::new(device_id, frame_dst), buffer)
        }
        Some(EtherType::Ipv6) => {
            ctx.receive_frame(RecvIpFrameMeta::<_, Ipv6>::new(device_id, frame_dst), buffer)
        }
        Some(EtherType::Other(_)) | None => {} // TODO(joshlf)
    }
}

/// Set the promiscuous mode flag on `device_id`.
pub(super) fn set_promiscuous_mode<C: EthernetIpDeviceContext>(
    ctx: &mut C,
    device_id: C::DeviceId,
    enabled: bool,
) {
    ctx.get_state_mut_with(device_id).link_mut().promiscuous_mode = enabled;
}

/// Get a single IP address for a device.
///
/// Note, tentative IP addresses (addresses which are not yet fully bound to a
/// device) will not be returned by `get_ip_addr`.
///
/// For IPv6, this only returns global (not link-local) addresses.
#[specialize_ip_address]
pub(super) fn get_ip_addr_subnet<C: EthernetIpDeviceContext, A: IpAddress>(
    ctx: &C,
    device_id: C::DeviceId,
) -> Option<AddrSubnet<A>> {
    #[ipv4addr]
    return get_assigned_ip_addr_subnets(ctx, device_id).nth(0);
    #[ipv6addr]
    return get_assigned_ip_addr_subnets(ctx, device_id).find(|a| {
        let addr: SpecifiedAddr<Ipv6Addr> = a.addr();
        !addr.is_linklocal()
    });
}

/// Get the IP address and subnet pais associated with this device which are in
/// the assigned state.
///
/// Tentative IP addresses (addresses which are not yet fully bound to a device)
/// and deprecated IP addresses (addresses which have been assigned but should
/// no longer be used for new connections) will not be returned by
/// `get_assigned_ip_addr_subnets`.
///
/// Returns an [`Iterator`] of `AddrSubnet<A>`.
///
/// See [`Tentative`] and [`AddrSubnet`] for more information.
#[specialize_ip_address]
pub(super) fn get_assigned_ip_addr_subnets<C: EthernetIpDeviceContext, A: IpAddress>(
    ctx: &C,
    device_id: C::DeviceId,
) -> Box<dyn Iterator<Item = AddrSubnet<A>> + '_> {
    let state = ctx.get_state_with(device_id).ip();

    #[ipv4addr]
    return Box::new(state.ipv4_addr_sub.iter().cloned());
    // let addresses = &state.ipv4_addr_sub;

    #[ipv6addr]
    return Box::new(state.ipv6_addr_sub.iter().filter_map(|a| {
        if a.state().is_assigned() {
            Some((*a.addr_sub()).into_witness())
        } else {
            None
        }
    }));
}

/// Get the IPv6 address/subnet pairs associated with this device, including
/// tentative and deprecated addresses.
pub(super) fn get_ipv6_addr_subnets<C: EthernetIpDeviceContext>(
    ctx: &C,
    device_id: C::DeviceId,
) -> Iter<'_, AddressEntry<Ipv6Addr, C::Instant, UnicastAddr<Ipv6Addr>>> {
    let state = ctx.get_state_with(device_id).ip();

    state.ipv6_addr_sub.iter()
}

/// Gets the state of an address on a device.
///
/// Returns `None` if `addr` is not associated with `device_id`.
pub(super) fn get_ip_addr_state<C: EthernetIpDeviceContext, A: IpAddress>(
    ctx: &C,
    device_id: C::DeviceId,
    addr: &SpecifiedAddr<A>,
) -> Option<AddressState> {
    get_ip_addr_state_inner(ctx, device_id, &addr.get())
}

/// Gets the state of an address on a device.
///
/// Returns `None` if `addr` is not associated with `device_id`.
// TODO(ghanan): Use `SpecializedAddr` for `addr`.
fn get_ip_addr_state_inner<C: EthernetIpDeviceContext, A: IpAddress>(
    ctx: &C,
    device_id: C::DeviceId,
    addr: &A,
) -> Option<AddressState> {
    let state = ctx.get_state_with(device_id).ip();
    match addr.clone().into() {
        IpAddr::V4(addr) => state.ipv4_addr_sub.iter().find_map(|a| {
            if a.addr().get() == addr {
                Some(AddressState::Assigned)
            } else {
                None
            }
        }),
        IpAddr::V6(addr) => state.ipv6_addr_sub.iter().find_map(|a| {
            if a.addr_sub().addr().get() == addr {
                Some(a.state())
            } else {
                None
            }
        }),
    }
}

/// Gets the state of an IPv6 address on a device with the an optional
/// configuration type.
///
/// If `config_type` is provided, only the state of an address that type will be
/// returned.
///
/// Returns `None` if `addr` is not associated with `device_id`.
// TODO(ghanan): Use `SpecializedAddr` for `addr`.
fn get_ipv6_addr_state_with_config<C: EthernetIpDeviceContext>(
    ctx: &C,
    device_id: C::DeviceId,
    addr: &Ipv6Addr,
    config_type: Option<AddrConfigType>,
) -> Option<AddressState> {
    ctx.get_state_with(device_id).ip().ipv6_addr_sub.iter().find_map(|a| {
        if a.addr_sub().addr().get() == *addr && config_type.map_or(true, |c| c == a.config_type())
        {
            Some(a.state())
        } else {
            None
        }
    })
}

/// Adds an IP address and associated subnet to this device.
///
/// For IPv6, this function also joins the solicited-node multicast group and
/// begins performing Duplicate Address Detection (DAD).
pub(super) fn add_ip_addr_subnet<C: EthernetIpDeviceContext, A: IpAddress>(
    ctx: &mut C,
    device_id: C::DeviceId,
    addr_sub: AddrSubnet<A>,
) -> Result<(), AddressError> {
    // Add the IP address and mark it as a manually added address.
    add_ip_addr_subnet_inner(ctx, device_id, addr_sub, AddrConfigType::Manual, None)
}

/// Adds an IP address and associated subnet to this device.
///
/// `config_type` is the way this address is being configured. See
/// [`AddrConfigType`] for more details.
///
/// For IPv6, this function also joins the solicited-node multicast group and
/// begins performing Duplicate Address Detection (DAD).
///
/// # Panics
///
/// `add_ip_addr_subnet_inner` panics if `A = Ipv4Addr` and `config_type !=
/// AddrConfigType::Manual` or `valid_until != None`.
#[specialize_ip_address]
fn add_ip_addr_subnet_inner<C: EthernetIpDeviceContext, A: IpAddress>(
    ctx: &mut C,
    device_id: C::DeviceId,
    addr_sub: AddrSubnet<A>,
    config_type: AddrConfigType,
    valid_until: Option<C::Instant>,
) -> Result<(), AddressError> {
    #[ipv4addr]
    {
        assert_eq!(config_type, AddrConfigType::Manual);
        assert_eq!(valid_until, None);
    }

    let addr = addr_sub.addr().get();

    if get_ip_addr_state_inner(ctx, device_id, &addr).is_some() {
        return Err(AddressError::AlreadyExists);
    }

    let state = ctx.get_state_mut_with(device_id).ip_mut();

    #[ipv4addr]
    state.ipv4_addr_sub.push(addr_sub);

    #[ipv6addr]
    {
        // First, join the solicited-node multicast group.
        join_ip_multicast(ctx, device_id, addr.to_solicited_node_address());

        let state = ctx.get_state_mut_with(device_id).ip_mut();

        state.ipv6_addr_sub.push(AddressEntry::new(
            addr_sub.into_unicast(),
            AddressState::Tentative,
            config_type,
            valid_until,
        ));

        // Do Duplicate Address Detection on `addr`.
        ctx.start_duplicate_address_detection(device_id, addr_sub.ipv6_unicast_addr());
    }

    Ok(())
}

/// Removes an IP address and associated subnet from this device.
///
/// # Panics
///
/// Panics if `addr` is a link-local address.
// TODO(ghanan): Use a witness type to guarantee non-link-local-ness for `addr`.
pub(super) fn del_ip_addr<C: EthernetIpDeviceContext, A: IpAddress>(
    ctx: &mut C,
    device_id: C::DeviceId,
    addr: &SpecifiedAddr<A>,
) -> Result<(), AddressError> {
    del_ip_addr_inner(ctx, device_id, &addr.get(), None)
}

/// Removes an IP address and associated subnet from this device.
///
/// If `config_type` is provided, then only an address of that
/// configuration type will be removed.
///
/// # Panics
///
/// `del_ip_addr_inner` panics if `A = Ipv4Addr` and `config_type != None`.
fn del_ip_addr_inner<C: EthernetIpDeviceContext, A: IpAddress>(
    ctx: &mut C,
    device_id: C::DeviceId,
    addr: &A,
    config_type: Option<AddrConfigType>,
) -> Result<(), AddressError> {
    match addr.clone().into() {
        IpAddr::V4(addr) => {
            assert_eq!(config_type, None);

            let state = ctx.get_state_mut_with(device_id).ip_mut();

            let original_size = state.ipv4_addr_sub.len();
            state.ipv4_addr_sub.retain(|x| x.addr().get() != addr);
            let new_size = state.ipv4_addr_sub.len();

            if new_size == original_size {
                return Err(AddressError::NotFound);
            }

            assert_eq!(original_size - new_size, 1);

            Ok(())
        }
        IpAddr::V6(addr) => {
            // TODO(fxbug.dev/69196): Give `addr` the type
            // `UnicastAddr<Ipv6Addr>` for IPv6 instead of doing this dynamic
            // check here.
            if let Some((addr, state)) = UnicastAddr::new(addr).and_then(|addr| {
                get_ipv6_addr_state_with_config(ctx, device_id, &addr, config_type)
                    .map(|state| (addr, state))
            }) {
                if state.is_tentative() {
                    // Cancel current duplicate address detection for `addr` as
                    // we are removing this IP.
                    //
                    // `cancel_duplicate_address_detection` may panic if we are
                    // not performing DAD on `addr`. However, we will only reach
                    // here if `addr` is marked as tentative. If `addr` is
                    // marked as tentative, then we know that we are performing
                    // DAD on it. Given this, we know
                    // `cancel_duplicate_address_detection` will not panic.
                    ctx.cancel_duplicate_address_detection(device_id, addr);
                }
            } else {
                return Err(AddressError::NotFound);
            }

            let state = ctx.get_state_mut_with(device_id).ip_mut();

            let original_size = state.ipv6_addr_sub.len();
            state.ipv6_addr_sub.retain(|x| x.addr_sub().addr().get() != addr);
            let new_size = state.ipv6_addr_sub.len();

            // Since we just checked earlier if we had the address, we must have
            // removed it now.
            assert_eq!(original_size - new_size, 1);

            // Leave the the solicited-node multicast group.
            leave_ip_multicast(ctx, device_id, addr.to_solicited_node_address());

            Ok(())
        }
    }
}

/// Get a (non-tentative) IPv6 link-local address associated with this device.
///
/// No guarantee is made that two calls to this function will return the same
/// link-local address if multiple are available.
///
/// Returns `None` if `device_id` does not have a non-tentative link-local
/// address.
pub(super) fn get_ipv6_link_local_addr<C: EthernetIpDeviceContext>(
    ctx: &C,
    device_id: C::DeviceId,
) -> Option<LinkLocalUnicastAddr<Ipv6Addr>> {
    ctx.get_state_with(device_id).ip().ipv6_addr_sub.iter().find_map(|a| {
        if a.state().is_assigned() {
            LinkLocalUnicastAddr::new(a.addr_sub().addr())
        } else {
            None
        }
    })
}

/// Add `device_id` to a link multicast group `multicast_addr`.
///
/// Calling `join_link_multicast` with the same `device_id` and `multicast_addr`
/// is completely safe. A counter will be kept for the number of times
/// `join_link_multicast` has been called with the same `device_id` and
/// `multicast_addr` pair. To completely leave a multicast group,
/// [`leave_link_multicast`] must be called the same number of times
/// `join_link_multicast` has been called for the same `device_id` and
/// `multicast_addr` pair. The first time `join_link_multicast` is called for a
/// new `device` and `multicast_addr` pair, the device will actually join the
/// multicast group.
///
/// `join_link_multicast` is different from [`join_ip_multicast`] as
/// `join_link_multicast` joins an L2 multicast group, whereas
/// `join_ip_multicast` joins an L3 multicast group.
pub(super) fn join_link_multicast<C: EthernetIpDeviceContext>(
    ctx: &mut C,
    device_id: C::DeviceId,
    multicast_addr: MulticastAddr<Mac>,
) {
    let device_state = ctx.get_state_mut_with(device_id).link_mut();

    let groups = &mut device_state.link_multicast_groups;

    match groups.insert(multicast_addr) {
        InsertResult::Inserted(()) => {
            trace!("ethernet::join_link_multicast: joining link multicast {:?}", multicast_addr);
        }
        InsertResult::AlreadyPresent => {
            trace!(
                "ethernet::join_link_multicast: already joined link multicast {:?}",
                multicast_addr,
            );
        }
    }
}

/// Remove `device_id` from a link multicast group `multicast_addr`.
///
/// `leave_link_multicast` will attempt to remove `device_id` from the multicast
/// group `multicast_addr`. `device_id` may have "joined" the same multicast
/// address multiple times, so `device_id` will only leave the multicast group
/// once `leave_ip_multicast` has been called for each corresponding
/// [`join_link_multicast`]. That is, if `join_link_multicast` gets called 3
/// times and `leave_link_multicast` gets called two times (after all 3
/// `join_link_multicast` calls), `device_id` will still be in the multicast
/// group until the next (final) call to `leave_link_multicast`.
///
/// `leave_link_multicast` is different from [`leave_ip_multicast`] as
/// `leave_link_multicast` leaves an L2 multicast group, whereas
/// `leave_ip_multicast` leaves an L3 multicast group.
///
/// # Panics
///
/// If `device_id` is not in the multicast group `multicast_addr`.
fn leave_link_multicast<C: EthernetIpDeviceContext>(
    ctx: &mut C,
    device_id: C::DeviceId,
    multicast_addr: MulticastAddr<Mac>,
) {
    let device_state = ctx.get_state_mut_with(device_id).link_mut();

    let groups = &mut device_state.link_multicast_groups;

    match groups.remove(multicast_addr) {
        RemoveResult::Removed(()) => {
            trace!("ethernet::leave_link_multicast: leaving link multicast {:?}", multicast_addr);
        }
        RemoveResult::StillPresent => {
            trace!(
                "ethernet::leave_link_multicast: not leaving link multicast {:?} as there are still listeners for it",
                multicast_addr,
            );
        }
        RemoveResult::NotPresent => {
            panic!(
                "ethernet::leave_link_multicast: device {:?} has not yet joined link multicast {:?}",
                device_id,
                multicast_addr,
            );
        }
    }
}

/// Add `device_id` to a multicast group `multicast_addr`.
///
/// Calling `join_ip_multicast` with the same `device_id` and `multicast_addr`
/// is completely safe. A counter will be kept for the number of times
/// `join_ip_multicast` has been called with the same `device_id` and
/// `multicast_addr` pair. To completely leave a multicast group,
/// [`leave_ip_multicast`] must be called the same number of times
/// `join_ip_multicast` has been called for the same `device_id` and
/// `multicast_addr` pair. The first time `join_ip_multicast` is called for a
/// new `device` and `multicast_addr` pair, the device will actually join the
/// multicast group.
///
/// `join_ip_multicast` is different from [`join_link_multicast`] as
/// `join_ip_multicast` joins an L3 multicast group, whereas
/// `join_link_multicast` joins an L2 multicast group.
#[specialize_ip_address]
pub(super) fn join_ip_multicast<C: EthernetIpDeviceContext, A: IpAddress>(
    ctx: &mut C,
    device_id: C::DeviceId,
    multicast_addr: MulticastAddr<A>,
) {
    #[ipv4addr]
    let res = <C as GmpHandler<_, Ipv4>>::gmp_join_group(ctx, device_id, multicast_addr);

    #[ipv6addr]
    let res = <C as GmpHandler<_, Ipv6>>::gmp_join_group(ctx, device_id, multicast_addr);

    match res {
        GroupJoinResult::Joined(()) => {
            let mac = MulticastAddr::from(&multicast_addr);

            trace!(
                "ethernet::join_ip_multicast: joining IP multicast {:?} and MAC multicast {:?}",
                multicast_addr,
                mac
            );

            join_link_multicast(ctx, device_id, mac);
        }
        GroupJoinResult::AlreadyMember => trace!(
            "ethernet::join_ip_multicast: already joinined IP multicast {:?}",
            multicast_addr,
        ),
    }
}

/// Remove `device_id` from a multicast group `multicast_addr`.
///
/// `leave_ip_multicast` will attempt to remove `device_id` from a multicast
/// group `multicast_addr`. `device_id` may have "joined" the same multicast
/// address multiple times, so `device_id` will only leave the multicast group
/// once `leave_ip_multicast` has been called for each corresponding
/// [`join_ip_multicast`]. That is, if `join_ip_multicast` gets called 3 times
/// and `leave_ip_multicast` gets called two times (after all 3
/// `join_ip_multicast` calls), `device_id` will still be in the multicast group
/// until the next (final) call to `leave_ip_multicast`.
///
/// `leave_ip_multicast` is different from [`leave_link_multicast`] as
/// `leave_ip_multicast` leaves an L3 multicast group, whereas
/// `leave_link_multicast` leaves an L2 multicast group.
///
/// # Panics
///
/// If `device_id` is not currently in the multicast group `multicast_addr`.
#[specialize_ip_address]
pub(super) fn leave_ip_multicast<C: EthernetIpDeviceContext, A: IpAddress>(
    ctx: &mut C,
    device_id: C::DeviceId,
    multicast_addr: MulticastAddr<A>,
) {
    #[ipv4addr]
    let res = <C as GmpHandler<_, Ipv4>>::gmp_leave_group(ctx, device_id, multicast_addr);

    #[ipv6addr]
    let res = <C as GmpHandler<_, Ipv6>>::gmp_leave_group(ctx, device_id, multicast_addr);

    match res {
        GroupLeaveResult::Left(()) => {
            let mac = MulticastAddr::from(&multicast_addr);

            trace!(
                "ethernet::leave_ip_multicast: leaving IP multicast {} and MAC multicast {}",
                multicast_addr,
                mac
            );

            leave_link_multicast(ctx, device_id, mac);
        }
        GroupLeaveResult::StillMember => trace!(
            "ethernet::leave_ip_multicast: not leaving IP multicast {} as there are still listeners for it",
            multicast_addr,
        ),
        GroupLeaveResult::NotMember => panic!(
            "attempted to leave IP multicast group we were not a member of: {}",
            multicast_addr,
        ),
    }
}

/// Is `device` in the IP multicast group `multicast_addr`?
#[specialize_ip_address]
pub(super) fn is_in_ip_multicast<C: EthernetIpDeviceContext, A: IpAddress>(
    ctx: &C,
    device_id: C::DeviceId,
    multicast_addr: MulticastAddr<A>,
) -> bool {
    #[ipv4addr]
    return ctx.get_state_with(device_id).ip().ipv4_multicast_groups.contains(&multicast_addr);

    #[ipv6addr]
    return ctx.get_state_with(device_id).ip().ipv6_multicast_groups.contains(&multicast_addr);
}

/// Get the MTU associated with this device.
pub(super) fn get_mtu<C: EthernetIpDeviceContext>(ctx: &C, device_id: C::DeviceId) -> u32 {
    ctx.get_state_with(device_id).link().mtu
}

/// Get the hop limit for new IPv6 packets that will be sent out from
/// `device_id`.
pub(super) fn get_ipv6_hop_limit<C: EthernetIpDeviceContext>(
    ctx: &C,
    device_id: C::DeviceId,
) -> NonZeroU8 {
    ctx.get_state_with(device_id).ip().ipv6_hop_limit
}

/// Is IP packet routing enabled on `device_id`?
///
/// Note, `true` does not necessarily mean that `device` is currently routing IP
/// packets. It only means that `device` is allowed to route packets. To route
/// packets, this netstack must be configured to allow IP packets to be routed
/// if it was not destined for this node.
pub(super) fn is_routing_enabled<C: EthernetIpDeviceContext, I: Ip>(
    ctx: &C,
    device_id: C::DeviceId,
) -> bool {
    let state = &ctx.get_state_with(device_id).ip();
    match I::VERSION {
        IpVersion::V4 => state.route_ipv4,
        IpVersion::V6 => state.route_ipv6,
    }
}

/// Sets the IP packet routing flag on `device_id`.
///
/// This method MUST NOT be called directly. It MUST only only called by
/// [`crate::device::set_routing_enabled`].
///
/// See [`crate::device::set_routing_enabled`] for more information.
pub(super) fn set_routing_enabled_inner<C: EthernetIpDeviceContext, I: Ip>(
    ctx: &mut C,
    device_id: C::DeviceId,
    enabled: bool,
) {
    let state = ctx.get_state_mut_with(device_id).ip_mut();
    match I::VERSION {
        IpVersion::V4 => state.route_ipv4 = enabled,
        IpVersion::V6 => state.route_ipv6 = enabled,
    }
}

/// Insert a static entry into this device's ARP table.
///
/// This will cause any conflicting dynamic entry to be removed, and
/// any future conflicting gratuitous ARPs to be ignored.
// TODO(rheacock): remove `cfg(test)` when this is used. Will probably be called
// by a pub fn in the device mod.
#[cfg(test)]
pub(super) fn insert_static_arp_table_entry<C: EthernetIpDeviceContext>(
    ctx: &mut C,
    device_id: C::DeviceId,
    addr: Ipv4Addr,
    mac: Mac,
) {
    arp::insert_static_neighbor(ctx, device_id, addr, mac)
}

/// Insert an entry into this device's NDP table.
///
/// This method only gets called when testing to force set a neighbor's link
/// address so that lookups succeed immediately, without doing address
/// resolution.
// TODO(rheacock): Remove when this is called from non-test code.
#[cfg(test)]
pub(super) fn insert_ndp_table_entry<C: EthernetIpDeviceContext>(
    ctx: &mut C,
    device_id: C::DeviceId,
    addr: UnicastAddr<Ipv6Addr>,
    mac: Mac,
) {
    <C as NdpHandler<_>>::insert_static_neighbor(ctx, device_id, addr, mac)
}

/// Deinitializes and cleans up state for ethernet devices
///
/// After this function is called, the ethernet device should not be used and
/// nothing else should be done with the state.
pub(super) fn deinitialize<C: EthernetIpDeviceContext>(ctx: &mut C, device_id: C::DeviceId) {
    arp::deinitialize(ctx, device_id);
    <C as NdpHandler<_>>::deinitialize(ctx, device_id);
}

impl<C: EthernetIpDeviceContext> StateContext<ArpState<EthernetLinkDevice, Ipv4Addr>, C::DeviceId>
    for C
{
    fn get_state_with(&self, id: C::DeviceId) -> &ArpState<EthernetLinkDevice, Ipv4Addr> {
        &self.get_state_with(id).link().ipv4_arp
    }

    fn get_state_mut_with(
        &mut self,
        id: C::DeviceId,
    ) -> &mut ArpState<EthernetLinkDevice, Ipv4Addr> {
        &mut self.get_state_mut_with(id).link_mut().ipv4_arp
    }
}

impl<
        B: BufferMut,
        C: EthernetIpDeviceContext
            + FrameContext<B, <C as DeviceIdContext<EthernetLinkDevice>>::DeviceId>,
    > FrameContext<B, ArpFrameMetadata<EthernetLinkDevice, C::DeviceId>> for C
{
    fn send_frame<S: Serializer<Buffer = B>>(
        &mut self,
        meta: ArpFrameMetadata<EthernetLinkDevice, C::DeviceId>,
        body: S,
    ) -> Result<(), S> {
        let src = self.get_state_with(meta.device_id).link().mac;
        self.send_frame(
            meta.device_id,
            body.encapsulate(EthernetFrameBuilder::new(src, meta.dst_addr, EtherType::Arp)),
        )
        .map_err(Nested::into_inner)
    }
}

impl<C: EthernetIpDeviceContext> ArpDeviceIdContext<EthernetLinkDevice> for C {
    type DeviceId = <C as DeviceIdContext<EthernetLinkDevice>>::DeviceId;
}

impl<C: EthernetIpDeviceContext> ArpContext<EthernetLinkDevice, Ipv4Addr> for C {
    fn get_protocol_addr(
        &self,
        device_id: <C as ArpDeviceIdContext<EthernetLinkDevice>>::DeviceId,
    ) -> Option<Ipv4Addr> {
        get_ip_addr_subnet::<_, Ipv4Addr>(self, device_id.into()).map(|a| a.addr().get())
    }
    fn get_hardware_addr(
        &self,
        device_id: <C as ArpDeviceIdContext<EthernetLinkDevice>>::DeviceId,
    ) -> Mac {
        self.get_state_with(device_id.into()).link().mac
    }
    fn address_resolved(
        &mut self,
        device_id: <C as ArpDeviceIdContext<EthernetLinkDevice>>::DeviceId,
        proto_addr: Ipv4Addr,
        hw_addr: Mac,
    ) {
        mac_resolved(self, device_id.into(), IpAddr::V4(proto_addr), hw_addr);
    }
    fn address_resolution_failed(
        &mut self,
        device_id: <C as ArpDeviceIdContext<EthernetLinkDevice>>::DeviceId,
        proto_addr: Ipv4Addr,
    ) {
        mac_resolution_failed(self, device_id.into(), IpAddr::V4(proto_addr));
    }
    fn address_resolution_expired(
        &mut self,
        _device_id: <C as ArpDeviceIdContext<EthernetLinkDevice>>::DeviceId,
        _proto_addr: Ipv4Addr,
    ) {
        log_unimplemented!((), "ArpContext::address_resolution_expired");
    }
}

impl<C: EthernetIpDeviceContext> StateContext<NdpState<EthernetLinkDevice, C::Instant>, C::DeviceId>
    for C
{
    fn get_state_with(&self, id: C::DeviceId) -> &NdpState<EthernetLinkDevice, C::Instant> {
        &self.get_state_with(id).link().ndp
    }

    fn get_state_mut_with(
        &mut self,
        id: C::DeviceId,
    ) -> &mut NdpState<EthernetLinkDevice, C::Instant> {
        &mut self.get_state_mut_with(id).link_mut().ndp
    }
}

impl<C: EthernetIpDeviceContext> NdpContext<EthernetLinkDevice> for C {
    fn get_link_layer_addr(&self, device_id: C::DeviceId) -> Mac {
        self.get_state_with(device_id).link().mac
    }

    fn get_interface_identifier(&self, device_id: C::DeviceId) -> [u8; 8] {
        self.get_state_with(device_id).link().mac.to_eui64()
    }

    fn get_link_local_addr(
        &self,
        device_id: C::DeviceId,
    ) -> Option<Tentative<LinkLocalUnicastAddr<Ipv6Addr>>> {
        self.get_state_with(device_id).ip().ipv6_addr_sub.iter().find_map(|a| {
            let addr = LinkLocalUnicastAddr::new(a.addr_sub().addr())?;
            Some(if a.state().is_tentative() {
                Tentative::new_tentative(addr)
            } else {
                Tentative::new_permanent(addr)
            })
        })
    }

    fn get_ipv6_addr(&self, device_id: C::DeviceId) -> Option<Ipv6Addr> {
        // Return a non tentative global address, or the link-local address if no non-tentative
        // global addresses are associated with `device_id`.

        match get_ip_addr_subnet::<_, Ipv6Addr>(self, device_id) {
            Some(addr_sub) => Some(addr_sub.addr().get()),
            None => Self::get_link_local_addr(self, device_id)
                .map(|a| a.try_into_permanent())
                .map(|a| a.map(Witness::into_addr))
                .unwrap_or(None),
        }
    }

    type AddrEntriesIter = IntoIter<AddressEntry<Ipv6Addr, C::Instant, UnicastAddr<Ipv6Addr>>>;

    fn get_ipv6_addr_entries(
        &self,
        device_id: C::DeviceId,
    ) -> IntoIter<AddressEntry<Ipv6Addr, C::Instant, UnicastAddr<Ipv6Addr>>> {
        // TODO(joshlf): The fact that we clone the entire list of entries here
        // is just so that we can avoid writing out a large, ugly function
        // signature for `get_ipv6_addr_entries`. We would like to have the
        // return value be `impl Iterator`, but impl trait is not yet supported
        // on trait methods. Instead, `NdpContext` has the associated
        // `AddrEntriesIter` type. However, due to lifetime issues, this
        // precludes us from returning an iterator whose lifetime returns on the
        // lifetime of `self` passed to this method, which in turn means that
        // our only option is to return entries by value, which requires
        // cloning. Since this isn't in the hot path, we accept the cost of
        // cloning the vector, but it would be great if we could solve this in a
        // better way.
        let mut addrs = self.get_state_with(device_id).ip().ipv6_addr_sub.clone();
        addrs.retain(|a| {
            let addr: UnicastAddr<Ipv6Addr> = a.addr_sub().addr();
            !addr.is_linklocal()
        });
        addrs.into_iter()
    }

    fn ipv6_addr_state(
        &self,
        device_id: C::DeviceId,
        address: &UnicastAddr<Ipv6Addr>,
    ) -> Option<AddressState> {
        let address = SpecifiedAddr::new(address.get())?;

        get_ip_addr_state::<_, Ipv6Addr>(self, device_id, &address)
    }

    fn send_ipv6_frame<S: Serializer<Buffer = EmptyBuf>>(
        &mut self,
        device_id: Self::DeviceId,
        next_hop: SpecifiedAddr<Ipv6Addr>,
        body: S,
    ) -> Result<(), S> {
        // `device_id` must not be uninitialized.
        assert!(self.is_device_usable(device_id));

        // TODO(joshlf): Wire `SpecifiedAddr` through the `ndp` module.
        send_ip_frame(self, device_id, next_hop, body)
    }

    fn address_resolved(
        &mut self,
        device_id: C::DeviceId,
        address: &UnicastAddr<Ipv6Addr>,
        link_address: Mac,
    ) {
        mac_resolved(self, device_id, IpAddr::V6(address.get()), link_address);
    }

    fn address_resolution_failed(
        &mut self,
        device_id: C::DeviceId,
        address: &UnicastAddr<Ipv6Addr>,
    ) {
        mac_resolution_failed(self, device_id, IpAddr::V6(address.get()));
    }

    fn duplicate_address_detected(&mut self, device_id: C::DeviceId, addr: UnicastAddr<Ipv6Addr>) {
        let state = self.get_state_mut_with(device_id).ip_mut();

        let original_size = state.ipv6_addr_sub.len();
        state.ipv6_addr_sub.retain(|x| x.addr_sub().addr() != addr);
        assert_eq!(
            state.ipv6_addr_sub.len(),
            original_size - 1,
            "duplicate address detected, but not in our list of addresses"
        );

        // Leave the the solicited-node multicast group.
        leave_ip_multicast(self, device_id, addr.to_solicited_node_address());

        // TODO: we need to pick a different address depending on what flow we
        // are using.
    }

    fn unique_address_determined(&mut self, device_id: C::DeviceId, addr: UnicastAddr<Ipv6Addr>) {
        trace!(
            "ethernet::unique_address_determined: device_id = {:?}; addr = {:?}",
            device_id,
            addr
        );

        let state = self.get_state_mut_with(device_id).ip_mut();

        if let Some(entry) = state.ipv6_addr_sub.iter_mut().find(|a| a.addr_sub().addr() == addr) {
            entry.mark_permanent();
        } else {
            panic!("Attempted to resolve an unknown tentative address");
        }
    }

    fn set_mtu(&mut self, device_id: C::DeviceId, mut mtu: u32) {
        // TODO(ghanan): Should this new MTU be updated only from the netstack's
        //               perspective or be exposed to the device hardware?

        // `mtu` must not be less than the minimum IPv6 MTU.
        assert!(mtu >= Ipv6::MINIMUM_LINK_MTU.into());

        let dev_state = self.get_state_mut_with(device_id).link_mut();

        // If `mtu` is greater than what the device supports, set `mtu` to the
        // maximum MTU the device supports.
        if mtu > dev_state.hw_mtu {
            trace!("ethernet::ndp_device::set_mtu: MTU of {:?} is greater than the device {:?}'s max MTU of {:?}, using device's max MTU instead", mtu, device_id, dev_state.hw_mtu);
            mtu = dev_state.hw_mtu;
        }

        trace!("ethernet::ndp_device::set_mtu: setting link MTU to {:?}", mtu);
        dev_state.mtu = mtu;
    }

    fn set_hop_limit(&mut self, device_id: Self::DeviceId, hop_limit: NonZeroU8) {
        self.get_state_mut_with(device_id).ip_mut().ipv6_hop_limit = hop_limit;
    }

    fn add_slaac_addr_sub(
        &mut self,
        device_id: Self::DeviceId,
        addr_sub: AddrSubnet<Ipv6Addr, UnicastAddr<Ipv6Addr>>,
        valid_until: Self::Instant,
    ) -> Result<(), AddressError> {
        trace!(
            "ethernet::add_slaac_addr_sub: adding address {:?} on device {:?}",
            addr_sub,
            device_id
        );

        add_ip_addr_subnet_inner(
            self,
            device_id,
            addr_sub.into_witness(),
            AddrConfigType::Slaac,
            Some(valid_until),
        )
    }

    fn deprecate_slaac_addr(&mut self, device_id: Self::DeviceId, addr: &UnicastAddr<Ipv6Addr>) {
        trace!(
            "ethernet::deprecate_slaac_addr: deprecating address {:?} on device {:?}",
            addr,
            device_id
        );

        let state = self.get_state_mut_with(device_id).ip_mut();

        if let Some(entry) = state
            .ipv6_addr_sub
            .iter_mut()
            .find(|a| (a.addr_sub().addr() == *addr) && a.config_type() == AddrConfigType::Slaac)
        {
            match entry.state {
                AddressState::Assigned => {
                    entry.state = AddressState::Deprecated;
                }
                AddressState::Tentative => {
                    trace!("ethernet::deprecate_slaac_addr: invalidating the deprecated tentative address {:?} on device {:?}", addr, device_id);
                    // If `addr` is currently tentative on `device_id`, the
                    // address should simply be invalidated as new connections
                    // should not use a deprecated address, and we should have
                    // no existing connections using a tentative address.

                    // We must have had an invalidation timeout if we just
                    // attempted to deprecate.
                    assert!(self
                        .cancel_timer(
                            ndp::NdpTimerId::new_invalidate_slaac_address(device_id, *addr).into()
                        )
                        .is_some());

                    Self::invalidate_slaac_addr(self, device_id, addr);
                }
                AddressState::Deprecated => unreachable!(
                    "We should never attempt to deprecate an already deprecated address"
                ),
            }
        } else {
            panic!("Address is not configured via SLAAC on this device");
        }
    }

    fn invalidate_slaac_addr(&mut self, device_id: Self::DeviceId, addr: &UnicastAddr<Ipv6Addr>) {
        trace!(
            "ethernet::invalidate_slaac_addr: invalidating address {:?} on device {:?}",
            addr,
            device_id
        );

        // `unwrap` will panic if `addr` is not an address configured via SLAAC
        // on `device_id`.
        del_ip_addr_inner(self, device_id, &addr.get(), Some(AddrConfigType::Slaac)).unwrap();
    }

    fn update_slaac_addr_valid_until(
        &mut self,
        device_id: Self::DeviceId,
        addr: &UnicastAddr<Ipv6Addr>,
        valid_until: Self::Instant,
    ) {
        trace!(
            "ethernet::update_slaac_addr_valid_until: updating address {:?}'s valid until instant to {:?} on device {:?}",
            addr,
            valid_until,
            device_id
        );

        let state = self.get_state_mut_with(device_id).ip_mut();

        if let Some(entry) = state
            .ipv6_addr_sub
            .iter_mut()
            .find(|a| (a.addr_sub().addr() == *addr) && a.config_type() == AddrConfigType::Slaac)
        {
            entry.valid_until = Some(valid_until);
        } else {
            panic!("Address is not configured via SLAAC on this device");
        }
    }

    fn is_router(&self, device_id: Self::DeviceId) -> bool {
        self.is_router_device::<Ipv6>(device_id)
    }
}

/// An implementation of the [`LinkDevice`] trait for Ethernet devices.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct EthernetLinkDevice;

impl LinkDevice for EthernetLinkDevice {
    type Address = Mac;
}

/// Sends out any pending frames that are waiting for link layer address
/// resolution.
///
/// `mac_resolved` is the common logic used when a link layer address is
/// resolved either by ARP or NDP.
fn mac_resolved<C: EthernetIpDeviceContext>(
    ctx: &mut C,
    device_id: C::DeviceId,
    address: IpAddr,
    dst_mac: Mac,
) {
    let state = ctx.get_state_mut_with(device_id).link_mut();
    let src_mac = state.mac;
    let ether_type = match &address {
        IpAddr::V4(_) => EtherType::Ipv4,
        IpAddr::V6(_) => EtherType::Ipv6,
    };
    if let Some(pending) = state.take_pending_frames(address) {
        for frame in pending {
            // NOTE(brunodalbo): We already performed MTU checking when we saved
            //  the buffer waiting for address resolution. It should be noted
            //  that the MTU check back then didn't account for ethernet frame
            //  padding required by EthernetFrameBuilder, but that's fine (as it
            //  stands right now) because the MTU is guaranteed to be larger
            //  than an Ethernet minimum frame body size.
            let res = ctx.send_frame(
                device_id.into(),
                frame.encapsulate(EthernetFrameBuilder::new(src_mac, dst_mac, ether_type)),
            );
            if let Err(_) = res {
                // TODO(joshlf): Do we want to handle this differently?
                debug!("Failed to send pending frame; MTU changed since frame was queued");
            }
        }
    }
}

/// Clears out any pending frames that are waiting for link layer address
/// resolution.
///
/// `mac_resolution_failed` is the common logic used when a link layer address
/// fails to resolve either by ARP or NDP.
fn mac_resolution_failed<C: EthernetIpDeviceContext>(
    ctx: &mut C,
    device_id: C::DeviceId,
    address: IpAddr,
) {
    // TODO(brunodalbo) what do we do here in regards to the pending frames?
    //  NDP's RFC explicitly states unreachable ICMP messages must be generated:
    //  "If no Neighbor Advertisement is received after MAX_MULTICAST_SOLICIT
    //  solicitations, address resolution has failed. The sender MUST return
    //  ICMP destination unreachable indications with code 3
    //  (Address Unreachable) for each packet queued awaiting address
    //  resolution."
    //  For ARP, we don't have such a clear statement on the RFC, it would make
    //  sense to do the same thing though.
    let state = ctx.get_state_mut_with(device_id).link_mut();
    if let Some(_) = state.take_pending_frames(address) {
        log_unimplemented!((), "ethernet mac resolution failed not implemented");
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use matches::assert_matches;
    use packet::Buf;
    use packet_formats::icmp::{IcmpDestUnreachable, IcmpIpExt};
    use packet_formats::ip::{IpExt, IpPacketBuilder, IpProto};
    use packet_formats::testdata::{dns_request_v4, dns_request_v6};
    use packet_formats::testutil::{
        parse_icmp_packet_in_ip_packet_in_ethernet_frame, parse_ip_packet_in_ethernet_frame,
    };
    use rand::Rng;
    use rand_xorshift::XorShiftRng;
    use specialize_ip_macro::{ip_test, specialize_ip};

    use super::*;
    use crate::context::testutil::DummyInstant;
    use crate::device::{
        arp::ArpHandler, is_routing_enabled, set_routing_enabled, DeviceId, EthernetDeviceId,
        IpLinkDeviceState,
    };
    use crate::ip::{
        dispatch_receive_ip_packet_name, receive_ip_packet, DummyDeviceId, IpDeviceIdContext,
    };
    use crate::testutil::{
        add_arp_or_ndp_table_entry, get_counter_val, new_rng, DummyEventDispatcher,
        DummyEventDispatcherBuilder, FakeCryptoRng, TestIpExt, DUMMY_CONFIG_V4,
    };
    use crate::{Ipv4StateBuilder, Ipv6StateBuilder, StackStateBuilder};

    struct DummyEthernetCtx {
        state: IpLinkDeviceState<DummyInstant, EthernetDeviceState<DummyInstant>>,
    }

    impl DummyEthernetCtx {
        fn new(mac: Mac, mtu: u32) -> DummyEthernetCtx {
            DummyEthernetCtx {
                state: IpLinkDeviceState::new(EthernetDeviceStateBuilder::new(mac, mtu).build()),
            }
        }
    }

    type DummyCtx = crate::context::testutil::DummyCtx<
        DummyEthernetCtx,
        EthernetTimerId<DummyDeviceId>,
        DummyDeviceId,
    >;

    impl
        DualStateContext<
            IpLinkDeviceState<DummyInstant, EthernetDeviceState<DummyInstant>>,
            FakeCryptoRng<XorShiftRng>,
            DummyDeviceId,
        > for DummyCtx
    {
        fn get_states_with(
            &self,
            _id0: DummyDeviceId,
            _id1: (),
        ) -> (
            &IpLinkDeviceState<DummyInstant, EthernetDeviceState<DummyInstant>>,
            &FakeCryptoRng<XorShiftRng>,
        ) {
            let (state, rng) = self.get_states_with((), ());
            (&state.state, rng)
        }

        fn get_states_mut_with(
            &mut self,
            _id0: DummyDeviceId,
            _id1: (),
        ) -> (
            &mut IpLinkDeviceState<DummyInstant, EthernetDeviceState<DummyInstant>>,
            &mut FakeCryptoRng<XorShiftRng>,
        ) {
            let (state, rng) = self.get_states_mut_with((), ());
            (&mut state.state, rng)
        }
    }

    impl DeviceIdContext<EthernetLinkDevice> for DummyCtx {
        type DeviceId = DummyDeviceId;
    }

    impl IpDeviceIdContext for DummyCtx {
        type DeviceId = DummyDeviceId;
    }

    impl
        IpDeviceContext<
            EthernetLinkDevice,
            EthernetTimerId<DummyDeviceId>,
            EthernetDeviceState<DummyInstant>,
        > for DummyCtx
    {
        fn is_router_device<I: Ip>(&self, _device: DummyDeviceId) -> bool {
            unimplemented!()
        }

        fn is_device_usable(&self, _device: DummyDeviceId) -> bool {
            unimplemented!()
        }
    }

    #[test]
    fn test_mtu() {
        // Test that we send an Ethernet frame whose size is less than the MTU,
        // and that we don't send an Ethernet frame whose size is greater than
        // the MTU.
        fn test(size: usize, expect_frames_sent: usize) {
            let mut ctx = DummyCtx::with_state(DummyEthernetCtx::new(
                DUMMY_CONFIG_V4.local_mac,
                Ipv6::MINIMUM_LINK_MTU.into(),
            ));
            <DummyCtx as ArpHandler<_, _>>::insert_static_neighbor(
                &mut ctx,
                DummyDeviceId,
                DUMMY_CONFIG_V4.remote_ip.get(),
                DUMMY_CONFIG_V4.remote_mac,
            );
            let _ = send_ip_frame(
                &mut ctx,
                DummyDeviceId,
                DUMMY_CONFIG_V4.remote_ip,
                Buf::new(&mut vec![0; size], ..),
            );
            assert_eq!(ctx.frames().len(), expect_frames_sent);
        }

        test(Ipv6::MINIMUM_LINK_MTU.into(), 1);
        test(usize::from(Ipv6::MINIMUM_LINK_MTU) + 1, 0);
    }

    #[test]
    fn test_pending_frames() {
        let mut state = EthernetDeviceStateBuilder::new(
            DUMMY_CONFIG_V4.local_mac,
            Ipv6::MINIMUM_LINK_MTU.into(),
        )
        .build::<DummyInstant>();
        let ip = IpAddr::V4(DUMMY_CONFIG_V4.local_ip.get());
        assert_matches!(state.add_pending_frame(ip, Buf::new(vec![1], ..)), None);
        assert_matches!(state.add_pending_frame(ip, Buf::new(vec![2], ..)), None);
        assert_matches!(state.add_pending_frame(ip, Buf::new(vec![3], ..)), None);

        // Check that we're accumulating correctly...
        assert_eq!(3, state.take_pending_frames(ip).unwrap().count());
        // ...and that take_pending_frames clears all the buffered data.
        assert!(state.take_pending_frames(ip).is_none());

        for i in 0..ETHERNET_MAX_PENDING_FRAMES {
            assert!(state.add_pending_frame(ip, Buf::new(vec![i as u8], ..)).is_none());
        }
        // Check that adding more than capacity will drop the older buffers as
        // a proper FIFO queue.
        assert_eq!(0, state.add_pending_frame(ip, Buf::new(vec![255], ..)).unwrap().as_ref()[0]);
        assert_eq!(1, state.add_pending_frame(ip, Buf::new(vec![255], ..)).unwrap().as_ref()[0]);
        assert_eq!(2, state.add_pending_frame(ip, Buf::new(vec![255], ..)).unwrap().as_ref()[0]);
    }

    #[specialize_ip]
    fn test_receive_ip_frame<I: Ip>(initialize: bool) {
        // Should only receive a frame if the device is initialized

        let config = I::DUMMY_CONFIG;
        let mut ctx = DummyEventDispatcherBuilder::default().build::<DummyEventDispatcher>();
        let device =
            ctx.state_mut().add_ethernet_device(config.local_mac, Ipv6::MINIMUM_LINK_MTU.into());

        #[ipv4]
        let mut bytes = dns_request_v4::ETHERNET_FRAME.bytes.to_vec();

        #[ipv6]
        let mut bytes = dns_request_v6::ETHERNET_FRAME.bytes.to_vec();

        let mac_bytes = config.local_mac.bytes();
        bytes[0..6].copy_from_slice(&mac_bytes);

        if initialize {
            crate::device::initialize_device(&mut ctx, device);
        }

        // Will panic if we do not initialize.
        crate::device::receive_frame(&mut ctx, device, Buf::new(bytes, ..));

        // If we did not initialize, we would not reach here since
        // `receive_frame` would have panicked.
        #[ipv4]
        assert_eq!(get_counter_val(&mut ctx, "receive_ipv4_packet"), 1);
        #[ipv6]
        assert_eq!(get_counter_val(&mut ctx, "receive_ipv6_packet"), 1);
    }

    #[ip_test]
    #[should_panic(expected = "assertion failed: is_device_initialized(ctx.state(), device)")]
    fn receive_frame_uninitialized<I: Ip>() {
        test_receive_ip_frame::<I>(false);
    }

    #[ip_test]
    fn receive_frame_initialized<I: Ip>() {
        test_receive_ip_frame::<I>(true);
    }

    #[specialize_ip]
    fn test_send_ip_frame<I: Ip>(initialize: bool) {
        // Should only send a frame if the device is initialized

        let config = I::DUMMY_CONFIG;
        let mut ctx = DummyEventDispatcherBuilder::default().build::<DummyEventDispatcher>();
        let device =
            ctx.state_mut().add_ethernet_device(config.local_mac, Ipv6::MINIMUM_LINK_MTU.into());

        #[ipv4]
        let mut bytes = dns_request_v4::ETHERNET_FRAME.bytes.to_vec();

        #[ipv6]
        let mut bytes = dns_request_v6::ETHERNET_FRAME.bytes.to_vec();

        let mac_bytes = config.local_mac.bytes();
        bytes[6..12].copy_from_slice(&mac_bytes);

        if initialize {
            crate::device::initialize_device(&mut ctx, device);
        }

        // Will panic if we do not initialize.
        let _ =
            crate::device::send_ip_frame(&mut ctx, device, config.remote_ip, Buf::new(bytes, ..));
    }

    #[ip_test]
    #[should_panic(expected = "assertion failed: is_device_usable(ctx.state(), device)")]
    fn test_send_frame_uninitialized<I: Ip>() {
        test_send_ip_frame::<I>(false);
    }

    #[ip_test]
    fn test_send_frame_initialized<I: Ip>() {
        test_send_ip_frame::<I>(true);
    }

    #[test]
    fn initialize_once() {
        let mut ctx = DummyEventDispatcherBuilder::default().build::<DummyEventDispatcher>();
        let device = ctx
            .state_mut()
            .add_ethernet_device(DUMMY_CONFIG_V4.local_mac, Ipv6::MINIMUM_LINK_MTU.into());
        crate::device::initialize_device(&mut ctx, device);
    }

    #[test]
    #[should_panic(expected = "assertion failed: state.is_uninitialized()")]
    fn initialize_multiple() {
        let mut ctx = DummyEventDispatcherBuilder::default().build::<DummyEventDispatcher>();
        let device = ctx
            .state_mut()
            .add_ethernet_device(DUMMY_CONFIG_V4.local_mac, Ipv6::MINIMUM_LINK_MTU.into());
        crate::device::initialize_device(&mut ctx, device);

        // Should panic since we are already initialized.
        crate::device::initialize_device(&mut ctx, device);
    }

    #[ip_test]
    fn test_set_ip_routing<I: Ip + TestIpExt + IcmpIpExt + IpExt>() {
        fn check_other_is_routing_enabled<I: Ip>(
            ctx: &Ctx<DummyEventDispatcher>,
            device: DeviceId,
            expected: bool,
        ) {
            let enabled = match I::VERSION {
                IpVersion::V4 => is_routing_enabled::<_, Ipv6>(ctx, device),
                IpVersion::V6 => is_routing_enabled::<_, Ipv4>(ctx, device),
            };

            assert_eq!(enabled, expected);
        }

        fn check_icmp<I: Ip>(buf: &[u8]) {
            match I::VERSION {
                IpVersion::V4 => {
                    let _ = parse_icmp_packet_in_ip_packet_in_ethernet_frame::<
                        Ipv4,
                        _,
                        IcmpDestUnreachable,
                        _,
                    >(buf, |_| {})
                    .unwrap();
                }
                IpVersion::V6 => {
                    let _ = parse_icmp_packet_in_ip_packet_in_ethernet_frame::<
                        Ipv6,
                        _,
                        IcmpDestUnreachable,
                        _,
                    >(buf, |_| {})
                    .unwrap();
                }
            }
        }

        let src_ip = I::get_other_ip_address(3);
        let src_mac = Mac::new([10, 11, 12, 13, 14, 15]);
        let config = I::DUMMY_CONFIG;
        let device = DeviceId::new_ethernet(0);
        let frame_dst = FrameDestination::Unicast;
        let mut rng = new_rng(70812476915813);
        let mut body: Vec<u8> = core::iter::repeat_with(|| rng.gen()).take(100).collect();
        let buf = Buf::new(&mut body[..], ..)
            .encapsulate(I::PacketBuilder::new(
                src_ip.get(),
                config.remote_ip.get(),
                64,
                IpProto::Tcp.into(),
            ))
            .serialize_vec_outer()
            .ok()
            .unwrap()
            .unwrap_b();

        // Test with netstack no forwarding

        let mut builder = DummyEventDispatcherBuilder::from_config(config.clone());
        add_arp_or_ndp_table_entry(&mut builder, device.id(), src_ip.get(), src_mac);
        let mut ctx = builder.build();

        // Should not be a router (default).
        assert!(!is_routing_enabled::<_, I>(&ctx, device));
        check_other_is_routing_enabled::<I>(&ctx, device, false);

        // Receiving a packet not destined for the node should only result in a
        // dest unreachable message if routing is enabled.
        receive_ip_packet::<_, _, I>(&mut ctx, device, frame_dst, buf.clone());
        assert_eq!(ctx.dispatcher().frames_sent().len(), 0);

        // Attempting to set router should work, but it still won't be able to
        // route packets.
        set_routing_enabled::<_, I>(&mut ctx, device, true);
        assert!(is_routing_enabled::<_, I>(&ctx, device));
        // Should not update other Ip routing status.
        check_other_is_routing_enabled::<I>(&ctx, device, false);
        receive_ip_packet::<_, _, I>(&mut ctx, device, frame_dst, buf.clone());
        // Still should not send ICMP because device has routing disabled.
        assert_eq!(ctx.dispatcher().frames_sent().len(), 0);

        // Test with netstack forwarding

        let mut state_builder = StackStateBuilder::default();
        let _: &mut Ipv4StateBuilder = state_builder.ipv4_builder().forward(true);
        let _: &mut Ipv6StateBuilder = state_builder.ipv6_builder().forward(true);
        // Most tests do not need NDP's DAD or router solicitation so disable it
        // here.
        let mut ndp_configs = ndp::NdpConfigurations::default();
        ndp_configs.set_dup_addr_detect_transmits(None);
        ndp_configs.set_max_router_solicitations(None);
        state_builder.device_builder().set_default_ndp_configs(ndp_configs);
        let mut builder = DummyEventDispatcherBuilder::from_config(config.clone());
        add_arp_or_ndp_table_entry(&mut builder, device.id(), src_ip.get(), src_mac);
        let mut ctx = builder.build_with(state_builder, DummyEventDispatcher::default());

        // Should not be a router (default).
        assert!(!is_routing_enabled::<_, I>(&ctx, device));
        check_other_is_routing_enabled::<I>(&ctx, device, false);

        // Receiving a packet not destined for the node should not result in an
        // unreachable message when routing is disabled.
        receive_ip_packet::<_, _, I>(&mut ctx, device, frame_dst, buf.clone());
        assert_eq!(ctx.dispatcher().frames_sent().len(), 0);

        // Attempting to set router should work
        set_routing_enabled::<_, I>(&mut ctx, device, true);
        assert!(is_routing_enabled::<_, I>(&ctx, device));
        // Should not update other Ip routing status.
        check_other_is_routing_enabled::<I>(&ctx, device, false);

        // Should route the packet since routing fully enabled (netstack &
        // device).
        receive_ip_packet::<_, _, I>(&mut ctx, device, frame_dst, buf.clone());
        assert_eq!(ctx.dispatcher().frames_sent().len(), 1);
        let (packet_buf, _, _, packet_src_ip, packet_dst_ip, proto, ttl) =
            parse_ip_packet_in_ethernet_frame::<I>(&ctx.dispatcher().frames_sent()[0].1[..])
                .unwrap();
        assert_eq!(src_ip.get(), packet_src_ip);
        assert_eq!(config.remote_ip.get(), packet_dst_ip);
        assert_eq!(proto, IpProto::Tcp.into());
        assert_eq!(body, packet_buf);
        assert_eq!(ttl, 63);

        // Test routing a packet to an unknown address.
        let buf_unknown_dest = Buf::new(&mut body[..], ..)
            .encapsulate(I::PacketBuilder::new(
                src_ip.get(),
                // Addr must be remote, otherwise this will cause an NDP/ARP
                // request rather than ICMP unreachable.
                I::get_other_remote_ip_address(10).get(),
                64,
                IpProto::Tcp.into(),
            ))
            .serialize_vec_outer()
            .ok()
            .unwrap()
            .unwrap_b();
        receive_ip_packet::<_, _, I>(&mut ctx, device, frame_dst, buf_unknown_dest);
        assert_eq!(ctx.dispatcher().frames_sent().len(), 2);
        check_icmp::<I>(&ctx.dispatcher().frames_sent()[1].1);

        // Attempt to unset router
        set_routing_enabled::<_, I>(&mut ctx, device, false);
        assert!(!is_routing_enabled::<_, I>(&ctx, device));
        check_other_is_routing_enabled::<I>(&ctx, device, false);

        // Should not route packets anymore
        receive_ip_packet::<_, _, I>(&mut ctx, device, frame_dst, buf.clone());
        assert_eq!(ctx.dispatcher().frames_sent().len(), 2);
    }

    #[ip_test]
    fn test_promiscuous_mode<I: Ip + TestIpExt + IpExt>() {
        // Test that frames not destined for a device will still be accepted
        // when the device is put into promiscuous mode. In all cases, frames
        // that are destined for a device must always be accepted.

        let config = I::DUMMY_CONFIG;
        let mut ctx = DummyEventDispatcherBuilder::from_config(config.clone())
            .build::<DummyEventDispatcher>();
        let device = DeviceId::new_ethernet(0);
        let other_mac = Mac::new([13, 14, 15, 16, 17, 18]);

        let buf = Buf::new(Vec::new(), ..)
            .encapsulate(I::PacketBuilder::new(
                config.remote_ip.get(),
                config.local_ip.get(),
                64,
                IpProto::Tcp.into(),
            ))
            .encapsulate(EthernetFrameBuilder::new(
                config.remote_mac,
                config.local_mac,
                I::ETHER_TYPE,
            ))
            .serialize_vec_outer()
            .ok()
            .unwrap()
            .unwrap_b();

        // Accept packet destined for this device if promiscuous mode is off.
        crate::device::set_promiscuous_mode(&mut ctx, device, false);
        crate::device::receive_frame(&mut ctx, device, buf.clone());
        assert_eq!(get_counter_val(&mut ctx, dispatch_receive_ip_packet_name::<I>()), 1);

        // Accept packet destined for this device if promiscuous mode is on.
        crate::device::set_promiscuous_mode(&mut ctx, device, true);
        crate::device::receive_frame(&mut ctx, device, buf.clone());
        assert_eq!(get_counter_val(&mut ctx, dispatch_receive_ip_packet_name::<I>()), 2);

        let buf = Buf::new(Vec::new(), ..)
            .encapsulate(I::PacketBuilder::new(
                config.remote_ip.get(),
                config.local_ip.get(),
                64,
                IpProto::Tcp.into(),
            ))
            .encapsulate(EthernetFrameBuilder::new(config.remote_mac, other_mac, I::ETHER_TYPE))
            .serialize_vec_outer()
            .ok()
            .unwrap()
            .unwrap_b();

        // Reject packet not destined for this device if promiscuous mode is
        // off.
        crate::device::set_promiscuous_mode(&mut ctx, device, false);
        crate::device::receive_frame(&mut ctx, device, buf.clone());
        assert_eq!(get_counter_val(&mut ctx, dispatch_receive_ip_packet_name::<I>()), 2);

        // Accept packet not destined for this device if promiscuous mode is on.
        crate::device::set_promiscuous_mode(&mut ctx, device, true);
        crate::device::receive_frame(&mut ctx, device, buf.clone());
        assert_eq!(get_counter_val(&mut ctx, dispatch_receive_ip_packet_name::<I>()), 3);
    }

    #[ip_test]
    fn test_add_remove_ip_addresses<I: Ip + TestIpExt>() {
        let config = I::DUMMY_CONFIG;
        let mut ctx = DummyEventDispatcherBuilder::default().build::<DummyEventDispatcher>();
        let device =
            ctx.state_mut().add_ethernet_device(config.local_mac, Ipv6::MINIMUM_LINK_MTU.into());
        crate::device::initialize_device(&mut ctx, device);

        let ip1 = I::get_other_ip_address(1);
        let ip2 = I::get_other_ip_address(2);
        let ip3 = I::get_other_ip_address(3);

        let prefix = I::Addr::BYTES * 8;
        let as1 = AddrSubnet::new(ip1.get(), prefix).unwrap();
        let as2 = AddrSubnet::new(ip2.get(), prefix).unwrap();

        assert!(crate::device::get_ip_addr_state(&ctx, device, &ip1).is_none());
        assert!(crate::device::get_ip_addr_state(&ctx, device, &ip2).is_none());
        assert!(crate::device::get_ip_addr_state(&ctx, device, &ip3).is_none());

        // Add ip1 (ok)
        crate::device::add_ip_addr_subnet(&mut ctx, device, as1).unwrap();
        assert!(crate::device::get_ip_addr_state(&ctx, device, &ip1).is_some());
        assert!(crate::device::get_ip_addr_state(&ctx, device, &ip2).is_none());
        assert!(crate::device::get_ip_addr_state(&ctx, device, &ip3).is_none());

        // Add ip2 (ok)
        crate::device::add_ip_addr_subnet(&mut ctx, device, as2).unwrap();
        assert!(crate::device::get_ip_addr_state(&ctx, device, &ip1).is_some());
        assert!(crate::device::get_ip_addr_state(&ctx, device, &ip2).is_some());
        assert!(crate::device::get_ip_addr_state(&ctx, device, &ip3).is_none());

        // Del ip1 (ok)
        crate::device::del_ip_addr(&mut ctx, device, &ip1).unwrap();
        assert!(crate::device::get_ip_addr_state(&ctx, device, &ip1).is_none());
        assert!(crate::device::get_ip_addr_state(&ctx, device, &ip2).is_some());
        assert!(crate::device::get_ip_addr_state(&ctx, device, &ip3).is_none());

        // Del ip1 again (ip1 not found)
        assert_eq!(
            crate::device::del_ip_addr(&mut ctx, device, &ip1).unwrap_err(),
            AddressError::NotFound
        );
        assert!(crate::device::get_ip_addr_state(&ctx, device, &ip1).is_none());
        assert!(crate::device::get_ip_addr_state(&ctx, device, &ip2).is_some());
        assert!(crate::device::get_ip_addr_state(&ctx, device, &ip3).is_none());

        // Add ip2 again (ip2 already exists)
        assert_eq!(
            crate::device::add_ip_addr_subnet(&mut ctx, device, as2).unwrap_err(),
            AddressError::AlreadyExists
        );
        assert!(crate::device::get_ip_addr_state(&ctx, device, &ip1).is_none());
        assert!(crate::device::get_ip_addr_state(&ctx, device, &ip2).is_some());
        assert!(crate::device::get_ip_addr_state(&ctx, device, &ip3).is_none());

        // Add ip2 with different subnet (ip2 already exists)
        assert_eq!(
            crate::device::add_ip_addr_subnet(
                &mut ctx,
                device,
                AddrSubnet::new(ip2.get(), prefix - 1).unwrap()
            )
            .unwrap_err(),
            AddressError::AlreadyExists
        );
        assert!(crate::device::get_ip_addr_state(&ctx, device, &ip1).is_none());
        assert!(crate::device::get_ip_addr_state(&ctx, device, &ip2).is_some());
        assert!(crate::device::get_ip_addr_state(&ctx, device, &ip3).is_none());
    }

    fn receive_simple_ip_packet_test<A: IpAddress>(
        ctx: &mut Ctx<DummyEventDispatcher>,
        device: DeviceId,
        src_ip: A,
        dst_ip: A,
        expected: usize,
    ) {
        let buf = Buf::new(Vec::new(), ..)
            .encapsulate(<A::Version as IpExt>::PacketBuilder::new(
                src_ip,
                dst_ip,
                64,
                IpProto::Tcp.into(),
            ))
            .serialize_vec_outer()
            .ok()
            .unwrap()
            .into_inner();

        receive_ip_packet::<_, _, A::Version>(ctx, device, FrameDestination::Unicast, buf);
        assert_eq!(get_counter_val(ctx, dispatch_receive_ip_packet_name::<A::Version>()), expected);
    }

    #[ip_test]
    fn test_multiple_ip_addresses<I: Ip + TestIpExt>() {
        let config = I::DUMMY_CONFIG;
        let mut ctx = DummyEventDispatcherBuilder::default().build::<DummyEventDispatcher>();
        let device =
            ctx.state_mut().add_ethernet_device(config.local_mac, Ipv6::MINIMUM_LINK_MTU.into());
        crate::device::initialize_device(&mut ctx, device);

        let ip1 = I::get_other_ip_address(1);
        let ip2 = I::get_other_ip_address(2);
        let from_ip = I::get_other_ip_address(3).get();

        assert!(crate::device::get_ip_addr_state(&ctx, device, &ip1).is_none());
        assert!(crate::device::get_ip_addr_state(&ctx, device, &ip2).is_none());

        // Should not receive packets on any IP.
        receive_simple_ip_packet_test(&mut ctx, device, from_ip, ip1.get(), 0);
        receive_simple_ip_packet_test(&mut ctx, device, from_ip, ip2.get(), 0);

        // Add ip1 to device.
        crate::device::add_ip_addr_subnet(
            &mut ctx,
            device,
            AddrSubnet::new(ip1.get(), I::Addr::BYTES * 8).unwrap(),
        )
        .unwrap();
        assert!(crate::device::get_ip_addr_state(&ctx, device, &ip1).unwrap().is_assigned());
        assert!(crate::device::get_ip_addr_state(&ctx, device, &ip2).is_none());

        // Should receive packets on ip1 but not ip2
        receive_simple_ip_packet_test(&mut ctx, device, from_ip, ip1.get(), 1);
        receive_simple_ip_packet_test(&mut ctx, device, from_ip, ip2.get(), 1);

        // Add ip2 to device.
        crate::device::add_ip_addr_subnet(
            &mut ctx,
            device,
            AddrSubnet::new(ip2.get(), I::Addr::BYTES * 8).unwrap(),
        )
        .unwrap();
        assert!(crate::device::get_ip_addr_state(&ctx, device, &ip1).unwrap().is_assigned());
        assert!(crate::device::get_ip_addr_state(&ctx, device, &ip2).unwrap().is_assigned());

        // Should receive packets on both ips
        receive_simple_ip_packet_test(&mut ctx, device, from_ip, ip1.get(), 2);
        receive_simple_ip_packet_test(&mut ctx, device, from_ip, ip2.get(), 3);

        // Remove ip1
        crate::device::del_ip_addr(&mut ctx, device, &ip1).unwrap();
        assert!(crate::device::get_ip_addr_state(&ctx, device, &ip1).is_none());
        assert!(crate::device::get_ip_addr_state(&ctx, device, &ip2).unwrap().is_assigned());

        // Should receive packets on ip2
        receive_simple_ip_packet_test(&mut ctx, device, from_ip, ip1.get(), 3);
        receive_simple_ip_packet_test(&mut ctx, device, from_ip, ip2.get(), 4);
    }

    /// Get a multicast address.
    #[specialize_ip]
    fn get_multicast_addr<I: Ip>() -> MulticastAddr<I::Addr> {
        #[ipv4]
        return MulticastAddr::new(Ipv4Addr::new([224, 0, 0, 1])).unwrap();

        #[ipv6]
        return MulticastAddr::new(Ipv6Addr::new([0xff00, 0, 0, 0, 0, 0, 0, 1])).unwrap();
    }

    /// Test that we can join and leave a multicast group, but we only truly
    /// leave it after calling `leave_ip_multicast` the same number of times as
    /// `join_ip_multicast`.
    #[ip_test]
    fn test_ip_join_leave_multicast_addr_ref_count<I: Ip + TestIpExt>() {
        let config = I::DUMMY_CONFIG;
        let mut ctx = DummyEventDispatcherBuilder::default().build::<DummyEventDispatcher>();
        let device =
            ctx.state_mut().add_ethernet_device(config.local_mac, Ipv6::MINIMUM_LINK_MTU.into());
        crate::device::initialize_device(&mut ctx, device);

        let multicast_addr = get_multicast_addr::<I>();

        // Should not be in the multicast group yet.
        assert!(!crate::device::is_in_ip_multicast(&mut ctx, device, multicast_addr));

        // Join the multicast group.
        crate::device::join_ip_multicast(&mut ctx, device, multicast_addr);
        assert!(crate::device::is_in_ip_multicast(&mut ctx, device, multicast_addr));

        // Leave the multicast group.
        crate::device::leave_ip_multicast(&mut ctx, device, multicast_addr);
        assert!(!crate::device::is_in_ip_multicast(&mut ctx, device, multicast_addr));

        // Join the multicst group.
        crate::device::join_ip_multicast(&mut ctx, device, multicast_addr);
        assert!(crate::device::is_in_ip_multicast(&mut ctx, device, multicast_addr));

        // Join it again...
        crate::device::join_ip_multicast(&mut ctx, device, multicast_addr);
        assert!(crate::device::is_in_ip_multicast(&mut ctx, device, multicast_addr));

        // Leave it (still in it because we joined twice).
        crate::device::leave_ip_multicast(&mut ctx, device, multicast_addr);
        assert!(crate::device::is_in_ip_multicast(&mut ctx, device, multicast_addr));

        // Leave it again... (actually left now).
        crate::device::leave_ip_multicast(&mut ctx, device, multicast_addr);
        assert!(!crate::device::is_in_ip_multicast(&mut ctx, device, multicast_addr));
    }

    /// Test leaving a multicast group a device has not yet joined.
    ///
    /// # Panics
    ///
    /// This method should always panic as leaving an unjoined multicast group
    /// is a panic condition.
    #[ip_test]
    #[should_panic(expected = "attempted to leave IP multicast group we were not a member of:")]
    fn test_ip_leave_unjoined_multicast<I: Ip + TestIpExt>() {
        let config = I::DUMMY_CONFIG;
        let mut ctx = DummyEventDispatcherBuilder::default().build::<DummyEventDispatcher>();
        let device =
            ctx.state_mut().add_ethernet_device(config.local_mac, Ipv6::MINIMUM_LINK_MTU.into());
        crate::device::initialize_device(&mut ctx, device);

        let multicast_addr = get_multicast_addr::<I>();

        // Should not be in the multicast group yet.
        assert!(!crate::device::is_in_ip_multicast(&mut ctx, device, multicast_addr));

        // Leave it (this should panic).
        crate::device::leave_ip_multicast(&mut ctx, device, multicast_addr);
    }

    #[test]
    fn test_ipv6_duplicate_solicited_node_address() {
        // Test that we still receive packets destined to a solicited-node
        // multicast address of an IP address we deleted because another
        // (distinct) IP address that is still assigned uses the same
        // solicited-node multicast address.

        let config = Ipv6::DUMMY_CONFIG;
        let mut ctx = DummyEventDispatcherBuilder::default().build::<DummyEventDispatcher>();
        let device =
            ctx.state_mut().add_ethernet_device(config.local_mac, Ipv6::MINIMUM_LINK_MTU.into());
        crate::device::initialize_device(&mut ctx, device);

        let ip1 = SpecifiedAddr::new(Ipv6Addr::new([0, 0, 0, 1, 0, 0, 0, 1])).unwrap();
        let ip2 = SpecifiedAddr::new(Ipv6Addr::new([0, 0, 0, 2, 0, 0, 0, 1])).unwrap();
        let from_ip = Ipv6Addr::new([0, 0, 0, 3, 0, 0, 0, 1]);

        // ip1 and ip2 are not equal but their solicited node addresses are the
        // same.
        assert_ne!(ip1, ip2);
        assert_eq!(ip1.to_solicited_node_address(), ip2.to_solicited_node_address());
        let sn_addr = ip1.to_solicited_node_address().get();

        let addr_sub1 = AddrSubnet::new(ip1.get(), 64).unwrap();
        let addr_sub2 = AddrSubnet::new(ip2.get(), 64).unwrap();

        assert_eq!(get_counter_val(&mut ctx, "dispatch_receive_ip_packet"), 0);

        // Add ip1 to the device.
        //
        // Should get packets destined for the solicited node address and ip1.
        crate::device::add_ip_addr_subnet(&mut ctx, device, addr_sub1).unwrap();
        receive_simple_ip_packet_test(&mut ctx, device, from_ip, ip1.get(), 1);
        receive_simple_ip_packet_test(&mut ctx, device, from_ip, ip2.get(), 1);
        receive_simple_ip_packet_test(&mut ctx, device, from_ip, sn_addr, 2);

        // Add ip2 to the device.
        //
        // Should get packets destined for the solicited node address, ip1 and
        // ip2.
        crate::device::add_ip_addr_subnet(&mut ctx, device, addr_sub2).unwrap();
        receive_simple_ip_packet_test(&mut ctx, device, from_ip, ip1.get(), 3);
        receive_simple_ip_packet_test(&mut ctx, device, from_ip, ip2.get(), 4);
        receive_simple_ip_packet_test(&mut ctx, device, from_ip, sn_addr, 5);

        // Remove ip1 from the device.
        //
        // Should get packets destined for the solicited node address and ip2.
        crate::device::del_ip_addr(&mut ctx, device, &ip1).unwrap();
        receive_simple_ip_packet_test(&mut ctx, device, from_ip, ip1.get(), 5);
        receive_simple_ip_packet_test(&mut ctx, device, from_ip, ip2.get(), 6);
        receive_simple_ip_packet_test(&mut ctx, device, from_ip, sn_addr, 7);
    }

    #[test]
    fn test_get_ip_addr_subnet() {
        // Test that `get_ip_addr_subnet` only returns non-local IPv6 addresses.

        let config = Ipv6::DUMMY_CONFIG;
        let mut ctx = DummyEventDispatcherBuilder::default().build::<DummyEventDispatcher>();
        let device =
            ctx.state_mut().add_ethernet_device(config.local_mac, Ipv6::MINIMUM_LINK_MTU.into());
        assert_eq!(device.id, 0);
        let device = EthernetDeviceId(0);

        // `initialize_device` adds the MAC-derived link-local IPv6 address.
        initialize_device(&mut ctx, device);
        let addr_sub = &ctx.state().device.ethernet.get(0).unwrap().device.ip.ipv6_addr_sub;
        // Verify that there is a single assigned address - the MAC-derived
        // link-local.
        assert_eq!(addr_sub.len(), 1);
        assert_eq!(
            addr_sub[0].addr_sub().addr().get(),
            config.local_mac.to_ipv6_link_local().addr().get()
        );
        // Verify that `get_ip_addr_subnet` returns no address since the only
        // address present is link-local.
        assert_eq!(get_ip_addr_subnet::<_, Ipv6Addr>(&ctx, device), None);
    }

    #[test]
    fn test_add_ip_addr_subnet_link_local() {
        // Test that `add_ip_addr_subnet` allows link-local addresses.

        let config = Ipv6::DUMMY_CONFIG;
        let mut ctx = DummyEventDispatcherBuilder::default().build::<DummyEventDispatcher>();
        let device =
            ctx.state_mut().add_ethernet_device(config.local_mac, Ipv6::MINIMUM_LINK_MTU.into());
        assert_eq!(device.id, 0);
        let device = EthernetDeviceId(0);

        initialize_device(&mut ctx, device);
        // Verify that there is a single assigned address.
        assert_eq!(ctx.state().device.ethernet.get(0).unwrap().device.ip.ipv6_addr_sub.len(), 1);
        add_ip_addr_subnet(
            &mut ctx,
            device,
            AddrSubnet::new(Ipv6::LINK_LOCAL_UNICAST_SUBNET.network(), 128).unwrap(),
        )
        .unwrap();
        // Assert that the new address got added.
        let addr_sub = &ctx.state().device.ethernet.get(0).unwrap().device.ip.ipv6_addr_sub;
        assert_eq!(addr_sub.len(), 2);
        assert_eq!(addr_sub[1].addr_sub().addr().get(), Ipv6::LINK_LOCAL_UNICAST_SUBNET.network());
    }
}
