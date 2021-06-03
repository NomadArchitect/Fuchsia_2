// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::constants::{get_socket, CURRENT_EXE_HASH, MDNS_BROADCAST_INTERVAL},
    crate::discovery::{TargetFinder, TargetFinderConfig},
    crate::events::{DaemonEvent, TargetInfo, WireTrafficType},
    crate::fastboot::{spawn_fastboot_discovery, Fastboot},
    crate::logger::streamer::GenericDiagnosticsStreamer,
    crate::manual_targets,
    crate::mdns::MdnsTargetFinder,
    crate::target::{
        target_addr_info_to_socketaddr, ConnectionState, RcsConnection, Target, TargetAddrEntry,
        TargetCollection, TargetEvent,
    },
    crate::target_control::TargetControl,
    crate::zedboot::zedboot_discovery,
    anyhow::{anyhow, Context, Result},
    ascendd::Ascendd,
    async_trait::async_trait,
    chrono::Utc,
    diagnostics_data::Timestamp,
    ffx_core::{build_info, TryStreamUtilExt},
    ffx_daemon_core::events::{self, EventHandler},
    ffx_daemon_services::create_service_register_map,
    fidl::endpoints::ClientEnd,
    fidl::endpoints::RequestStream,
    fidl::endpoints::ServiceMarker,
    fidl_fuchsia_developer_bridge::{
        DaemonError, DaemonMarker, DaemonRequest, DaemonRequestStream, DiagnosticsStreamError,
    },
    fidl_fuchsia_developer_remotecontrol::{
        ArchiveIteratorEntry, ArchiveIteratorError, ArchiveIteratorRequest, RemoteControlMarker,
    },
    fidl_fuchsia_overnet::{ServiceProviderRequest, ServiceProviderRequestStream},
    fidl_fuchsia_overnet_protocol::NodeId,
    fuchsia_async::{Task, TimeoutExt, Timer},
    futures::prelude::*,
    hoist::{hoist, OvernetInstance},
    services::{DaemonServiceProvider, ServiceError, ServiceRegister},
    std::cell::Cell,
    std::net::SocketAddr,
    std::rc::{Rc, Weak},
    std::time::{Duration, Instant},
};

// Daemon

// This is just for mocking config values for unit testing.
#[async_trait(?Send)]
trait ConfigReader: Send + Sync {
    async fn get(&self, q: &str) -> Result<Option<String>>;
}

#[derive(Default)]
struct DefaultConfigReader {}

#[async_trait(?Send)]
impl ConfigReader for DefaultConfigReader {
    async fn get(&self, q: &str) -> Result<Option<String>> {
        Ok(ffx_config::get(q).await?)
    }
}

pub struct DaemonEventHandler {
    // This must be a weak ref or else it will cause a circular reference.
    // Generally this should be the common practice for any handler pointing
    // to shared state, as it could be the handler's parent state that is
    // holding the event queue itself (as is the case with the target collection
    // here).
    target_collection: Weak<TargetCollection>,
    config_reader: Rc<dyn ConfigReader>,
}

impl DaemonEventHandler {
    fn new(target_collection: Weak<TargetCollection>) -> Self {
        Self { target_collection, config_reader: Rc::new(DefaultConfigReader::default()) }
    }

    fn handle_mdns<'a>(
        &self,
        t: TargetInfo,
        tc: &'a TargetCollection,
        cr: Weak<dyn ConfigReader>,
    ) -> impl Future<Output = ()> + 'a {
        log::trace!(
            "Found new target via mdns: {}",
            t.nodename.clone().unwrap_or("<unknown>".to_string())
        );
        let target = tc.merge_insert(Target::from_target_info(t.clone()));
        async move {
            let nodename = target.nodename();

            let t_clone = target.clone();
            let autoconnect_fut = async move {
                if let Some(cr) = cr.upgrade() {
                    if let Some(s) = cr.get("target.default").await.ok().flatten() {
                        if nodename.as_ref().map(|x| x == &s).unwrap_or(false) {
                            log::trace!(
                                "Doing autoconnect for default target: {}",
                                nodename.as_ref().map(|x| x.as_str()).unwrap_or("<unknown>")
                            );
                            t_clone.run_host_pipe();
                        }
                    }
                }
            };

            let _ = autoconnect_fut.await;

            // Updates state last so that if tasks are waiting on this state, everything is
            // already running and there aren't any races.
            target.update_connection_state(|s| match s {
                ConnectionState::Disconnected | ConnectionState::Mdns(_) => {
                    ConnectionState::Mdns(Instant::now())
                }
                _ => s,
            });
        }
    }

    async fn handle_overnet_peer(&self, node_id: u64, tc: &TargetCollection) {
        let rcs = match RcsConnection::new(&mut NodeId { id: node_id }) {
            Ok(rcs) => rcs,
            Err(e) => {
                log::error!("Target from Overnet {} failed to connect to RCS: {:?}", node_id, e);
                return;
            }
        };

        let target = match Target::from_rcs_connection(rcs).await {
            Ok(target) => target,
            Err(err) => {
                log::error!("Target from Overnet {} could not be identified: {:?}", node_id, err);
                return;
            }
        };

        log::trace!("Target from Overnet {} is {}", node_id, target.nodename_str());
        let target = tc.merge_insert(target);
        target.run_logger();
    }

    fn handle_fastboot(&self, t: TargetInfo, tc: &TargetCollection) {
        log::trace!(
            "Found new target via fastboot: {}",
            t.nodename.clone().unwrap_or("<unknown>".to_string())
        );
        let target = tc.merge_insert(Target::from_target_info(t.into()));
        target.update_connection_state(|s| match s {
            ConnectionState::Disconnected | ConnectionState::Fastboot(_) => {
                ConnectionState::Fastboot(Instant::now())
            }
            _ => s,
        });
    }

    async fn handle_zedboot(&self, t: TargetInfo, tc: &TargetCollection) {
        log::trace!(
            "Found new target via zedboot: {}",
            t.nodename.clone().unwrap_or("<unknown>".to_string())
        );
        let target = tc.merge_insert(Target::from_netsvc_target_info(t.into()));
        target.update_connection_state(|s| match s {
            ConnectionState::Disconnected | ConnectionState::Zedboot(_) => {
                ConnectionState::Zedboot(Instant::now())
            }
            _ => s,
        });
    }
}

#[async_trait(?Send)]
impl DaemonServiceProvider for Daemon {
    async fn open_service_proxy(&self, service_name: String) -> Result<fidl::Channel> {
        let (server, client) = fidl::Channel::create().context("creating zx channel")?;
        self.service_register
            .open(
                service_name,
                services::Context::new(self.clone()),
                fidl::AsyncChannel::from_channel(server)?,
            )
            .await?;
        Ok(client)
    }

    async fn open_target_proxy(
        &self,
        target_identifier: Option<String>,
        service_selector: fidl_fuchsia_diagnostics::Selector,
    ) -> Result<fidl::Channel> {
        let target = self
            .get_target(target_identifier)
            .await
            .map_err(|e| anyhow!("{:#?}", e))
            .context("getting default target")?;
        // Ensure auto-connect has at least started.
        target.run_host_pipe();
        target
            .events
            .wait_for(None, |e| e == TargetEvent::RcsActivated)
            .await
            .context("waiting for RCS activation")?;
        let rcs = target
            .rcs()
            .ok_or(anyhow!("rcs disconnected after event fired"))
            .context("getting rcs instance")?;
        let (server, client) = fidl::Channel::create().context("creating zx channel")?;

        // TODO(awdavies): Handle these errors properly so the client knows what happened.
        rcs.proxy
            .connect(service_selector, server)
            .await
            .context("FIDL connection")?
            .map_err(|e| anyhow!("{:#?}", e))
            .context("proxy connect")?;
        Ok(client)
    }
}

#[async_trait(?Send)]
impl EventHandler<DaemonEvent> for DaemonEventHandler {
    async fn on_event(&self, event: DaemonEvent) -> Result<bool> {
        let tc = match self.target_collection.upgrade() {
            Some(t) => t,
            None => {
                log::debug!("dropped target collection");
                return Ok(true); // We're done, as the parent has been dropped.
            }
        };

        log::info!("! DaemonEvent::{:?}", event);

        match event {
            DaemonEvent::WireTraffic(traffic) => match traffic {
                WireTrafficType::Mdns(t) => {
                    self.handle_mdns(t, &tc, Rc::downgrade(&self.config_reader)).await;
                }
                WireTrafficType::Fastboot(t) => {
                    self.handle_fastboot(t, &tc);
                }
                WireTrafficType::Zedboot(t) => {
                    self.handle_zedboot(t, &tc).await;
                }
            },
            DaemonEvent::OvernetPeer(node_id) => {
                self.handle_overnet_peer(node_id, &tc).await;
            }
            _ => (),
        }

        // This handler is never done unless the target_collection is dropped,
        // so always return false.
        Ok(false)
    }
}

#[derive(Clone)]
pub struct Daemon {
    event_queue: events::Queue<DaemonEvent>,
    target_collection: Rc<TargetCollection>,
    ascendd: Rc<Cell<Option<Ascendd>>>,
    manual_targets: Rc<dyn manual_targets::ManualTargets>,
    service_register: ServiceRegister,
    tasks: Vec<Rc<Task<()>>>,
}

impl Daemon {
    pub fn new() -> Daemon {
        let target_collection = Rc::new(TargetCollection::new());
        let event_queue = events::Queue::new(&target_collection);
        target_collection.set_event_queue(event_queue.clone());

        #[cfg(not(test))]
        let manual_targets = manual_targets::Config::default();
        #[cfg(test)]
        let manual_targets = manual_targets::Mock::default();

        Self {
            target_collection,
            event_queue,
            service_register: ServiceRegister::new(create_service_register_map()),
            ascendd: Rc::new(Cell::new(None)),
            manual_targets: Rc::new(manual_targets),
            tasks: Vec::new(),
        }
    }

    pub async fn start(&mut self) -> Result<()> {
        self.load_manual_targets().await;
        self.start_discovery().await?;
        self.start_ascendd().await?;
        self.start_target_expiry(Duration::from_secs(1));
        self.serve().await
    }

    /// Start all discovery tasks
    async fn start_discovery(&mut self) -> Result<()> {
        let daemon_event_handler = DaemonEventHandler::new(Rc::downgrade(&self.target_collection));
        self.event_queue.add_handler(daemon_event_handler).await;

        // TODO: these tasks could and probably should be managed by the daemon
        // instead of being detached.
        Daemon::spawn_onet_discovery(self.event_queue.clone());
        spawn_fastboot_discovery(self.event_queue.clone());
        self.tasks.push(Rc::new(zedboot_discovery(self.event_queue.clone())?));

        let config = TargetFinderConfig {
            interface_discovery_interval: Duration::from_secs(1),
            broadcast_interval: MDNS_BROADCAST_INTERVAL,
            mdns_ttl: 255,
        };
        let mut mdns = MdnsTargetFinder::new(&config)?;
        mdns.start(self.event_queue.clone())?;
        Ok(())
    }

    async fn start_ascendd(&mut self) -> Result<()> {
        // Start the ascendd socket only after we have registered our services.
        log::info!("Starting ascendd");

        let ascendd = Ascendd::new(
            ascendd::Opt { sockpath: Some(get_socket().await), ..Default::default() },
            // TODO: this just prints serial output to stdout - ffx probably wants to take a more
            // nuanced approach here.
            blocking::Unblock::new(std::io::stdout()),
        )?;

        self.ascendd.replace(Some(ascendd));

        Ok(())
    }

    fn start_target_expiry(&mut self, frequency: Duration) {
        let target_collection = Rc::downgrade(&self.target_collection);
        self.tasks.push(Rc::new(Task::local(async move {
            loop {
                Timer::new(frequency.clone()).await;

                match target_collection.upgrade() {
                    Some(target_collection) => {
                        for target in target_collection.targets() {
                            target.expire_state();
                        }
                    }
                    None => return,
                }
            }
        })))
    }

    async fn load_manual_targets(&self) {
        for str in self.manual_targets.get_or_default().await {
            let sa = match str.parse::<std::net::SocketAddr>() {
                Ok(sa) => sa,
                Err(e) => {
                    log::error!("Parse of manual target config failed: {}", e);
                    continue;
                }
            };
            self.add_manual_target(sa).await;
        }
    }

    async fn add_manual_target(&self, addr: SocketAddr) {
        let tae: TargetAddrEntry = (addr.into(), Utc::now(), true).into();

        let _ = self.manual_targets.add(format!("{}", addr)).await.map_err(|e| {
            log::error!("Unable to persist manual target: {:?}", e);
        });

        let target = Target::new_with_addr_entries(Option::<String>::None, Some(tae).into_iter());
        if addr.port() != 0 {
            target.set_ssh_port(Some(addr.port()));
        }

        target.update_connection_state(|s| match s {
            ConnectionState::Disconnected => ConnectionState::Manual,
            _ => s,
        });

        let target = self.target_collection.merge_insert(target);
        target.run_host_pipe();
    }

    /// get_target attempts to get the target that matches the match string if
    /// provided, otherwise the default target from the target collection.
    async fn get_target(&self, matcher: Option<String>) -> Result<Rc<Target>, DaemonError> {
        #[cfg(not(test))]
        const GET_TARGET_TIMEOUT: Duration = Duration::from_secs(8);
        #[cfg(test)]
        const GET_TARGET_TIMEOUT: Duration = Duration::from_secs(1);

        // TODO(72818): make target match timeout configurable / paramterable
        self.target_collection
            .wait_for_match(matcher)
            .on_timeout(GET_TARGET_TIMEOUT, || Err(DaemonError::TargetNotFound))
            .await
    }

    async fn handle_requests_from_stream(&self, stream: DaemonRequestStream) -> Result<()> {
        stream
            .map_err(|e| anyhow!("reading FIDL stream: {:#}", e))
            .try_for_each_concurrent_while_connected(None, |r| self.handle_request(r))
            .await
    }

    fn spawn_onet_discovery(queue: events::Queue<DaemonEvent>) {
        fuchsia_async::Task::local(async move {
            loop {
                let svc = match hoist().connect_as_service_consumer() {
                    Ok(svc) => svc,
                    Err(err) => {
                        log::info!("Overnet setup failed: {}, will retry in 1s", err);
                        Timer::new(Duration::from_secs(1)).await;
                        continue;
                    }
                };
                loop {
                    let peers = match svc.list_peers().await {
                        Ok(peers) => peers,
                        Err(err) => {
                            log::info!("Overnet peer discovery failed: {}, will retry", err);
                            Timer::new(Duration::from_secs(1)).await;
                            // break out of the peer discovery loop on error in
                            // order to reconnect, in case the error causes the
                            // overnet interface to go bad.
                            break;
                        }
                    };
                    for peer in peers {
                        let peer_has_rcs = peer.description.services.map_or(false, |s| {
                            s.iter().find(|name| *name == RemoteControlMarker::NAME).is_some()
                        });
                        if peer_has_rcs {
                            queue.push(DaemonEvent::OvernetPeer(peer.id.id)).unwrap_or_else(
                                |err| {
                                    log::warn!(
                                        "Overnet discovery failed to enqueue event: {}",
                                        err
                                    );
                                },
                            );
                        }
                    }
                    Timer::new(Duration::from_millis(100)).await;
                }
            }
        })
        .detach();
    }

    async fn handle_request(&self, req: DaemonRequest) -> Result<()> {
        log::debug!("daemon received request: {:?}", req);
        match req {
            DaemonRequest::Crash { .. } => panic!("instructed to crash by client!"),
            DaemonRequest::EchoString { value, responder } => {
                log::info!("Received echo request for string {:?}", value);
                responder.send(value.as_ref()).context("error sending response")?;
                log::info!("echo response sent successfully");
            }
            // Hang intends to block the reactor indefinitely, however
            // that's a little tricky to do exactly. This approximation
            // is strong enough for right now, though it may be awoken
            // again periodically on timers, depending on implementation
            // details of the underlying reactor.
            DaemonRequest::Hang { .. } => loop {
                std::thread::park()
            },
            DaemonRequest::ListTargets { value, responder } => {
                log::info!("Received list target request for '{:?}'", value);
                responder
                    .send(
                        &mut future::join_all(match value.as_ref() {
                            "" => self
                                .target_collection
                                .targets()
                                .drain(..)
                                .map(|t| {
                                    async move {
                                        if t.is_connected() {
                                            Some(t.as_ref().into())
                                        } else {
                                            None
                                        }
                                    }
                                    .boxed_local()
                                })
                                .collect(),
                            _ => match self.target_collection.get_connected(value) {
                                Some(t) => {
                                    vec![async move { Some(t.as_ref().into()) }.boxed_local()]
                                }
                                None => vec![],
                            },
                        })
                        .await
                        .drain(..)
                        .filter_map(|m| m)
                        .collect::<Vec<_>>()
                        .drain(..),
                    )
                    .context("error sending response")?;
            }
            DaemonRequest::GetRemoteControl { target, remote, responder } => {
                let target = match self.get_target(target).await {
                    Ok(t) => t,
                    Err(e) => {
                        responder.send(&mut Err(e)).context("sending error response")?;
                        return Ok(());
                    }
                };
                if matches!(target.get_connection_state(), ConnectionState::Fastboot(_)) {
                    let nodename = target.nodename().unwrap_or("<No Nodename>".to_string());
                    log::warn!("Attempting to connect to RCS on a fastboot target: {}", nodename);
                    responder
                        .send(&mut Err(DaemonError::TargetInFastboot))
                        .context("sending error response")?;
                    return Ok(());
                }
                if matches!(target.get_connection_state(), ConnectionState::Zedboot(_)) {
                    let nodename = target.nodename().unwrap_or("<No Nodename>".to_string());
                    log::warn!("Attempting to connect to RCS on a zedboot target: {}", nodename);
                    responder
                        .send(&mut Err(DaemonError::TargetInZedboot))
                        .context("sending error response")?;
                    return Ok(());
                }

                // Ensure auto-connect has at least started.
                target.run_host_pipe();
                match target.events.wait_for(None, |e| e == TargetEvent::RcsActivated).await {
                    Ok(()) => (),
                    Err(e) => {
                        log::warn!("{}", e);
                        responder
                            .send(&mut Err(DaemonError::RcsConnectionError))
                            .context("sending error response")?;
                        return Ok(());
                    }
                }
                let mut rcs = match target.rcs() {
                    Some(r) => r,
                    None => {
                        log::warn!("rcs dropped after event fired");
                        responder
                            .send(&mut Err(DaemonError::TargetStateError))
                            .context("sending error response")?;
                        return Ok(());
                    }
                };
                let mut response = rcs
                    .copy_to_channel(remote.into_channel())
                    .map_err(|_| DaemonError::RcsConnectionError);
                responder.send(&mut response).context("error sending response")?;
            }
            DaemonRequest::GetFastboot { target, fastboot, responder } => {
                let target = match self.get_target(target).await {
                    Ok(t) => t,
                    Err(e) => {
                        responder.send(&mut Err(e)).context("sending error response")?;
                        return Ok(());
                    }
                };
                let mut fastboot_manager = Fastboot::new(target);
                let stream = fastboot.into_stream()?;
                fuchsia_async::Task::local(async move {
                    match fastboot_manager.0.handle_fastboot_requests_from_stream(stream).await {
                        Ok(_) => log::debug!("Fastboot proxy finished - client disconnected"),
                        Err(e) => {
                            log::error!("There was an error handling fastboot requests: {:?}", e)
                        }
                    }
                })
                .detach();
                responder.send(&mut Ok(())).context("error sending response")?;
            }
            DaemonRequest::Quit { responder } => {
                log::info!("Received quit request.");

                match std::fs::remove_file(get_socket().await) {
                    Ok(()) => {}
                    Err(e) => log::error!("failed to remove socket file: {}", e),
                }

                if cfg!(test) {
                    panic!("quit() should not be invoked in test code");
                }

                self.service_register
                    .shutdown(services::Context::new(self.clone()))
                    .await
                    .unwrap_or_else(|e| log::error!("shutting down service register: {:?}", e));

                // It is desirable for the client to receive an ACK for the quit
                // request. As Overnet has a potentially complicated routing
                // path, it is tricky to implement some notion of a bounded
                // "flush" for this response, however in practice it is only
                // necessary here to wait long enough for the message to likely
                // leave the local process before exiting. Enqueue a detached
                // timer to shut down the daemon before sending the response.
                // This is detached because once the client receives the
                // response, the client will disconnect it's socket. If the
                // local reactor observes this disconnection before the timer
                // expires, an in-line timer wait would never fire, and the
                // daemon would never exit.
                Task::local(
                    Timer::new(std::time::Duration::from_millis(20)).map(|_| std::process::exit(0)),
                )
                .detach();

                responder.send(true).context("error sending response")?;
            }
            DaemonRequest::GetSshAddress { responder, target, timeout } => {
                use std::convert::TryInto;
                let timeout_time = Instant::now()
                    + Duration::from_nanos(timeout.try_into().context("invalid timeout")?);

                loop {
                    match self
                        .target_collection
                        .wait_for_match(target.clone())
                        .on_timeout(timeout_time, || Err(DaemonError::Timeout))
                        .await
                    {
                        Ok(t) => match t.get_connection_state() {
                            ConnectionState::Zedboot(_) => {
                                return responder
                                    .send(&mut Err(DaemonError::TargetInZedboot))
                                    .context("error sending response");
                            }
                            ConnectionState::Fastboot(_) => {
                                return responder
                                    .send(&mut Err(DaemonError::TargetInFastboot))
                                    .context("error sending response");
                            }
                            _ => {
                                if let Some(addr_info) = t.ssh_address_info() {
                                    log::info!("Responding to {:?} with {:?}", target, t);
                                    return responder
                                        .send(&mut Ok(addr_info))
                                        .context("error sending response");
                                }
                            }
                        },
                        Err(e) => {
                            return responder.send(&mut Err(e)).context("error sending response");
                        }
                    }
                }
            }
            DaemonRequest::GetVersionInfo { responder } => {
                return responder.send(build_info()).context("sending GetVersionInfo response");
            }
            DaemonRequest::AddTarget { ip, responder } => {
                self.add_manual_target(target_addr_info_to_socketaddr(ip)).await;
                responder.send(&mut Ok(())).context("error sending response")?;
            }
            DaemonRequest::RemoveTarget { target_id, responder } => {
                let target = self.target_collection.get(target_id.clone());
                if let Some(target) = target {
                    let ssh_port = target.ssh_port();
                    for addr in target.manual_addrs() {
                        let mut sockaddr = SocketAddr::from(addr);
                        ssh_port.map(|p| sockaddr.set_port(p));
                        let _ = self.manual_targets.remove(format!("{}", sockaddr)).await.map_err(
                            |e| {
                                log::error!("Unable to persist target removal: {}", e);
                            },
                        );
                    }
                }
                let result = self.target_collection.remove_target(target_id.clone());
                responder.send(&mut Ok(result)).context("error sending response")?;
            }
            DaemonRequest::GetHash { responder } => {
                let hash: String =
                    ffx_config::get((CURRENT_EXE_HASH, ffx_config::ConfigLevel::Runtime)).await?;
                responder.send(&hash).context("error sending response")?;
            }
            DaemonRequest::ConnectToService { name, server_channel, responder } => {
                match self
                    .service_register
                    .open(
                        name,
                        services::Context::new(self.clone()),
                        fidl::AsyncChannel::from_channel(server_channel)?,
                    )
                    .await
                {
                    Ok(()) => responder.send(&mut Ok(())).context("fidl response")?,
                    Err(e) => {
                        log::error!("{}", e);
                        match e {
                            ServiceError::NoServiceFound(_) => {
                                responder.send(&mut Err(DaemonError::ServiceNotFound))?
                            }
                            ServiceError::StreamOpenError(_) => {
                                responder.send(&mut Err(DaemonError::ServiceOpenError))?
                            }
                            ServiceError::BadRegisterState(_)
                            | ServiceError::DuplicateTaskId(..) => {
                                responder.send(&mut Err(DaemonError::BadServiceRegisterState))?
                            }
                        }
                    }
                }
            }
            DaemonRequest::StreamDiagnostics { target, parameters, iterator, responder } => {
                let target = match self
                    .get_target(target.clone())
                    .on_timeout(Duration::from_secs(3), || Err(DaemonError::Timeout))
                    .await
                {
                    Ok(t) => t,
                    Err(DaemonError::Timeout) => {
                        responder
                            .send(&mut Err(DiagnosticsStreamError::NoMatchingTargets))
                            .context("sending error response")?;
                        return Ok(());
                    }
                    Err(e) => {
                        log::warn!(
                            "got error fetching target with filter '{}': {:?}",
                            target.as_ref().unwrap_or(&String::default()),
                            e
                        );
                        responder
                            .send(&mut Err(DiagnosticsStreamError::TargetMatchFailed))
                            .context("sending error response")?;
                        return Ok(());
                    }
                };

                let stream = target.stream_info();
                match stream
                    .wait_for_setup()
                    .map(|_| Ok(()))
                    .on_timeout(Duration::from_secs(3), || {
                        Err(DiagnosticsStreamError::NoStreamForTarget)
                    })
                    .await
                {
                    Ok(_) => {}
                    Err(e) => {
                        // TODO(jwing): we should be able to interact with inactive targets here for
                        // stream modes that don't involve subscription.
                        return responder.send(&mut Err(e)).context("sending error response");
                    }
                }

                if parameters.stream_mode.is_none() {
                    log::info!("StreamDiagnostics failed: stream mode is required");
                    return responder
                        .send(&mut Err(DiagnosticsStreamError::MissingParameter))
                        .context("sending missing parameter response");
                }

                let mut log_iterator = stream
                    .stream_entries(
                        parameters.stream_mode.unwrap(),
                        parameters.min_target_timestamp_nanos.map(|t| Timestamp::from(t as i64)),
                    )
                    .await?;
                let task = Task::local(async move {
                    let mut iter_stream = iterator.into_stream()?;

                    while let Some(request) = iter_stream.next().await {
                        match request? {
                            ArchiveIteratorRequest::GetNext { responder } => {
                                let res = log_iterator.iter().await?;
                                match res {
                                    Some(Ok(entry)) => {
                                        // TODO(jwing): implement truncation or migrate to a socket-based
                                        // API.
                                        responder.send(&mut Ok(vec![ArchiveIteratorEntry {
                                            data: Some(serde_json::to_string(&entry)?),
                                            truncated_chars: Some(0),
                                            ..ArchiveIteratorEntry::EMPTY
                                        }]))?;
                                    }
                                    Some(Err(e)) => {
                                        log::warn!("got error streaming diagnostics: {}", e);
                                        responder
                                            .send(&mut Err(ArchiveIteratorError::DataReadFailed))?;
                                    }
                                    None => {
                                        responder.send(&mut Ok(vec![]))?;
                                        break;
                                    }
                                }
                            }
                        }
                    }

                    Ok::<(), anyhow::Error>(())
                });
                responder.send(&mut Ok(()))?;
                task.await?;
            }
            DaemonRequest::GetTarget { target, target_controller, responder } => {
                let target = match self.get_target(target).await {
                    Ok(t) => t,
                    Err(e) => {
                        responder.send(&mut Err(e)).context("sending error response")?;
                        return Ok(());
                    }
                };
                let mut target_control_server = TargetControl::new(target);
                let stream = target_controller.into_stream()?;
                fuchsia_async::Task::local(async move {
                    match target_control_server.handle_requests_from_stream(stream).await {
                        Ok(_) => log::debug!("Target Control proxy finished - client disconnected"),
                        Err(e) => {
                            log::error!("There was an error handling fastboot requests: {:?}", e)
                        }
                    }
                })
                .detach();
                responder.send(&mut Ok(())).context("error sending response")?;
            }
        }
        Ok(())
    }

    async fn serve(&self) -> Result<()> {
        let (s, p) = fidl::Channel::create().context("failed to create zx channel")?;
        let chan = fidl::AsyncChannel::from_channel(s).context("failed to make async channel")?;
        let mut stream = ServiceProviderRequestStream::from_channel(chan);

        log::info!("Starting daemon overnet server");
        hoist::hoist().publish_service(DaemonMarker::NAME, ClientEnd::new(p))?;

        while let Some(ServiceProviderRequest::ConnectToService {
            chan,
            info: _,
            control_handle: _control_handle,
        }) = stream.try_next().await.context("error running service provider server")?
        {
            log::trace!("Received service request for service");
            let chan =
                fidl::AsyncChannel::from_channel(chan).context("failed to make async channel")?;
            let daemon_clone = self.clone();
            Task::local(async move {
                daemon_clone
                    .handle_requests_from_stream(DaemonRequestStream::from_channel(chan))
                    .await
                    .unwrap_or_else(|err| panic!("fatal error handling request: {:?}", err));
            })
            .detach();
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        addr::TargetAddr,
        fidl_fuchsia_developer_bridge::{
            self as bridge, DaemonMarker, DaemonProxy, TargetAddrInfo, TargetIpPort,
        },
        fidl_fuchsia_developer_remotecontrol::{
            self as rcs, RemoteControlMarker, RemoteControlProxy,
        },
        fidl_fuchsia_net::{IpAddress, Ipv6Address, Subnet},
        fuchsia_async::Task,
        matches::assert_matches,
        std::collections::BTreeSet,
        std::iter::FromIterator,
        std::net::SocketAddr,
    };

    fn setup_fake_target_service(
        nodename: String,
        addrs: Vec<SocketAddr>,
    ) -> (RemoteControlProxy, Task<()>) {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<RemoteControlMarker>().unwrap();

        let task = Task::local(async move {
            while let Ok(Some(req)) = stream.try_next().await {
                match req {
                    rcs::RemoteControlRequest::IdentifyHost { responder } => {
                        let addrs = addrs
                            .iter()
                            .map(|addr| Subnet {
                                addr: match addr.ip() {
                                    std::net::IpAddr::V4(i) => fidl_fuchsia_net::IpAddress::Ipv4(
                                        fidl_fuchsia_net::Ipv4Address { addr: i.octets() },
                                    ),
                                    std::net::IpAddr::V6(i) => fidl_fuchsia_net::IpAddress::Ipv6(
                                        fidl_fuchsia_net::Ipv6Address { addr: i.octets() },
                                    ),
                                },
                                // XXX: fictitious.
                                prefix_len: 24,
                            })
                            .collect();
                        let nodename = Some(nodename.clone());
                        responder
                            .send(&mut Ok(rcs::IdentifyHostResponse {
                                nodename,
                                addresses: Some(addrs),
                                ..rcs::IdentifyHostResponse::EMPTY
                            }))
                            .context("sending testing response")
                            .unwrap();
                    }
                    _ => assert!(false),
                }
            }
        });

        (proxy, task)
    }

    fn spawn_test_daemon() -> (DaemonProxy, Daemon, Task<Result<()>>) {
        let d = Daemon::new();

        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<DaemonMarker>().unwrap();

        let d2 = d.clone();
        let task = Task::local(async move { d2.handle_requests_from_stream(stream).await });

        (proxy, d, task)
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_multiple_connected_targets_none_query_is_ambiguous() {
        let (proxy, daemon, _task) = spawn_test_daemon();
        daemon.target_collection.merge_insert(Target::new_autoconnected("bazmumble"));
        daemon.target_collection.merge_insert(Target::new_autoconnected("foobar"));
        let (_, server_end) = fidl::endpoints::create_proxy::<RemoteControlMarker>().unwrap();

        assert_matches!(
            proxy.get_remote_control(None, server_end).await.unwrap(),
            Err(DaemonError::TargetAmbiguous)
        );
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_multiple_connected_targets_empty_query_is_ambiguous() {
        let (proxy, daemon, _task) = spawn_test_daemon();
        daemon.target_collection.merge_insert(Target::new_autoconnected("bazmumble"));
        daemon.target_collection.merge_insert(Target::new_autoconnected("foobar"));
        let (_, server_end) = fidl::endpoints::create_proxy::<RemoteControlMarker>().unwrap();

        assert_matches!(
            proxy.get_remote_control(Some(""), server_end).await.unwrap(),
            Err(DaemonError::TargetAmbiguous)
        );
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_get_remote_control() {
        let (proxy, daemon, _task) = spawn_test_daemon();
        let (_, server_end) = fidl::endpoints::create_proxy::<RemoteControlMarker>().unwrap();

        // get_remote_control only returns targets that have an RCS connection.
        let (rcs_proxy, _task) =
            setup_fake_target_service("foobar".to_string(), vec!["[fe80::1%1]:0".parse().unwrap()]);
        daemon.target_collection.merge_insert(
            Target::from_rcs_connection(RcsConnection::new_with_proxy(
                rcs_proxy,
                &NodeId { id: 0u64 },
            ))
            .await
            .unwrap(),
        );

        assert_matches!(proxy.get_remote_control(Some("foobar"), server_end).await.unwrap(), Ok(_));
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_multiple_connected_targets_with_non_matching_query() {
        let (proxy, daemon, _task) = spawn_test_daemon();
        daemon.target_collection.merge_insert(Target::new_autoconnected("bazmumble"));
        daemon.target_collection.merge_insert(Target::new_autoconnected("foobar"));
        let (_, server_end) = fidl::endpoints::create_proxy::<RemoteControlMarker>().unwrap();

        assert_matches!(
            proxy.get_remote_control(Some("doesnotexist"), server_end).await.unwrap(),
            Err(DaemonError::TargetNotFound)
        );
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_list_targets_mdns_discovery() {
        let (proxy, daemon, _task) = spawn_test_daemon();
        daemon.target_collection.merge_insert(Target::new_autoconnected("foobar"));
        daemon.target_collection.merge_insert(Target::new_autoconnected("baz"));
        daemon.target_collection.merge_insert(Target::new_autoconnected("quux"));

        let res = proxy.list_targets("").await.unwrap();

        // Daemon server contains one fake target plus these two.
        assert_eq!(res.len(), 3);

        let has_nodename =
            |v: &Vec<bridge::Target>, s: &str| v.iter().any(|x| x.nodename.as_ref().unwrap() == s);
        assert!(has_nodename(&res, "foobar"));
        assert!(has_nodename(&res, "baz"));
        assert!(has_nodename(&res, "quux"));

        let res = proxy.list_targets("mlorp").await.unwrap();
        assert!(!has_nodename(&res, "foobar"));
        assert!(!has_nodename(&res, "baz"));
        assert!(!has_nodename(&res, "quux"));

        let res = proxy.list_targets("foobar").await.unwrap();
        assert!(has_nodename(&res, "foobar"));
        assert!(!has_nodename(&res, "baz"));
        assert!(!has_nodename(&res, "quux"));

        let res = proxy.list_targets("baz").await.unwrap();
        assert!(!has_nodename(&res, "foobar"));
        assert!(has_nodename(&res, "baz"));
        assert!(!has_nodename(&res, "quux"));

        let res = proxy.list_targets("quux").await.unwrap();
        assert!(!has_nodename(&res, "foobar"));
        assert!(!has_nodename(&res, "baz"));
        assert!(has_nodename(&res, "quux"));
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_get_ssh_address() {
        let (proxy, daemon, _task) = spawn_test_daemon();

        let target = Target::new_autoconnected("foobar");
        target.addrs_insert(TargetAddr::new("[fe80::1%1]:0").unwrap());
        let addr_info = target.ssh_address_info().unwrap();

        daemon.target_collection.merge_insert(target);

        assert_eq!(daemon.target_collection.targets().len(), 1);
        assert!(daemon.target_collection.get("foobar").is_some());

        let r = proxy.get_ssh_address(Some("foobar"), std::i64::MAX).await.unwrap();
        assert_eq!(r, Ok(addr_info));

        let r = proxy.get_ssh_address(Some("toothpaste"), 1).await.unwrap();
        assert_eq!(r, Err(DaemonError::Timeout));
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_get_ssh_address_target_ambiguous() {
        let (proxy, daemon, _task) = spawn_test_daemon();

        let target = Target::new_autoconnected("foobar");
        target.addrs_insert(TargetAddr::new("[fe80::1%1]:0").unwrap());
        daemon.target_collection.merge_insert(target);

        let target = Target::new_autoconnected("whoistarget");
        target.addrs_insert(TargetAddr::new("[fe80::2%1]:0").unwrap());
        daemon.target_collection.merge_insert(target);

        assert_eq!(daemon.target_collection.targets().len(), 2);
        assert!(daemon.target_collection.get("foobar").is_some());
        assert!(daemon.target_collection.get("whoistarget").is_some());

        let r = proxy.get_ssh_address(None, std::i64::MAX).await.unwrap();
        assert_eq!(r, Err(DaemonError::TargetAmbiguous));
    }

    struct FakeConfigReader {
        query_expected: String,
        value: String,
    }

    #[async_trait(?Send)]
    impl ConfigReader for FakeConfigReader {
        async fn get(&self, q: &str) -> Result<Option<String>> {
            assert_eq!(q, self.query_expected);
            Ok(Some(self.value.clone()))
        }
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_daemon_mdns_event_handler() {
        let t = Target::new_named("this-town-aint-big-enough-for-the-three-of-us");

        let tc = Rc::new(TargetCollection::new());
        tc.merge_insert(t.clone());

        let handler = DaemonEventHandler {
            target_collection: Rc::downgrade(&tc),
            config_reader: Rc::new(FakeConfigReader {
                query_expected: "target.default".to_owned(),
                value: "florp".to_owned(),
            }),
        };
        assert!(!handler
            .on_event(DaemonEvent::WireTraffic(WireTrafficType::Mdns(TargetInfo {
                nodename: Some(t.nodename().expect("mdns target should always have a name.")),
                ..Default::default()
            })))
            .await
            .unwrap());

        // This is a bit of a hack to check the target state without digging
        // into internals. This also avoids running into a race waiting for an
        // event to be propagated.
        t.update_connection_state(|s| {
            if let ConnectionState::Mdns(ref _t) = s {
                s
            } else {
                panic!("state not updated by event, should be set to MDNS state.");
            }
        });

        assert!(!t.is_host_pipe_running());

        // This handler will now return the value of the default target as
        // intended.
        let handler = DaemonEventHandler {
            target_collection: Rc::downgrade(&tc),
            config_reader: Rc::new(FakeConfigReader {
                query_expected: "target.default".to_owned(),
                value: "this-town-aint-big-enough-for-the-three-of-us".to_owned(),
            }),
        };
        assert!(!handler
            .on_event(DaemonEvent::WireTraffic(WireTrafficType::Mdns(TargetInfo {
                nodename: Some(t.nodename().expect("Handling Mdns traffic for unnamed node")),
                ..Default::default()
            })))
            .await
            .unwrap());

        while !t.is_host_pipe_running() {
            Timer::new(Duration::from_millis(10)).await
        }

        // TODO(awdavies): RCS, Fastboot, etc. events.
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_add_target() {
        let (proxy, daemon, _task) = spawn_test_daemon();
        let target_addr = TargetAddr::new("[::1]:0").unwrap();
        let event = daemon.event_queue.wait_for(None, |e| matches!(e, DaemonEvent::NewTarget(_)));

        proxy.add_target(&mut target_addr.into()).await.unwrap().unwrap();

        assert!(event.await.is_ok());
        let target = daemon.target_collection.get(target_addr.to_string()).unwrap();
        assert_eq!(target.addrs().len(), 1);
        assert_eq!(target.addrs().into_iter().next(), Some(target_addr));
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_add_target_with_port() {
        let (proxy, daemon, _task) = spawn_test_daemon();
        let target_addr = TargetAddr::new("[::1]:8022").unwrap();
        let event = daemon.event_queue.wait_for(None, |e| matches!(e, DaemonEvent::NewTarget(_)));

        proxy.add_target(&mut target_addr.into()).await.unwrap().unwrap();

        assert!(event.await.is_ok());
        let target = daemon.target_collection.get(target_addr.to_string()).unwrap();
        assert_eq!(target.addrs().len(), 1);
        assert_eq!(target.addrs().into_iter().next(), Some(target_addr));
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_get_target_empty() {
        let d = Daemon::new();
        let nodename = "where-is-my-hasenpfeffer";
        let t = Target::new_autoconnected(nodename);
        d.target_collection.merge_insert(t.clone());
        assert_eq!(nodename, d.get_target(None).await.unwrap().nodename().unwrap());
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_get_target_query() {
        let d = Daemon::new();
        let nodename = "where-is-my-hasenpfeffer";
        let t = Target::new_autoconnected(nodename);
        d.target_collection.merge_insert(t.clone());
        assert_eq!(
            nodename,
            d.get_target(Some(nodename.to_string())).await.unwrap().nodename().unwrap()
        );
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_get_target_ambiguous() {
        let d = Daemon::new();
        let t = Target::new_autoconnected("where-is-my-hasenpfeffer");
        let t2 = Target::new_autoconnected("it-is-rabbit-season");
        d.target_collection.merge_insert(t.clone());
        d.target_collection.merge_insert(t2.clone());
        assert_eq!(Err(DaemonError::TargetAmbiguous), d.get_target(None).await);
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_rcs_on_fastboot_error_msg() {
        let (proxy, daemon, _task) = spawn_test_daemon();
        let target = Target::new_with_serial("abc");
        daemon.target_collection.merge_insert(target);
        let (_, server_end) = fidl::endpoints::create_proxy::<RemoteControlMarker>().unwrap();

        let result = proxy.get_remote_control(None, server_end).await.unwrap();

        assert_matches!(result, Err(DaemonError::TargetInFastboot));
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_rcs_on_zedboot_error_msg() {
        let (proxy, daemon, _task) = spawn_test_daemon();
        let target = Target::new_with_netsvc_addrs(
            Some("abc"),
            BTreeSet::from_iter(vec![TargetAddr::new("[fe80::1%1]:22").unwrap()].into_iter()),
        );
        daemon.target_collection.merge_insert(target);
        let (_, server_end) = fidl::endpoints::create_proxy::<RemoteControlMarker>().unwrap();

        let result = proxy.get_remote_control(None, server_end).await.unwrap();

        assert_matches!(result, Err(DaemonError::TargetInZedboot));
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_persisted_manual_target_load() {
        let daemon = Daemon::new();
        daemon.manual_targets.add("127.0.0.1:8022".to_string()).await.unwrap();

        // This happens in daemon.start(), but we want to avoid binding the
        // network sockets in unit tests, thus not calling start.
        daemon.load_manual_targets().await;

        let target = daemon.get_target(Some("127.0.0.1:8022".to_string())).await.unwrap();
        assert_eq!(target.ssh_address(), Some("127.0.0.1:8022".parse::<SocketAddr>().unwrap()));
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_persisted_manual_target_add() {
        let (proxy, daemon, _task) = spawn_test_daemon();
        daemon.load_manual_targets().await;

        let r = proxy
            .add_target(&mut TargetAddrInfo::IpPort(TargetIpPort {
                ip: IpAddress::Ipv6(Ipv6Address {
                    addr: [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
                }),
                port: 8022,
                scope_id: 1,
            }))
            .await
            .unwrap();

        assert_eq!(r, Ok(()));

        assert_eq!(1, daemon.target_collection.targets().len());

        assert_eq!(daemon.manual_targets.get().await.unwrap(), vec!["[fe80::1%1]:8022"]);
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_persisted_manual_target_remove() {
        let (proxy, daemon, _task) = spawn_test_daemon();
        daemon.manual_targets.add("127.0.0.1:8022".to_string()).await.unwrap();
        daemon.load_manual_targets().await;

        assert_eq!(1, daemon.target_collection.targets().len());

        let r = proxy.remove_target("127.0.0.1:8022").await.unwrap();

        assert_eq!(r, Ok(true));

        assert_eq!(0, daemon.target_collection.targets().len());

        assert_eq!(daemon.manual_targets.get_or_default().await, Vec::<String>::new());
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_target_expiry() {
        let mut daemon = Daemon::new();
        let target = Target::new_named("goodbye-world");
        let then = Instant::now() - Duration::from_secs(10);
        target.update_connection_state(|_| ConnectionState::Mdns(then));
        daemon.target_collection.merge_insert(target.clone());

        assert_eq!(ConnectionState::Mdns(then), target.get_connection_state());

        daemon.start_target_expiry(Duration::from_millis(1));

        while target.get_connection_state() == ConnectionState::Mdns(then) {
            futures_lite::future::yield_now().await
        }

        assert_eq!(ConnectionState::Disconnected, target.get_connection_state());
    }
}
