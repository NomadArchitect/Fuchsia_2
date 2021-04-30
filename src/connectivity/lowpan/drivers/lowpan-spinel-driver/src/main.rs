// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! LoWPAN Spinel Driver

#![warn(rust_2018_idioms)]
#![warn(clippy::all)]

// NOTE: This line is a hack to work around some issues
//       with respect to external rust crates.
use spinel_pack::{self as spinel_pack};

use std::fmt::Debug;

mod driver;
mod flow_window;
mod spinel;
mod tun;

#[cfg(test)]
#[macro_export]
macro_rules! traceln (($($args:tt)*) => { fuchsia_syslog::macros::fx_log_trace!($($args)*); }; );

#[cfg(not(test))]
#[macro_export]
macro_rules! traceln (($($args:tt)*) => { }; );

#[macro_use]
mod prelude {
    pub use traceln;

    pub use anyhow::{format_err, Context as _};
    pub use fasync::TimeoutExt as _;
    pub use fidl_fuchsia_net_ext as fnet_ext;
    pub use fuchsia_async as fasync;
    pub use fuchsia_syslog::macros::*;
    pub use fuchsia_zircon_status::Status as ZxStatus;
    pub use futures::prelude::*;
    pub use spinel_pack::prelude::*;

    pub use net_declare::{fidl_ip, fidl_ip_v6};
}

use crate::prelude::*;

use crate::driver::{NetworkInterface, SpinelDriver};
use crate::spinel::SpinelDeviceSink;
use crate::tun::*;

use anyhow::Error;
use fidl_fuchsia_factory_lowpan::{FactoryRegisterMarker, FactoryRegisterProxyInterface};
use fidl_fuchsia_lowpan_device::{RegisterMarker, RegisterProxyInterface};
use fidl_fuchsia_lowpan_spinel::{
    DeviceMarker as SpinelDeviceMarker, DeviceProxy as SpinelDeviceProxy,
    DeviceSetupProxy as SpinelDeviceSetupProxy,
};
use fuchsia_component::client::{connect_to_protocol, connect_to_protocol_at};
use fuchsia_component::client::{launch, launcher, App};
use futures::future::LocalBoxFuture;
use lowpan_driver_common::{register_and_serve_driver, register_and_serve_driver_factory};

/// This struct contains the arguments decoded from the command
/// line invocation of the driver.
#[derive(argh::FromArgs, PartialEq, Debug)]
struct DriverArgs {
    #[argh(
        option,
        long = "serviceprefix",
        description = "service namespace prefix (default: '/svc')",
        default = "\"/svc\".to_string()"
    )]
    pub service_prefix: String,

    #[argh(switch, long = "otstack", description = "launch and connect to ot-stack")]
    pub use_ot_stack: bool,

    #[argh(
        option,
        long = "max-auto-restarts",
        description = "maximum number of automatic restarts",
        default = "10"
    )]
    pub max_auto_restarts: u32,

    #[argh(switch, long = "integration", description = "enable integration test mode")]
    pub is_integration_test: bool,

    #[argh(
        option,
        long = "name",
        description = "name of interface",
        default = "\"lowpan0\".to_string()"
    )]
    pub name: String,
}

async fn run_driver<N, RP, RFP, NI>(
    name: N,
    registry: RP,
    factory_registry: Option<RFP>,
    spinel_device_proxy: SpinelDeviceProxy,
    net_if: NI,
) -> Result<(), Error>
where
    N: AsRef<str>,
    RP: RegisterProxyInterface,
    RFP: FactoryRegisterProxyInterface,
    NI: NetworkInterface + Debug,
{
    let name = name.as_ref();
    let driver = SpinelDriver::new(SpinelDeviceSink::new(spinel_device_proxy), net_if);
    let driver_ref = &driver;

    let lowpan_device_task = register_and_serve_driver(name, registry, driver_ref).boxed_local();

    fx_log_info!("Registered Spinel LoWPAN device {:?}", name);

    let lowpan_device_factory_task = async move {
        if let Some(factory_registry) = factory_registry {
            if let Err(err) =
                register_and_serve_driver_factory(name, factory_registry, driver_ref).await
            {
                fx_log_warn!(
                    "Unable to register and serve factory commands for {:?}: {:?}",
                    name,
                    err
                );
            }
        }

        // If the factory interface throws an error, don't kill the driver;
        // just let the rest keep running.
        futures::future::pending::<Result<(), Error>>().await
    }
    .boxed_local();

    let driver_stream = driver.take_inbound_stream().try_collect::<()>();

    // All three of these tasks will run indefinitely
    // as long as there are no irrecoverable problems.
    //
    // And, yes, strangely the parenthesis seem
    // necessary, rustc complains about the `?` without
    // them.
    (futures::select! {
        ret = driver_stream.fuse() => ret,
        ret = lowpan_device_task.fuse() => ret,
        _ = lowpan_device_factory_task.fuse() => unreachable!(),
    })?;

    fx_log_info!("Spinel LoWPAN device {:?} has shutdown.", name);

    Ok(())
}

async fn connect_to_spinel_device_proxy_hack() -> Result<(Option<App>, SpinelDeviceProxy), Error> {
    use {std::fs::File, std::path::Path};
    const OT_PROTOCOL_PATH: &str = "/dev/class/ot-radio";

    let ot_radio_dir = File::open(OT_PROTOCOL_PATH).context("opening dir in devmgr")?;
    let directory_proxy = fidl_fuchsia_io::DirectoryProxy::new(
        fuchsia_async::Channel::from_channel(fdio::clone_channel(&ot_radio_dir)?)?,
    );

    let ot_radio_devices = files_async::readdir(&directory_proxy).await?;

    // Should have 1 device that implements OT_RADIO
    if ot_radio_devices.len() != 1 {
        return Err(format_err!("incorrect device number {}, shuold be 1", ot_radio_devices.len()));
    }

    let last_device: &files_async::DirEntry = ot_radio_devices.last().unwrap();

    let found_device_path = Path::new(OT_PROTOCOL_PATH).join(last_device.name.clone());

    fx_log_info!("device path {} got", found_device_path.to_str().unwrap());

    let file = File::open(found_device_path).context("err opening ot radio device")?;

    let spinel_device_setup_channel = fasync::Channel::from_channel(fdio::clone_channel(&file)?)?;
    let spinel_device_setup_proxy = SpinelDeviceSetupProxy::new(spinel_device_setup_channel);

    let (client_side, server_side) = fidl::endpoints::create_endpoints::<SpinelDeviceMarker>()?;

    spinel_device_setup_proxy
        .set_channel(server_side)
        .await?
        .map_err(|x| format_err!("spinel_device_setup.set_channel() returned error {}", x))?;

    Ok((None, client_side.into_proxy()?))
}

fn connect_to_spinel_device_proxy() -> Result<(Option<App>, SpinelDeviceProxy), Error> {
    let server_url = "fuchsia-pkg://fuchsia.com/ot-stack#meta/ot-stack.cmx".to_string();
    let arg = Some(vec!["/dev/class/ot-radio/000".to_string()]);
    let launcher = launcher().expect("Failed to open launcher service");
    let app = launch(&launcher, server_url, arg).expect("Failed to launch ot-stack service");
    let ot_stack_proxy = app
        .connect_to_protocol::<SpinelDeviceMarker>()
        .expect("Failed to connect to ot-stack service");
    Ok((Some(app), ot_stack_proxy))
}

fn connect_to_spinel_device_proxy_test() -> Result<(Option<App>, SpinelDeviceProxy), Error> {
    let ot_stack_proxy =
        connect_to_protocol::<SpinelDeviceMarker>().expect("Failed to connect to ot-stack service");
    Ok((None, ot_stack_proxy))
}

#[allow(unused)]
async fn prepare_to_run(
    args: &DriverArgs,
) -> Result<(Option<App>, LocalBoxFuture<'_, Result<(), Error>>), Error> {
    let (app, spinel_device) = if args.is_integration_test {
        connect_to_spinel_device_proxy_test().context("connect_to_spinel_device_proxy_test")?
    } else if args.use_ot_stack {
        connect_to_spinel_device_proxy().context("connect_to_spinel_device_proxy")?
    } else {
        connect_to_spinel_device_proxy_hack()
            .await
            .context("connect_to_spinel_device_proxy_hack")?
    };

    let network_device_interface = TunNetworkInterface::try_new(Some(args.name.clone()))
        .await
        .context("Unable to start TUN driver")?;

    let driver_future = run_driver(
        args.name.clone(),
        connect_to_protocol_at::<RegisterMarker>(args.service_prefix.as_str())
            .context("Failed to connect to Lowpan Registry service")?,
        connect_to_protocol_at::<FactoryRegisterMarker>(args.service_prefix.as_str()).ok(),
        spinel_device,
        network_device_interface,
    );

    Ok((app, driver_future.boxed_local()))
}

#[fasync::run_singlethreaded]
async fn main() -> Result<(), Error> {
    use std::path::Path;

    fuchsia_syslog::init_with_tags(&["lowpan-spinel-driver"]).context("initialize logging")?;

    if Path::new("/config/data/bootstrap_config.json").exists() {
        fx_log_err!("Bootstrapping ot-stack. Skipping lowpan-spinel-driver launch");
        return Ok(());
    }

    let args: DriverArgs = argh::from_env();
    let mut attempt_count = 0;

    loop {
        let (app, driver_future) = prepare_to_run(&args).await?;

        let start_timestamp = fasync::Time::now();

        let ret = if let Some(app) = app {
            futures::select! {
                ret = driver_future.fuse() => {
                    fx_log_err!("run_driver stopped: {:?}", ret);
                    ret
                },
                ret = app.wait_with_output().fuse() => {
                    let ret = ret.map(|out|out.ok());
                    fx_log_err!("ot-stack stopped: {:?}", ret);
                    ret.map(|_|())
                }
            }
        } else {
            driver_future.await
        };

        if (fasync::Time::now() - start_timestamp).into_minutes() >= 5 {
            // If the past run has been running for 5 minutes or longer,
            // then we go ahead and reset the attempt count.
            attempt_count = 0;
        }

        if args.max_auto_restarts <= attempt_count {
            break ret;
        }

        // Implement an exponential backoff for restarts.
        let delay = if attempt_count < 6 { 1 << attempt_count } else { 60 };

        fx_log_err!(
            "lowpan-spinel-driver unexpectedly shutdown. Will attempt to restart in {} seconds.",
            delay
        );

        fasync::Timer::new(fasync::Time::after(fuchsia_zircon::Duration::from_seconds(delay)))
            .await;

        attempt_count += 1;

        fx_log_info!(
            "lowpan-spinel-driver restart attempt {} ({} max)",
            attempt_count,
            args.max_auto_restarts
        );
    }
}
