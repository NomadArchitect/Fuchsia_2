// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::common::{BusConnection, SERVER_READY},
    anyhow::{format_err, Context as _, Error},
    fidl_fuchsia_net,
    fidl_fuchsia_netemul_network::{DeviceConnection, EndpointManagerMarker, NetworkContextMarker},
    fuchsia_component::client,
    std::io::{Read, Write},
    std::net::{SocketAddr, TcpListener, TcpStream},
    std::str::FromStr,
};

const PORT: i32 = 8080;
const HELLO_MSG_REQ: &str = "Hello World from Client!";
const HELLO_MSG_RSP: &str = "Hello World from Server!";
const DEFAULT_METRIC: u32 = 100;

pub struct ChildOptions {
    pub endpoint: String,
    pub ip: String,
    pub connect_ip: Option<String>,
}

fn publish_server_ready() -> Result<(), Error> {
    let bc = BusConnection::new("server")?;
    bc.publish_code(SERVER_READY)
}

fn run_server(ip: &str) -> Result<(), Error> {
    let addr = format!("{}:{}", ip, PORT);
    let listener = TcpListener::bind(&addr).context(format!("Can't bind to address: {}", addr))?;
    println!("Waiting for connections...");
    let () = publish_server_ready()?;
    let (mut stream, remote) = listener.accept().context("Accept failed")?;
    println!("Accepted connection from {}", remote);
    let mut buffer = [0; 512];
    let rd = stream.read(&mut buffer).context("read failed")?;

    let req = String::from_utf8_lossy(&buffer[0..rd]);
    if req != HELLO_MSG_REQ {
        return Err(format_err!("Got unexpected request from client: '{}'", req));
    }
    println!("Got request ({} bytes) '{}'", rd, req);
    println!("Sending response '{}'", HELLO_MSG_RSP);
    assert_eq!(
        stream.write(HELLO_MSG_RSP.as_bytes()).context("write failed")?,
        HELLO_MSG_RSP.as_bytes().len()
    );
    stream.flush().context("flush failed")?;
    println!("Server done");
    Ok(())
}

fn run_client(server_ip: &str) -> Result<(), Error> {
    println!("Connecting to server...");
    let addr = SocketAddr::from_str(&format!("{}:{}", server_ip, PORT))?;
    let mut stream = TcpStream::connect(&addr).context("Tcp connection failed")?;
    println!("Connected to server!");
    let request = HELLO_MSG_REQ.as_bytes();
    println!("Sending message '{}'", HELLO_MSG_REQ);
    assert_eq!(stream.write(request).context("write failed")?, request.len());
    stream.flush().context("flush failed")?;

    let mut buffer = [0; 512];
    println!("Waiting for server response...");
    let rd = stream.read(&mut buffer)?;
    let rsp = String::from_utf8_lossy(&buffer[0..rd]);
    println!("Got response ({} bytes) '{}'", rd, rsp);
    if rsp != HELLO_MSG_RSP {
        return Err(format_err!("Got unexpected echo from server: '{}'", rsp));
    }
    println!("Client done");
    Ok(())
}

pub async fn run_child(opt: ChildOptions) -> Result<(), Error> {
    println!("Running child with endpoint '{}'", opt.endpoint);

    // get the network context service:
    let netctx = client::connect_to_protocol::<NetworkContextMarker>()?;
    // get the endpoint manager
    let (epm, epmch) = fidl::endpoints::create_proxy::<EndpointManagerMarker>()?;
    netctx.get_endpoint_manager(epmch)?;

    // retrieve the created endpoint:
    let ep = epm.get_endpoint(&opt.endpoint).await?;
    let ep = ep.ok_or_else(|| format_err!("can't find endpoint {}", opt.endpoint))?.into_proxy()?;
    // and the device connection:
    let device_connection = ep.get_device().await?;

    let if_name = format!("eth-{}", opt.endpoint);
    // connect to netstack:
    let netstack = client::connect_to_protocol::<fidl_fuchsia_netstack::NetstackMarker>()?;
    let debug_interfaces =
        client::connect_to_protocol::<fidl_fuchsia_net_debug::InterfacesMarker>()?;
    let static_ip =
        opt.ip.parse::<fidl_fuchsia_net_ext::Subnet>().expect("must be able to parse ip");
    println!("static ip = {:?}", static_ip);

    let use_ip = match opt.ip.split("/").next() {
        Some(v) => String::from(v),
        None => opt.ip,
    };

    let mut cfg = fidl_fuchsia_netstack::InterfaceConfig {
        name: if_name.to_string(),
        filepath: "[TBD]".to_string(),
        metric: DEFAULT_METRIC,
    };
    let (control, control_server_end) =
        fidl_fuchsia_net_interfaces_ext::admin::Control::create_endpoints()
            .context("failed to create control endpoints")?;
    let nicid = match device_connection {
        DeviceConnection::Ethernet(eth) => {
            let nicid = netstack
                .add_ethernet_device(&format!("/vdev/{}", opt.endpoint), &mut cfg, eth)
                .await
                .context("add_ethernet_device FIDL error")?
                .map_err(fuchsia_zircon::Status::from_raw)
                .context("add_ethernet_device error")?;
            let () = debug_interfaces
                .get_admin(nicid.into(), control_server_end)
                .context("calling get_admin")?;
            nicid
        }
        DeviceConnection::NetworkDevice(netdevice) => {
            panic!(
                "got unexpected NetworkDevice {:?}; expected to have been configured with Ethernet",
                netdevice
            );
        }
    };

    let _: bool = control.enable().await.context("call enable")?.map_err(
        |e: fidl_fuchsia_net_interfaces_admin::ControlEnableError| {
            anyhow::format_err!("enable interface: {:?}", e)
        },
    )?;
    let fidl_fuchsia_net::Subnet { mut addr, prefix_len } = static_ip.clone().into();
    let _: fidl_fuchsia_netstack::NetErr =
        netstack.set_interface_address(nicid, &mut addr, prefix_len).await?;

    let interface_state =
        client::connect_to_protocol::<fidl_fuchsia_net_interfaces::StateMarker>()?;
    let () = fidl_fuchsia_net_interfaces_ext::wait_interface_with_id(
        fidl_fuchsia_net_interfaces_ext::event_stream_from_state(&interface_state)?,
        &mut fidl_fuchsia_net_interfaces_ext::InterfaceState::Unknown(nicid.into()),
        |&fidl_fuchsia_net_interfaces_ext::Properties { online, .. }| {
            // TODO(https://github.com/rust-lang/rust/issues/80967): use bool::then_some.
            online.then(|| ())
        },
    )
    .await
    .context("wait for interface online")?;

    println!("Found ethernet with id {}", nicid);

    if let Some(remote) = opt.connect_ip {
        run_client(&remote)
    } else {
        run_server(&use_ip)
    }
}
