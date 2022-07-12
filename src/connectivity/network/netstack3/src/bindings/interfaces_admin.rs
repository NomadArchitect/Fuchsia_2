// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::ops::DerefMut as _;

use fidl::endpoints::ProtocolMarker;
use fidl_fuchsia_hardware_network as fhardware_network;
use fidl_fuchsia_net_interfaces_admin as fnet_interfaces_admin;
use fuchsia_async as fasync;
use fuchsia_zircon as zx;

use futures::{
    future::FusedFuture as _, FutureExt as _, SinkExt as _, StreamExt as _, TryFutureExt as _,
    TryStreamExt as _,
};
use netstack3_core::Ctx;

use crate::bindings::{
    devices, netdevice_worker, BindingId, InterfaceControl as _, Netstack, NetstackContext,
};

pub(crate) fn serve(
    ns: Netstack,
    req: fnet_interfaces_admin::InstallerRequestStream,
) -> impl futures::Stream<Item = Result<fasync::Task<()>, fidl::Error>> {
    req.map_ok(
        move |fnet_interfaces_admin::InstallerRequest::InstallDevice {
                  device,
                  device_control,
                  control_handle: _,
              }| {
            fasync::Task::spawn(
                run_device_control(
                    ns.clone(),
                    device,
                    device_control.into_stream().expect("failed to obtain stream"),
                )
                .map(|r| r.unwrap_or_else(|e| log::warn!("device control finished with {:?}", e))),
            )
        },
    )
}

#[derive(thiserror::Error, Debug)]
enum DeviceControlError {
    #[error("worker error: {0}")]
    Worker(#[from] netdevice_worker::Error),
    #[error("fidl error: {0}")]
    Fidl(#[from] fidl::Error),
}

async fn run_device_control(
    ns: Netstack,
    device: fidl::endpoints::ClientEnd<fhardware_network::DeviceMarker>,
    device_control: fnet_interfaces_admin::DeviceControlRequestStream,
) -> Result<(), DeviceControlError> {
    let worker = netdevice_worker::NetdeviceWorker::new(ns.ctx.clone(), device).await?;
    let handler = worker.new_handler();
    let worker_fut = worker.run().map_err(DeviceControlError::Worker);
    let stop_event = async_utils::event::Event::new();
    let control_stream = device_control
        .take_until(stop_event.wait_or_dropped())
        .map_err(DeviceControlError::Fidl)
        .try_filter_map(|req| match req {
            fnet_interfaces_admin::DeviceControlRequest::CreateInterface {
                port,
                control,
                options,
                control_handle: _,
            } => create_interface(
                port,
                control,
                options,
                &ns,
                &handler,
                stop_event.wait_or_dropped(),
            ),
            fnet_interfaces_admin::DeviceControlRequest::Detach { control_handle: _ } => {
                todo!("https://fxbug.dev/100867 support detach");
            }
        });
    futures::pin_mut!(worker_fut);
    futures::pin_mut!(control_stream);
    let mut tasks = futures::stream::FuturesUnordered::new();
    let res = loop {
        let mut tasks_fut = if tasks.is_empty() {
            futures::future::pending().left_future()
        } else {
            tasks.by_ref().next().right_future()
        };
        let result = futures::select! {
            r = control_stream.try_next() => r,
            r = worker_fut => match r {
                Ok(never) => match never {},
                Err(e) => Err(e)
            },
            ready_task = tasks_fut => {
                let () = ready_task.unwrap_or_else(|| ());
                continue;
            }
        };
        match result {
            Ok(Some(task)) => tasks.push(task),
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        }
    };

    // Send a stop signal to all tasks.
    assert!(stop_event.signal(), "event was already signaled");
    match &res {
        // Control stream has finished, don't need to drain it.
        Ok(()) | Err(DeviceControlError::Fidl(_)) => (),
        Err(DeviceControlError::Worker(_)) => {
            // Drain control stream to make sure we have all the tasks. The stop
            // trigger will make it stop operating on new requests.
            control_stream
                .try_for_each(|t| futures::future::ok(tasks.push(t)))
                .await
                .unwrap_or_else(|e| log::warn!("failed to accumulate remaining tasks: {:?}", e));
        }
    }
    // Run all the tasks to completion. We sent the stop signal, they should all
    // complete and perform interface cleanup.
    tasks.collect::<()>().await;

    res
}

const INTERFACES_ADMIN_CHANNEL_SIZE: usize = 16;
/// A wrapper over `fuchsia.net.interfaces.admin/Control` handles to express ownership semantics.
///
/// If `owns_interface` is true, this handle 'owns' the interfaces, and when the handle closes the
/// interface should be removed.
pub struct OwnedControlHandle {
    request_stream: fnet_interfaces_admin::ControlRequestStream,
    control_handle: fnet_interfaces_admin::ControlControlHandle,
    owns_interface: bool,
}

impl OwnedControlHandle {
    pub(crate) fn new_unowned(
        handle: fidl::endpoints::ServerEnd<fnet_interfaces_admin::ControlMarker>,
    ) -> OwnedControlHandle {
        let (stream, control) =
            handle.into_stream_and_control_handle().expect("failed to decompose control handle");
        OwnedControlHandle {
            request_stream: stream,
            control_handle: control,
            owns_interface: false,
        }
    }

    // Constructs a new channel of `OwnedControlHandle` with no owner.
    pub(crate) fn new_channel() -> (
        futures::channel::mpsc::Sender<OwnedControlHandle>,
        futures::channel::mpsc::Receiver<OwnedControlHandle>,
    ) {
        futures::channel::mpsc::channel(INTERFACES_ADMIN_CHANNEL_SIZE)
    }

    // Constructs a new channel of `OwnedControlHandle` with the given handle as the owner.
    // TODO(https://fxbug.dev/87963): This currently enforces that there is only ever one owner,
    // which will need to be revisited to implement `Clone`.
    pub(crate) async fn new_channel_with_owned_handle(
        handle: fidl::endpoints::ServerEnd<fnet_interfaces_admin::ControlMarker>,
    ) -> (
        futures::channel::mpsc::Sender<OwnedControlHandle>,
        futures::channel::mpsc::Receiver<OwnedControlHandle>,
    ) {
        let (mut sender, receiver) = Self::new_channel();
        let (stream, control) =
            handle.into_stream_and_control_handle().expect("failed to decompose control handle");
        sender
            .send(OwnedControlHandle {
                request_stream: stream,
                control_handle: control,
                owns_interface: true,
            })
            .await
            .expect("failed to attach initial control handle");
        (sender, receiver)
    }
}

/// Operates a fuchsia.net.interfaces.admin/DeviceControl.CreateInterface
/// request.
///
/// Returns `Ok(Some(fuchsia_async::Task))` if an interface was created
/// successfully. The returned `Task` must be polled to completion and is tied
/// to the created interface's lifetime.
async fn create_interface(
    port: fhardware_network::PortId,
    control: fidl::endpoints::ServerEnd<fnet_interfaces_admin::ControlMarker>,
    options: fnet_interfaces_admin::Options,
    ns: &Netstack,
    handler: &netdevice_worker::DeviceHandler,
    device_stopped_fut: async_utils::event::EventWaitResult,
) -> Result<Option<fuchsia_async::Task<()>>, DeviceControlError> {
    log::debug!("creating interface from {:?} with {:?}", port, options);
    let fnet_interfaces_admin::Options { name, metric: _, .. } = options;
    let (control_sender, mut control_receiver) =
        OwnedControlHandle::new_channel_with_owned_handle(control).await;
    match handler
        .add_port(&ns, netdevice_worker::InterfaceOptions { name }, port, control_sender)
        .await
    {
        Ok((binding_id, status_stream)) => {
            Ok(Some(fasync::Task::spawn(run_netdevice_interface_control(
                ns.ctx.clone(),
                binding_id,
                status_stream,
                device_stopped_fut,
                control_receiver,
            ))))
        }
        Err(e) => {
            log::warn!("failed to add port {:?} to device: {:?}", port, e);
            let removed_reason = match e {
                netdevice_worker::Error::Client(e) => match e {
                    // Assume any fidl errors are port closed
                    // errors.
                    netdevice_client::Error::Fidl(_) => {
                        Some(fnet_interfaces_admin::InterfaceRemovedReason::PortClosed)
                    }
                    netdevice_client::Error::RxFlags(_)
                    | netdevice_client::Error::FrameType(_)
                    | netdevice_client::Error::NoProgress
                    | netdevice_client::Error::Config(_)
                    | netdevice_client::Error::LargeChain(_)
                    | netdevice_client::Error::Index(_, _)
                    | netdevice_client::Error::Pad(_, _)
                    | netdevice_client::Error::TxLength(_, _)
                    | netdevice_client::Error::Open(_, _)
                    | netdevice_client::Error::Vmo(_, _)
                    | netdevice_client::Error::Fifo(_, _, _)
                    | netdevice_client::Error::VmoSize(_, _)
                    | netdevice_client::Error::Map(_, _)
                    | netdevice_client::Error::DeviceInfo(_)
                    | netdevice_client::Error::PortStatus(_)
                    | netdevice_client::Error::InvalidPortId(_)
                    | netdevice_client::Error::Attach(_, _)
                    | netdevice_client::Error::Detach(_, _) => None,
                },
                netdevice_worker::Error::AlreadyInstalled(_) => {
                    Some(fnet_interfaces_admin::InterfaceRemovedReason::PortAlreadyBound)
                }
                netdevice_worker::Error::CantConnectToPort(_)
                | netdevice_worker::Error::PortClosed => {
                    Some(fnet_interfaces_admin::InterfaceRemovedReason::PortClosed)
                }
                netdevice_worker::Error::ConfigurationNotSupported
                | netdevice_worker::Error::MacNotUnicast { .. } => {
                    Some(fnet_interfaces_admin::InterfaceRemovedReason::BadPort)
                }
                netdevice_worker::Error::SystemResource(_)
                | netdevice_worker::Error::InvalidPortInfo(_) => None,
            };
            if let Some(removed_reason) = removed_reason {
                // Retrieve the original control handle from the receiver.
                let OwnedControlHandle { request_stream: _, control_handle, owns_interface: _ } =
                    control_receiver
                        .try_next()
                        .expect("expected control handle to be ready in the receiver")
                        .expect("expected receiver to not be closed/empty");
                control_handle.send_on_interface_removed(removed_reason).unwrap_or_else(|e| {
                    log::warn!("failed to send removed reason: {:?}", e);
                });
            }
            Ok(None)
        }
    }
}

/// Manages the lifetime of a newly created Netdevice interface, including spawning an
/// interface control worker, spawning a link state worker, and cleaning up the interface on
/// deletion.
async fn run_netdevice_interface_control<
    S: futures::Stream<Item = netdevice_client::Result<netdevice_client::client::PortStatus>>,
>(
    ctx: NetstackContext,
    id: BindingId,
    status_stream: S,
    mut device_stopped_fut: async_utils::event::EventWaitResult,
    control_receiver: futures::channel::mpsc::Receiver<OwnedControlHandle>,
) {
    let link_state_fut = run_link_state_watcher(ctx.clone(), id, status_stream).fuse();
    let (interface_control_stop_sender, interface_control_stop_receiver) =
        futures::channel::oneshot::channel();
    let interface_control_fut =
        run_interface_control(ctx.clone(), id, interface_control_stop_receiver, control_receiver)
            .fuse();
    futures::pin_mut!(link_state_fut);
    futures::pin_mut!(interface_control_fut);
    futures::select! {
        o = device_stopped_fut => o.expect("event was orphaned"),
        () = link_state_fut => {},
        () = interface_control_fut => {},
    };
    if !interface_control_fut.is_terminated() {
        // Cancel interface control and drive it to completion, allowing it to terminate each
        // control handle.
        interface_control_stop_sender
            .send(fnet_interfaces_admin::InterfaceRemovedReason::PortClosed)
            .expect("failed to cancel interface control");
        interface_control_fut.await;
    }
    remove_interface(ctx, id).await;
}

/// Runs a worker to watch the given status_stream and update the interface state accordingly.
async fn run_link_state_watcher<
    S: futures::Stream<Item = netdevice_client::Result<netdevice_client::client::PortStatus>>,
>(
    ctx: NetstackContext,
    id: BindingId,
    status_stream: S,
) {
    let result = status_stream
        .try_for_each(|netdevice_client::client::PortStatus { flags, mtu: _ }| {
            let ctx = &ctx;
            async move {
                let online = flags.contains(fhardware_network::StatusFlags::ONLINE);
                log::debug!("observed interface {} online = {}", id, online);
                let mut ctx = ctx.lock().await;
                match ctx
                    .non_sync_ctx
                    .devices
                    .get_device_mut(id)
                    .expect("device not present")
                    .info_mut()
                {
                    devices::DeviceSpecificInfo::Netdevice(devices::NetdeviceInfo {
                        common_info: _,
                        handler: _,
                        mac: _,
                        phy_up,
                    }) => *phy_up = online,
                    i @ devices::DeviceSpecificInfo::Ethernet(_)
                    | i @ devices::DeviceSpecificInfo::Loopback(_) => {
                        unreachable!("unexpected device info {:?} for interface {}", i, id)
                    }
                };
                // Enable or disable interface with context depending on new online
                // status. The helper functions take care of checking if admin
                // enable is the expected value.
                if online {
                    ctx.enable_interface(id).expect("failed to enable interface");
                } else {
                    ctx.disable_interface(id).expect("failed to enable interface");
                }
                Ok(())
            }
        })
        .await;
    match result {
        Ok(()) => log::debug!("state stream closed for interface {}", id),
        Err(e) => {
            let level = match &e {
                netdevice_client::Error::Fidl(e) if e.is_closed() => log::Level::Debug,
                _ => log::Level::Error,
            };
            log::log!(level, "error operating port state stream {:?} for interface {}", e, id);
        }
    }
}

/// Runs a worker to serve incoming `fuchsia.net.interfaces.admin/Control` handles.
pub(crate) async fn run_interface_control(
    ctx: NetstackContext,
    id: BindingId,
    mut stop_receiver: futures::channel::oneshot::Receiver<
        fnet_interfaces_admin::InterfaceRemovedReason,
    >,
    control_receiver: futures::channel::mpsc::Receiver<OwnedControlHandle>,
) {
    // An event indicating that the individual control request streams should stop.
    let cancel_request_streams = async_utils::event::Event::new();
    // A struct to retain per-handle state of the individual request streams in `control_receiver`.
    struct ReqStreamState {
        owns_interface: bool,
        control_handle: fnet_interfaces_admin::ControlControlHandle,
        ctx: NetstackContext,
        id: BindingId,
    }
    // Convert `control_receiver` (a stream-of-streams) into a stream of futures, where each future
    // represents the termination of an inner `ControlRequestStream`.
    let stream_of_fut = control_receiver
        .map(|OwnedControlHandle { request_stream, control_handle, owns_interface }| {
            let initial_state =
                ReqStreamState { owns_interface, control_handle, ctx: ctx.clone(), id };
            // Attach `cancel_request_streams` as a short-circuit mechanism to stop handling new
            // `ControlRequest`.
            request_stream
                .take_until(cancel_request_streams.wait_or_dropped())
                // Convert the request stream into a future, dispatching each incoming
                // `ControlRequest` and retaining the `ReqStreamState` along the way.
                .fold(initial_state, |state, request| async move {
                    let ReqStreamState { ctx, id, owns_interface: _, control_handle: _ } = &state;
                    match request {
                        Err(e) => log::error!(
                            "error operating {} stream for interface {}: {:?}",
                            fnet_interfaces_admin::ControlMarker::DEBUG_NAME,
                            id,
                            e,
                        ),
                        Ok(req) => match dispatch_control_request(req, ctx, *id).await {
                            Err(e) => {
                                log::error!(
                                    "failed to handle request for interface {}: {:?}",
                                    id,
                                    e
                                )
                            }
                            Ok(()) => {}
                        },
                    }
                    // Return `state` to be used when handling the next `ControlRequest`.
                    state
                })
        })
        // Attach `cancel_request_streams` as a short-circuit mechanism to stop attaching new
        // `ControlRequestStreams`.
        .take_until(cancel_request_streams.wait_or_dropped());
    // Enable the stream of futures to be polled concurrently.
    let mut stream_of_fut = stream_of_fut.buffer_unordered(std::usize::MAX);

    let remove_reason = {
        // Drive the `ControlRequestStreams` to completion, short-circuiting if an owner terminates.
        let interface_control_fut = async {
            while let Some(ReqStreamState { owns_interface, control_handle: _, ctx: _, id: _ }) =
                stream_of_fut.next().await
            {
                if owns_interface {
                    return;
                }
            }
        }
        .fuse();

        futures::pin_mut!(interface_control_fut);
        futures::select! {
            // One of the interface's owning channels hung up; inform the other channels.
            () = interface_control_fut => fnet_interfaces_admin::InterfaceRemovedReason::User,
            // Cancelation event was received with a specified remove reason.
            reason = stop_receiver => reason.expect("failed to receive stop"),
        }
    };

    // Cancel the active request streams, and drive the remaining `ControlRequestStreams` to
    // completion, allowing each handle to send termination events.
    assert!(cancel_request_streams.signal(), "expected the event to be unsignaled");
    while let Some(ReqStreamState { owns_interface: _, control_handle, ctx: _, id: _ }) =
        stream_of_fut.next().await
    {
        control_handle.send_on_interface_removed(remove_reason).unwrap_or_else(|e| {
            if !e.is_closed() {
                log::error!("failed to send terminal event: {:?} for interface {}", e, id)
            }
        });
    }
}

/// Serves a `fuchsia.net.interfaces.admin/Control` Request.
async fn dispatch_control_request(
    req: fnet_interfaces_admin::ControlRequest,
    ctx: &NetstackContext,
    id: BindingId,
) -> Result<(), fidl::Error> {
    log::debug!("serving {:?}", req);
    match req {
        fnet_interfaces_admin::ControlRequest::AddAddress {
            address: _,
            parameters: _,
            address_state_provider: _,
            control_handle: _,
        } => todo!("https://fxbug.dev/100870 support add address"),
        fnet_interfaces_admin::ControlRequest::RemoveAddress { address: _, responder: _ } => {
            todo!("https://fxbug.dev/100870 support remove address")
        }
        fnet_interfaces_admin::ControlRequest::GetId { responder } => responder.send(id),
        fnet_interfaces_admin::ControlRequest::SetConfiguration { config: _, responder: _ } => {
            todo!("https://fxbug.dev/76987 support enable/disable forwarding")
        }
        fnet_interfaces_admin::ControlRequest::GetConfiguration { responder: _ } => {
            todo!("https://fxbug.dev/76987 support enable/disable forwarding")
        }
        fnet_interfaces_admin::ControlRequest::Enable { responder } => {
            responder.send(&mut Ok(set_interface_enabled(ctx, true, id).await))
        }
        fnet_interfaces_admin::ControlRequest::Disable { responder } => {
            responder.send(&mut Ok(set_interface_enabled(ctx, false, id).await))
        }
        fnet_interfaces_admin::ControlRequest::Detach { control_handle: _ } => {
            todo!("https://fxbug.dev/100867 support detach");
        }
    }
}

/// Cleans up and removes the specified NetDevice interface.
async fn remove_interface(ctx: NetstackContext, id: BindingId) {
    let device_info = {
        let mut ctx = ctx.lock().await;
        let Ctx { sync_ctx, non_sync_ctx } = ctx.deref_mut();
        let info = non_sync_ctx
            .devices
            .remove_device(id)
            .expect("device lifetime should be tied to channel lifetime");
        netstack3_core::remove_device(sync_ctx, non_sync_ctx, info.core_id());
        info
    };
    let handler = match device_info.into_info() {
        devices::DeviceSpecificInfo::Netdevice(devices::NetdeviceInfo {
            handler,
            common_info: _,
            mac: _,
            phy_up: _,
        }) => handler,
        i @ devices::DeviceSpecificInfo::Ethernet(_)
        | i @ devices::DeviceSpecificInfo::Loopback(_) => {
            unreachable!("unexpected device info {:?} for interface {}", i, id)
        }
    };
    handler.uninstall().await.unwrap_or_else(|e| {
        log::warn!("error uninstalling netdevice handler for interface {}: {:?}", id, e)
    })
}

/// Sets interface with `id` to `admin_enabled = enabled`.
///
/// Returns `true` if the value of `admin_enabled` changed in response to
/// this call.
async fn set_interface_enabled(ctx: &NetstackContext, enabled: bool, id: BindingId) -> bool {
    let mut ctx = ctx.lock().await;
    let (common_info, port_handler) =
        match ctx.non_sync_ctx.devices.get_device_mut(id).expect("device not present").info_mut() {
            devices::DeviceSpecificInfo::Ethernet(devices::EthernetInfo {
                common_info,
                // NB: In theory we should also start and stop the ethernet
                // device when we enable and disable, we'll skip that because
                // it's work and Ethernet is going to be deleted soon.
                client: _,
                mac: _,
                features: _,
                phy_up: _,
                interface_control: _,
            })
            | devices::DeviceSpecificInfo::Loopback(devices::LoopbackInfo { common_info }) => {
                (common_info, None)
            }
            devices::DeviceSpecificInfo::Netdevice(devices::NetdeviceInfo {
                common_info,
                handler,
                mac: _,
                phy_up: _,
            }) => (common_info, Some(handler)),
        };
    // Already set to expected value.
    if common_info.admin_enabled == enabled {
        return false;
    }
    common_info.admin_enabled = enabled;
    if let Some(handler) = port_handler {
        let r = if enabled { handler.attach().await } else { handler.detach().await };
        match r {
            Ok(()) => (),
            Err(e) => {
                log::warn!("failed to set port {:?} to {}: {:?}", handler, enabled, e);
                // NB: There might be other errors here to consider in the
                // future, we start with a very strict set of known errors to
                // allow and panic on anything that is unexpected.
                match e {
                    // We can race with the port being removed or the device
                    // being destroyed.
                    netdevice_client::Error::Attach(_, zx::Status::NOT_FOUND)
                    | netdevice_client::Error::Detach(_, zx::Status::NOT_FOUND) => (),
                    netdevice_client::Error::Fidl(e) if e.is_closed() => (),
                    e => panic!(
                        "unexpected error setting enabled={} on port {:?}: {:?}",
                        enabled, handler, e
                    ),
                }
            }
        }
    }
    if enabled {
        ctx.enable_interface(id).expect("failed to enable interface");
    } else {
        ctx.disable_interface(id).expect("failed to disable interface");
    }
    true
}
