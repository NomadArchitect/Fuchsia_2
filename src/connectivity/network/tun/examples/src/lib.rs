// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context as _, Error};
use fidl::endpoints::Proxy;
use net_declare::{fidl_ip, fidl_mac, fidl_subnet};

struct IpLayerConfig {
    alice_subnet: fidl_fuchsia_net::Subnet,
    bob_ip: fidl_fuchsia_net::IpAddress,
}

struct EthernetLayerConfig {
    alice_mac: fidl_fuchsia_net::MacAddress,
    bob_mac: fidl_fuchsia_net::MacAddress,
    ip_layer: IpLayerConfig,
}

/// The network configuration used for TAP-like example.
///
/// Note that the tests in this crate run in parallel against the same Netstack,
/// so the IP addresses between tests must be different to avoid interference.
const CONFIG_FOR_TAP_LIKE: EthernetLayerConfig = EthernetLayerConfig {
    alice_mac: fidl_mac!("02:03:04:05:06:07"),
    bob_mac: fidl_mac!("02:AA:BB:CC:DD:EE"),
    ip_layer: IpLayerConfig {
        alice_subnet: fidl_subnet!("192.168.0.1/24"),
        bob_ip: fidl_ip!("192.168.0.2"),
    },
};

/// The network configuration used for TUN-like example.
///
/// Note that the tests in this crate run in parallel against the same Netstack,
/// so the IP addresses between tests must be different to avoid interference.
const CONFIG_FOR_TUN_LIKE: IpLayerConfig = IpLayerConfig {
    alice_subnet: fidl_subnet!("192.168.86.1/24"),
    bob_ip: fidl_ip!("192.168.86.2"),
};

/// The port identifier used in examples.
const PORT_ID: u8 = 0;

/// Creates a new [`fidl_fuchsia_net_tun::DeviceConfig`] and
/// [`fidl_fuchsia_net_tun::DevicePortConfig`] using the provided `frame_type` and `mac`.
fn new_device_and_port_config(
    frame_types: Vec<fidl_fuchsia_hardware_network::FrameType>,
    mac: Option<fidl_fuchsia_net::MacAddress>,
) -> (fidl_fuchsia_net_tun::DeviceConfig, fidl_fuchsia_net_tun::DevicePortConfig) {
    let rx_types = frame_types;
    let tx_types = rx_types
        .iter()
        .map(|frame_type| fidl_fuchsia_hardware_network::FrameTypeSupport {
            type_: *frame_type,
            features: 0,
            supported_flags: fidl_fuchsia_hardware_network::TxFlags::empty(),
        })
        .collect();
    let device_config = fidl_fuchsia_net_tun::DeviceConfig {
        base: Some(fidl_fuchsia_net_tun::BaseDeviceConfig {
            // Discard frame metadata. It is reported through the `read_frame`
            // method.
            report_metadata: Some(false),
            min_tx_buffer_length: None,
            ..fidl_fuchsia_net_tun::BaseDeviceConfig::EMPTY
        }),
        // Blocking will cause the read and write frame methods to block. See
        // the documentation on fuchsia.net.tun/DeviceConfig for more details.
        blocking: Some(true),

        ..fidl_fuchsia_net_tun::DeviceConfig::EMPTY
    };
    let port_config = fidl_fuchsia_net_tun::DevicePortConfig {
        base: Some(fidl_fuchsia_net_tun::BasePortConfig {
            // The port identifier that we'll use to refer to this port.
            id: Some(PORT_ID),
            // Device MTU reported to Netstack.
            mtu: Some(1500),
            // The frame types we're going to accept. TAP and TUN examples will
            // request different frame types.
            rx_types: Some(rx_types),
            tx_types: Some(tx_types),
            ..fidl_fuchsia_net_tun::BasePortConfig::EMPTY
        }),
        // Create the port with the link online signal.
        online: Some(true),
        // Use the MAC requested by the caller. TAP-like devices require a MAC
        // address, while TUN-like devices don't.
        mac,
        ..fidl_fuchsia_net_tun::DevicePortConfig::EMPTY
    };
    (device_config, port_config)
}

/// An example of configuring and operating a TAP-like interface using
/// fuchsia.net.tun.
#[fuchsia_async::run_singlethreaded(test)]
async fn tap_like_over_network_tun() -> Result<(), Error> {
    // Connect to the ambient fuchsia.net.tun/Control service.
    let tun =
        fuchsia_component::client::connect_to_protocol::<fidl_fuchsia_net_tun::ControlMarker>()
            .context("failed to connect to service")?;

    // Define the tun device configuration we want to use.
    // For a TAP-like device, the frame type must be Ethernet, and we must
    // provide a MAC address for the virtual interface.
    let (device_config, port_config) = new_device_and_port_config(
        vec![fidl_fuchsia_hardware_network::FrameType::Ethernet],
        Some(CONFIG_FOR_TAP_LIKE.alice_mac),
    );

    // Request device creation.
    let (tun_device, server_end) =
        fidl::endpoints::create_proxy::<fidl_fuchsia_net_tun::DeviceMarker>()
            .context("failed to create device endpoints")?;
    let () = tun.create_device(device_config, server_end).context("failed to create device")?;
    // Add a port.
    let (tun_port, server_end) =
        fidl::endpoints::create_proxy::<fidl_fuchsia_net_tun::PortMarker>()
            .context("failed to create port endpoints")?;
    let () = tun_device.add_port(port_config, server_end).context("failed to add port")?;

    // Get the Netstack ends of the tun Ethernet device.
    let (network_device, mac) = {
        let (network_device, netdevice_server_end) =
            fidl::endpoints::create_proxy::<fidl_fuchsia_hardware_network::DeviceMarker>()
                .context("failed to create netdevice proxy")?;
        let (port, port_server_end) =
            fidl::endpoints::create_proxy::<fidl_fuchsia_hardware_network::PortMarker>()
                .context("failed to create port proxy")?;
        let (mac, mac_server_end) = fidl::endpoints::create_endpoints::<
            fidl_fuchsia_hardware_network::MacAddressingMarker,
        >()
        .context("failed to create mac endpoints")?;

        let () =
            tun_device.get_device(netdevice_server_end).context("failed to connect protocols")?;
        let () = tun_port.get_port(port_server_end).context("get_port failed")?;
        let () = port.get_mac(mac_server_end).context("get_mac failed")?;
        let network_device: fidl::endpoints::ClientEnd<_> = network_device
            .into_channel()
            .expect("failed to get inner channel")
            .into_zx_channel()
            .into();
        (network_device, mac)
    };

    // Connect to Netstack and install the device.
    let stack =
        fuchsia_component::client::connect_to_protocol::<fidl_fuchsia_net_stack::StackMarker>()
            .context("failed to connect to stack")?;
    let interface_id = stack
        .add_interface(
            // Interface configuration.
            fidl_fuchsia_net_stack::InterfaceConfig {
                name: Some("tap-like".to_string()),
                topopath: Some("/fake-topopath".to_string()),
                metric: Some(100),
                ..fidl_fuchsia_net_stack::InterfaceConfig::EMPTY
            },
            // The device definition tells Netstack this is an Ethernet device
            // and gives it handles to access the data plane and the MAC
            // addressing control plane.
            &mut fidl_fuchsia_net_stack::DeviceDefinition::Ethernet(
                fidl_fuchsia_net_stack::EthernetDeviceDefinition { network_device, mac },
            ),
        )
        .await
        .context("add_interface FIDL error")?
        .map_err(|e: fidl_fuchsia_net_stack::Error| {
            anyhow::anyhow!("add_interface failed: {:?}", e)
        })?;

    // Enable the interface.
    let () = stack
        .enable_interface(interface_id)
        .await
        .context("enable_interface FIDL error")?
        .map_err(|e: fidl_fuchsia_net_stack::Error| {
            anyhow::anyhow!("enable_interface failed: {:?}", e)
        })?;

    // Add an IPv4 address to it.
    let () = stack
        .add_interface_address(interface_id, &mut CONFIG_FOR_TAP_LIKE.ip_layer.alice_subnet.clone())
        .await
        .context("add_interface_address FIDL error")?
        .map_err(|e: fidl_fuchsia_net_stack::Error| {
            anyhow::anyhow!("add_interface_address failed: {:?}", e)
        })?;

    // Wait for Netstack to report the interface online. Netstack may drop
    // frames if they're sent before the online signal is observed. This is
    // necessary to prevent this test from flaking since we're only going to
    // send a single echo request.
    let () = helpers::wait_interface_online(interface_id)
        .await
        .context("failed to observe interface online")?;

    // Now we can send frames. In this example we'll send an ICMP echo request
    // and wait for the Netstack to respond.
    let () = tun_device
        .write_frame(fidl_fuchsia_net_tun::Frame {
            port: Some(PORT_ID),
            frame_type: Some(fidl_fuchsia_hardware_network::FrameType::Ethernet),
            data: Some(helpers::bob_pings_alice_for_tap_like(&CONFIG_FOR_TAP_LIKE)),
            meta: None,
            ..fidl_fuchsia_net_tun::Frame::EMPTY
        })
        .await
        .context("write_frame FIDL error")?
        .map_err(fuchsia_zircon::Status::from_raw)
        .context("write_frame failed")?;

    // Read frames until we see the echo response.
    loop {
        let fidl_fuchsia_net_tun::Frame {
            // Port over which the frame was sent.
            port,
            // The frame's type will always be Ethernet in this example. If multiple
            // frame types are supported (like IPv4 and IPv6, for example) that
            // information will be here.
            frame_type,
            // Frame payload.
            data,
            // Metadata associated with the frame.
            meta: _,
            ..
        } = tun_device
            .read_frame()
            .await
            .context("read_frame FIDL error")?
            .map_err(fuchsia_zircon::Status::from_raw)
            .context("read_frame failed")?;
        let data = data.ok_or_else(|| anyhow::anyhow!("received Frame with missing data field"))?;
        assert_eq!(port, Some(PORT_ID));
        assert_eq!(frame_type, Some(fidl_fuchsia_hardware_network::FrameType::Ethernet));
        match helpers::get_icmp_response_for_tap_like(&data[..])
            .context("failed to parse ICMP response")?
        {
            None => (),
            Some(helpers::ParsedFrame::ArpRequest { target_ip, sender_mac, sender_ip }) => {
                // When we get an ARP request for bob's IP, we need to send an
                // ARP response back in order for the ICMP echo response to come
                // in.
                if target_ip == CONFIG_FOR_TAP_LIKE.ip_layer.bob_ip {
                    assert_eq!(sender_mac, CONFIG_FOR_TAP_LIKE.alice_mac);
                    assert_eq!(sender_ip, CONFIG_FOR_TAP_LIKE.ip_layer.alice_subnet.addr);
                    let () = tun_device
                        .write_frame(fidl_fuchsia_net_tun::Frame {
                            port: Some(PORT_ID),
                            frame_type: Some(fidl_fuchsia_hardware_network::FrameType::Ethernet),
                            data: Some(helpers::build_bob_arp_response(&CONFIG_FOR_TAP_LIKE)),
                            meta: None,
                            ..fidl_fuchsia_net_tun::Frame::EMPTY
                        })
                        .await
                        .context("write_frame FIDL error")?
                        .map_err(fuchsia_zircon::Status::from_raw)
                        .context("write_frame failed")?;
                }
            }
            Some(helpers::ParsedFrame::IcmpEchoResponse { src_mac, dst_mac, src_ip, dst_ip }) => {
                assert_eq!(src_mac, CONFIG_FOR_TAP_LIKE.alice_mac);
                assert_eq!(dst_mac, CONFIG_FOR_TAP_LIKE.bob_mac);
                assert_eq!(src_ip, CONFIG_FOR_TAP_LIKE.ip_layer.alice_subnet.addr);
                assert_eq!(dst_ip, CONFIG_FOR_TAP_LIKE.ip_layer.bob_ip);
                break Ok(());
            }
        }
    }
}

/// An example of configuring and operating a TUN-like interface using
/// fuchsia.net.tun.
#[fuchsia_async::run_singlethreaded(test)]
async fn tun_like_over_network_tun() -> Result<(), Error> {
    // Connect to the ambient fuchsia.net.tun/Control service.
    let tun =
        fuchsia_component::client::connect_to_protocol::<fidl_fuchsia_net_tun::ControlMarker>()
            .context("failed to connect to service")?;

    // Define the tun device configuration we want to use. For a TUN-like
    // device, the frame types must be IPv4 and IPv6, and we don't provide a MAC
    // address.
    let (device_config, port_config) = new_device_and_port_config(
        vec![
            fidl_fuchsia_hardware_network::FrameType::Ipv4,
            fidl_fuchsia_hardware_network::FrameType::Ipv6,
        ],
        None,
    );

    // Request device creation.
    let (tun_device, server_end) =
        fidl::endpoints::create_proxy::<fidl_fuchsia_net_tun::DeviceMarker>()
            .context("failed to create device endpoints")?;
    let () = tun.create_device(device_config, server_end).context("failed to create device")?;
    // Add a port.
    let (_tun_port, server_end) =
        fidl::endpoints::create_proxy::<fidl_fuchsia_net_tun::PortMarker>()
            .context("failed to create port endpoints")?;
    let () = tun_device.add_port(port_config, server_end).context("failed to add port")?;

    // Get the Netstack end of the tun IPv4/IPv6 device.
    let (network_device, netdevice_server_end) =
        fidl::endpoints::create_endpoints::<fidl_fuchsia_hardware_network::DeviceMarker>()
            .context("failed to create netdevice endpoints")?;
    let () = tun_device.get_device(netdevice_server_end).context("failed to get device")?;

    // Connect to Netstack and install the device.
    let stack =
        fuchsia_component::client::connect_to_protocol::<fidl_fuchsia_net_stack::StackMarker>()
            .context("failed to connect to stack")?;
    let interface_id = stack
        .add_interface(
            // Interface configuration.
            fidl_fuchsia_net_stack::InterfaceConfig {
                name: Some("tun-like".to_string()),
                topopath: Some("/fake-topopath".to_string()),
                metric: Some(100),
                ..fidl_fuchsia_net_stack::InterfaceConfig::EMPTY
            },
            // The device definition tells Netstack this is a TUN-like device
            // operating at the IP layer and gives it handles to access the data
            // plane.
            &mut fidl_fuchsia_net_stack::DeviceDefinition::Ip(network_device),
        )
        .await
        .context("add_interface FIDL error")?
        .map_err(|e: fidl_fuchsia_net_stack::Error| {
            anyhow::anyhow!("add_interface failed: {:?}", e)
        })?;

    // Enable the interface.
    let () = stack
        .enable_interface(interface_id)
        .await
        .context("enable_interface FIDL error")?
        .map_err(|e: fidl_fuchsia_net_stack::Error| {
            anyhow::anyhow!("enable_interface failed: {:?}", e)
        })?;

    // Add an IPv4 address to it.
    let () = stack
        .add_interface_address(interface_id, &mut CONFIG_FOR_TUN_LIKE.alice_subnet.clone())
        .await
        .context("add_interface_address FIDL error")?
        .map_err(|e: fidl_fuchsia_net_stack::Error| {
            anyhow::anyhow!("add_interface_address failed: {:?}", e)
        })?;

    // Wait for Netstack to report the interface online. Netstack may drop
    // frames if they're sent before the online signal is observed. This is
    // necessary to prevent this test from flaking since we're only going to
    // send a single echo request.
    let () = helpers::wait_interface_online(interface_id)
        .await
        .context("failed to observe interface online")?;

    // Now we can send frames. In this example we'll send an ICMP echo request
    // and wait for the Netstack to respond.
    let () = tun_device
        .write_frame(fidl_fuchsia_net_tun::Frame {
            port: Some(PORT_ID),
            frame_type: Some(fidl_fuchsia_hardware_network::FrameType::Ipv4),
            data: Some(helpers::bob_pings_alice_for_tun_like(&CONFIG_FOR_TUN_LIKE)),
            meta: None,
            ..fidl_fuchsia_net_tun::Frame::EMPTY
        })
        .await
        .context("write_frame FIDL error")?
        .map_err(fuchsia_zircon::Status::from_raw)
        .context("write_frame failed")?;

    // Read frames until we see the echo response.
    loop {
        let fidl_fuchsia_net_tun::Frame {
            // Port over which the frame was sent.
            port,
            // A TUN-like device can receive IPv4 or IPv6 frames, in this
            // example we're going to be looking only into IPv4 packets.
            frame_type,
            // Frame payload.
            data,
            // Metadata associated with the frame.
            meta: _,
            ..
        } = tun_device
            .read_frame()
            .await
            .context("read_frame FIDL error")?
            .map_err(fuchsia_zircon::Status::from_raw)
            .context("read_frame failed")?;
        assert_eq!(port, Some(PORT_ID));
        match frame_type {
            Some(fidl_fuchsia_hardware_network::FrameType::Ipv4) => (),
            Some(fidl_fuchsia_hardware_network::FrameType::Ipv6) => continue,
            unexpected => {
                break Err(anyhow::anyhow!("received unexpected frame_type: {:?}", unexpected))
            }
        }
        let data = data.ok_or_else(|| anyhow::anyhow!("received Frame with missing data field"))?;

        if let Some((src_ip, dst_ip)) = helpers::get_icmp_response_for_tun_like(&data[..])
            .context("failed to parse ICMP response")?
        {
            assert_eq!(src_ip, CONFIG_FOR_TUN_LIKE.alice_subnet.addr);
            assert_eq!(dst_ip, CONFIG_FOR_TUN_LIKE.bob_ip);
            break Ok(());
        }
    }
}

/// This module contains some helpers to remove noise from the examples in this
/// crate.
mod helpers {
    use anyhow::{Context as _, Error};
    use net_types::{
        ethernet::Mac,
        ip::{Ipv4, Ipv4Addr},
    };
    use packet::{InnerPacketBuilder as _, ParsablePacket as _, Serializer as _};
    use packet_formats::{
        arp::{ArpOp, ArpPacket, ArpPacketBuilder},
        ethernet::{EtherType, EthernetFrame, EthernetFrameBuilder, EthernetFrameLengthCheck},
        icmp::{IcmpEchoRequest, IcmpPacketBuilder, IcmpParseArgs, IcmpUnusedCode, Icmpv4Packet},
        ip::Ipv4Proto,
        ipv4::{Ipv4Header as _, Ipv4Packet, Ipv4PacketBuilder},
    };

    fn ip_v4(fidl_ip: fidl_fuchsia_net::IpAddress) -> Ipv4Addr {
        match fidl_ip {
            fidl_fuchsia_net::IpAddress::Ipv4(ip) => Ipv4Addr::new(ip.addr),
            fidl_fuchsia_net::IpAddress::Ipv6(v6) => {
                panic!("can't convert from FIDL IPv6 {:?}", v6)
            }
        }
    }

    const fn mac(fidl_mac: fidl_fuchsia_net::MacAddress) -> Mac {
        Mac::new(fidl_mac.octets)
    }

    fn fidl_mac(mac: Mac) -> fidl_fuchsia_net::MacAddress {
        fidl_fuchsia_net::MacAddress { octets: mac.bytes() }
    }

    fn fidl_ip(ip_v4: Ipv4Addr) -> fidl_fuchsia_net::IpAddress {
        fidl_fuchsia_net::IpAddress::Ipv4(fidl_fuchsia_net::Ipv4Address {
            addr: ip_v4.ipv4_bytes(),
        })
    }

    const ECHO_PAYLOAD: [u8; 4] = [1, 2, 3, 4];
    const ICMP_ID: u16 = 1;
    const ICMP_SEQNUM: u16 = 1;

    /// Creates the ICMP bytes for bob pinging alice to use in TAP-like
    /// examples.
    pub(super) fn bob_pings_alice_for_tap_like(config: &super::EthernetLayerConfig) -> Vec<u8> {
        packet::Buf::new(&mut bob_pings_alice_for_tun_like(&config.ip_layer)[..], ..)
            .encapsulate(EthernetFrameBuilder::new(
                mac(config.bob_mac),
                mac(config.alice_mac),
                EtherType::Ipv4,
            ))
            .serialize_vec_outer()
            .expect("serialization failed")
            .as_ref()
            .to_vec()
    }

    /// Creates the ICMP bytes for bob pinging alice to use in TUN-like
    /// examples.
    pub(super) fn bob_pings_alice_for_tun_like(config: &super::IpLayerConfig) -> Vec<u8> {
        let alice_ip = ip_v4(config.alice_subnet.addr);
        let bob_ip = ip_v4(config.bob_ip);
        packet::Buf::new(&mut ECHO_PAYLOAD.to_vec()[..], ..)
            .encapsulate(IcmpPacketBuilder::<Ipv4, &[u8], _>::new(
                bob_ip,
                alice_ip,
                IcmpUnusedCode,
                IcmpEchoRequest::new(ICMP_ID, ICMP_SEQNUM),
            ))
            .encapsulate(Ipv4PacketBuilder::new(bob_ip, alice_ip, 1, Ipv4Proto::Icmp))
            .serialize_vec_outer()
            .expect("serialization failed")
            .as_ref()
            .to_vec()
    }

    /// A parsed frame for a tap-like interface.
    pub(super) enum ParsedFrame {
        /// An ARP request was observed, an ARP response should be sent.
        ArpRequest {
            target_ip: fidl_fuchsia_net::IpAddress,
            sender_mac: fidl_fuchsia_net::MacAddress,
            sender_ip: fidl_fuchsia_net::IpAddress,
        },
        /// An ICMP echo response was observed.
        IcmpEchoResponse {
            src_mac: fidl_fuchsia_net::MacAddress,
            dst_mac: fidl_fuchsia_net::MacAddress,
            src_ip: fidl_fuchsia_net::IpAddress,
            dst_ip: fidl_fuchsia_net::IpAddress,
        },
    }

    /// Attempts to parse an ICMP echo response in an Ethernet frame contained
    /// in `data`.
    ///
    /// Returns `Err` if the frame can't be parsed, `Ok(None)` if the frame is
    /// valid but it's not an ICMP response.
    ///
    /// Otherwise, returns either a [`ParsedFrame::IcmpEchoResponse`] with the
    /// echo response or a [`ParsedFrame::ArpRequest`] indicating that an ARP
    /// response must be sent for LL resolution.
    pub(super) fn get_icmp_response_for_tap_like(
        data: &[u8],
    ) -> Result<Option<ParsedFrame>, Error> {
        let mut bv = data;
        let ethernet = EthernetFrame::parse(&mut bv, EthernetFrameLengthCheck::NoCheck)
            .context("failed to parse Ethernet frame")?;
        let src_mac = fidl_mac(ethernet.src_mac());
        let dst_mac = fidl_mac(ethernet.dst_mac());
        match ethernet.ethertype() {
            Some(EtherType::Ipv4) => get_icmp_response_for_tun_like(bv).map(|r| {
                r.map(|(src_ip, dst_ip)| ParsedFrame::IcmpEchoResponse {
                    src_mac,
                    dst_mac,
                    src_ip,
                    dst_ip,
                })
            }),
            Some(EtherType::Arp) => {
                let arp = ArpPacket::<_, Mac, Ipv4Addr>::parse(&mut bv, ())
                    .context("failed to parse ARP packet")?;
                match arp.operation() {
                    ArpOp::Request => Ok(Some(ParsedFrame::ArpRequest {
                        target_ip: fidl_ip(arp.target_protocol_address()),
                        sender_mac: fidl_mac(arp.sender_hardware_address()),
                        sender_ip: fidl_ip(arp.sender_protocol_address()),
                    })),
                    ArpOp::Response | ArpOp::Other(_) => Ok(None),
                }
            }
            _ => Ok(None),
        }
    }

    /// Attempts to parse an ICMP echo response in an Ethernet frame contained
    /// in `data`.
    ///
    /// Returns `Err` if the frame can't be parsed, `Ok(None)` if the frame is
    /// valid but it's not an ICMP response.
    ///
    /// Otherwise, returns the `(src_ip, dst_ip)` addressing tuple.
    ///
    /// Note that this function is provided as a support to the examples in this
    /// crate and it makes assumptions that may not be valid in a production
    /// environment, such as assuming that no IP fragmentation happens, and the
    /// ICMP echo responses are not validated.
    pub(super) fn get_icmp_response_for_tun_like(
        data: &[u8],
    ) -> Result<Option<(fidl_fuchsia_net::IpAddress, fidl_fuchsia_net::IpAddress)>, Error> {
        let mut bv = data;
        let ipv4 = Ipv4Packet::parse(&mut bv, ()).context("failed to parse IPv4")?;
        if ipv4.proto() != Ipv4Proto::Icmp {
            return Ok(None);
        }
        let src_ip = ipv4.src_ip();
        let dst_ip = ipv4.dst_ip();
        let icmp = Icmpv4Packet::parse(&mut bv, IcmpParseArgs::new(src_ip, dst_ip))
            .context("failed to parse ICMP")?;
        if let Icmpv4Packet::EchoReply(_echo) = icmp {
            Ok(Some((fidl_ip(src_ip), fidl_ip(dst_ip))))
        } else {
            Ok(None)
        }
    }

    /// Creates an ARP response from bob to alice for use in tap-like tests.
    pub(super) fn build_bob_arp_response(config: &super::EthernetLayerConfig) -> Vec<u8> {
        ArpPacketBuilder::new(
            ArpOp::Response,
            mac(config.bob_mac),
            ip_v4(config.ip_layer.bob_ip),
            mac(config.alice_mac),
            ip_v4(config.ip_layer.alice_subnet.addr),
        )
        .into_serializer()
        .encapsulate(EthernetFrameBuilder::new(
            mac(config.bob_mac),
            mac(config.alice_mac),
            EtherType::Arp,
        ))
        .serialize_vec_outer()
        .expect("serialization failed")
        .as_ref()
        .to_vec()
    }

    /// Waits for interface with ID `interface_id` to come online.
    pub(super) async fn wait_interface_online(interface_id: u64) -> Result<(), Error> {
        let interface_state = fuchsia_component::client::connect_to_protocol::<
            fidl_fuchsia_net_interfaces::StateMarker,
        >()
        .context("failed to connect to interfaces state service")?;
        fidl_fuchsia_net_interfaces_ext::wait_interface_with_id(
            fidl_fuchsia_net_interfaces_ext::event_stream_from_state(&interface_state)
                .context("failed to create event stream")?,
            &mut fidl_fuchsia_net_interfaces_ext::InterfaceState::Unknown(interface_id),
            |&fidl_fuchsia_net_interfaces_ext::Properties {
                 id: _,
                 addresses: _,
                 online,
                 device_class: _,
                 has_default_ipv4_route: _,
                 has_default_ipv6_route: _,
                 name: _,
             }| {
                if online {
                    Some(())
                } else {
                    None
                }
            },
        )
        .await
        .context("failed to wait for interface online")
    }
}
