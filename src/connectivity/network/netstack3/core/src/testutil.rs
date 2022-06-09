// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Testing-related utilities.

use alloc::{
    borrow::ToOwned,
    collections::HashMap,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use core::{convert::Infallible as Never, fmt::Debug, time::Duration};

use net_types::{
    ethernet::Mac,
    ip::{AddrSubnet, Ip, IpAddr, IpAddress, Ipv4, Ipv4Addr, Ipv6, Ipv6Addr, Subnet, SubnetEither},
    MulticastAddr, SpecifiedAddr, UnicastAddr, Witness,
};
use packet::{BufferMut, Serializer};
use packet_formats::ip::IpProto;
use rand::{self, CryptoRng, Rng as _, RngCore, SeedableRng};
use rand_xorshift::XorShiftRng;

use crate::{
    context::{
        testutil::{
            DummyEventCtx, DummyFrameCtx, DummyNetworkContext, DummyTimerCtx, InstantAndData,
        },
        EventContext, FrameContext as _, InstantContext, TimerContext,
    },
    device::{DeviceId, DeviceLayerEventDispatcher},
    ip::{
        device::{dad::DadEvent, route_discovery::Ipv6RouteDiscoveryEvent, IpDeviceEvent},
        icmp::{BufferIcmpContext, IcmpConnId, IcmpContext, IcmpIpExt},
        AddableEntryEither, DummyDeviceId, IpLayerEvent, SendIpPacketMeta,
    },
    transport::udp::{BufferUdpContext, UdpContext},
    BlanketCoreContext, Ctx, EventDispatcher, NonSyncContext, StackStateBuilder, SyncCtx, TimerId,
};

/// Asserts that an iterable object produces zero items.
///
/// `assert_empty` drains `into_iter.into_iter()` and asserts that zero
/// items are produced. It panics with a message which includes the produced
/// items if this assertion fails.
#[track_caller]
pub(crate) fn assert_empty<I: IntoIterator>(into_iter: I)
where
    I::Item: Debug + PartialEq,
{
    // NOTE: Collecting into a `Vec` is cheap in the happy path because
    // zero-capacity vectors are guaranteed not to allocate.
    assert_eq!(into_iter.into_iter().collect::<Vec<_>>(), &[]);
}

/// Utilities to allow running benchmarks as tests.
///
/// Our benchmarks rely on the unstable `test` feature, which is disallowed in
/// Fuchsia's build system. In order to ensure that our benchmarks are always
/// compiled and tested, this module provides mocks that allow us to run our
/// benchmarks as normal tests when the `benchmark` feature is disabled.
///
/// See the `bench!` macro for details on how this module is used.
pub(crate) mod benchmarks {
    /// A trait to allow mocking of the `test::Bencher` type.
    pub(crate) trait Bencher {
        fn iter<T, F: FnMut() -> T>(&mut self, inner: F);
    }

    #[cfg(benchmark)]
    impl Bencher for criterion::Bencher {
        fn iter<T, F: FnMut() -> T>(&mut self, inner: F) {
            criterion::Bencher::iter(self, inner)
        }
    }

    /// A `Bencher` whose `iter` method runs the provided argument a small,
    /// fixed number of times.
    #[cfg(not(benchmark))]
    pub(crate) struct TestBencher;

    #[cfg(not(benchmark))]
    impl Bencher for TestBencher {
        fn iter<T, F: FnMut() -> T>(&mut self, mut inner: F) {
            const NUM_TEST_ITERS: u32 = 256;
            super::set_logger_for_test();
            for _ in 0..NUM_TEST_ITERS {
                let _: T = inner();
            }
        }
    }

    #[inline(always)]
    pub(crate) fn black_box<T>(placeholder: T) -> T {
        #[cfg(benchmark)]
        return criterion::black_box(placeholder);
        #[cfg(not(benchmark))]
        return placeholder;
    }
}

// Use the `Never` type for the `crate::context::testutil::DummyCtx`'s frame
// metadata type. This ensures that we don't accidentally send frames to its
// `DummyFrameCtx`, which isn't actually used (instead, we use the
// `DummyFrameCtx` stored in `DummyEventDispatcher`). Note that this doesn't
// prevent code from attempting to read from this context (code which only
// accesses the frame contents rather than the frame metadata will still
// compile).
pub(crate) type DummyCtx = Ctx<
    DummyEventDispatcher,
    crate::context::testutil::DummySyncCtx<(), TimerId, Never, (), DummyDeviceId>,
    (),
>;

pub(crate) type DummySyncCtx = SyncCtx<
    DummyEventDispatcher,
    crate::context::testutil::DummySyncCtx<(), TimerId, Never, (), DummyDeviceId>,
    (),
>;

/// A wrapper which implements `RngCore` and `CryptoRng` for any `RngCore`.
///
/// This is used to satisfy [`EventDispatcher`]'s requirement that the
/// associated `Rng` type implements `CryptoRng`.
///
/// # Security
///
/// This is obviously insecure. Don't use it except in testing!
#[derive(Clone, Debug)]
pub(crate) struct FakeCryptoRng<R>(R);

impl Default for FakeCryptoRng<XorShiftRng> {
    fn default() -> FakeCryptoRng<XorShiftRng> {
        FakeCryptoRng::new_xorshift(12957992561116578403)
    }
}

impl FakeCryptoRng<XorShiftRng> {
    /// Creates a new [`FakeCryptoRng<XorShiftRng>`] from a seed.
    pub(crate) fn new_xorshift(seed: u128) -> FakeCryptoRng<XorShiftRng> {
        FakeCryptoRng(new_rng(seed))
    }
}

impl<R: RngCore> RngCore for FakeCryptoRng<R> {
    fn next_u32(&mut self) -> u32 {
        self.0.next_u32()
    }
    fn next_u64(&mut self) -> u64 {
        self.0.next_u64()
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        self.0.fill_bytes(dest)
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand::Error> {
        self.0.try_fill_bytes(dest)
    }
}

impl<R: RngCore> CryptoRng for FakeCryptoRng<R> {}

impl<R: SeedableRng> SeedableRng for FakeCryptoRng<R> {
    type Seed = R::Seed;

    fn from_seed(seed: Self::Seed) -> Self {
        Self(R::from_seed(seed))
    }
}

impl<R: RngCore> crate::context::RngContext for FakeCryptoRng<R> {
    type Rng = Self;

    fn rng(&self) -> &Self::Rng {
        self
    }

    fn rng_mut(&mut self) -> &mut Self::Rng {
        self
    }
}

/// Create a new deterministic RNG from a seed.
pub(crate) fn new_rng(mut seed: u128) -> XorShiftRng {
    if seed == 0 {
        // XorShiftRng can't take 0 seeds
        seed = 1;
    }
    XorShiftRng::from_seed(seed.to_ne_bytes())
}

/// Creates `iterations` fake RNGs.
///
/// `with_fake_rngs` will create `iterations` different [`FakeCryptoRng`]s and
/// call the function `f` for each one of them.
///
/// This function can be used for tests that weed out weirdness that can
/// happen with certain random number sequences.
pub(crate) fn with_fake_rngs<F: Fn(FakeCryptoRng<XorShiftRng>)>(iterations: u128, f: F) {
    for seed in 0..iterations {
        f(FakeCryptoRng::new_xorshift(seed))
    }
}

/// Invokes a function multiple times with different RNG seeds.
pub(crate) fn run_with_many_seeds<F: FnMut(u128)>(mut f: F) {
    // Arbitrary seed.
    let mut rng = new_rng(0x0fe50fae6c37593d71944697f1245847);
    for _ in 0..64 {
        f(rng.gen());
    }
}

#[derive(Default, Debug)]
pub(crate) struct TestCounters {
    data: HashMap<String, usize>,
}

impl TestCounters {
    pub(crate) fn increment(&mut self, key: &str) {
        *(self.data.entry(key.to_string()).or_insert(0)) += 1;
    }

    pub(crate) fn get(&self, key: &str) -> &usize {
        self.data.get(key).unwrap_or(&0)
    }
}

/// log::Log implementation that uses stdout.
///
/// Useful when debugging tests.
struct Logger;

impl log::Log for Logger {
    fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        teststd::println!("{}", record.args())
    }

    fn flush(&self) {}
}

static LOGGER: Logger = Logger;

static LOGGER_ONCE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

/// Install a logger for tests.
///
/// Call this method at the beginning of the test for which logging is desired.
/// This function sets global program state, so all tests that run after this
/// function is called will use the logger.
pub(crate) fn set_logger_for_test() {
    // log::set_logger will panic if called multiple times.
    if LOGGER_ONCE.swap(false, core::sync::atomic::Ordering::AcqRel) {
        log::set_logger(&LOGGER).unwrap();
        log::set_max_level(log::LevelFilter::Trace);
    }
}

/// Get the counter value for a `key`.
pub(crate) fn get_counter_val(ctx: &DummySyncCtx, key: &str) -> usize {
    *ctx.state.test_counters.borrow().get(key)
}

/// An extension trait for `Ip` providing test-related functionality.
pub(crate) trait TestIpExt: Ip {
    /// Either [`DUMMY_CONFIG_V4`] or [`DUMMY_CONFIG_V6`].
    const DUMMY_CONFIG: DummyEventDispatcherConfig<Self::Addr>;

    /// Get an IP address in the same subnet as `Self::DUMMY_CONFIG`.
    ///
    /// `last` is the value to be put in the last octet of the IP address.
    fn get_other_ip_address(last: u8) -> SpecifiedAddr<Self::Addr>;

    /// Get an IP address in a different subnet from `Self::DUMMY_CONFIG`.
    ///
    /// `last` is the value to be put in the last octet of the IP address.
    fn get_other_remote_ip_address(last: u8) -> SpecifiedAddr<Self::Addr>;

    /// Get a multicast IP address.
    ///
    /// `last` is the value to be put in the last octet of the IP address.
    fn get_multicast_addr(last: u8) -> MulticastAddr<Self::Addr>;
}

impl TestIpExt for Ipv4 {
    const DUMMY_CONFIG: DummyEventDispatcherConfig<Ipv4Addr> = DUMMY_CONFIG_V4;

    fn get_other_ip_address(last: u8) -> SpecifiedAddr<Ipv4Addr> {
        let mut bytes = Self::DUMMY_CONFIG.local_ip.get().ipv4_bytes();
        bytes[bytes.len() - 1] = last;
        SpecifiedAddr::new(Ipv4Addr::new(bytes)).unwrap()
    }

    fn get_other_remote_ip_address(last: u8) -> SpecifiedAddr<Self::Addr> {
        let mut bytes = Self::DUMMY_CONFIG.local_ip.get().ipv4_bytes();
        bytes[bytes.len() - 3] += 1;
        bytes[bytes.len() - 1] = last;
        SpecifiedAddr::new(Ipv4Addr::new(bytes)).unwrap()
    }

    fn get_multicast_addr(last: u8) -> MulticastAddr<Self::Addr> {
        assert!(u32::from(Self::Addr::BYTES * 8 - Self::MULTICAST_SUBNET.prefix()) > u8::BITS);
        let mut bytes = Self::MULTICAST_SUBNET.network().ipv4_bytes();
        bytes[bytes.len() - 1] = last;
        MulticastAddr::new(Ipv4Addr::new(bytes)).unwrap()
    }
}

impl TestIpExt for Ipv6 {
    const DUMMY_CONFIG: DummyEventDispatcherConfig<Ipv6Addr> = DUMMY_CONFIG_V6;

    fn get_other_ip_address(last: u8) -> SpecifiedAddr<Ipv6Addr> {
        let mut bytes = Self::DUMMY_CONFIG.local_ip.get().ipv6_bytes();
        bytes[bytes.len() - 1] = last;
        SpecifiedAddr::new(Ipv6Addr::from(bytes)).unwrap()
    }

    fn get_other_remote_ip_address(last: u8) -> SpecifiedAddr<Self::Addr> {
        let mut bytes = Self::DUMMY_CONFIG.local_ip.get().ipv6_bytes();
        bytes[bytes.len() - 3] += 1;
        bytes[bytes.len() - 1] = last;
        SpecifiedAddr::new(Ipv6Addr::from(bytes)).unwrap()
    }

    fn get_multicast_addr(last: u8) -> MulticastAddr<Self::Addr> {
        assert!((Self::Addr::BYTES * 8 - Self::MULTICAST_SUBNET.prefix()) as u32 > u8::BITS);
        let mut bytes = Self::MULTICAST_SUBNET.network().ipv6_bytes();
        bytes[bytes.len() - 1] = last;
        MulticastAddr::new(Ipv6Addr::from_bytes(bytes)).unwrap()
    }
}

/// A configuration for a simple network.
///
/// `DummyEventDispatcherConfig` describes a simple network with two IP hosts
/// - one remote and one local - both on the same Ethernet network.
#[derive(Clone)]
pub(crate) struct DummyEventDispatcherConfig<A: IpAddress> {
    /// The subnet of the local Ethernet network.
    pub(crate) subnet: Subnet<A>,
    /// The IP address of our interface to the local network (must be in
    /// subnet).
    pub(crate) local_ip: SpecifiedAddr<A>,
    /// The MAC address of our interface to the local network.
    pub(crate) local_mac: UnicastAddr<Mac>,
    /// The remote host's IP address (must be in subnet if provided).
    pub(crate) remote_ip: SpecifiedAddr<A>,
    /// The remote host's MAC address.
    pub(crate) remote_mac: UnicastAddr<Mac>,
}

/// A `DummyEventDispatcherConfig` with reasonable values for an IPv4 network.
pub(crate) const DUMMY_CONFIG_V4: DummyEventDispatcherConfig<Ipv4Addr> = unsafe {
    DummyEventDispatcherConfig {
        subnet: Subnet::new_unchecked(Ipv4Addr::new([192, 168, 0, 0]), 16),
        local_ip: SpecifiedAddr::new_unchecked(Ipv4Addr::new([192, 168, 0, 1])),
        local_mac: UnicastAddr::new_unchecked(Mac::new([0, 1, 2, 3, 4, 5])),
        remote_ip: SpecifiedAddr::new_unchecked(Ipv4Addr::new([192, 168, 0, 2])),
        remote_mac: UnicastAddr::new_unchecked(Mac::new([6, 7, 8, 9, 10, 11])),
    }
};

/// A `DummyEventDispatcherConfig` with reasonable values for an IPv6 network.
pub(crate) const DUMMY_CONFIG_V6: DummyEventDispatcherConfig<Ipv6Addr> = unsafe {
    DummyEventDispatcherConfig {
        subnet: Subnet::new_unchecked(
            Ipv6Addr::from_bytes([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 192, 168, 0, 0]),
            112,
        ),
        local_ip: SpecifiedAddr::new_unchecked(Ipv6Addr::from_bytes([
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 192, 168, 0, 1,
        ])),
        local_mac: UnicastAddr::new_unchecked(Mac::new([0, 1, 2, 3, 4, 5])),
        remote_ip: SpecifiedAddr::new_unchecked(Ipv6Addr::from_bytes([
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 192, 168, 0, 2,
        ])),
        remote_mac: UnicastAddr::new_unchecked(Mac::new([6, 7, 8, 9, 10, 11])),
    }
};

impl<A: IpAddress> DummyEventDispatcherConfig<A> {
    /// Creates a copy of `self` with all the remote and local fields reversed.
    pub(crate) fn swap(&self) -> Self {
        Self {
            subnet: self.subnet,
            local_ip: self.remote_ip,
            local_mac: self.remote_mac,
            remote_ip: self.local_ip,
            remote_mac: self.local_mac,
        }
    }

    /// Shorthand for `DummyEventDispatcherBuilder::from_config(self)`.
    pub(crate) fn into_builder(self) -> DummyEventDispatcherBuilder {
        DummyEventDispatcherBuilder::from_config(self)
    }
}

/// A builder for `DummyEventDispatcher`s.
///
/// A `DummyEventDispatcherBuilder` is capable of storing the configuration of a
/// network stack including forwarding table entries, devices and their assigned
/// IP addresses, ARP table entries, etc. It can be built using `build`,
/// producing a `Context<DummyEventDispatcher>` with all of the appropriate
/// state configured.
#[derive(Clone, Default)]
pub(crate) struct DummyEventDispatcherBuilder {
    devices: Vec<(UnicastAddr<Mac>, Option<(IpAddr, SubnetEither)>)>,
    arp_table_entries: Vec<(usize, Ipv4Addr, UnicastAddr<Mac>)>,
    ndp_table_entries: Vec<(usize, UnicastAddr<Ipv6Addr>, UnicastAddr<Mac>)>,
    // usize refers to index into devices Vec.
    device_routes: Vec<(SubnetEither, usize)>,
    routes: Vec<(SubnetEither, SpecifiedAddr<IpAddr>)>,
}

impl DummyEventDispatcherBuilder {
    /// Construct a `DummyEventDispatcherBuilder` from a
    /// `DummyEventDispatcherConfig`.
    pub(crate) fn from_config<A: IpAddress>(
        cfg: DummyEventDispatcherConfig<A>,
    ) -> DummyEventDispatcherBuilder {
        assert!(cfg.subnet.contains(&cfg.local_ip));
        assert!(cfg.subnet.contains(&cfg.remote_ip));

        let mut builder = DummyEventDispatcherBuilder::default();
        builder.devices.push((cfg.local_mac, Some((cfg.local_ip.get().into(), cfg.subnet.into()))));

        match cfg.remote_ip.get().into() {
            IpAddr::V4(ip) => builder.arp_table_entries.push((0, ip, cfg.remote_mac)),
            IpAddr::V6(ip) => {
                builder.ndp_table_entries.push((0, UnicastAddr::new(ip).unwrap(), cfg.remote_mac))
            }
        };

        // Even with fixed ipv4 address we can have IPv6 link local addresses
        // pre-cached.
        builder.ndp_table_entries.push((
            0,
            cfg.remote_mac.to_ipv6_link_local().addr().get(),
            cfg.remote_mac,
        ));

        builder.device_routes.push((cfg.subnet.into(), 0));
        builder
    }

    /// Add a device.
    ///
    /// `add_device` returns a key which can be used to refer to the device in
    /// future calls to `add_arp_table_entry` and `add_device_route`.
    pub(crate) fn add_device(&mut self, mac: UnicastAddr<Mac>) -> usize {
        let idx = self.devices.len();
        self.devices.push((mac, None));
        idx
    }

    /// Add a device with an associated IP address.
    ///
    /// `add_device_with_ip` is like `add_device`, except that it takes an
    /// associated IP address and subnet to assign to the device.
    pub(crate) fn add_device_with_ip<A: IpAddress>(
        &mut self,
        mac: UnicastAddr<Mac>,
        ip: A,
        subnet: Subnet<A>,
    ) {
        let idx = self.devices.len();
        self.devices.push((mac, Some((ip.into(), subnet.into()))));
        self.device_routes.push((subnet.into(), idx));
    }

    /// Add an ARP table entry for a device's ARP table.
    pub(crate) fn add_arp_table_entry(
        &mut self,
        device: usize,
        ip: Ipv4Addr,
        mac: UnicastAddr<Mac>,
    ) {
        self.arp_table_entries.push((device, ip, mac));
    }

    /// Add an NDP table entry for a device's NDP table.
    pub(crate) fn add_ndp_table_entry(
        &mut self,
        device: usize,
        ip: UnicastAddr<Ipv6Addr>,
        mac: UnicastAddr<Mac>,
    ) {
        self.ndp_table_entries.push((device, ip, mac));
    }

    /// Builds a `Ctx` from the present configuration with a default dispatcher.
    pub(crate) fn build(self) -> DummyCtx {
        self.build_with_modifications(|_| {})
    }

    /// `build_with_modifications` is equivalent to `build`, except that after
    /// the `StackStateBuilder` is initialized, it is passed to `f` for further
    /// modification before the `Ctx` is constructed.
    pub(crate) fn build_with_modifications<F: FnOnce(&mut StackStateBuilder)>(
        self,
        f: F,
    ) -> DummyCtx {
        let mut stack_builder = StackStateBuilder::default();
        f(&mut stack_builder);
        self.build_with(stack_builder, Default::default(), Default::default())
    }

    /// Build a `Ctx` from the present configuration with a caller-provided
    /// dispatcher and `StackStateBuilder`.
    pub(crate) fn build_with<
        D: EventDispatcher,
        C: BlanketCoreContext,
        NonSyncCtx: NonSyncContext + Default,
    >(
        self,
        state_builder: StackStateBuilder,
        dispatcher: D,
        ctx: C,
    ) -> Ctx<D, C, NonSyncCtx> {
        let mut ctx = Ctx::new(state_builder.build(), dispatcher, ctx);
        let Ctx { sync_ctx, non_sync_ctx } = &mut ctx;

        let DummyEventDispatcherBuilder {
            devices,
            arp_table_entries,
            ndp_table_entries,
            device_routes,
            routes,
        } = self;
        let idx_to_device_id: HashMap<_, _> = devices
            .into_iter()
            .enumerate()
            .map(|(idx, (mac, ip_subnet))| {
                let id = crate::add_ethernet_device(
                    sync_ctx,
                    non_sync_ctx,
                    mac,
                    Ipv6::MINIMUM_LINK_MTU.into(),
                );
                crate::device::testutil::enable_device(sync_ctx, non_sync_ctx, id);
                match ip_subnet {
                    Some((IpAddr::V4(ip), SubnetEither::V4(subnet))) => {
                        let addr_sub = AddrSubnet::new(ip, subnet.prefix()).unwrap();
                        crate::device::add_ip_addr_subnet(sync_ctx, non_sync_ctx, id, addr_sub)
                            .unwrap();
                    }
                    Some((IpAddr::V6(ip), SubnetEither::V6(subnet))) => {
                        let addr_sub = AddrSubnet::new(ip, subnet.prefix()).unwrap();
                        crate::device::add_ip_addr_subnet(sync_ctx, non_sync_ctx, id, addr_sub)
                            .unwrap();
                    }
                    None => {}
                    _ => unreachable!(),
                }
                (idx, id)
            })
            .collect();
        for (idx, ip, mac) in arp_table_entries {
            let device = *idx_to_device_id.get(&idx).unwrap();
            crate::device::insert_static_arp_table_entry(sync_ctx, non_sync_ctx, device, ip, mac)
                .expect("error inserting static ARP entry");
        }
        for (idx, ip, mac) in ndp_table_entries {
            let device = *idx_to_device_id.get(&idx).unwrap();
            crate::device::insert_ndp_table_entry(sync_ctx, non_sync_ctx, device, ip, mac.get())
                .expect("error inserting static NDP entry");
        }
        for (subnet, idx) in device_routes {
            let device = *idx_to_device_id.get(&idx).unwrap();
            crate::add_route(
                sync_ctx,
                non_sync_ctx,
                AddableEntryEither::new(subnet, Some(device), None)
                    .expect("valid forwarding table entry"),
            )
            .expect("add device route");
        }
        for (subnet, next_hop) in routes {
            crate::add_route(
                sync_ctx,
                non_sync_ctx,
                AddableEntryEither::new(subnet, None, Some(next_hop.into()))
                    .expect("valid forwarding table entry"),
            )
            .expect("add remote route");
        }

        ctx
    }
}

/// Add either an NDP entry (if IPv6) or ARP entry (if IPv4) to a
/// `DummyEventDispatcherBuilder`.
pub(crate) fn add_arp_or_ndp_table_entry<A: IpAddress>(
    builder: &mut DummyEventDispatcherBuilder,
    device: usize,
    ip: A,
    mac: UnicastAddr<Mac>,
) {
    match ip.into() {
        IpAddr::V4(ip) => builder.add_arp_table_entry(device, ip, mac),
        IpAddr::V6(ip) => builder.add_ndp_table_entry(device, UnicastAddr::new(ip).unwrap(), mac),
    }
}

/// A dummy `EventDispatcher` used for testing.
///
/// A `DummyEventDispatcher` implements the `EventDispatcher` interface for
/// testing purposes. It provides facilities to inspect the history of what
/// events have been emitted to the system.
#[derive(Default)]
pub(crate) struct DummyEventDispatcher {
    events: DummyEventCtx<DispatchedEvent>,
    frames: DummyFrameCtx<DeviceId>,
    icmpv4_replies: HashMap<IcmpConnId<Ipv4>, Vec<(u16, Vec<u8>)>>,
    icmpv6_replies: HashMap<IcmpConnId<Ipv6>, Vec<(u16, Vec<u8>)>>,
}

impl AsMut<DummyEventCtx<DispatchedEvent>> for DummySyncCtx {
    fn as_mut(&mut self) -> &mut DummyEventCtx<DispatchedEvent> {
        &mut self.dispatcher.events
    }
}

impl AsRef<DummyTimerCtx<TimerId>> for DummySyncCtx {
    fn as_ref(&self) -> &DummyTimerCtx<TimerId> {
        self.ctx.timer_ctx()
    }
}

impl AsMut<DummyTimerCtx<TimerId>> for DummySyncCtx {
    fn as_mut(&mut self) -> &mut DummyTimerCtx<TimerId> {
        self.ctx.timer_ctx_mut()
    }
}

impl AsMut<DummyEventCtx<DispatchedEvent>> for DummyCtx {
    fn as_mut(&mut self) -> &mut DummyEventCtx<DispatchedEvent> {
        self.sync_ctx.as_mut()
    }
}

impl AsRef<DummyTimerCtx<TimerId>> for DummyCtx {
    fn as_ref(&self) -> &DummyTimerCtx<TimerId> {
        self.sync_ctx.as_ref()
    }
}

impl AsMut<DummyTimerCtx<TimerId>> for DummyCtx {
    fn as_mut(&mut self) -> &mut DummyTimerCtx<TimerId> {
        self.sync_ctx.as_mut()
    }
}

impl AsMut<DummyFrameCtx<DeviceId>> for DummyCtx {
    fn as_mut(&mut self) -> &mut DummyFrameCtx<DeviceId> {
        &mut self.sync_ctx.dispatcher.frames
    }
}

impl DummyNetworkContext for DummyCtx {
    type TimerId = TimerId;
    type SendMeta = DeviceId;
}

pub(crate) trait TestutilIpExt: Ip {
    fn icmp_replies(
        evt: &mut DummyEventDispatcher,
    ) -> &mut HashMap<IcmpConnId<Self>, Vec<(u16, Vec<u8>)>>;
}

impl TestutilIpExt for Ipv4 {
    fn icmp_replies(
        evt: &mut DummyEventDispatcher,
    ) -> &mut HashMap<IcmpConnId<Ipv4>, Vec<(u16, Vec<u8>)>> {
        &mut evt.icmpv4_replies
    }
}

impl TestutilIpExt for Ipv6 {
    fn icmp_replies(
        evt: &mut DummyEventDispatcher,
    ) -> &mut HashMap<IcmpConnId<Ipv6>, Vec<(u16, Vec<u8>)>> {
        &mut evt.icmpv6_replies
    }
}

impl DummyEventDispatcher {
    pub(crate) fn take_frames(&mut self) -> Vec<(DeviceId, Vec<u8>)> {
        self.frames.take_frames()
    }

    pub(crate) fn frames_sent(&self) -> &[(DeviceId, Vec<u8>)] {
        self.frames.frames()
    }

    /// Takes all the received ICMP replies for a given `conn`.
    pub(crate) fn take_icmp_replies<I: TestutilIpExt>(
        &mut self,
        conn: IcmpConnId<I>,
    ) -> Vec<(u16, Vec<u8>)> {
        I::icmp_replies(self).remove(&conn).unwrap_or_else(Vec::default)
    }
}

impl<I: IcmpIpExt> UdpContext<I> for DummyEventDispatcher {}

impl<I: crate::ip::IpExt, B: BufferMut> BufferUdpContext<I, B> for DummyEventDispatcher {}

impl<I: IcmpIpExt> IcmpContext<I> for DummyEventDispatcher {
    fn receive_icmp_error(&mut self, _conn: IcmpConnId<I>, _seq_num: u16, _err: I::ErrorCode) {
        unimplemented!()
    }
}

impl<B: BufferMut> BufferIcmpContext<Ipv4, B> for DummyEventDispatcher {
    fn receive_icmp_echo_reply(
        &mut self,
        conn: IcmpConnId<Ipv4>,
        _src_ip: Ipv4Addr,
        _dst_ip: Ipv4Addr,
        _id: u16,
        seq_num: u16,
        data: B,
    ) {
        let replies = self.icmpv4_replies.entry(conn).or_insert_with(Vec::default);
        replies.push((seq_num, data.as_ref().to_owned()))
    }
}

impl<B: BufferMut> BufferIcmpContext<Ipv6, B> for DummyEventDispatcher {
    fn receive_icmp_echo_reply(
        &mut self,
        conn: IcmpConnId<Ipv6>,
        _src_ip: Ipv6Addr,
        _dst_ip: Ipv6Addr,
        _id: u16,
        seq_num: u16,
        data: B,
    ) {
        let replies = self.icmpv6_replies.entry(conn).or_insert_with(Vec::default);
        replies.push((seq_num, data.as_ref().to_owned()))
    }
}

impl<B: BufferMut> DeviceLayerEventDispatcher<B> for DummyEventDispatcher {
    fn send_frame<S: Serializer<Buffer = B>>(
        &mut self,
        device: DeviceId,
        frame: S,
    ) -> Result<(), S> {
        self.frames.send_frame(&mut (), device, frame)
    }
}

#[derive(Debug, Eq, PartialEq, Hash)]
pub(crate) enum DispatchedEvent {
    Ipv6RouteDiscovery(Ipv6RouteDiscoveryEvent<DeviceId>),
}

impl From<Ipv6RouteDiscoveryEvent<DeviceId>> for DispatchedEvent {
    fn from(e: Ipv6RouteDiscoveryEvent<DeviceId>) -> DispatchedEvent {
        DispatchedEvent::Ipv6RouteDiscovery(e)
    }
}

impl<I: Ip> EventContext<IpLayerEvent<DeviceId, I>> for DummyEventDispatcher {
    fn on_event(&mut self, _event: IpLayerEvent<DeviceId, I>) {}
}

impl<I: Ip> EventContext<IpDeviceEvent<DeviceId, I>> for DummyEventDispatcher {
    fn on_event(&mut self, _event: IpDeviceEvent<DeviceId, I>) {}
}

impl EventContext<DadEvent<DeviceId>> for DummyEventDispatcher {
    fn on_event(&mut self, _event: DadEvent<DeviceId>) {}
}

impl EventContext<Ipv6RouteDiscoveryEvent<DeviceId>> for DummyEventDispatcher {
    fn on_event(&mut self, event: Ipv6RouteDiscoveryEvent<DeviceId>) {
        self.events.on_event(DispatchedEvent::from(event))
    }
}

pub(crate) fn handle_timer(
    DummyCtx { sync_ctx, non_sync_ctx }: &mut DummyCtx,
    _ctx: &mut (),
    id: TimerId,
) {
    crate::handle_timer(sync_ctx, non_sync_ctx, id)
}

#[cfg(test)]
mod tests {
    use packet::{Buf, Serializer};
    use packet_formats::{
        icmp::{IcmpEchoRequest, IcmpPacketBuilder, IcmpUnusedCode},
        ip::Ipv4Proto,
    };
    use specialize_ip_macro::{ip_test, specialize_ip_address};

    use super::*;
    use crate::{
        context::testutil::{DummyNetwork, DummyNetworkLinks},
        device::testutil::receive_frame_or_panic,
        ip::socket::BufferIpSocketHandler,
        TimerIdInner,
    };

    #[test]
    fn test_dummy_network_transmits_packets() {
        set_logger_for_test();
        let mut net = crate::context::testutil::new_legacy_simple_dummy_network(
            "alice",
            DUMMY_CONFIG_V4.into_builder().build(),
            "bob",
            DUMMY_CONFIG_V4.swap().into_builder().build(),
        );

        // Alice sends Bob a ping.

        BufferIpSocketHandler::<Ipv4, _, _>::send_oneshot_ip_packet(
            net.sync_ctx("alice"),
            &mut (),
            None, // device
            None, // local_ip
            DUMMY_CONFIG_V4.remote_ip,
            Ipv4Proto::Icmp,
            None, // builder
            |_| {
                let req = IcmpEchoRequest::new(0, 0);
                let req_body = &[1, 2, 3, 4];
                Buf::new(req_body.to_vec(), ..).encapsulate(
                    IcmpPacketBuilder::<Ipv4, &[u8], _>::new(
                        DUMMY_CONFIG_V4.local_ip,
                        DUMMY_CONFIG_V4.remote_ip,
                        IcmpUnusedCode,
                        req,
                    ),
                )
            },
            None,
        )
        .unwrap();

        // Send from Alice to Bob.
        assert_eq!(net.step(receive_frame_or_panic, handle_timer).frames_sent, 1);
        // Respond from Bob to Alice.
        assert_eq!(net.step(receive_frame_or_panic, handle_timer).frames_sent, 1);
        // Should've starved all events.
        assert!(net.step(receive_frame_or_panic, handle_timer).is_idle());
    }

    #[test]
    fn test_dummy_network_timers() {
        set_logger_for_test();
        let mut net = crate::context::testutil::new_legacy_simple_dummy_network(
            1,
            DUMMY_CONFIG_V4.into_builder().build(),
            2,
            DUMMY_CONFIG_V4.swap().into_builder().build(),
        );

        assert_eq!(
            net.sync_ctx(1)
                .ctx
                .schedule_timer(Duration::from_secs(1), TimerId(TimerIdInner::Nop(1))),
            None
        );
        assert_eq!(
            net.sync_ctx(2)
                .ctx
                .schedule_timer(Duration::from_secs(2), TimerId(TimerIdInner::Nop(2))),
            None
        );
        assert_eq!(
            net.sync_ctx(2)
                .ctx
                .schedule_timer(Duration::from_secs(3), TimerId(TimerIdInner::Nop(3))),
            None
        );
        assert_eq!(
            net.sync_ctx(1)
                .ctx
                .schedule_timer(Duration::from_secs(4), TimerId(TimerIdInner::Nop(4))),
            None
        );
        assert_eq!(
            net.sync_ctx(1)
                .ctx
                .schedule_timer(Duration::from_secs(5), TimerId(TimerIdInner::Nop(5))),
            None
        );
        assert_eq!(
            net.sync_ctx(2)
                .ctx
                .schedule_timer(Duration::from_secs(5), TimerId(TimerIdInner::Nop(6))),
            None
        );

        // No timers fired before.
        assert_eq!(get_counter_val(net.sync_ctx(1), "timer::nop"), 0);
        assert_eq!(get_counter_val(net.sync_ctx(2), "timer::nop"), 0);
        assert_eq!(net.step(receive_frame_or_panic, handle_timer).timers_fired, 1);
        // Only timer in context 1 should have fired.
        assert_eq!(get_counter_val(net.sync_ctx(1), "timer::nop"), 1);
        assert_eq!(get_counter_val(net.sync_ctx(2), "timer::nop"), 0);
        assert_eq!(net.step(receive_frame_or_panic, handle_timer).timers_fired, 1);
        // Only timer in context 2 should have fired.
        assert_eq!(get_counter_val(net.sync_ctx(1), "timer::nop"), 1);
        assert_eq!(get_counter_val(net.sync_ctx(2), "timer::nop"), 1);
        assert_eq!(net.step(receive_frame_or_panic, handle_timer).timers_fired, 1);
        // Only timer in context 2 should have fired.
        assert_eq!(get_counter_val(net.sync_ctx(1), "timer::nop"), 1);
        assert_eq!(get_counter_val(net.sync_ctx(2), "timer::nop"), 2);
        assert_eq!(net.step(receive_frame_or_panic, handle_timer).timers_fired, 1);
        // Only timer in context 1 should have fired.
        assert_eq!(get_counter_val(net.sync_ctx(1), "timer::nop"), 2);
        assert_eq!(get_counter_val(net.sync_ctx(2), "timer::nop"), 2);
        assert_eq!(net.step(receive_frame_or_panic, handle_timer).timers_fired, 2);
        // Both timers have fired at the same time.
        assert_eq!(get_counter_val(net.sync_ctx(1), "timer::nop"), 3);
        assert_eq!(get_counter_val(net.sync_ctx(2), "timer::nop"), 3);

        assert!(net.step(receive_frame_or_panic, handle_timer).is_idle());
        // Check that current time on contexts tick together.
        let t1 = net.sync_ctx(1).now();
        let t2 = net.sync_ctx(2).now();
        assert_eq!(t1, t2);
    }

    #[test]
    fn test_dummy_network_until_idle() {
        set_logger_for_test();
        let mut net = crate::context::testutil::new_legacy_simple_dummy_network(
            1,
            DUMMY_CONFIG_V4.into_builder().build(),
            2,
            DUMMY_CONFIG_V4.swap().into_builder().build(),
        );
        assert_eq!(
            net.sync_ctx(1)
                .ctx
                .schedule_timer(Duration::from_secs(1), TimerId(TimerIdInner::Nop(1))),
            None
        );
        assert_eq!(
            net.sync_ctx(2)
                .ctx
                .schedule_timer(Duration::from_secs(2), TimerId(TimerIdInner::Nop(2))),
            None
        );
        assert_eq!(
            net.sync_ctx(2)
                .ctx
                .schedule_timer(Duration::from_secs(3), TimerId(TimerIdInner::Nop(3))),
            None
        );

        while !net.step(receive_frame_or_panic, handle_timer).is_idle()
            && (get_counter_val(net.sync_ctx(1), "timer::nop") < 1
                || get_counter_val(net.sync_ctx(2), "timer::nop") < 1)
        {}
        // Assert that we stopped before all times were fired, meaning we can
        // step again.
        assert_eq!(net.step(receive_frame_or_panic, handle_timer).timers_fired, 1);
    }

    #[test]
    fn test_delayed_packets() {
        set_logger_for_test();
        // Create a network that takes 5ms to get any packet to go through.
        let latency = Duration::from_millis(5);
        let device_id = DeviceId::new_ethernet(0);
        let mut net = DummyNetwork::new(
            [
                ("alice", DUMMY_CONFIG_V4.into_builder().build()),
                ("bob", DUMMY_CONFIG_V4.swap().into_builder().build()),
            ],
            move |net: &'static str, _device_id: DeviceId| {
                if net == "alice" {
                    vec![("bob", device_id, Some(latency))]
                } else {
                    vec![("alice", device_id, Some(latency))]
                }
            },
        );

        // Alice sends Bob a ping.
        BufferIpSocketHandler::<Ipv4, _, _>::send_oneshot_ip_packet(
            net.sync_ctx("alice"),
            &mut (),
            None, // device
            None, // local_ip
            DUMMY_CONFIG_V4.remote_ip,
            Ipv4Proto::Icmp,
            None, // builder
            |_| {
                let req = IcmpEchoRequest::new(0, 0);
                let req_body = &[1, 2, 3, 4];
                Buf::new(req_body.to_vec(), ..).encapsulate(
                    IcmpPacketBuilder::<Ipv4, &[u8], _>::new(
                        DUMMY_CONFIG_V4.local_ip,
                        DUMMY_CONFIG_V4.remote_ip,
                        IcmpUnusedCode,
                        req,
                    ),
                )
            },
            None,
        )
        .unwrap();

        assert_eq!(
            net.sync_ctx("alice")
                .ctx
                .schedule_timer(Duration::from_millis(3), TimerId(TimerIdInner::Nop(1))),
            None
        );
        assert_eq!(
            net.sync_ctx("bob")
                .ctx
                .schedule_timer(Duration::from_millis(7), TimerId(TimerIdInner::Nop(2))),
            None
        );
        assert_eq!(
            net.sync_ctx("bob")
                .ctx
                .schedule_timer(Duration::from_millis(10), TimerId(TimerIdInner::Nop(1))),
            None
        );

        // Order of expected events is as follows:
        // - Alice's timer expires at t = 3
        // - Bob receives Alice's packet at t = 5
        // - Bob's timer expires at t = 7
        // - Alice receives Bob's response and Bob's last timer fires at t = 10

        fn assert_full_state<'a, L: DummyNetworkLinks<DeviceId, DeviceId, &'a str>>(
            net: &mut DummyNetwork<&'a str, DeviceId, DummyCtx, L>,
            alice_nop: usize,
            bob_nop: usize,
            bob_echo_request: usize,
            alice_echo_response: usize,
        ) {
            let alice = net.sync_ctx("alice");
            assert_eq!(get_counter_val(alice, "timer::nop"), alice_nop);
            assert_eq!(get_counter_val(alice, "<IcmpIpTransportContext as BufferIpTransportContext<Ipv4>>::receive_ip_packet::echo_reply"),
                alice_echo_response
            );

            let bob = net.sync_ctx("bob");
            assert_eq!(get_counter_val(bob, "timer::nop"), bob_nop);
            assert_eq!(get_counter_val(bob, "<IcmpIpTransportContext as BufferIpTransportContext<Ipv4>>::receive_ip_packet::echo_request"),
                bob_echo_request
            );
        }

        assert_eq!(net.step(receive_frame_or_panic, handle_timer).timers_fired, 1);
        assert_full_state(&mut net, 1, 0, 0, 0);
        assert_eq!(net.step(receive_frame_or_panic, handle_timer).frames_sent, 1);
        assert_full_state(&mut net, 1, 0, 1, 0);
        assert_eq!(net.step(receive_frame_or_panic, handle_timer).timers_fired, 1);
        assert_full_state(&mut net, 1, 1, 1, 0);
        let step = net.step(receive_frame_or_panic, handle_timer);
        assert_eq!(step.frames_sent, 1);
        assert_eq!(step.timers_fired, 1);
        assert_full_state(&mut net, 1, 2, 1, 1);

        // Should've starved all events.
        assert!(net.step(receive_frame_or_panic, handle_timer).is_idle());
    }

    #[ip_test]
    fn test_send_to_many<I: Ip + TestIpExt>() {
        #[specialize_ip_address]
        fn send_packet<A: IpAddress>(
            ctx: &mut DummySyncCtx,
            src_ip: SpecifiedAddr<A>,
            dst_ip: SpecifiedAddr<A>,
            device: DeviceId,
        ) {
            let meta = SendIpPacketMeta {
                device,
                src_ip: Some(src_ip),
                dst_ip,
                next_hop: dst_ip,
                proto: IpProto::Udp.into(),
                ttl: None,
                mtu: None,
            };
            #[ipv4addr]
            crate::ip::send_ipv4_packet_from_device(
                ctx,
                &mut (),
                meta,
                Buf::new(vec![1, 2, 3, 4], ..),
            )
            .unwrap();

            #[ipv6addr]
            crate::ip::send_ipv6_packet_from_device(
                ctx,
                &mut (),
                meta,
                Buf::new(vec![1, 2, 3, 4], ..),
            )
            .unwrap();
        }

        let device_builder_id = 0;
        let device = DeviceId::new_ethernet(device_builder_id);
        let mac_a = UnicastAddr::new(Mac::new([2, 3, 4, 5, 6, 7])).unwrap();
        let mac_b = UnicastAddr::new(Mac::new([2, 3, 4, 5, 6, 8])).unwrap();
        let mac_c = UnicastAddr::new(Mac::new([2, 3, 4, 5, 6, 9])).unwrap();
        let ip_a = I::get_other_ip_address(1);
        let ip_b = I::get_other_ip_address(2);
        let ip_c = I::get_other_ip_address(3);
        let subnet = Subnet::new(I::get_other_ip_address(0).get(), I::Addr::BYTES * 8 - 8).unwrap();
        let mut alice = DummyEventDispatcherBuilder::default();
        alice.add_device_with_ip(mac_a, ip_a.get(), subnet);
        let mut bob = DummyEventDispatcherBuilder::default();
        bob.add_device_with_ip(mac_b, ip_b.get(), subnet);
        let mut calvin = DummyEventDispatcherBuilder::default();
        calvin.add_device_with_ip(mac_c, ip_c.get(), subnet);
        add_arp_or_ndp_table_entry(&mut alice, device_builder_id, ip_b.get(), mac_b);
        add_arp_or_ndp_table_entry(&mut alice, device_builder_id, ip_c.get(), mac_c);
        add_arp_or_ndp_table_entry(&mut bob, device_builder_id, ip_a.get(), mac_a);
        add_arp_or_ndp_table_entry(&mut bob, device_builder_id, ip_c.get(), mac_c);
        add_arp_or_ndp_table_entry(&mut calvin, device_builder_id, ip_a.get(), mac_a);
        add_arp_or_ndp_table_entry(&mut calvin, device_builder_id, ip_b.get(), mac_b);
        let mut net = DummyNetwork::new(
            [("alice", alice.build()), ("bob", bob.build()), ("calvin", calvin.build())],
            move |net: &'static str, _device_id: DeviceId| match net {
                "alice" => vec![("bob", device, None), ("calvin", device, None)],
                "bob" => vec![("alice", device, None)],
                "calvin" => Vec::new(),
                _ => unreachable!(),
            },
        );

        net.collect_frames();
        assert_empty(net.sync_ctx("alice").dispatcher.frames_sent().iter());
        assert_empty(net.sync_ctx("bob").dispatcher.frames_sent().iter());
        assert_empty(net.sync_ctx("calvin").dispatcher.frames_sent().iter());
        assert_empty(net.iter_pending_frames());

        // Bob and Calvin should get any packet sent by Alice.

        send_packet(net.sync_ctx("alice"), ip_a, ip_b, device);
        assert_eq!(net.sync_ctx("alice").dispatcher.frames_sent().len(), 1);
        assert_empty(net.sync_ctx("bob").dispatcher.frames_sent().iter());
        assert_empty(net.sync_ctx("calvin").dispatcher.frames_sent().iter());
        assert_empty(net.iter_pending_frames());
        net.collect_frames();
        assert_empty(net.sync_ctx("alice").dispatcher.frames_sent().iter());
        assert_empty(net.sync_ctx("bob").dispatcher.frames_sent().iter());
        assert_empty(net.sync_ctx("calvin").dispatcher.frames_sent().iter());
        assert_eq!(net.iter_pending_frames().count(), 2);
        assert!(net
            .iter_pending_frames()
            .any(|InstantAndData(_, x)| (x.dst_context == "bob") && (x.meta == device)));
        assert!(net
            .iter_pending_frames()
            .any(|InstantAndData(_, x)| (x.dst_context == "calvin") && (x.meta == device)));

        // Only Alice should get packets sent by Bob.

        net.drop_pending_frames();
        send_packet(net.sync_ctx("bob"), ip_b, ip_a, device);
        assert_empty(net.sync_ctx("alice").dispatcher.frames_sent().iter());
        assert_eq!(net.sync_ctx("bob").dispatcher.frames_sent().len(), 1);
        assert_empty(net.sync_ctx("calvin").dispatcher.frames_sent().iter());
        assert_empty(net.iter_pending_frames());
        net.collect_frames();
        assert_empty(net.sync_ctx("alice").dispatcher.frames_sent().iter());
        assert_empty(net.sync_ctx("bob").dispatcher.frames_sent().iter());
        assert_empty(net.sync_ctx("calvin").dispatcher.frames_sent().iter());
        assert_eq!(net.iter_pending_frames().count(), 1);
        assert!(net
            .iter_pending_frames()
            .any(|InstantAndData(_, x)| (x.dst_context == "alice") && (x.meta == device)));

        // No one gets packets sent by Calvin.

        net.drop_pending_frames();
        send_packet(net.sync_ctx("calvin"), ip_c, ip_a, device);
        assert_empty(net.sync_ctx("alice").dispatcher.frames_sent().iter());
        assert_empty(net.sync_ctx("bob").dispatcher.frames_sent().iter());
        assert_eq!(net.sync_ctx("calvin").dispatcher.frames_sent().len(), 1);
        assert_empty(net.iter_pending_frames());
        net.collect_frames();
        assert_empty(net.sync_ctx("alice").dispatcher.frames_sent().iter());
        assert_empty(net.sync_ctx("bob").dispatcher.frames_sent().iter());
        assert_empty(net.sync_ctx("calvin").dispatcher.frames_sent().iter());
        assert_empty(net.iter_pending_frames());
    }
}
