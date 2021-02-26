// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The Archivist collects and stores diagnostic data from components.

#![warn(missing_docs)]

use {
    anyhow::{Context, Error},
    archivist_lib::{
        archivist, configs, diagnostics, events::sources::LogConnectorEventSource, logs,
    },
    argh::FromArgs,
    fdio::service_connect,
    fidl_fuchsia_sys_internal::LogConnectorMarker,
    fuchsia_async as fasync,
    fuchsia_component::client::connect_to_service,
    fuchsia_component::server::MissingStartupHandle,
    fuchsia_syslog, fuchsia_zircon as zx,
    std::path::PathBuf,
    tracing::{debug, error, info, warn},
};

/// Monitor, collect, and store diagnostics from components.
#[derive(Debug, Default, FromArgs)]
pub struct Args {
    /// disables proxying kernel logger
    #[argh(switch)]
    disable_klog: bool,

    /// disables log connector so that indivisual instances of
    /// observer don't compete for log connector listener.
    #[argh(switch)]
    disable_log_connector: bool,

    /// whether to connecto to event source or not.
    #[argh(switch)]
    disable_event_source: bool,

    /// initializes syslog library with a log socket to itself
    #[argh(switch)]
    consume_own_logs: bool,

    /// serve fuchsia.diagnostics.test.Controller
    #[argh(switch)]
    install_controller: bool,

    /// retrieve a fuchsia.process.Lifecycle handle from the runtime and listen to shutdown events
    #[argh(switch)]
    listen_to_lifecycle: bool,

    /// path to a JSON configuration file
    #[argh(option)]
    config_path: PathBuf,

    /// path to additional configuration for services to connect to
    #[argh(option)]
    service_config_path: Option<PathBuf>,
}

fn main() -> Result<(), Error> {
    let opt: Args = argh::from_env();

    let mut log_server = None;
    if opt.consume_own_logs {
        let (log_client, server) = zx::Socket::create(zx::SocketOpts::DATAGRAM)?;
        log_server = Some(server);
        fuchsia_syslog::init_with_socket_and_name(log_client, "archivist")?;
        info!("Logging started.");
        logs::redact::emit_canary();
    } else {
        fuchsia_syslog::init_with_tags(&["embedded"])?;
    }

    let mut executor = fasync::Executor::new()?;

    diagnostics::init();

    let archivist_configuration: configs::Config = match configs::parse_config(&opt.config_path) {
        Ok(config) => config,
        Err(parsing_error) => panic!("Parsing configuration failed: {}", parsing_error),
    };
    debug!("Configuration parsed.");

    let num_threads = archivist_configuration.num_threads;

    let mut archivist = archivist::ArchivistBuilder::new(archivist_configuration)?;
    debug!("Archivist initialized from configuration.");

    executor.run_singlethreaded(archivist.install_log_services());
    executor
        .run_singlethreaded(archivist.install_event_sources(!opt.disable_event_source))
        .context("failed to add event lifecycle event sources")?;

    if let Some(socket) = log_server {
        archivist.consume_own_logs(socket);
    }

    assert!(
        !(opt.install_controller && opt.listen_to_lifecycle),
        "only one shutdown mechanism can be specified."
    );

    if opt.install_controller {
        archivist.install_controller_service();
    }

    if opt.listen_to_lifecycle {
        archivist.install_lifecycle_listener();
    }

    if !opt.disable_log_connector {
        let connector = connect_to_service::<LogConnectorMarker>()?;
        executor.run_singlethreaded(
            archivist.add_event_source(
                "log_connector",
                Box::new(LogConnectorEventSource::new(connector)),
            ),
        );
    }

    if !opt.disable_klog {
        let debuglog = executor
            .run_singlethreaded(logs::KernelDebugLog::new())
            .context("Failed to read kernel logs")?;
        fasync::Task::spawn(archivist.data_repo().clone().drain_debuglog(debuglog)).detach();
    }

    let mut services = vec![];

    if let Some(service_config_path) = &opt.service_config_path {
        match configs::parse_service_config(service_config_path) {
            Err(e) => {
                error!("Couldn't parse service config: {}", e);
            }
            Ok(config) => {
                for name in config.service_list.iter() {
                    info!("Connecting to service {}", name);
                    let (local, remote) = zx::Channel::create().expect("cannot create channels");
                    match service_connect(&format!("/svc/{}", name), remote) {
                        Ok(_) => {
                            services.push(local);
                        }
                        Err(e) => {
                            error!("Couldn't connect to service {}: {:?}", name, e);
                        }
                    }
                }
            }
        }
    }

    let startup_handle =
        fuchsia_runtime::take_startup_handle(fuchsia_runtime::HandleType::DirectoryRequest.into())
            .ok_or(MissingStartupHandle)?;

    debug!("Running executor with {} threads.", num_threads);
    executor.run(archivist.run(zx::Channel::from(startup_handle)), num_threads)?;

    debug!("Exiting.");
    Ok(())
}
