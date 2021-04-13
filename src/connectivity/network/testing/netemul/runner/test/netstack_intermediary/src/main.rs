// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::{format_err, Context as _, Error},
    ethernet,
    fidl_fuchsia_netemul_network::{
        EndpointManagerMarker, FakeEndpointMarker, NetworkContextMarker, NetworkManagerMarker,
    },
    fidl_fuchsia_netemul_sync::{BusMarker, BusProxy, SyncManagerMarker},
    fidl_fuchsia_netstack::{InterfaceConfig, NetstackMarker},
    fuchsia_async as fasync,
    fuchsia_component::client,
    fuchsia_zircon as zx,
    futures::{self, TryStreamExt},
    std::{str, task::Poll},
    structopt::StructOpt,
};

#[derive(StructOpt, Debug)]
struct Opt {
    #[structopt(long = "mock_guest")]
    is_mock_guest: bool,
    #[structopt(long = "server")]
    is_server: bool,
    #[structopt(long)]
    network_name: Option<String>,
    #[structopt(long)]
    endpoint_name: Option<String>,
    #[structopt(long)]
    server_name: Option<String>,
}

const DEFAULT_METRIC: u32 = 100;
const BUS_NAME: &'static str = "netstack-itm-bus";

fn open_bus(cli_name: &str) -> Result<BusProxy, Error> {
    let syncm = client::connect_to_service::<SyncManagerMarker>()?;
    let (bus, bus_server_end) = fidl::endpoints::create_proxy::<BusMarker>()?;
    syncm.bus_subscribe(BUS_NAME, cli_name, bus_server_end)?;
    Ok(bus)
}

async fn run_mock_guest(
    network_name: String,
    ep_name: String,
    server_name: String,
) -> Result<(), Error> {
    // Create an ethertap client and an associated ethernet device.
    let ctx = client::connect_to_service::<NetworkContextMarker>()?;
    let (epm, epm_server_end) = fidl::endpoints::create_proxy::<EndpointManagerMarker>()?;
    ctx.get_endpoint_manager(epm_server_end)?;
    let (netm, netm_server_end) = fidl::endpoints::create_proxy::<NetworkManagerMarker>()?;
    ctx.get_network_manager(netm_server_end)?;

    let ep = epm.get_endpoint(&ep_name).await?.unwrap().into_proxy()?;
    let net = netm.get_network(&network_name).await?.unwrap().into_proxy()?;
    let (fake_ep, fake_ep_server_end) = fidl::endpoints::create_proxy::<FakeEndpointMarker>()?;
    net.create_fake_endpoint(fake_ep_server_end)?;

    let netstack = client::connect_to_service::<NetstackMarker>()?;
    let mut cfg = InterfaceConfig {
        name: "eth-test".to_string(),
        filepath: "[TBD]".to_string(),
        metric: DEFAULT_METRIC,
    };

    let _nicid = match ep.get_device().await? {
        fidl_fuchsia_netemul_network::DeviceConnection::Ethernet(eth_device) => {
            netstack.add_ethernet_device(&format!("/{}", ep_name), &mut cfg, eth_device).await?
        }
        fidl_fuchsia_netemul_network::DeviceConnection::NetworkDevice(netdevice) => {
            todo!("(48860) Support and test for NetworkDevice connections. Got unexpected NetworkDevice {:?}", netdevice)
        }
    };

    // Send a message to the server and expect it to be echoed back.
    let echo_string = String::from("hello");

    let bus = open_bus(&ep_name)?;
    let (success, absent) =
        bus.wait_for_clients(&mut vec![server_name.as_str()].drain(..), 0).await?;
    assert!(success);
    assert_eq!(absent, None);

    fake_ep.write(echo_string.as_bytes()).await.context("write failed")?;

    println!("To Server: {}", echo_string);

    let (data, dropped_frames) = fake_ep.read().await.context("read failed")?;
    assert_eq!(dropped_frames, 0);
    let server_string = str::from_utf8(&data)?;
    assert!(
        echo_string == server_string,
        "Server reply ({}) did not match client message ({})",
        server_string,
        echo_string
    );
    println!("From Server: {}", server_string);

    Ok(())
}

async fn run_echo_server_ethernet(
    ep_name: String,
    eth_dev: fidl::endpoints::ClientEnd<fidl_fuchsia_hardware_ethernet::DeviceMarker>,
) -> Result<(), Error> {
    // Create an EthernetClient to wrap around the Endpoint's ethernet device.
    let vmo = zx::Vmo::create(256 * ethernet::DEFAULT_BUFFER_SIZE as u64)?;

    let eth_proxy = match eth_dev.into_proxy() {
        Ok(proxy) => proxy,
        _ => return Err(format_err!("Could not get ethernet proxy")),
    };

    let mut eth_client =
        ethernet::Client::new(eth_proxy, vmo, ethernet::DEFAULT_BUFFER_SIZE, &ep_name).await?;

    eth_client.start().await?;

    // Listen for a receive event from the client, echo back the client's
    // message, and then exit.
    let mut eth_events = eth_client.get_stream();
    let mut sent_response = false;

    // Before connecting to the message bus to notify the client of the server's existence, poll
    // for events.  Buffers will not be allocated until polling is performed so this ensures that
    // there will be buffers to receive the client's message.
    loop {
        match futures::poll!(eth_events.try_next()) {
            Poll::Pending => break,
            Poll::Ready(result) => match result {
                Ok(result) => match result {
                    Some(_) => continue,
                    None => panic!("event stream produced empty event"),
                },
                Err(e) => panic!("event stream returned an error: {}", e),
            },
        }
    }

    // get on bus to unlock mock_guest part of test
    let _bus = open_bus(&ep_name)?;

    while let Some(event) = eth_events.try_next().await? {
        match event {
            ethernet::Event::Receive(rx, _flags) => {
                if !sent_response {
                    let mut data: [u8; 100] = [0; 100];
                    let sz = rx.read(&mut data);
                    let user_message =
                        str::from_utf8(&data[0..sz]).expect("failed to parse string");
                    println!("From client: {}", user_message);
                    let () = eth_client.send(&data[0..sz]);
                    sent_response = true;

                    // Start listening for the server's response to be
                    // transmitted to the guest.
                    eth_client.tx_listen_start().await?;
                } else {
                    // The mock guest will not send anything to the server
                    // beyond its initial request.  After the server has echoed
                    // the response, the next received message will be the
                    // server's own output since it is listening for its own
                    // Tx messages.
                    break;
                }
            }
            _ => {
                continue;
            }
        }
    }

    Ok(())
}

async fn run_echo_server(ep_name: String) -> Result<(), Error> {
    // Get the Endpoint that was created in the server's environment.
    let netctx = client::connect_to_service::<NetworkContextMarker>()?;
    let (epm, epm_server_end) = fidl::endpoints::create_proxy::<EndpointManagerMarker>()?;
    netctx.get_endpoint_manager(epm_server_end)?;

    let ep = epm.get_endpoint(&ep_name).await?;

    let ep = match ep {
        Some(ep) => ep.into_proxy()?,
        None => return Err(format_err!("Can't find endpoint {}", &ep_name)),
    };

    match ep.get_device().await? {
        fidl_fuchsia_netemul_network::DeviceConnection::Ethernet(e) => {
            run_echo_server_ethernet(ep_name, e).await
        }
        fidl_fuchsia_netemul_network::DeviceConnection::NetworkDevice(netdevice) => {
            todo!("(48860) Support and test for NetworkDevice connections. Got unexpected NetworkDevice {:?}", netdevice)
        }
    }
}

#[fasync::run_singlethreaded]
async fn main() -> Result<(), Error> {
    let opt = Opt::from_args();

    if opt.is_mock_guest {
        if opt.network_name == None || opt.endpoint_name == None || opt.server_name == None {
            return Err(format_err!(
                "Must provide network_name, endpoint_name, and server_name for mock guests"
            ));
        }
        run_mock_guest(
            opt.network_name.unwrap(),
            opt.endpoint_name.unwrap(),
            opt.server_name.unwrap(),
        )
        .await?;
    } else if opt.is_server {
        match opt.endpoint_name {
            Some(endpoint_name) => {
                run_echo_server(endpoint_name).await?;
            }
            None => {
                return Err(format_err!("Must provide endpoint_name for server"));
            }
        }
    }
    Ok(())
}
