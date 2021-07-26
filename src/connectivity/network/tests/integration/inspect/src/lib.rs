// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg(test)]

use anyhow::Context as _;
use diagnostics_hierarchy::Property;
use fuchsia_async as fasync;
use fuchsia_inspect::testing::TreeAssertion;
use fuchsia_zircon as zx;
use futures::{FutureExt as _, StreamExt as _, TryStreamExt as _};
use itertools::Itertools as _;
use net_declare::{fidl_ip, fidl_mac, fidl_subnet};
use net_types::ip::{self as net_types_ip, Ip as _};
use netemul::Endpoint as _;
use netstack_testing_common::realms::{Netstack2, TestSandboxExt as _};
use netstack_testing_common::{constants, get_inspect_data, Result};
use netstack_testing_macros::variants_test;
use packet::Serializer as _;
use packet_formats::ethernet::{EtherType, EthernetFrameBuilder};
use packet_formats::ipv4::Ipv4PacketBuilder;
use packet_formats::udp::UdpPacketBuilder;
use std::collections::HashMap;
use std::num::NonZeroU16;
use test_case::test_case;

/// A helper type to provide address verification in inspect NIC data.
///
/// Address matcher implements `PropertyAssertion` in a stateful manner. It
/// expects all addresses in its internal set to be consumed as part of property
/// matching.
#[derive(Clone)]
struct AddressMatcher {
    set: std::rc::Rc<std::cell::RefCell<std::collections::HashSet<String>>>,
}

impl AddressMatcher {
    /// Creates an `AddressMatcher` from interface properties.
    fn new(props: &fidl_fuchsia_net_interfaces_ext::Properties) -> Self {
        let set = props
            .addresses
            .iter()
            .map(|&fidl_fuchsia_net_interfaces_ext::Address { addr: subnet, valid_until: _ }| {
                let fidl_fuchsia_net::Subnet { addr, prefix_len: _ } = subnet;
                let prefix = match addr {
                    fidl_fuchsia_net::IpAddress::Ipv4(_) => "ipv4",
                    fidl_fuchsia_net::IpAddress::Ipv6(_) => "ipv6",
                };
                format!("[{}] {}", prefix, fidl_fuchsia_net_ext::Subnet::from(subnet))
            })
            .collect::<std::collections::HashSet<_>>();

        Self { set: std::rc::Rc::new(std::cell::RefCell::new(set)) }
    }

    /// Checks that the internal set has been entirely consumed.
    ///
    /// Empties the internal set on return. Subsequent calls to check will
    /// always succeed.
    fn check(&self) -> Result<()> {
        let set = self.set.replace(Default::default());
        if set.is_empty() {
            Ok(())
        } else {
            Err(anyhow::anyhow!("unseen addresses left in set: {:?}", set))
        }
    }
}

impl std::ops::Drop for AddressMatcher {
    fn drop(&mut self) {
        // Always check for left over addresses on drop. Prevents the caller
        // from forgetting to do so.
        let () = self.check().expect("AddressMatcher was not emptied");
    }
}

impl fuchsia_inspect::testing::PropertyAssertion for AddressMatcher {
    fn run(&self, actual: &Property<String>) -> Result<()> {
        let actual = actual.string().ok_or_else(|| {
            anyhow::anyhow!("invalid property {:#?} for AddressMatcher, want String", actual)
        })?;
        if self.set.borrow_mut().remove(actual) {
            Ok(())
        } else {
            Err(anyhow::anyhow!("{} not in expected address set", actual))
        }
    }
}

#[fasync::run_singlethreaded(test)]
async fn inspect_nic() -> Result {
    // The number of IPv6 addresses that the stack will assign to an interface.
    const EXPECTED_NUM_IPV6_ADDRESSES: usize = 1;

    let sandbox = netemul::TestSandbox::new().context("failed to create sandbox")?;
    let network = sandbox.create_network("net").await.context("failed to create network")?;
    let realm = sandbox
        .create_netstack_realm::<Netstack2, _>("inspect_nic")
        .context("failed to create realm")?;

    const ETH_MAC: fidl_fuchsia_net::MacAddress = fidl_mac!("02:01:02:03:04:05");
    const NETDEV_MAC: fidl_fuchsia_net::MacAddress = fidl_mac!("02:0A:0B:0C:0D:0E");

    let eth = realm
        .join_network_with(
            &network,
            "eth-ep",
            netemul::Ethernet::make_config(netemul::DEFAULT_MTU, Some(ETH_MAC)),
            &netemul::InterfaceConfig::StaticIp(fidl_subnet!("192.168.0.1/24")),
        )
        .await
        .context("failed to join network with ethernet endpoint")?;
    let netdev = realm
        .join_network_with(
            &network,
            "netdev-ep",
            netemul::NetworkDevice::make_config(netemul::DEFAULT_MTU, Some(NETDEV_MAC)),
            &netemul::InterfaceConfig::StaticIp(fidl_subnet!("192.168.0.2/24")),
        )
        .await
        .context("failed to join network with netdevice endpoint")?;

    let interfaces_state = realm
        .connect_to_protocol::<fidl_fuchsia_net_interfaces::StateMarker>()
        .context("failed to connect to fuchsia.net.interfaces/State")?;

    // Wait for the world to stabilize and capture the state to verify inspect
    // data.
    let (loopback_props, netdev_props, eth_props) =
        fidl_fuchsia_net_interfaces_ext::wait_interface(
            fidl_fuchsia_net_interfaces_ext::event_stream_from_state(&interfaces_state)?,
            &mut std::collections::HashMap::new(),
            |if_map| {
                let loopback =
                    if_map.values().find_map(|properties| match properties.device_class {
                        fidl_fuchsia_net_interfaces::DeviceClass::Loopback(
                            fidl_fuchsia_net_interfaces::Empty {},
                        ) => Some(properties.clone()),
                        fidl_fuchsia_net_interfaces::DeviceClass::Device(_) => None,
                    })?;
                // Endpoint is up, has assigned IPv4 and at least the expected number of
                // IPv6 addresses.
                let get_properties = |id| {
                    let properties = if_map.get(&id)?;
                    let fidl_fuchsia_net_interfaces_ext::Properties { online, addresses, .. } =
                        properties;
                    if !online {
                        return None;
                    }
                    let (v4_count, v6_count) = addresses.iter().fold(
                        (0, 0),
                        |(v4_count, v6_count),
                         fidl_fuchsia_net_interfaces_ext::Address {
                             addr: fidl_fuchsia_net::Subnet { addr, prefix_len: _ },
                             valid_until: _,
                         }| match addr {
                            fidl_fuchsia_net::IpAddress::Ipv4(_) => (v4_count + 1, v6_count),
                            fidl_fuchsia_net::IpAddress::Ipv6(_) => (v4_count, v6_count + 1),
                        },
                    );
                    if v4_count > 0 && v6_count >= EXPECTED_NUM_IPV6_ADDRESSES {
                        Some(properties.clone())
                    } else {
                        None
                    }
                };
                Some((loopback, get_properties(netdev.id())?, get_properties(eth.id())?))
            },
        )
        .await
        .context("failed to wait for interfaces up and addresses configured")?;
    let loopback_addrs = AddressMatcher::new(&loopback_props);
    let netdev_addrs = AddressMatcher::new(&netdev_props);
    let eth_addrs = AddressMatcher::new(&eth_props);

    // Populate the neighbor table so we can verify inspection of its entries.
    const BOB_IP: fidl_fuchsia_net::IpAddress = fidl_ip!("192.168.0.1");
    const BOB_MAC: fidl_fuchsia_net::MacAddress = fidl_mac!("02:0A:0B:0C:0D:0E");
    let () = realm
        .connect_to_protocol::<fidl_fuchsia_net_neighbor::ControllerMarker>()
        .context("failed to connect to Controller")?
        .add_entry(eth.id(), &mut BOB_IP.clone(), &mut BOB_MAC.clone())
        .await
        .context("add_entry FIDL error")?
        .map_err(zx::Status::from_raw)
        .context("add_entry failed")?;

    let data = get_inspect_data(&realm, "netstack", "NICs", "interfaces")
        .await
        .context("get_inspect_data failed")?;
    // Debug print the tree to make debugging easier in case of failures.
    println!("Got inspect data: {:#?}", data);
    use fuchsia_inspect::testing::{AnyProperty, NonZeroUintProperty};
    fuchsia_inspect::assert_data_tree!(data, NICs: {
        loopback_props.id.to_string() => {
            Name: loopback_props.name,
            Loopback: "true",
            LinkOnline: "true",
            AdminUp: "true",
            Promiscuous: "false",
            Up: "true",
            MTU: 65536u64,
            NICID: loopback_props.id.to_string(),
            Running: "true",
            "DHCP enabled": "false",
            ProtocolAddress0: loopback_addrs.clone(),
            ProtocolAddress1: loopback_addrs.clone(),
            Stats: {
                DisabledRx: {
                    Bytes: 0u64,
                    Packets: 0u64,
                },
                Tx: {
                   Bytes: 0u64,
                   Packets: 0u64,
                },
                Rx: {
                    Bytes: 0u64,
                    Packets: 0u64,
                },
                Neighbor: {
                    UnreachableEntryLookups: 0u64,
                },
                MalformedL4RcvdPackets: 0u64,
                UnknownL3ProtocolRcvdPackets: 0u64,
                UnknownL4ProtocolRcvdPackets: 0u64,
            },
            "Network Endpoint Stats": {
                ARP: contains {},
                IPv4: contains {},
                IPv6: contains {},
            }
        },
        eth.id().to_string() => {
            Name: eth_props.name,
            Loopback: "false",
            LinkOnline: "true",
            AdminUp: "true",
            Promiscuous: "false",
            Up: "true",
            MTU: u64::from(netemul::DEFAULT_MTU),
            NICID: eth.id().to_string(),
            Running: "true",
            "DHCP enabled": "false",
            LinkAddress: fidl_fuchsia_net_ext::MacAddress::from(ETH_MAC).to_string(),
            // IPv4.
            ProtocolAddress0: eth_addrs.clone(),
            // Link-local IPv6.
            ProtocolAddress1: eth_addrs.clone(),
            Stats: {
                DisabledRx: {
                    Bytes: AnyProperty,
                    Packets: AnyProperty,
                },
                Tx: {
                   Bytes: AnyProperty,
                   Packets: AnyProperty,
                },
                Rx: {
                    Bytes: AnyProperty,
                    Packets: AnyProperty,
                },
                Neighbor: {
                    UnreachableEntryLookups: AnyProperty,
                },
                MalformedL4RcvdPackets: 0u64,
                UnknownL3ProtocolRcvdPackets: 0u64,
                UnknownL4ProtocolRcvdPackets: 0u64,
            },
            "Ethernet Info": {
                Filepath: "",
                Topopath: "eth-ep",
                Features: "Synthetic",
                TxDrops: AnyProperty,
                RxReads: contains {},
                RxWrites: contains {},
                TxReads: contains {},
                TxWrites: contains {}
            },
            Neighbors: {
                fidl_fuchsia_net_ext::IpAddress::from(BOB_IP).to_string() => {
                    "Link address": fidl_fuchsia_net_ext::MacAddress::from(BOB_MAC).to_string(),
                    State: "Static",
                    // TODO(fxbug.dev/64524): Use NonZeroIntProperty once we are
                    // able to distinguish between signed and unsigned integers
                    // from the fuchsia.diagnostics FIDL. This is currently not
                    // possible because the inspect data is serialized into JSON
                    // then converted back, losing type information.
                    "Last updated": NonZeroUintProperty,
                }
            },
            "Network Endpoint Stats": {
                ARP: contains {},
                IPv4: contains {},
                IPv6: contains {},
            }
        },
        netdev.id().to_string() => {
            Name: netdev_props.name,
            Loopback: "false",
            LinkOnline: "true",
            AdminUp: "true",
            Promiscuous: "false",
            Up: "true",
            MTU: u64::from(netemul::DEFAULT_MTU),
            NICID: netdev.id().to_string(),
            Running: "true",
            "DHCP enabled": "false",
            LinkAddress: fidl_fuchsia_net_ext::MacAddress::from(NETDEV_MAC).to_string(),
            // IPv4.
            ProtocolAddress0: netdev_addrs.clone(),
            // Link-local IPv6.
            ProtocolAddress1: netdev_addrs.clone(),
            Stats: {
                DisabledRx: {
                    Bytes: AnyProperty,
                    Packets: AnyProperty,
                },
                Tx: {
                   Bytes: AnyProperty,
                   Packets: AnyProperty,
                },
                Rx: {
                    Bytes: AnyProperty,
                    Packets: AnyProperty,
                },
                Neighbor: {
                    UnreachableEntryLookups: AnyProperty,
                },
                MalformedL4RcvdPackets: 0u64,
                UnknownL3ProtocolRcvdPackets: 0u64,
                UnknownL4ProtocolRcvdPackets: 0u64,
            },
            "Network Device Info": {
                TxDrops: AnyProperty,
                Class: "Unknown",
                RxReads: contains {},
                RxWrites: contains {},
                TxReads: contains {},
                TxWrites: contains {}
            },
            Neighbors: {},
            "Network Endpoint Stats": {
                ARP: contains {},
                IPv4: contains {},
                IPv6: contains {},
            }
        }
    });

    let () = loopback_addrs.check().context("loopback addresses match failed")?;
    let () = eth_addrs.check().context("ethernet addresses match failed")?;
    let () = netdev_addrs.check().context("netdev addresses match failed")?;

    Ok(())
}

#[fasync::run_singlethreaded(test)]
async fn inspect_routing_table() -> Result {
    let sandbox = netemul::TestSandbox::new().context("failed to create sandbox")?;
    let realm = sandbox
        .create_netstack_realm::<Netstack2, _>("inspect_routing_table")
        .context("failed to create realm")?;

    let netstack = realm
        .connect_to_protocol::<fidl_fuchsia_netstack::NetstackMarker>()
        .context("failed to connect to fuchsia.netstack/Netstack")?;

    // Capture the state of the routing table to verify the inspect data, and
    // confirm that it's not empty.
    let routing_table = netstack.get_route_table().await.context("get_route_table FIDL error")?;
    assert!(!routing_table.is_empty());
    println!("Got routing table: {:#?}", routing_table);

    use fuchsia_inspect::testing::{AnyProperty, TreeAssertion};
    let mut routing_table_assertion = TreeAssertion::new("Routes", true);
    for (i, route) in routing_table.into_iter().enumerate() {
        let index = &i.to_string();
        let fidl_fuchsia_netstack::RouteTableEntry { destination, gateway, nicid, metric } = route;
        let route_assertion = fuchsia_inspect::tree_assertion!(var index: {
            "Destination": format!(
                "{}",
                fidl_fuchsia_net_ext::Subnet::from(destination),
            ),
            "Gateway": match gateway {
                Some(addr) => fidl_fuchsia_net_ext::IpAddress::from(*addr).to_string(),
                None => "".to_string(),
            },
            "NIC": nicid.to_string(),
            "Metric": metric.to_string(),
            "MetricTracksInterface": AnyProperty,
            "Dynamic": AnyProperty,
            "Enabled": AnyProperty,
        });
        routing_table_assertion.add_child_assertion(route_assertion);
    }

    let data = get_inspect_data(&realm, "netstack", "Routes", "routes")
        .await
        .context("get_inspect_data failed")?;
    if let Err(e) = routing_table_assertion.run(&data) {
        panic!("tree assertion fails: {}, inspect data is: {:#?}", e, data);
    }

    Ok(())
}

struct PacketAttributes {
    ip_proto: packet_formats::ip::Ipv4Proto,
    port: NonZeroU16,
}

const INVALID_PORT: NonZeroU16 = unsafe { NonZeroU16::new_unchecked(1234) };

#[variants_test]
#[test_case(
    vec![
        PacketAttributes {
            ip_proto: packet_formats::ip::Ipv4Proto::Proto(packet_formats::ip::IpProto::Tcp),
            port: unsafe { NonZeroU16::new_unchecked(dhcp::protocol::CLIENT_PORT) }
        }
    ]; "invalid trans proto")]
#[test_case(
    vec![
        PacketAttributes {
            ip_proto: packet_formats::ip::Ipv4Proto::Proto(packet_formats::ip::IpProto::Udp),
            port: INVALID_PORT
        }
    ]; "invalid port")]
#[test_case(
    vec![
        PacketAttributes {
            ip_proto: packet_formats::ip::Ipv4Proto::Proto(packet_formats::ip::IpProto::Udp),
            port: unsafe { NonZeroU16::new_unchecked(dhcp::protocol::CLIENT_PORT) }
        }
    ]; "valid")]
#[test_case(
    vec![
        PacketAttributes {
            ip_proto: packet_formats::ip::Ipv4Proto::Proto(packet_formats::ip::IpProto::Udp),
            port: INVALID_PORT
        },
        PacketAttributes {
            ip_proto: packet_formats::ip::Ipv4Proto::Proto(packet_formats::ip::IpProto::Udp),
            port: INVALID_PORT
        },
            PacketAttributes {
            ip_proto: packet_formats::ip::Ipv4Proto::Proto(packet_formats::ip::IpProto::Tcp),
            port: unsafe { NonZeroU16::new_unchecked(dhcp::protocol::CLIENT_PORT) }
        }
    ]; "multiple invalid port and single invalid trans proto")]
async fn inspect_dhcp<E: netemul::Endpoint>(
    _test_name: &str,
    inbound_packets: Vec<PacketAttributes>,
) {
    // TODO(https://fxbug.dev/79556): Extend this test to cover the stat tracking frames discarded
    // due to an invalid PacketType.
    let sandbox = netemul::TestSandbox::new().expect("failed to create sandbox");
    let network = sandbox.create_network("net").await.expect("failed to create network");
    let realm = sandbox
        .create_netstack_realm::<Netstack2, _>("inspect_dhcp")
        .expect("failed to create realm");
    let eth = realm
        .join_network::<E, _>(&network, "ep1", &netemul::InterfaceConfig::Dhcp)
        .await
        .expect("failed to join network");

    let fake_ep = network.create_fake_endpoint().expect("failed to create fake endpoint");

    const SRC_IP: net_types_ip::Ipv4Addr = net_types_ip::Ipv4::UNSPECIFIED_ADDRESS;
    const DST_IP: net_types::SpecifiedAddr<net_types_ip::Ipv4Addr> =
        net_types_ip::Ipv4::GLOBAL_BROADCAST_ADDRESS;

    for PacketAttributes { ip_proto, port } in &inbound_packets {
        let ser = packet::Buf::new(&mut [], ..)
            .encapsulate(UdpPacketBuilder::new(SRC_IP, *DST_IP, None, *port))
            .encapsulate(Ipv4PacketBuilder::new(SRC_IP, DST_IP, /* ttl */ 1, *ip_proto))
            .encapsulate(EthernetFrameBuilder::new(
                net_types::ethernet::Mac::BROADCAST,
                constants::eth::MAC_ADDR,
                EtherType::Ipv4,
            ))
            .serialize_vec_outer()
            .expect("failed to serialize UDP packet")
            .unwrap_b();
        let () = fake_ep.write(ser.as_ref()).await.expect("failed to write to endpoint");
    }

    const DISCARD_STATS_NAME: &str = "PacketDiscardStats";
    const INVALID_PORT_STAT_NAME: &str = "InvalidPort";
    const INVALID_TRANS_PROTO_STAT_NAME: &str = "InvalidTransProto";
    const INVALID_PACKET_TYPE_STAT_NAME: &str = "InvalidPacketType";
    const COUNTER_PROPERTY_NAME: &str = "Count";
    let path = ["Stats", "DHCP Info", &eth.id().to_string(), "NICs"];

    let mut invalid_ports = HashMap::<NonZeroU16, u64>::new();
    let mut invalid_trans_protos = HashMap::<u8, u64>::new();

    for PacketAttributes { port, ip_proto } in inbound_packets {
        if ip_proto != packet_formats::ip::Ipv4Proto::Proto(packet_formats::ip::IpProto::Udp) {
            let _: &mut u64 =
                invalid_trans_protos.entry(ip_proto.into()).and_modify(|v| *v += 1).or_insert(1);
        } else if port != unsafe { NonZeroU16::new_unchecked(dhcp::protocol::CLIENT_PORT) } {
            let _: &mut u64 = invalid_ports.entry(port.into()).and_modify(|v| *v += 1).or_insert(1);
        }
    }

    let mut invalid_port_assertion = TreeAssertion::new(INVALID_PORT_STAT_NAME, true);
    let mut invalid_trans_proto_assertion = TreeAssertion::new(INVALID_TRANS_PROTO_STAT_NAME, true);
    let invalid_packet_type_assertion = TreeAssertion::new(INVALID_PACKET_TYPE_STAT_NAME, true);

    for (port, count) in invalid_ports {
        let mut port_assertion = TreeAssertion::new(&port.to_string(), true);
        let () = port_assertion
            .add_property_assertion(COUNTER_PROPERTY_NAME, Box::new(count.to_string()));
        let () = invalid_port_assertion.add_child_assertion(port_assertion);
    }

    for (proto, count) in invalid_trans_protos {
        let mut trans_proto_assertion = TreeAssertion::new(&proto.to_string(), true);
        let () = trans_proto_assertion
            .add_property_assertion(COUNTER_PROPERTY_NAME, Box::new(count.to_string()));
        let () = invalid_trans_proto_assertion.add_child_assertion(trans_proto_assertion);
    }

    let mut discard_stats_assertion = TreeAssertion::new(DISCARD_STATS_NAME, true);
    let () = discard_stats_assertion.add_child_assertion(invalid_port_assertion);
    let () = discard_stats_assertion.add_child_assertion(invalid_trans_proto_assertion);
    let () = discard_stats_assertion.add_child_assertion(invalid_packet_type_assertion);

    let tree_assertion = path.iter().fold(discard_stats_assertion, |acc, name| {
        let mut assertion = TreeAssertion::new(name, true);
        let () = assertion.add_child_assertion(acc);
        assertion
    });

    const POLL_INTERVAL: zx::Duration = zx::Duration::from_millis(100);
    const TIMEOUT: std::time::Duration = std::time::Duration::from_millis(60000);

    let inspect_stats_stream = fasync::Interval::new(POLL_INTERVAL)
        .then(|()| {
            get_inspect_data(
                &realm,
                "netstack",
                std::iter::once(DISCARD_STATS_NAME).chain(path).into_iter().rev().join("/"),
                "interfaces",
            )
        })
        .fuse();

    futures::pin_mut!(inspect_stats_stream);

    let timeout_fut = fasync::Timer::new(TIMEOUT).fuse();

    futures::pin_mut!(timeout_fut);

    // We poll for the inspect data here because we aren't synchronized
    // with the processing of the packets by the DHCP client.
    loop {
        futures::select! {
            data = inspect_stats_stream.try_next() => {
                let data = data.expect("get_inspect_data failed").expect("dhcp inspect stats: unexpected err");
                println!("Got inspect data: {:#?}", data);
                if let Ok(_) = tree_assertion.run(&data) {
                    return;
                }
            },
            () = timeout_fut => panic!("inspect data lacked expected dhcp stats"),
        }
    }
}

#[fasync::run_singlethreaded(test)]
async fn inspect_for_sampler() {
    let sandbox = netemul::TestSandbox::new().expect("failed to create sandbox");
    let realm = sandbox
        .create_netstack_realm::<Netstack2, _>("inspect_for_sampler")
        .expect("failed to create realm");
    // Connect to netstack service to spawn a netstack instance.
    let _netstack = realm
        .connect_to_protocol::<fidl_fuchsia_netstack::NetstackMarker>()
        .expect("failed to connect to fuchsia.netstack/Netstack");

    // We can pass any sample rate here. It is not used at all in this test.
    const MINIMUM_SAMPLE_RATE_SEC: i64 = 60;
    let sampler_config = sampler_config::SamplerConfig::from_directory(
        MINIMUM_SAMPLE_RATE_SEC,
        "/pkg/data/sampler-config",
    )
    .expect("SamplerConfig::from_directory failed");
    let project_config = match &sampler_config.project_configs[..] {
        [project_config] => project_config,
        project_configs => panic!("expected one project_config but got {:#?}", project_configs),
    };
    for metric_config in &project_config.metrics {
        let tree_selector = metric_config
            .selector
            .strip_prefix("netstack.cmx:")
            .expect("failed to strip \"netstack.cmx:\"");
        // Debug print data to make debugging easier in case of failures.
        println!("tree_selector={:#?}", tree_selector);
        let expected_key = metric_config.selector.split(":").last().expect("failed to find a key");

        let data = get_inspect_data(&realm, "netstack", tree_selector, "counters")
            .await
            .expect("get_inspect_data failed");
        let properties: Vec<_> = data
            .property_iter()
            .filter_map(|(_hierarchy_path, property_opt): (Vec<&String>, _)| property_opt)
            .collect();
        matches::assert_matches!(properties[..], [Property::Uint(key, _)] if key == expected_key);
    }
}
