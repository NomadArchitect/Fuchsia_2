// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg(test)]

use std::mem::size_of;

use fidl_fuchsia_net as net;
use fidl_fuchsia_net_stack as net_stack;
use fidl_fuchsia_netstack as netstack;
use fidl_fuchsia_netstack_ext::RouteTable;
use fidl_fuchsia_sys as sys;
use fuchsia_async::{self as fasync, DurationExt as _, TimeoutExt as _};
use fuchsia_component::client::{AppBuilder, ExitStatus};
use fuchsia_zircon as zx;
use test_case::test_case;

use anyhow::{self, Context};
use futures::{
    future, Future, FutureExt as _, StreamExt as _, TryFutureExt as _, TryStreamExt as _,
};
use net_types::ethernet::Mac;
use net_types::ip::{self as net_types_ip, Ip};
use net_types::{
    LinkLocalAddress as _, MulticastAddress as _, SpecifiedAddress as _, Witness as _,
};
use netstack_testing_common::constants::{eth as eth_consts, ipv6 as ipv6_consts};
use netstack_testing_common::environments::{KnownServices, Netstack, Netstack2};
use netstack_testing_common::{
    send_ra_with_router_lifetime, setup_network, setup_network_with, sleep, write_ndp_message,
    EthertapName, Result, ASYNC_EVENT_CHECK_INTERVAL, ASYNC_EVENT_NEGATIVE_CHECK_TIMEOUT,
    ASYNC_EVENT_POSITIVE_CHECK_TIMEOUT, NDP_MESSAGE_TTL,
};
use netstack_testing_macros::variants_test;
use packet::ParsablePacket as _;
use packet_formats::ethernet::{EtherType, EthernetFrame, EthernetFrameLengthCheck};
use packet_formats::icmp::mld::MldPacket;
use packet_formats::icmp::ndp::{
    options::{NdpOption, NdpOptionBuilder, PrefixInformation},
    NeighborAdvertisement, NeighborSolicitation, RouterAdvertisement, RouterSolicitation,
};
use packet_formats::icmp::{IcmpParseArgs, Icmpv6Packet};
use packet_formats::ip::Ipv6Proto;
use packet_formats::testutil::{parse_icmp_packet_in_ip_packet_in_ethernet_frame, parse_ip_packet};

/// The expected number of Router Solicitations sent by the netstack when an
/// interface is brought up as a host.
const EXPECTED_ROUTER_SOLICIATIONS: u8 = 3;

/// The expected interval between sending Router Solicitation messages when
/// soliciting IPv6 routers.
const EXPECTED_ROUTER_SOLICITATION_INTERVAL: zx::Duration = zx::Duration::from_seconds(4);

/// The expected number of Neighbor Solicitations sent by the netstack when
/// performing Duplicate Address Detection.
const EXPECTED_DUP_ADDR_DETECT_TRANSMITS: u8 = 1;

/// The expected interval between sending Neighbor Solicitation messages when
/// performing Duplicate Address Detection.
const EXPECTED_DAD_RETRANSMIT_TIMER: zx::Duration = zx::Duration::from_seconds(1);

/// As per [RFC 7217 section 6] Hosts SHOULD introduce a random delay between 0 and
/// `IDGEN_DELAY` before trying a new tentative address.
///
/// [RFC 7217]: https://tools.ietf.org/html/rfc7217#section-6
const DAD_IDGEN_DELAY: zx::Duration = zx::Duration::from_seconds(1);

/// Launches a new netstack with the endpoint and returns the IPv6 addresses
/// initially assigned to it.
///
/// If `run_netstack_and_get_ipv6_addrs_for_endpoint` returns successfully, it
/// is guaranteed that the launched netstack has been terminated. Note, if
/// `run_netstack_and_get_ipv6_addrs_for_endpoint` does not return successfully,
/// the launched netstack will still be terminated, but no guarantees are made
/// about when that will happen.
async fn run_netstack_and_get_ipv6_addrs_for_endpoint<N: Netstack>(
    endpoint: &netemul::TestEndpoint<'_>,
    launcher: &sys::LauncherProxy,
    name: String,
) -> Result<Vec<net::Subnet>> {
    // Launch the netstack service.

    let mut app = AppBuilder::new(N::VERSION.get_url())
        .spawn(launcher)
        .context("failed to spawn netstack")?;
    let netstack = app
        .connect_to_protocol::<netstack::NetstackMarker>()
        .context("failed to connect to netstack service")?;

    // Add the device and get its interface state from netstack.
    // TODO(fxbug.dev/48907) Support Network Device. This helper fn should use stack.fidl
    // and be agnostic over interface type.
    let id = netstack
        .add_ethernet_device(
            &name,
            &mut netstack::InterfaceConfig {
                name: name[..fidl_fuchsia_posix_socket::INTERFACE_NAME_LENGTH.into()].to_string(),
                filepath: "/fake/filepath/for_test".to_string(),
                metric: 0,
            },
            endpoint
                .get_ethernet()
                .await
                .context("add_ethernet_device requires an Ethernet endpoint")?,
        )
        .await
        .context("add_ethernet_device FIDL error")?
        .map_err(fuchsia_zircon::Status::from_raw)
        .context("add_ethernet_device error")?;
    let interface_state = app
        .connect_to_protocol::<fidl_fuchsia_net_interfaces::StateMarker>()
        .context("failed to connect to fuchsia.net.interfaces/State service")?;
    let mut state = fidl_fuchsia_net_interfaces_ext::InterfaceState::Unknown(id.into());
    let ipv6_addresses = fidl_fuchsia_net_interfaces_ext::wait_interface_with_id(
        fidl_fuchsia_net_interfaces_ext::event_stream_from_state(&interface_state)?,
        &mut state,
        |fidl_fuchsia_net_interfaces_ext::Properties {
             id: _,
             name: _,
             device_class: _,
             online: _,
             addresses,
             has_default_ipv4_route: _,
             has_default_ipv6_route: _,
         }| {
            Some(
                addresses
                    .iter()
                    .map(|fidl_fuchsia_net_interfaces_ext::Address { addr, valid_until: _ }| addr)
                    .filter(|fidl_fuchsia_net::Subnet { addr, prefix_len: _ }| match addr {
                        net::IpAddress::Ipv4(net::Ipv4Address { addr: _ }) => false,
                        net::IpAddress::Ipv6(net::Ipv6Address { addr: _ }) => true,
                    })
                    .copied()
                    .collect(),
            )
        },
    )
    .await
    .context("failed to observe interface addition")?;

    // Kill the netstack.
    //
    // Note, simply dropping `component_controller` would also kill the netstack
    // but we explicitly kill it and wait for the terminated event before
    // proceeding.
    let () = app.kill().context("failed to kill app")?;
    let _: ExitStatus = app.wait().await.context("failed to observe netstack termination")?;

    Ok(ipv6_addresses)
}

/// Test that across netstack runs, a device will initially be assigned the same
/// IPv6 addresses.
#[variants_test]
async fn consistent_initial_ipv6_addrs<E: netemul::Endpoint>(name: &str) {
    let sandbox = netemul::TestSandbox::new().expect("failed to create sandbox");
    let env = sandbox
        .create_environment(name, &[KnownServices::SecureStash])
        .expect("failed to create environment");
    let launcher = env.get_launcher().expect("failed to get launcher");
    let endpoint = sandbox
        .create_endpoint::<netemul::Ethernet, _>(name.ethertap_compatible_name())
        .await
        .expect("failed to create endpoint");

    // Make sure netstack uses the same addresses across runs for a device.
    let first_run_addrs = run_netstack_and_get_ipv6_addrs_for_endpoint::<Netstack2>(
        &endpoint,
        &launcher,
        name.to_string(),
    )
    .await
    .expect("error running netstack and getting addresses for the first time");
    let second_run_addrs = run_netstack_and_get_ipv6_addrs_for_endpoint::<Netstack2>(
        &endpoint,
        &launcher,
        name.to_string(),
    )
    .await
    .expect("error running netstack and getting addresses for the second time");
    assert_eq!(first_run_addrs, second_run_addrs);
}

/// Tests that `EXPECTED_ROUTER_SOLICIATIONS` Router Solicitation messages are transmitted
/// when the interface is brought up.
#[variants_test]
#[test_case("host", false ; "host")]
#[test_case("router", true ; "router")]
async fn sends_router_solicitations<E: netemul::Endpoint>(
    test_name: &str,
    sub_test_name: &str,
    forwarding: bool,
) {
    let name = format!("{}_{}", test_name, sub_test_name);
    let name = name.as_str();

    let sandbox = netemul::TestSandbox::new().expect("failed to create sandbox");
    let (_network, environment, _netstack, _iface, fake_ep) =
        setup_network::<E, _>(&sandbox, name).await.expect("error setting up network");

    if forwarding {
        let stack = environment
            .connect_to_service::<net_stack::StackMarker>()
            .expect("failed to get stack proxy");
        let () = stack.enable_ip_forwarding().await.expect("error enabling IP forwarding");
    }

    // Make sure exactly `EXPECTED_ROUTER_SOLICIATIONS` RS messages are transmitted
    // by the netstack.
    let mut observed_rs = 0;
    loop {
        // When we have already observed the expected number of RS messages, do a
        // negative check to make sure that we don't send anymore.
        let extra_timeout = if observed_rs == EXPECTED_ROUTER_SOLICIATIONS {
            ASYNC_EVENT_NEGATIVE_CHECK_TIMEOUT
        } else {
            ASYNC_EVENT_POSITIVE_CHECK_TIMEOUT
        };

        let ret = fake_ep
            .frame_stream()
            .try_filter_map(|(data, dropped)| {
                assert_eq!(dropped, 0);
                let mut observed_slls = Vec::new();
                future::ok(
                    parse_icmp_packet_in_ip_packet_in_ethernet_frame::<
                        net_types_ip::Ipv6,
                        _,
                        RouterSolicitation,
                        _,
                    >(&data, |p| {
                        for option in p.body().iter() {
                            if let NdpOption::SourceLinkLayerAddress(a) = option {
                                let mut mac_bytes = [0; 6];
                                mac_bytes.copy_from_slice(&a[..size_of::<Mac>()]);
                                observed_slls.push(Mac::new(mac_bytes));
                            } else {
                                // We should only ever have an NDP Source Link-Layer Address
                                // option in a RS.
                                panic!("unexpected option in RS = {:?}", option);
                            }
                        }
                    })
                    .map_or(
                        None,
                        |(_src_mac, dst_mac, src_ip, dst_ip, ttl, _message, _code)| {
                            Some((dst_mac, src_ip, dst_ip, ttl, observed_slls))
                        },
                    ),
                )
            })
            .try_next()
            .map(|r| r.context("error getting OnData event"))
            .on_timeout((EXPECTED_ROUTER_SOLICITATION_INTERVAL + extra_timeout).after_now(), || {
                // If we already observed `EXPECTED_ROUTER_SOLICIATIONS` RS, then we shouldn't
                // have gotten any more; the timeout is expected.
                if observed_rs == EXPECTED_ROUTER_SOLICIATIONS {
                    return Ok(None);
                }

                return Err(anyhow::anyhow!("timed out waiting for the {}-th RS", observed_rs));
            })
            .await
            .unwrap();

        let (dst_mac, src_ip, dst_ip, ttl, observed_slls) = match ret {
            Some((dst_mac, src_ip, dst_ip, ttl, observed_slls)) => {
                (dst_mac, src_ip, dst_ip, ttl, observed_slls)
            }
            None => break,
        };

        assert_eq!(
            dst_mac,
            Mac::from(&net_types_ip::Ipv6::ALL_ROUTERS_LINK_LOCAL_MULTICAST_ADDRESS)
        );

        // DAD should have resolved for the link local IPv6 address that is assigned to
        // the interface when it is first brought up. When a link local address is
        // assigned to the interface, it should be used for transmitted RS messages.
        if observed_rs > 0 {
            assert!(src_ip.is_specified())
        }

        assert_eq!(dst_ip, net_types_ip::Ipv6::ALL_ROUTERS_LINK_LOCAL_MULTICAST_ADDRESS.get());

        assert_eq!(ttl, NDP_MESSAGE_TTL);

        // The Router Solicitation should only ever have at max 1 source
        // link-layer option.
        assert!(observed_slls.len() <= 1);
        let observed_sll = observed_slls.into_iter().nth(0);
        if src_ip.is_specified() {
            if observed_sll.is_none() {
                panic!("expected source-link-layer address option if RS has a specified source IP address");
            }
        } else if observed_sll.is_some() {
            panic!("unexpected source-link-layer address option for RS with unspecified source IP address");
        }

        observed_rs += 1;
    }

    assert_eq!(observed_rs, EXPECTED_ROUTER_SOLICIATIONS);
}

/// Tests that both stable and temporary SLAAC addresses are generated for a SLAAC prefix.
#[variants_test]
#[test_case("host", false ; "host")]
#[test_case("router", true ; "router")]
async fn slaac_with_privacy_extensions<E: netemul::Endpoint>(
    test_name: &str,
    sub_test_name: &str,
    forwarding: bool,
) {
    let name = format!("{}_{}", test_name, sub_test_name);
    let name = name.as_str();
    let sandbox = netemul::TestSandbox::new().expect("failed to create sandbox");
    let (_network, environment, _netstack, iface, fake_ep) =
        setup_network::<E, _>(&sandbox, name).await.expect("error setting up network");

    if forwarding {
        let stack = environment
            .connect_to_service::<net_stack::StackMarker>()
            .expect("failed to get stack proxy");
        let () = stack.enable_ip_forwarding().await.expect("error enabling IP forwarding");
    }

    // Wait for a Router Solicitation.
    //
    // The first RS should be sent immediately.
    let () = fake_ep
        .frame_stream()
        .try_filter_map(|(data, dropped)| {
            assert_eq!(dropped, 0);
            future::ok(
                parse_icmp_packet_in_ip_packet_in_ethernet_frame::<
                    net_types_ip::Ipv6,
                    _,
                    RouterSolicitation,
                    _,
                >(&data, |_| {})
                .map_or(None, |_| Some(())),
            )
        })
        .try_next()
        .map(|r| r.context("error getting OnData event"))
        .on_timeout(ASYNC_EVENT_POSITIVE_CHECK_TIMEOUT.after_now(), || {
            Err(anyhow::anyhow!("timed out waiting for RS packet"))
        })
        .await
        .unwrap()
        .expect("failed to get next OnData event");

    // Send a Router Advertisement with information for a SLAAC prefix.
    let ra = RouterAdvertisement::new(
        0,     /* current_hop_limit */
        false, /* managed_flag */
        false, /* other_config_flag */
        0,     /* router_lifetime */
        0,     /* reachable_time */
        0,     /* retransmit_timer */
    );
    let pi = PrefixInformation::new(
        ipv6_consts::PREFIX.prefix(),  /* prefix_length */
        false,                         /* on_link_flag */
        true,                          /* autonomous_address_configuration_flag */
        99999,                         /* valid_lifetime */
        99999,                         /* preferred_lifetime */
        ipv6_consts::PREFIX.network(), /* prefix */
    );
    let options = [NdpOptionBuilder::PrefixInformation(pi)];
    let () = write_ndp_message::<&[u8], _>(
        eth_consts::MAC_ADDR,
        Mac::from(&net_types_ip::Ipv6::ALL_NODES_LINK_LOCAL_MULTICAST_ADDRESS),
        ipv6_consts::LINK_LOCAL_ADDR,
        net_types_ip::Ipv6::ALL_NODES_LINK_LOCAL_MULTICAST_ADDRESS.get(),
        ra,
        &options,
        &fake_ep,
    )
    .await
    .expect("failed to write NDP message");

    // Wait for the SLAAC addresses to be generated.
    //
    // We expect two addresses for the SLAAC prefixes to be assigned to the NIC as the
    // netstack should generate both a stable and temporary SLAAC address.
    let interface_state = environment
        .connect_to_service::<fidl_fuchsia_net_interfaces::StateMarker>()
        .expect("failed to connect to fuchsia.net.interfaces/State");
    let expected_addrs = 2;
    fidl_fuchsia_net_interfaces_ext::wait_interface_with_id(
        fidl_fuchsia_net_interfaces_ext::event_stream_from_state(&interface_state)
            .expect("error getting interface state event stream"),
        &mut fidl_fuchsia_net_interfaces_ext::InterfaceState::Unknown(iface.id()),
        |fidl_fuchsia_net_interfaces_ext::Properties { addresses, .. }| {
            if addresses
                .iter()
                .filter_map(
                    |&fidl_fuchsia_net_interfaces_ext::Address {
                         addr: fidl_fuchsia_net::Subnet { addr, prefix_len: _ },
                         valid_until: _,
                     }| {
                        match addr {
                            net::IpAddress::Ipv4(net::Ipv4Address { addr: _ }) => None,
                            net::IpAddress::Ipv6(net::Ipv6Address { addr }) => {
                                // TODO(https://github.com/rust-lang/rust/issues/80967): use bool::then_some.
                                ipv6_consts::PREFIX
                                    .contains(&net_types_ip::Ipv6Addr::new(addr))
                                    .then(|| ())
                            }
                        }
                    },
                )
                .count()
                == expected_addrs as usize
            {
                Some(())
            } else {
                None
            }
        },
    )
    .map_err(anyhow::Error::from)
    .on_timeout(
        (EXPECTED_DAD_RETRANSMIT_TIMER * EXPECTED_DUP_ADDR_DETECT_TRANSMITS * expected_addrs
            + ASYNC_EVENT_POSITIVE_CHECK_TIMEOUT)
            .after_now(),
        || Err(anyhow::anyhow!("timed out")),
    )
    .await
    .expect("failed to wait for SLAAC addresses to be generated")
}

/// Tests that if the netstack attempts to assign an address to an interface, and a remote node
/// is already assigned the address or attempts to assign the address at the same time, DAD
/// fails on the local interface.
///
/// If no remote node has any interest in an address the netstack is attempting to assign to
/// an interface, DAD should succeed.
#[variants_test]
async fn duplicate_address_detection<E: netemul::Endpoint>(name: &str) {
    /// Makes sure that `ipv6_consts::LINK_LOCAL_ADDR` is not assigned to the interface after the
    /// DAD resolution time.
    async fn check_address_failed_dad(iface: &netemul::TestInterface<'_>) {
        // Clocks sometimes jump in infrastructure, which can cause a timer to expire prematurely.
        // Fortunately such jumps are rarely seen in quick succession - if we repeatedly wait for
        // shorter durations we can be reasonably sure that the intended amount of time truly did
        // elapse. It is expected that at most one timer worth of time may be lost.
        const STEP: zx::Duration = zx::Duration::from_millis(10);
        let duration = EXPECTED_DAD_RETRANSMIT_TIMER * EXPECTED_DUP_ADDR_DETECT_TRANSMITS
            + ASYNC_EVENT_NEGATIVE_CHECK_TIMEOUT;
        let iterations =
            (duration + STEP - zx::Duration::from_nanos(1)).into_micros() / STEP.into_micros();
        for _ in 0..iterations {
            let () = fasync::Timer::new(fasync::Time::after(STEP)).await;
        }

        let addr = net::Subnet {
            addr: net::IpAddress::Ipv6(net::Ipv6Address {
                addr: ipv6_consts::LINK_LOCAL_ADDR.ipv6_bytes(),
            }),
            prefix_len: 64,
        };
        assert!(!iface
            .get_addrs()
            .await
            .expect("error getting interfacea addresses")
            .iter()
            .any(|a| a == &addr));
    }

    /// Transmits a Neighbor Solicitation message and expects `ipv6_consts::LINK_LOCAL_ADDR`
    /// to not be assigned to the interface after the normal resolution time for DAD.
    async fn fail_dad_with_ns(
        iface: &netemul::TestInterface<'_>,
        fake_ep: &netemul::TestFakeEndpoint<'_>,
    ) {
        let snmc = ipv6_consts::LINK_LOCAL_ADDR.to_solicited_node_address();
        let () = write_ndp_message::<&[u8], _>(
            eth_consts::MAC_ADDR,
            Mac::from(&snmc),
            net_types_ip::Ipv6::UNSPECIFIED_ADDRESS,
            snmc.get(),
            NeighborSolicitation::new(ipv6_consts::LINK_LOCAL_ADDR),
            &[],
            fake_ep,
        )
        .await
        .expect("failed to write NDP message");

        check_address_failed_dad(iface).await
    }

    /// Transmits a Neighbor Advertisement message and expects `ipv6_consts::LINK_LOCAL_ADDR`
    /// to not be assigned to the interface after the normal resolution time for DAD.
    async fn fail_dad_with_na(
        iface: &netemul::TestInterface<'_>,
        fake_ep: &netemul::TestFakeEndpoint<'_>,
    ) {
        let () = write_ndp_message::<&[u8], _>(
            eth_consts::MAC_ADDR,
            Mac::from(&net_types_ip::Ipv6::ALL_NODES_LINK_LOCAL_MULTICAST_ADDRESS),
            ipv6_consts::LINK_LOCAL_ADDR,
            net_types_ip::Ipv6::ALL_NODES_LINK_LOCAL_MULTICAST_ADDRESS.get(),
            NeighborAdvertisement::new(
                false, /* router_flag */
                false, /* solicited_flag */
                false, /* override_flag */
                ipv6_consts::LINK_LOCAL_ADDR,
            ),
            &[NdpOptionBuilder::TargetLinkLayerAddress(&eth_consts::MAC_ADDR.bytes())],
            fake_ep,
        )
        .await
        .expect("failed to write NDP message");

        check_address_failed_dad(iface).await
    }

    /// Adds `ipv6_consts::LINK_LOCAL_ADDR` to the interface and makes sure a Neighbor Solicitation
    /// message is transmitted by the netstack for DAD.
    ///
    /// Calls `fail_dad_fn` after the DAD message is observed so callers can simulate a remote
    /// node that has some interest in the same address.
    async fn add_address_for_dad<
        'a,
        'b: 'a,
        R: 'b + Future<Output = ()>,
        FN: FnOnce(&'b netemul::TestInterface<'a>, &'b netemul::TestFakeEndpoint<'a>) -> R,
    >(
        iface: &'b netemul::TestInterface<'a>,
        fake_ep: &'b netemul::TestFakeEndpoint<'a>,
        fail_dad_fn: FN,
    ) {
        let () = iface
            .add_ip_addr(net::Subnet {
                addr: net::IpAddress::Ipv6(net::Ipv6Address {
                    addr: ipv6_consts::LINK_LOCAL_ADDR.ipv6_bytes(),
                }),
                prefix_len: 64,
            })
            .await
            .expect("error adding IP address");

        // The first DAD message should be sent immediately.
        let ret = fake_ep
            .frame_stream()
            .try_filter_map(|(data, dropped)| {
                assert_eq!(dropped, 0);
                future::ok(
                    parse_icmp_packet_in_ip_packet_in_ethernet_frame::<
                        net_types_ip::Ipv6,
                        _,
                        NeighborSolicitation,
                        _,
                    >(&data, |p| assert_eq!(p.body().iter().count(), 0))
                    .map_or(None, |(_src_mac, dst_mac, src_ip, dst_ip, ttl, message, _code)| {
                        // If the NS is not for the address we just added, this is for some
                        // other address. We ignore it as it is not relevant to our test.
                        if message.target_address() != &ipv6_consts::LINK_LOCAL_ADDR {
                            return None;
                        }

                        Some((dst_mac, src_ip, dst_ip, ttl))
                    }),
                )
            })
            .try_next()
            .map(|r| r.context("error getting OnData event"))
            .on_timeout(ASYNC_EVENT_POSITIVE_CHECK_TIMEOUT.after_now(), || {
                Err(anyhow::anyhow!(
                    "timed out waiting for a neighbor solicitation targetting {}",
                    ipv6_consts::LINK_LOCAL_ADDR
                ))
            })
            .await
            .unwrap()
            .expect("failed to get next OnData event");

        let (dst_mac, src_ip, dst_ip, ttl) = ret;
        let expected_dst = ipv6_consts::LINK_LOCAL_ADDR.to_solicited_node_address();
        assert_eq!(src_ip, net_types_ip::Ipv6::UNSPECIFIED_ADDRESS);
        assert_eq!(dst_ip, expected_dst.get());
        assert_eq!(dst_mac, Mac::from(&expected_dst));
        assert_eq!(ttl, NDP_MESSAGE_TTL);

        fail_dad_fn(iface, fake_ep).await;
    }

    let sandbox = netemul::TestSandbox::new().expect("failed to create sandbox");
    let (_network, environment, _netstack, iface, fake_ep) =
        setup_network::<E, _>(&sandbox, name).await.expect("error setting up network");

    // Add an address and expect it to fail DAD because we simulate another node
    // performing DAD at the same time.
    let () = add_address_for_dad(&iface, &fake_ep, fail_dad_with_ns).await;

    // Add an address and expect it to fail DAD because we simulate another node
    // already owning the address.
    let () = add_address_for_dad(&iface, &fake_ep, fail_dad_with_na).await;

    // Add the address, and make sure it gets assigned.
    let () = add_address_for_dad(&iface, &fake_ep, |_, _| async {}).await;

    let interface_state = environment
        .connect_to_service::<fidl_fuchsia_net_interfaces::StateMarker>()
        .expect("failed to connect to fuchsia.net.interfaces/State");
    fidl_fuchsia_net_interfaces_ext::wait_interface_with_id(
        fidl_fuchsia_net_interfaces_ext::event_stream_from_state(&interface_state)
            .expect("error getting interfaces state event stream"),
        &mut fidl_fuchsia_net_interfaces_ext::InterfaceState::Unknown(iface.id()),
        |fidl_fuchsia_net_interfaces_ext::Properties { addresses, .. }| {
            addresses.iter().find_map(
                |&fidl_fuchsia_net_interfaces_ext::Address {
                     addr: fidl_fuchsia_net::Subnet { addr, prefix_len: _ },
                     valid_until: _,
                 }| {
                    match addr {
                        net::IpAddress::Ipv6(net::Ipv6Address { addr }) => {
                            if ipv6_consts::LINK_LOCAL_ADDR == net_types_ip::Ipv6Addr::new(addr) {
                                Some(())
                            } else {
                                None
                            }
                        }
                        net::IpAddress::Ipv4(_) => None,
                    }
                },
            )
        },
    )
    .map_err(anyhow::Error::from)
    .on_timeout(
        (EXPECTED_DAD_RETRANSMIT_TIMER * EXPECTED_DUP_ADDR_DETECT_TRANSMITS
            + ASYNC_EVENT_POSITIVE_CHECK_TIMEOUT)
            .after_now(),
        || Err(anyhow::anyhow!("timed out")),
    )
    .await
    .expect("error waiting for address to be assigned")
}

#[variants_test]
#[test_case("host", false ; "host")]
#[test_case("router", true ; "router")]
async fn router_and_prefix_discovery<E: netemul::Endpoint>(
    test_name: &str,
    sub_test_name: &str,
    forwarding: bool,
) {
    async fn check_route_table<P>(netstack: &netstack::NetstackProxy, pred: P)
    where
        P: Fn(&Vec<netstack::RouteTableEntry>) -> bool,
    {
        let check_attempts = ASYNC_EVENT_POSITIVE_CHECK_TIMEOUT.into_seconds()
            / ASYNC_EVENT_CHECK_INTERVAL.into_seconds();
        for attempt in 0..check_attempts {
            let () = sleep(ASYNC_EVENT_CHECK_INTERVAL.into_seconds()).await;
            let route_table = netstack.get_route_table().await.expect("failed to get route table");
            if pred(&route_table) {
                return;
            }

            let route_table =
                RouteTable::new(route_table).display().expect("failed to format route table");
            println!("route table at attempt={}:\n{}", attempt, route_table);
        }

        panic!(
            "timed out on waiting for a route table entry after {} seconds",
            ASYNC_EVENT_POSITIVE_CHECK_TIMEOUT.into_seconds(),
        )
    }

    let name = format!("{}_{}", test_name, sub_test_name);
    let name = name.as_str();

    let sandbox = netemul::TestSandbox::new().expect("failed to create sandbox");
    let (_network, environment, netstack, iface, fake_ep) =
        setup_network::<E, _>(&sandbox, name).await.expect("failed to setup network");

    if forwarding {
        let stack = environment
            .connect_to_service::<net_stack::StackMarker>()
            .expect("failed to get stack proxy");
        let () = stack.enable_ip_forwarding().await.expect("error enabling IP forwarding");
    }

    let pi = PrefixInformation::new(
        ipv6_consts::PREFIX.prefix(),  /* prefix_length */
        true,                          /* on_link_flag */
        false,                         /* autonomous_address_configuration_flag */
        1000,                          /* valid_lifetime */
        0,                             /* preferred_lifetime */
        ipv6_consts::PREFIX.network(), /* prefix */
    );
    let options = [NdpOptionBuilder::PrefixInformation(pi)];
    let () = send_ra_with_router_lifetime(&fake_ep, 1000, &options)
        .await
        .expect("failed to send router advertisement");

    // Test that the default router should be discovered after it is advertised.
    let () = check_route_table(&netstack, |route_table| {
        route_table.iter().any(
            |netstack::RouteTableEntry {
                 destination: net::Subnet { addr, prefix_len: _ },
                 gateway,
                 ..
             }| {
                (match addr {
                    net::IpAddress::Ipv4(net::Ipv4Address { addr: _ }) => false,
                    net::IpAddress::Ipv6(net::Ipv6Address { addr }) => {
                        net_types_ip::Ipv6Addr::new(*addr)
                            == net_types_ip::Ipv6::UNSPECIFIED_ADDRESS
                    }
                }) && (match gateway.as_deref() {
                    None | Some(net::IpAddress::Ipv4(net::Ipv4Address { addr: _ })) => false,
                    Some(net::IpAddress::Ipv6(net::Ipv6Address { addr })) => {
                        net_types_ip::Ipv6Addr::new(*addr) == ipv6_consts::LINK_LOCAL_ADDR
                    }
                })
            },
        )
    })
    .await;

    // Test that the prefix should be discovered after it is advertised.
    let () = check_route_table(&netstack, |route_table| {
        route_table.iter().any(
            |netstack::RouteTableEntry {
                 destination: net::Subnet { addr, prefix_len: _ },
                 nicid,
                 ..
             }| {
                if let net::IpAddress::Ipv6(net::Ipv6Address { addr }) = addr {
                    let destination = net_types_ip::Ipv6Addr::new(*addr);
                    if destination == ipv6_consts::PREFIX.network()
                        && u64::from(*nicid) == iface.id()
                    {
                        return true;
                    }
                }
                false
            },
        )
    })
    .await;
}

#[variants_test]
async fn slaac_regeneration_after_dad_failure<E: netemul::Endpoint>(name: &str) {
    // Expects an NS message for DAD within timeout and returns the target address of the message.
    async fn expect_ns_message_in(
        fake_ep: &netemul::TestFakeEndpoint<'_>,
        timeout: zx::Duration,
    ) -> net_types_ip::Ipv6Addr {
        fake_ep
            .frame_stream()
            .try_filter_map(|(data, dropped)| {
                assert_eq!(dropped, 0);
                future::ok(
                    parse_icmp_packet_in_ip_packet_in_ethernet_frame::<
                        net_types_ip::Ipv6,
                        _,
                        NeighborSolicitation,
                        _,
                    >(&data, |p| assert_eq!(p.body().iter().count(), 0))
                    .map_or(None, |(_src_mac, _dst_mac, _src_ip, _dst_ip, _ttl, message, _code)| {
                        // If the NS target_address does not have the prefix we have advertised,
                        // this is for some other address. We ignore it as it is not relevant to
                        // our test.
                        if !ipv6_consts::PREFIX.contains(message.target_address()) {
                            return None;
                        }

                        Some(*message.target_address())
                    }),
                )
            })
            .try_next()
            .map(|r| r.context("error getting OnData event"))
            .on_timeout(timeout.after_now(), || {
                Err(anyhow::anyhow!(
                    "timed out waiting for a neighbor solicitation targetting address of prefix: {}",
                    ipv6_consts::PREFIX,
                ))
            })
            .await.unwrap().expect("failed to get next OnData event")
    }

    let sandbox = netemul::TestSandbox::new().expect("failed to create sandbox");
    let (_network, environment, _netstack, iface, fake_ep) =
        setup_network_with::<E, _, _>(&sandbox, name, &[KnownServices::SecureStash])
            .await
            .expect("error setting up network");

    // Send a Router Advertisement with information for a SLAAC prefix.
    let ra = RouterAdvertisement::new(
        0,     /* current_hop_limit */
        false, /* managed_flag */
        false, /* other_config_flag */
        0,     /* router_lifetime */
        0,     /* reachable_time */
        0,     /* retransmit_timer */
    );
    let pi = PrefixInformation::new(
        ipv6_consts::PREFIX.prefix(),  /* prefix_length */
        false,                         /* on_link_flag */
        true,                          /* autonomous_address_configuration_flag */
        99999,                         /* valid_lifetime */
        99999,                         /* preferred_lifetime */
        ipv6_consts::PREFIX.network(), /* prefix */
    );
    let options = [NdpOptionBuilder::PrefixInformation(pi)];
    let () = write_ndp_message::<&[u8], _>(
        eth_consts::MAC_ADDR,
        Mac::from(&net_types_ip::Ipv6::ALL_NODES_LINK_LOCAL_MULTICAST_ADDRESS),
        ipv6_consts::LINK_LOCAL_ADDR,
        net_types_ip::Ipv6::ALL_NODES_LINK_LOCAL_MULTICAST_ADDRESS.get(),
        ra,
        &options,
        &fake_ep,
    )
    .await
    .expect("failed to write RA message");

    let tried_address = expect_ns_message_in(&fake_ep, ASYNC_EVENT_POSITIVE_CHECK_TIMEOUT).await;

    // We pretend there is a duplicate address situation.
    let snmc = tried_address.to_solicited_node_address();
    let () = write_ndp_message::<&[u8], _>(
        eth_consts::MAC_ADDR,
        Mac::from(&snmc),
        net_types_ip::Ipv6::UNSPECIFIED_ADDRESS,
        snmc.get(),
        NeighborSolicitation::new(tried_address),
        &[],
        &fake_ep,
    )
    .await
    .expect("failed to write DAD message");

    let target_address =
        expect_ns_message_in(&fake_ep, DAD_IDGEN_DELAY + ASYNC_EVENT_POSITIVE_CHECK_TIMEOUT).await;

    // We expect two addresses for the SLAAC prefixes to be assigned to the NIC as the
    // netstack should generate both a stable and temporary SLAAC address.
    let expected_addrs = 2;
    let interface_state = environment
        .connect_to_service::<fidl_fuchsia_net_interfaces::StateMarker>()
        .expect("failed to connect to fuchsia.net.interfaces/State");
    let () = fidl_fuchsia_net_interfaces_ext::wait_interface_with_id(
        fidl_fuchsia_net_interfaces_ext::event_stream_from_state(&interface_state).expect("error getting interfaces state event stream"),
        &mut fidl_fuchsia_net_interfaces_ext::InterfaceState::Unknown(iface.id()),
        |fidl_fuchsia_net_interfaces_ext::Properties { addresses, .. }| {
            // We have to make sure 2 things:
            // 1. We have `expected_addrs` addrs which have the advertised prefix for the
            // interface.
            // 2. The last tried address should be among the addresses for the interface.
            let (slaac_addrs, has_target_addr) = addresses.iter().fold(
                (0, false),
                |(mut slaac_addrs, mut has_target_addr), &fidl_fuchsia_net_interfaces_ext::Address { addr: fidl_fuchsia_net::Subnet { addr, prefix_len: _ }, valid_until: _ }| {
                    match addr {
                        net::IpAddress::Ipv6(net::Ipv6Address { addr }) => {
                            let configured_addr = net_types_ip::Ipv6Addr::new(addr);
                            assert!(configured_addr != tried_address,
                                "unexpected address ({}) assigned to the interface which previously failed DAD",
                                configured_addr
                            );
                            if ipv6_consts::PREFIX.contains(&configured_addr) {
                                slaac_addrs += 1;
                            }
                            if configured_addr == target_address {
                                has_target_addr = true;
                            }
                        }
                        net::IpAddress::Ipv4(_) => {}
                    }
                    (slaac_addrs, has_target_addr)
                },
            );

            assert!(
                slaac_addrs <= expected_addrs,
                "more addresses found than expected, found {}, expected {}",
                slaac_addrs,
                expected_addrs
            );
            if slaac_addrs == expected_addrs && has_target_addr {
                Some(())
            } else {
                None
            }
        },
    )
    .map_err(anyhow::Error::from)
    .on_timeout(
        (EXPECTED_DAD_RETRANSMIT_TIMER * EXPECTED_DUP_ADDR_DETECT_TRANSMITS * expected_addrs
            + ASYNC_EVENT_POSITIVE_CHECK_TIMEOUT)
            .after_now(),
        || Err(anyhow::anyhow!("timed out")),
    )
    .await
    .expect("failed to wait for SLAAC addresses");
}

#[variants_test]
async fn sends_mld_reports<E: netemul::Endpoint>(name: &str) {
    let sandbox = netemul::TestSandbox::new().expect("error creating sandbox");
    let (_network, _environment, _netstack, iface, fake_ep) =
        setup_network::<E, _>(&sandbox, name).await.expect("error setting up networking");

    // Add an address so we join the address's solicited node multicast group.
    let () = iface
        .add_ip_addr(net::Subnet {
            addr: net::IpAddress::Ipv6(net::Ipv6Address {
                addr: ipv6_consts::LINK_LOCAL_ADDR.ipv6_bytes(),
            }),
            prefix_len: 64,
        })
        .await
        .expect("error adding IP address");
    let snmc = ipv6_consts::LINK_LOCAL_ADDR.to_solicited_node_address();

    let stream = fake_ep
        .frame_stream()
        .map(|r| r.context("error getting OnData event"))
        .try_filter_map(|(data, dropped)| {
            async move {
                assert_eq!(dropped, 0);
                let mut data = &data[..];

                let eth = EthernetFrame::parse(&mut data, EthernetFrameLengthCheck::Check)
                    .context("error parsing ethernet frame")?;

                if eth.ethertype() != Some(EtherType::Ipv6) {
                    // Ignore non-IPv6 packets.
                    return Ok(None);
                }

                let (mut payload, src_ip, dst_ip, proto, ttl) =
                    parse_ip_packet::<net_types_ip::Ipv6>(&data)
                        .context("error parsing IPv6 packet")?;

                if proto != Ipv6Proto::Icmpv6 {
                    // Ignore non-ICMPv6 packets.
                    return Ok(None);
                }

                let icmp = Icmpv6Packet::parse(&mut payload, IcmpParseArgs::new(src_ip, dst_ip))
                    .context("error parsing ICMPv6 packet")?;

                let mld = if let Icmpv6Packet::Mld(mld) = icmp {
                    mld
                } else {
                    // Ignore non-MLD packets.
                    return Ok(None);
                };

                // As per RFC 3590 section 4,
                //
                //   MLD Report and Done messages are sent with a link-local address as
                //   the IPv6 source address, if a valid address is available on the
                //   interface. If a valid link-local address is not available (e.g., one
                //   has not been configured), the message is sent with the unspecified
                //   address (::) as the IPv6 source address.
                assert!(!src_ip.is_specified() || src_ip.is_linklocal(), "MLD messages must be sent from the unspecified or link local address; src_ip = {}", src_ip);

                assert!(dst_ip.is_multicast(), "all MLD messages must be sent to a multicast address; dst_ip = {}", dst_ip);

                // As per RFC 2710 section 3,
                //
                //   All MLD messages described in this document are sent with a
                //   link-local IPv6 Source Address, an IPv6 Hop Limit of 1, ...
                assert_eq!(ttl, 1, "MLD messages must have a hop limit of 1");

                let report = if let MldPacket::MulticastListenerReport(report) = mld {
                    report
                } else {
                    // Ignore non-report messages.
                    return Ok(None);
                };

                let group_addr = report.body().group_addr;
                assert!(group_addr.is_multicast(),  "MLD reports must only be sent for multicast addresses; group_addr = {}", group_addr);

                if group_addr != *snmc {
                    // We are only interested in the report for the solicited node
                    // multicast group we joined.
                    return Ok(None);
                }

                assert_eq!(dst_ip, group_addr, "the destination of an MLD report should be the multicast group the report is for");

                Ok(Some(()))
            }
        });
    futures::pin_mut!(stream);
    let () = stream
        .try_next()
        .on_timeout(ASYNC_EVENT_POSITIVE_CHECK_TIMEOUT.after_now(), || {
            return Err(anyhow::anyhow!("timed out waiting for the MLD report"));
        })
        .await
        .unwrap()
        .expect("error getting our expected MLD report");
}
