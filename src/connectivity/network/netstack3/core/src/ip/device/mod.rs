// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! An IP device.

pub(crate) mod dad;
mod integration;
pub(crate) mod route_discovery;
pub(crate) mod router_solicitation;
pub(crate) mod state;

use alloc::{boxed::Box, vec::Vec};
use core::{num::NonZeroU8, time::Duration};

#[cfg(test)]
use net_types::ip::IpVersion;
use net_types::{
    ip::{AddrSubnet, Ip, IpAddress as _, Ipv4, Ipv4Addr, Ipv6, Ipv6Addr},
    MulticastAddr, SpecifiedAddr, UnicastAddr, Witness as _,
};
use packet::{BufferMut, EmptyBuf, Serializer};

use crate::{
    context::{EventContext, InstantContext, RngContext, TimerContext, TimerHandler},
    error::{ExistsError, NotFoundError},
    ip::{
        device::{
            dad::{DadHandler, DadTimerId},
            route_discovery::{Ipv6DiscoveredRouteTimerId, RouteDiscoveryHandler},
            router_solicitation::{RsHandler, RsTimerId},
            state::{
                AddrConfig, AddressState, IpDeviceConfiguration, IpDeviceState, IpDeviceStateIpExt,
                Ipv4DeviceConfiguration, Ipv4DeviceState, Ipv6AddressEntry,
                Ipv6DeviceConfiguration, Ipv6DeviceState,
            },
        },
        gmp::{
            igmp::IgmpTimerId, mld::MldReportDelay, GmpHandler, GroupJoinResult, GroupLeaveResult,
        },
        IpDeviceIdContext,
    },
    Instant,
};
#[cfg(test)]
use crate::{error::NotSupportedError, ip::IpDeviceId as _};

/// A timer ID for IPv4 devices.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Hash)]
pub(crate) struct Ipv4DeviceTimerId<DeviceId>(IgmpTimerId<DeviceId>);

impl<DeviceId> From<IgmpTimerId<DeviceId>> for Ipv4DeviceTimerId<DeviceId> {
    fn from(id: IgmpTimerId<DeviceId>) -> Ipv4DeviceTimerId<DeviceId> {
        Ipv4DeviceTimerId(id)
    }
}

// If we are provided with an impl of `TimerContext<Ipv4DeviceTimerId<_>>`, then
// we can in turn provide an impl of `TimerContext` for IGMP.
impl_timer_context!(
    IpDeviceIdContext<Ipv4>,
    Ipv4DeviceTimerId<C::DeviceId>,
    IgmpTimerId<C::DeviceId>,
    Ipv4DeviceTimerId(id),
    id
);

/// Handle an IPv4 device timer firing.
pub(crate) fn handle_ipv4_timer<C: BufferIpDeviceContext<Ipv4, EmptyBuf>>(
    sync_ctx: &mut C,
    Ipv4DeviceTimerId(id): Ipv4DeviceTimerId<C::DeviceId>,
) {
    TimerHandler::handle_timer(sync_ctx, id)
}

/// A timer ID for IPv6 devices.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Hash)]
pub(crate) enum Ipv6DeviceTimerId<DeviceId> {
    Mld(MldReportDelay<DeviceId>),
    Dad(DadTimerId<DeviceId>),
    Rs(RsTimerId<DeviceId>),
    RouteDiscovery(Ipv6DiscoveredRouteTimerId<DeviceId>),
}

impl<DeviceId> From<MldReportDelay<DeviceId>> for Ipv6DeviceTimerId<DeviceId> {
    fn from(id: MldReportDelay<DeviceId>) -> Ipv6DeviceTimerId<DeviceId> {
        Ipv6DeviceTimerId::Mld(id)
    }
}

impl<DeviceId> From<DadTimerId<DeviceId>> for Ipv6DeviceTimerId<DeviceId> {
    fn from(id: DadTimerId<DeviceId>) -> Ipv6DeviceTimerId<DeviceId> {
        Ipv6DeviceTimerId::Dad(id)
    }
}

impl<DeviceId> From<RsTimerId<DeviceId>> for Ipv6DeviceTimerId<DeviceId> {
    fn from(id: RsTimerId<DeviceId>) -> Ipv6DeviceTimerId<DeviceId> {
        Ipv6DeviceTimerId::Rs(id)
    }
}

impl<DeviceId> From<Ipv6DiscoveredRouteTimerId<DeviceId>> for Ipv6DeviceTimerId<DeviceId> {
    fn from(id: Ipv6DiscoveredRouteTimerId<DeviceId>) -> Ipv6DeviceTimerId<DeviceId> {
        Ipv6DeviceTimerId::RouteDiscovery(id)
    }
}

// If we are provided with an impl of `TimerContext<Ipv6DeviceTimerId<_>>`, then
// we can in turn provide an impl of `TimerContext` for MLD and DAD.
impl_timer_context!(
    IpDeviceIdContext<Ipv6>,
    Ipv6DeviceTimerId<C::DeviceId>,
    MldReportDelay<C::DeviceId>,
    Ipv6DeviceTimerId::Mld(id),
    id
);
impl_timer_context!(
    IpDeviceIdContext<Ipv6>,
    Ipv6DeviceTimerId<C::DeviceId>,
    DadTimerId<C::DeviceId>,
    Ipv6DeviceTimerId::Dad(id),
    id
);
impl_timer_context!(
    IpDeviceIdContext<Ipv6>,
    Ipv6DeviceTimerId<C::DeviceId>,
    RsTimerId<C::DeviceId>,
    Ipv6DeviceTimerId::Rs(id),
    id
);
impl_timer_context!(
    IpDeviceIdContext<Ipv6>,
    Ipv6DeviceTimerId<C::DeviceId>,
    Ipv6DiscoveredRouteTimerId<C::DeviceId>,
    Ipv6DeviceTimerId::RouteDiscovery(id),
    id
);

/// Handle an IPv6 device timer firing.
pub(crate) fn handle_ipv6_timer<
    C: BufferIpDeviceContext<Ipv6, EmptyBuf>
        + DadHandler
        + RsHandler
        + TimerHandler<Ipv6DiscoveredRouteTimerId<C::DeviceId>>
        + TimerHandler<MldReportDelay<C::DeviceId>>,
>(
    sync_ctx: &mut C,
    id: Ipv6DeviceTimerId<C::DeviceId>,
) {
    match id {
        Ipv6DeviceTimerId::Mld(id) => TimerHandler::handle_timer(sync_ctx, id),
        Ipv6DeviceTimerId::Dad(id) => DadHandler::handle_timer(sync_ctx, id),
        Ipv6DeviceTimerId::Rs(id) => RsHandler::handle_timer(sync_ctx, id),
        Ipv6DeviceTimerId::RouteDiscovery(id) => TimerHandler::handle_timer(sync_ctx, id),
    }
}

/// An extension trait adding IP device properties.
pub(crate) trait IpDeviceIpExt<Instant, DeviceId>: IpDeviceStateIpExt<Instant> {
    type State: AsRef<IpDeviceState<Instant, Self>> + AsRef<IpDeviceConfiguration>;
    type Timer;
}

impl<I: Instant, DeviceId> IpDeviceIpExt<I, DeviceId> for Ipv4 {
    type State = Ipv4DeviceState<I>;
    type Timer = Ipv4DeviceTimerId<DeviceId>;
}

impl<I: Instant, DeviceId> IpDeviceIpExt<I, DeviceId> for Ipv6 {
    type State = Ipv6DeviceState<I>;
    type Timer = Ipv6DeviceTimerId<DeviceId>;
}

#[derive(Debug)]
/// Events emitted from IP devices.
pub enum IpDeviceEvent<DeviceId, I: Ip> {
    /// Address was assigned.
    AddressAssigned {
        /// The device.
        device: DeviceId,
        /// The new address.
        addr: I::InterfaceAddress,
    },
    /// Address was unassigned.
    AddressUnassigned {
        /// The device.
        device: DeviceId,
        /// The removed address.
        addr: I::InterfaceAddress,
    },
}

/// The execution context for IP devices.
pub(crate) trait IpDeviceContext<
    I: IpDeviceIpExt<<Self as InstantContext>::Instant, Self::DeviceId>,
>:
    IpDeviceIdContext<I>
    + TimerContext<I::Timer>
    + RngContext
    + EventContext<IpDeviceEvent<Self::DeviceId, I>>
{
    /// Gets immutable access to an IP device's state.
    fn get_ip_device_state(&self, device_id: Self::DeviceId) -> &I::State;

    /// Gets mutable access to an IP device's state.
    fn get_ip_device_state_mut(&mut self, device_id: Self::DeviceId) -> &mut I::State {
        let (state, _rng) = self.get_ip_device_state_mut_and_rng(device_id);
        state
    }

    /// Get mutable access to an IP device's state.
    fn get_ip_device_state_mut_and_rng(
        &mut self,
        device_id: Self::DeviceId,
    ) -> (&mut I::State, &mut Self::Rng);

    /// Returns an [`Iterator`] of IDs for all initialized devices.
    fn iter_devices(&self) -> Box<dyn Iterator<Item = Self::DeviceId> + '_>;

    /// Gets the MTU for a device.
    ///
    /// The MTU is the maximum size of an IP packet.
    fn get_mtu(&self, device_id: Self::DeviceId) -> u32;

    /// Joins the link-layer multicast group associated with the given IP
    /// multicast group.
    fn join_link_multicast_group(
        &mut self,
        device_id: Self::DeviceId,
        multicast_addr: MulticastAddr<I::Addr>,
    );

    /// Leaves the link-layer multicast group associated with the given IP
    /// multicast group.
    fn leave_link_multicast_group(
        &mut self,
        device_id: Self::DeviceId,
        multicast_addr: MulticastAddr<I::Addr>,
    );
}

/// The execution context for an IPv6 device.
pub(crate) trait Ipv6DeviceContext: IpDeviceContext<Ipv6> {
    /// Returns the NDP retransmission timer configured on the device.
    // TODO(https://fxbug.dev/72378): Remove this method once DAD operates at
    // L3.
    fn retrans_timer(&self, device_id: Self::DeviceId) -> Duration;

    /// Gets the device's link-layer address bytes, if the device supports
    /// link-layer addressing.
    fn get_link_layer_addr_bytes(&self, device_id: Self::DeviceId) -> Option<&[u8]>;

    /// Gets the device's EUI-64 based interface identifier.
    ///
    /// A `None` value indicates the device does not have an EUI-64 based
    /// interface identifier.
    fn get_eui64_iid(&self, device_id: Self::DeviceId) -> Option<[u8; 8]>;
}

/// The execution context for an IP device with a buffer.
pub(crate) trait BufferIpDeviceContext<
    I: IpDeviceIpExt<Self::Instant, Self::DeviceId>,
    B: BufferMut,
>: IpDeviceContext<I>
{
    /// Sends an IP packet through the device.
    fn send_ip_frame<S: Serializer<Buffer = B>>(
        &mut self,
        device_id: Self::DeviceId,
        local_addr: SpecifiedAddr<I::Addr>,
        body: S,
    ) -> Result<(), S>;
}

fn enable_ipv6_device<C: Ipv6DeviceContext + GmpHandler<Ipv6> + RsHandler + DadHandler>(
    sync_ctx: &mut C,
    device_id: C::DeviceId,
) {
    let Ipv6DeviceState {
        ip_state: _,
        config:
            Ipv6DeviceConfiguration {
                dad_transmits,
                max_router_solicitations: _,
                ip_config: IpDeviceConfiguration { ip_enabled: _, gmp_enabled: _ },
            },
        router_soliciations_remaining: _,
        route_discovery: _,
    } = sync_ctx.get_ip_device_state_mut(device_id);
    let dad_transmits = *dad_transmits;

    // All nodes should join the all-nodes multicast group.
    join_ip_multicast(sync_ctx, device_id, Ipv6::ALL_NODES_LINK_LOCAL_MULTICAST_ADDRESS);
    GmpHandler::gmp_handle_maybe_enabled(sync_ctx, device_id);

    // Perform DAD for all addresses when enabling a device.
    //
    // We have to do this for all addresses (including ones that had DAD
    // performed) as while the device was disabled, another node could have
    // assigned the address and we wouldn't have responded to its DAD
    // solicitations.
    sync_ctx
        .get_ip_device_state_mut(device_id)
        .ip_state
        .iter_addrs_mut()
        .map(|Ipv6AddressEntry { addr_sub, state, config: _ }| {
            *state = AddressState::Tentative { dad_transmits_remaining: dad_transmits };
            addr_sub.ipv6_unicast_addr()
        })
        .collect::<Vec<_>>()
        .into_iter()
        .for_each(|addr| {
            DadHandler::do_duplicate_address_detection(sync_ctx, device_id, addr);
        });

    // TODO(https://fxbug.dev/95946): Generate link-local address with opaque
    // IIDs.
    if let Some(iid) = sync_ctx.get_eui64_iid(device_id) {
        let link_local_addr_sub = {
            let mut addr = [0; 16];
            addr[0..2].copy_from_slice(&[0xfe, 0x80]);
            addr[(Ipv6::UNICAST_INTERFACE_IDENTIFIER_BITS / 8) as usize..].copy_from_slice(&iid);

            AddrSubnet::new(
                Ipv6Addr::from(addr),
                Ipv6Addr::BYTES * 8 - Ipv6::UNICAST_INTERFACE_IDENTIFIER_BITS,
            )
            .expect("valid link-local address")
        };

        match add_ipv6_addr_subnet(
            sync_ctx,
            device_id,
            link_local_addr_sub,
            AddrConfig::SLAAC_LINK_LOCAL,
        ) {
            Ok(()) => {}
            Err(ExistsError) => {
                // The address may have been added by admin action so it is safe
                // to swallow the exists error.
            }
        }
    }

    // As per RFC 4861 section 6.3.7,
    //
    //    A host sends Router Solicitations to the all-routers multicast
    //    address.
    //
    // If we are operating as a router, we do not solicit routers.
    if !is_ipv6_routing_enabled(sync_ctx, device_id) {
        RsHandler::start_router_solicitation(sync_ctx, device_id);
    }
}

fn disable_ipv6_device<
    C: Ipv6DeviceContext + GmpHandler<Ipv6> + RsHandler + DadHandler + RouteDiscoveryHandler,
>(
    sync_ctx: &mut C,
    device_id: C::DeviceId,
) {
    RouteDiscoveryHandler::invalidate_routes(sync_ctx, device_id);

    RsHandler::stop_router_solicitation(sync_ctx, device_id);

    // Delete the link-local address generated when enabling the device and stop
    // DAD on the other addresses.
    sync_ctx
        .get_ip_device_state(device_id)
        .ip_state
        .iter_addrs()
        .map(|Ipv6AddressEntry { addr_sub, state: _, config }| {
            (addr_sub.ipv6_unicast_addr(), *config)
        })
        .collect::<Vec<_>>()
        .into_iter()
        .for_each(|(addr, config)| {
            if config == AddrConfig::SLAAC_LINK_LOCAL {
                del_ipv6_addr(sync_ctx, device_id, &addr.into_specified())
                    .expect("delete listed address")
            } else {
                DadHandler::stop_duplicate_address_detection(sync_ctx, device_id, addr)
            }
        });

    GmpHandler::gmp_handle_disabled(sync_ctx, device_id);
    leave_ip_multicast(sync_ctx, device_id, Ipv6::ALL_NODES_LINK_LOCAL_MULTICAST_ADDRESS);
}

fn enable_ipv4_device<C: IpDeviceContext<Ipv4> + GmpHandler<Ipv4>>(
    sync_ctx: &mut C,
    device_id: C::DeviceId,
) {
    GmpHandler::gmp_handle_maybe_enabled(sync_ctx, device_id);
}

fn disable_ipv4_device<C: IpDeviceContext<Ipv4> + GmpHandler<Ipv4>>(
    sync_ctx: &mut C,
    device_id: C::DeviceId,
) {
    GmpHandler::gmp_handle_disabled(sync_ctx, device_id);
}

/// Gets the IPv4 address and subnet pairs associated with this device.
///
/// Returns an [`Iterator`] of `AddrSubnet`.
pub(crate) fn get_assigned_ipv4_addr_subnets<C: IpDeviceContext<Ipv4>>(
    sync_ctx: &C,
    device_id: C::DeviceId,
) -> impl Iterator<Item = AddrSubnet<Ipv4Addr>> + '_ {
    sync_ctx.get_ip_device_state(device_id).ip_state.iter_addrs().cloned()
}

/// Gets the IPv6 address and subnet pairs associated with this device which are
/// in the assigned state.
///
/// Tentative IP addresses (addresses which are not yet fully bound to a device)
/// and deprecated IP addresses (addresses which have been assigned but should
/// no longer be used for new connections) will not be returned by
/// `get_assigned_ipv6_addr_subnets`.
///
/// Returns an [`Iterator`] of `AddrSubnet`.
///
/// See [`Tentative`] and [`AddrSubnet`] for more information.
pub(crate) fn get_assigned_ipv6_addr_subnets<C: IpDeviceContext<Ipv6>>(
    sync_ctx: &C,
    device_id: C::DeviceId,
) -> impl Iterator<Item = AddrSubnet<Ipv6Addr>> + '_ {
    sync_ctx.get_ip_device_state(device_id).ip_state.iter_addrs().filter_map(|a| {
        if a.state.is_assigned() {
            Some((*a.addr_sub()).to_witness())
        } else {
            None
        }
    })
}

/// Gets a single IPv4 address and subnet for a device.
pub(super) fn get_ipv4_addr_subnet<C: IpDeviceContext<Ipv4>>(
    sync_ctx: &C,
    device_id: C::DeviceId,
) -> Option<AddrSubnet<Ipv4Addr>> {
    get_assigned_ipv4_addr_subnets(sync_ctx, device_id).nth(0)
}

/// Gets the state associated with an IPv4 device.
pub(crate) fn get_ipv4_device_state<C: IpDeviceContext<Ipv4>>(
    sync_ctx: &C,
    device_id: C::DeviceId,
) -> &IpDeviceState<C::Instant, Ipv4> {
    &sync_ctx.get_ip_device_state(device_id).ip_state
}

/// Gets the state associated with an IPv6 device.
pub(crate) fn get_ipv6_device_state<C: IpDeviceContext<Ipv6>>(
    sync_ctx: &C,
    device_id: C::DeviceId,
) -> &IpDeviceState<C::Instant, Ipv6> {
    &sync_ctx.get_ip_device_state(device_id).ip_state
}

/// Gets the hop limit for new IPv6 packets that will be sent out from `device`.
pub(crate) fn get_ipv6_hop_limit<C: IpDeviceContext<Ipv6>>(
    sync_ctx: &C,
    device: C::DeviceId,
) -> NonZeroU8 {
    get_ipv6_device_state(sync_ctx, device).default_hop_limit
}

/// Iterates over all of the IPv4 devices in the stack.
pub(super) fn iter_ipv4_devices<C: IpDeviceContext<Ipv4>>(
    sync_ctx: &C,
) -> impl Iterator<Item = (C::DeviceId, &IpDeviceState<C::Instant, Ipv4>)> + '_ {
    sync_ctx.iter_devices().map(move |device| (device, get_ipv4_device_state(sync_ctx, device)))
}

/// Iterates over all of the IPv6 devices in the stack.
pub(super) fn iter_ipv6_devices<C: IpDeviceContext<Ipv6>>(
    sync_ctx: &C,
) -> impl Iterator<Item = (C::DeviceId, &IpDeviceState<C::Instant, Ipv6>)> + '_ {
    sync_ctx.iter_devices().map(move |device| (device, get_ipv6_device_state(sync_ctx, device)))
}

/// Is IPv4 packet routing enabled on `device`?
pub(crate) fn is_ipv4_routing_enabled<C: IpDeviceContext<Ipv4>>(
    sync_ctx: &C,
    device_id: C::DeviceId,
) -> bool {
    get_ipv4_device_state(sync_ctx, device_id).routing_enabled
}

/// Is IPv6 packet routing enabled on `device`?
pub(crate) fn is_ipv6_routing_enabled<C: IpDeviceContext<Ipv6>>(
    sync_ctx: &C,
    device_id: C::DeviceId,
) -> bool {
    get_ipv6_device_state(sync_ctx, device_id).routing_enabled
}

/// Enables or disables IP packet routing on `device`.
#[cfg(test)]
pub(crate) fn set_routing_enabled<
    C: IpDeviceContext<Ipv4> + Ipv6DeviceContext + GmpHandler<Ipv6> + RsHandler,
    I: Ip,
>(
    sync_ctx: &mut C,
    device: <C as IpDeviceIdContext<Ipv6>>::DeviceId,
    enabled: bool,
) -> Result<(), NotSupportedError>
where
    C: IpDeviceIdContext<Ipv6, DeviceId = <C as IpDeviceIdContext<Ipv4>>::DeviceId>,
{
    match I::VERSION {
        IpVersion::V4 => set_ipv4_routing_enabled(sync_ctx, device, enabled),
        IpVersion::V6 => set_ipv6_routing_enabled(sync_ctx, device, enabled),
    }
}

/// Enables or disables IPv4 packet routing on `device_id`.
#[cfg(test)]
fn set_ipv4_routing_enabled<C: IpDeviceContext<Ipv4>>(
    sync_ctx: &mut C,
    device_id: C::DeviceId,
    enabled: bool,
) -> Result<(), NotSupportedError> {
    if device_id.is_loopback() {
        return Err(NotSupportedError);
    }

    sync_ctx.get_ip_device_state_mut(device_id).ip_state.routing_enabled = enabled;
    Ok(())
}

/// Enables or disables IPv4 packet routing on `device_id`.
///
/// When routing is enabled/disabled, the interface will leave/join the all
/// routers link-local multicast group and stop/start soliciting routers.
///
/// Does nothing if the routing status does not change as a consequence of this
/// call.
#[cfg(test)]
pub(crate) fn set_ipv6_routing_enabled<C: Ipv6DeviceContext + GmpHandler<Ipv6> + RsHandler>(
    sync_ctx: &mut C,
    device_id: C::DeviceId,
    enabled: bool,
) -> Result<(), NotSupportedError> {
    if device_id.is_loopback() {
        return Err(NotSupportedError);
    }

    if is_ipv6_routing_enabled(sync_ctx, device_id) == enabled {
        return Ok(());
    }

    if enabled {
        RsHandler::stop_router_solicitation(sync_ctx, device_id);
        sync_ctx.get_ip_device_state_mut(device_id).ip_state.routing_enabled = true;
        join_ip_multicast(sync_ctx, device_id, Ipv6::ALL_ROUTERS_LINK_LOCAL_MULTICAST_ADDRESS);
    } else {
        leave_ip_multicast(sync_ctx, device_id, Ipv6::ALL_ROUTERS_LINK_LOCAL_MULTICAST_ADDRESS);
        sync_ctx.get_ip_device_state_mut(device_id).ip_state.routing_enabled = false;
        RsHandler::start_router_solicitation(sync_ctx, device_id);
    }

    Ok(())
}

/// Gets the MTU for a device.
///
/// The MTU is the maximum size of an IP packet.
pub(crate) fn get_mtu<I: IpDeviceIpExt<C::Instant, C::DeviceId>, C: IpDeviceContext<I>>(
    sync_ctx: &C,
    device_id: C::DeviceId,
) -> u32 {
    sync_ctx.get_mtu(device_id)
}

/// Adds `device_id` to a multicast group `multicast_addr`.
///
/// Calling `join_ip_multicast` multiple times is completely safe. A counter
/// will be kept for the number of times `join_ip_multicast` has been called
/// with the same `device_id` and `multicast_addr` pair. To completely leave a
/// multicast group, [`leave_ip_multicast`] must be called the same number of
/// times `join_ip_multicast` has been called for the same `device_id` and
/// `multicast_addr` pair. The first time `join_ip_multicast` is called for a
/// new `device` and `multicast_addr` pair, the device will actually join the
/// multicast group.
pub(crate) fn join_ip_multicast<
    I: IpDeviceIpExt<C::Instant, C::DeviceId>,
    C: IpDeviceContext<I> + GmpHandler<I>,
>(
    sync_ctx: &mut C,
    device_id: C::DeviceId,
    multicast_addr: MulticastAddr<I::Addr>,
) {
    match sync_ctx.gmp_join_group(device_id, multicast_addr) {
        GroupJoinResult::Joined(()) => {
            sync_ctx.join_link_multicast_group(device_id, multicast_addr)
        }
        GroupJoinResult::AlreadyMember => {}
    }
}

/// Removes `device_id` from a multicast group `multicast_addr`.
///
/// `leave_ip_multicast` will attempt to remove `device_id` from a multicast
/// group `multicast_addr`. `device_id` may have "joined" the same multicast
/// address multiple times, so `device_id` will only leave the multicast group
/// once `leave_ip_multicast` has been called for each corresponding
/// [`join_ip_multicast`]. That is, if `join_ip_multicast` gets called 3
/// times and `leave_ip_multicast` gets called two times (after all 3
/// `join_ip_multicast` calls), `device_id` will still be in the multicast
/// group until the next (final) call to `leave_ip_multicast`.
///
/// # Panics
///
/// If `device_id` is not currently in the multicast group `multicast_addr`.
pub(crate) fn leave_ip_multicast<
    I: IpDeviceIpExt<C::Instant, C::DeviceId>,
    C: IpDeviceContext<I> + GmpHandler<I>,
>(
    sync_ctx: &mut C,
    device_id: C::DeviceId,
    multicast_addr: MulticastAddr<I::Addr>,
) {
    match sync_ctx.gmp_leave_group(device_id, multicast_addr) {
        GroupLeaveResult::Left(()) => {
            sync_ctx.leave_link_multicast_group(device_id, multicast_addr)
        }
        GroupLeaveResult::StillMember => {}
        GroupLeaveResult::NotMember => panic!(
            "attempted to leave IP multicast group we were not a member of: {}",
            multicast_addr,
        ),
    }
}

/// Adds an IPv4 address and associated subnet to this device.
pub(crate) fn add_ipv4_addr_subnet<
    C: IpDeviceContext<Ipv4> + BufferIpDeviceContext<Ipv4, EmptyBuf>,
>(
    sync_ctx: &mut C,
    device_id: C::DeviceId,
    addr_sub: AddrSubnet<Ipv4Addr>,
) -> Result<(), ExistsError> {
    sync_ctx.get_ip_device_state_mut(device_id).ip_state.add_addr(addr_sub).map(|()| {
        sync_ctx.on_event(IpDeviceEvent::AddressAssigned { device: device_id, addr: addr_sub })
    })
}

/// Adds an IPv6 address (with duplicate address detection) and associated
/// subnet to this device and joins the address's solicited-node multicast
/// group.
///
/// `config` is the way this address is being configured. See [`AddrConfig`]
/// for more details.
pub(crate) fn add_ipv6_addr_subnet<C: Ipv6DeviceContext + GmpHandler<Ipv6> + DadHandler>(
    sync_ctx: &mut C,
    device_id: C::DeviceId,
    addr_sub: AddrSubnet<Ipv6Addr>,
    config: AddrConfig<C::Instant>,
) -> Result<(), ExistsError> {
    let Ipv6DeviceState {
        ref mut ip_state,
        config:
            Ipv6DeviceConfiguration {
                dad_transmits,
                max_router_solicitations: _,
                ip_config: IpDeviceConfiguration { ip_enabled, gmp_enabled: _ },
            },
        router_soliciations_remaining: _,
        route_discovery: _,
    } = sync_ctx.get_ip_device_state_mut(device_id);
    let ip_enabled = *ip_enabled;

    let addr_sub = addr_sub.to_unicast();
    ip_state
        .add_addr(Ipv6AddressEntry::new(
            addr_sub,
            AddressState::Tentative { dad_transmits_remaining: *dad_transmits },
            config,
        ))
        .map(|()| {
            // As per RFC 4861 section 5.6.2,
            //
            //   Before sending a Neighbor Solicitation, an interface MUST join
            //   the all-nodes multicast address and the solicited-node
            //   multicast address of the tentative address.
            //
            // Note that we join the all-nodes multicast address on interface
            // enable.
            join_ip_multicast(sync_ctx, device_id, addr_sub.addr().to_solicited_node_address());

            // NB: We don't start DAD if the device is disabled. DAD will be
            // performed when the device is enabled for all addressed.
            if ip_enabled {
                DadHandler::do_duplicate_address_detection(sync_ctx, device_id, addr_sub.addr());
            }

            // NB: We don't emit an address assigned event here, addresses are
            // only exposed when they've moved from the Tentative state.
        })
}

/// Removes an IPv4 address and associated subnet from this device.
pub(crate) fn del_ipv4_addr<C: IpDeviceContext<Ipv4> + BufferIpDeviceContext<Ipv4, EmptyBuf>>(
    sync_ctx: &mut C,
    device_id: C::DeviceId,
    addr: &SpecifiedAddr<Ipv4Addr>,
) -> Result<(), NotFoundError> {
    sync_ctx
        .get_ip_device_state_mut(device_id)
        .ip_state
        .remove_addr(&addr)
        .map(|addr| sync_ctx.on_event(IpDeviceEvent::AddressUnassigned { device: device_id, addr }))
}

/// Removes an IPv6 address and associated subnet from this device.
pub(crate) fn del_ipv6_addr<C: Ipv6DeviceContext + GmpHandler<Ipv6> + DadHandler>(
    sync_ctx: &mut C,
    device_id: C::DeviceId,
    addr: &SpecifiedAddr<Ipv6Addr>,
) -> Result<(), NotFoundError> {
    sync_ctx.get_ip_device_state_mut(device_id).ip_state.remove_addr(&addr).map(
        |entry: Ipv6AddressEntry<_>| {
            // TODO(https://fxbug.dev/69196): Give `addr` the type
            // `UnicastAddr<Ipv6Addr>` for IPv6 instead of doing this
            // dynamic check here and statically guarantee only unicast
            // addresses are added for IPv6.
            if let Some(addr) = UnicastAddr::new(addr.get()) {
                DadHandler::stop_duplicate_address_detection(sync_ctx, device_id, addr);
                leave_ip_multicast(sync_ctx, device_id, addr.to_solicited_node_address());

                match entry.state {
                    AddressState::Assigned | AddressState::Deprecated => sync_ctx
                        .on_event(IpDeviceEvent::AddressUnassigned { device: device_id, addr }),
                    AddressState::Tentative { .. } => {}
                }
            }
        },
    )
}

/// Sends an IP packet through the device.
pub(crate) fn send_ip_frame<
    I: IpDeviceIpExt<C::Instant, C::DeviceId>,
    C: BufferIpDeviceContext<I, B>,
    B: BufferMut,
    S: Serializer<Buffer = B>,
>(
    sync_ctx: &mut C,
    device_id: C::DeviceId,
    local_addr: SpecifiedAddr<I::Addr>,
    body: S,
) -> Result<(), S> {
    is_ip_device_enabled(sync_ctx, device_id)
        .then(|| sync_ctx.send_ip_frame(device_id, local_addr, body))
        .unwrap_or(Ok(()))
}

pub(crate) fn get_ipv4_configuration<C: IpDeviceContext<Ipv4>>(
    sync_ctx: &C,
    device_id: C::DeviceId,
) -> Ipv4DeviceConfiguration {
    sync_ctx.get_ip_device_state(device_id).config.clone()
}

pub(crate) fn get_ipv6_configuration<C: IpDeviceContext<Ipv6>>(
    sync_ctx: &C,
    device_id: C::DeviceId,
) -> Ipv6DeviceConfiguration {
    sync_ctx.get_ip_device_state(device_id).config.clone()
}

/// Updates the IPv4 Configuration for the device.
pub(crate) fn set_ipv4_configuration<C: IpDeviceContext<Ipv4> + GmpHandler<Ipv4>>(
    sync_ctx: &mut C,
    device_id: C::DeviceId,
    config: Ipv4DeviceConfiguration,
) {
    let Ipv4DeviceConfiguration {
        ip_config:
            IpDeviceConfiguration { ip_enabled: next_ip_enabled, gmp_enabled: next_gmp_enabled },
    } = config;
    let Ipv4DeviceConfiguration {
        ip_config:
            IpDeviceConfiguration { ip_enabled: prev_ip_enabled, gmp_enabled: prev_gmp_enabled },
    } = sync_ctx.get_ip_device_state_mut(device_id).config;
    sync_ctx.get_ip_device_state_mut(device_id).config = config;

    if !prev_ip_enabled && next_ip_enabled {
        enable_ipv4_device(sync_ctx, device_id);
    } else if prev_ip_enabled && !next_ip_enabled {
        disable_ipv4_device(sync_ctx, device_id);
    }

    if !prev_gmp_enabled && next_gmp_enabled {
        GmpHandler::gmp_handle_maybe_enabled(sync_ctx, device_id);
    } else if prev_gmp_enabled && !next_gmp_enabled {
        GmpHandler::gmp_handle_disabled(sync_ctx, device_id);
    }
}

pub(super) fn is_ip_device_enabled<
    I: IpDeviceIpExt<C::Instant, C::DeviceId>,
    C: IpDeviceContext<I>,
>(
    sync_ctx: &C,
    device_id: C::DeviceId,
) -> bool {
    AsRef::<IpDeviceConfiguration>::as_ref(sync_ctx.get_ip_device_state(device_id)).ip_enabled
}

/// Updates the IPv6 Configuration for the device.
pub(crate) fn set_ipv6_configuration<
    C: Ipv6DeviceContext + GmpHandler<Ipv6> + RsHandler + DadHandler + RouteDiscoveryHandler,
>(
    sync_ctx: &mut C,
    device_id: C::DeviceId,
    config: Ipv6DeviceConfiguration,
) {
    let Ipv6DeviceConfiguration {
        dad_transmits: _,
        max_router_solicitations: _,
        ip_config:
            IpDeviceConfiguration { ip_enabled: next_ip_enabled, gmp_enabled: next_gmp_enabled },
    } = config;
    let Ipv6DeviceConfiguration {
        dad_transmits: _,
        max_router_solicitations: _,
        ip_config:
            IpDeviceConfiguration { ip_enabled: prev_ip_enabled, gmp_enabled: prev_gmp_enabled },
    } = sync_ctx.get_ip_device_state_mut(device_id).config;
    sync_ctx.get_ip_device_state_mut(device_id).config = config;

    if !prev_ip_enabled && next_ip_enabled {
        enable_ipv6_device(sync_ctx, device_id);
    } else if prev_ip_enabled && !next_ip_enabled {
        disable_ipv6_device(sync_ctx, device_id);
    }

    if !prev_gmp_enabled && next_gmp_enabled {
        GmpHandler::gmp_handle_maybe_enabled(sync_ctx, device_id);
    } else if prev_gmp_enabled && !next_gmp_enabled {
        GmpHandler::gmp_handle_disabled(sync_ctx, device_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloc::vec;
    use fakealloc::collections::HashSet;

    use net_types::ip::Ipv6;

    use crate::{
        testutil::{assert_empty, DummyCtx, DummyEventDispatcher, TestIpExt as _},
        Ctx, StackStateBuilder, TimerId, TimerIdInner,
    };

    #[test]
    fn enable_disable_ipv6() {
        let mut ctx: DummyCtx = Ctx::new(
            StackStateBuilder::default().build(),
            DummyEventDispatcher::default(),
            Default::default(),
        );
        ctx.ctx.timer_ctx().assert_no_timers_installed();
        let local_mac = Ipv6::DUMMY_CONFIG.local_mac;
        let device_id =
            ctx.state.device.add_ethernet_device(local_mac, Ipv6::MINIMUM_LINK_MTU.into());
        ctx.ctx.timer_ctx().assert_no_timers_installed();

        let ll_addr = local_mac.to_ipv6_link_local();

        // Enable the device and observe an auto-generated link-local address,
        // router solicitation and DAD for the auto-generated address.
        let test_enable_device =
            |ctx: &mut DummyCtx, extra_group: Option<MulticastAddr<Ipv6Addr>>| {
                crate::ip::device::set_ipv6_configuration(ctx, device_id, {
                    let mut config = crate::ip::device::get_ipv6_configuration(ctx, device_id);
                    config.ip_config.ip_enabled = true;
                    config.ip_config.gmp_enabled = true;
                    config
                });
                assert_eq!(
                    IpDeviceContext::<Ipv6>::get_ip_device_state(ctx, device_id)
                        .ip_state
                        .iter_addrs()
                        .map(|Ipv6AddressEntry { addr_sub, state: _, config: _ }| {
                            addr_sub.ipv6_unicast_addr()
                        })
                        .collect::<HashSet<_>>(),
                    HashSet::from([ll_addr.ipv6_unicast_addr()]),
                    "enabled device expected to generate link-local address"
                );
                let mut timers = vec![
                    (
                        TimerId(TimerIdInner::Ipv6Device(Ipv6DeviceTimerId::Rs(RsTimerId {
                            device_id,
                        }))),
                        ..,
                    ),
                    (
                        TimerId(TimerIdInner::Ipv6Device(Ipv6DeviceTimerId::Dad(DadTimerId {
                            device_id,
                            addr: ll_addr.ipv6_unicast_addr(),
                        }))),
                        ..,
                    ),
                    (
                        TimerId(TimerIdInner::Ipv6Device(Ipv6DeviceTimerId::Mld(
                            MldReportDelay {
                                device: device_id,
                                group_addr: local_mac
                                    .to_ipv6_link_local()
                                    .addr()
                                    .to_solicited_node_address(),
                            }
                            .into(),
                        ))),
                        ..,
                    ),
                ];
                if let Some(group_addr) = extra_group {
                    timers.push((
                        TimerId(TimerIdInner::Ipv6Device(Ipv6DeviceTimerId::Mld(
                            MldReportDelay { device: device_id, group_addr }.into(),
                        ))),
                        ..,
                    ))
                }
                ctx.ctx.timer_ctx().assert_timers_installed(timers);
            };
        test_enable_device(&mut ctx, None);

        let test_disable_device = |ctx: &mut DummyCtx| {
            crate::ip::device::set_ipv6_configuration(ctx, device_id, {
                let mut config = crate::ip::device::get_ipv6_configuration(ctx, device_id);
                config.ip_config.ip_enabled = false;
                config
            });
            ctx.ctx.timer_ctx().assert_no_timers_installed();
        };
        test_disable_device(&mut ctx);
        assert_empty(
            IpDeviceContext::<Ipv6>::get_ip_device_state(&ctx, device_id).ip_state.iter_addrs(),
        );

        let multicast_addr = Ipv6::ALL_ROUTERS_LINK_LOCAL_MULTICAST_ADDRESS;
        join_ip_multicast::<Ipv6, _>(&mut ctx, device_id, multicast_addr);
        add_ipv6_addr_subnet(&mut ctx, device_id, ll_addr.to_witness(), AddrConfig::Manual)
            .expect("add MAC based IPv6 link-local address");
        assert_eq!(
            IpDeviceContext::<Ipv6>::get_ip_device_state(&ctx, device_id)
                .ip_state
                .iter_addrs()
                .map(|Ipv6AddressEntry { addr_sub, state: _, config: _ }| {
                    addr_sub.ipv6_unicast_addr()
                })
                .collect::<HashSet<_>>(),
            HashSet::from([ll_addr.ipv6_unicast_addr()])
        );

        test_enable_device(&mut ctx, Some(multicast_addr));
        test_disable_device(&mut ctx);
        assert_eq!(
            IpDeviceContext::<Ipv6>::get_ip_device_state(&ctx, device_id)
                .ip_state
                .iter_addrs()
                .map(|Ipv6AddressEntry { addr_sub, state: _, config: _ }| {
                    addr_sub.ipv6_unicast_addr()
                })
                .collect::<HashSet<_>>(),
            HashSet::from([ll_addr.ipv6_unicast_addr()]),
            "manual addresses should not be removed on device disable"
        );

        leave_ip_multicast::<Ipv6, _>(&mut ctx, device_id, multicast_addr);
        test_enable_device(&mut ctx, None);
    }
}
