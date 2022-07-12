// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::ops::{Deref as _, DerefMut as _};

use super::{
    devices::{
        self, CommonInfo, DeviceSpecificInfo, Devices, EthernetInfo, EthernetInterfaceControl,
    },
    ethernet_worker, interfaces_admin,
    util::{IntoFidl, TryFromFidlWithContext as _, TryIntoCore as _, TryIntoFidlWithContext as _},
    DeviceStatusNotifier, InterfaceControl as _, InterfaceControlRunner,
    InterfaceEventProducerFactory, Lockable, LockableContext, MutableDeviceState as _,
};

use fidl_fuchsia_hardware_ethernet as fhardware_ethernet;
use fidl_fuchsia_hardware_network as fhardware_network;
use fidl_fuchsia_net as fidl_net;
use fidl_fuchsia_net_interfaces as finterfaces;
use fidl_fuchsia_net_interfaces_admin as fnet_interfaces_admin;
use fidl_fuchsia_net_stack::{
    self as fidl_net_stack, ForwardingEntry, StackRequest, StackRequestStream,
};
use futures::{TryFutureExt as _, TryStreamExt as _};
use log::{debug, error};
use net_types::{ethernet::Mac, SpecifiedAddr, UnicastAddr};
use netstack3_core::{
    add_ip_addr_subnet, add_route, del_ip_addr, del_route, get_all_routes, AddableEntryEither, Ctx,
};

pub(crate) struct StackFidlWorker<C> {
    ctx: C,
}

struct LockedFidlWorker<'a, C: LockableContext> {
    ctx: <C as Lockable<'a, Ctx<C::NonSyncCtx>>>::Guard,
    worker: &'a StackFidlWorker<C>,
}

impl<C: LockableContext> StackFidlWorker<C> {
    async fn lock_worker(&self) -> LockedFidlWorker<'_, C> {
        let ctx = self.ctx.lock().await;
        LockedFidlWorker { ctx, worker: self }
    }
}

impl<C> StackFidlWorker<C>
where
    C: ethernet_worker::EthernetWorkerContext
        + InterfaceEventProducerFactory
        + InterfaceControlRunner,
    C: Clone,
{
    pub(crate) async fn serve(ctx: C, stream: StackRequestStream) -> Result<(), fidl::Error> {
        stream
            .try_fold(Self { ctx }, |worker, req| async {
                match req {
                    StackRequest::AddEthernetInterface { topological_path, device, responder } => {
                        responder_send!(
                            responder,
                            &mut worker
                                .lock_worker()
                                .await
                                .fidl_add_ethernet_interface(topological_path, device)
                                .await
                        );
                    }
                    StackRequest::DelEthernetInterface { id, responder } => {
                        responder_send!(
                            responder,
                            &mut worker.lock_worker().await.fidl_del_ethernet_interface(id).await
                        );
                    }
                    StackRequest::EnableInterfaceDeprecated { id, responder } => {
                        responder_send!(
                            responder,
                            &mut worker.lock_worker().await.fidl_enable_interface(id)
                        );
                    }
                    StackRequest::DisableInterfaceDeprecated { id, responder } => {
                        responder_send!(
                            responder,
                            &mut worker.lock_worker().await.fidl_disable_interface(id)
                        );
                    }
                    StackRequest::AddInterfaceAddressDeprecated { id, addr, responder } => {
                        responder_send!(
                            responder,
                            &mut worker.lock_worker().await.fidl_add_interface_address(id, addr)
                        );
                    }
                    StackRequest::DelInterfaceAddressDeprecated { id, addr, responder } => {
                        responder_send!(
                            responder,
                            &mut worker.lock_worker().await.fidl_del_interface_address(id, addr)
                        );
                    }
                    StackRequest::GetForwardingTable { responder } => {
                        responder_send!(
                            responder,
                            &mut worker.lock_worker().await.fidl_get_forwarding_table().iter_mut()
                        );
                    }
                    StackRequest::AddForwardingEntry { entry, responder } => {
                        responder_send!(
                            responder,
                            &mut worker.lock_worker().await.fidl_add_forwarding_entry(entry)
                        );
                    }
                    StackRequest::DelForwardingEntry {
                        entry:
                            fidl_net_stack::ForwardingEntry {
                                subnet,
                                device_id: _,
                                next_hop: _,
                                metric: _,
                            },
                        responder,
                    } => {
                        responder_send!(
                            responder,
                            &mut worker.lock_worker().await.fidl_del_forwarding_entry(subnet)
                        );
                    }
                    StackRequest::SetInterfaceIpForwardingDeprecated {
                        id: _,
                        ip_version: _,
                        enabled: _,
                        responder,
                    } => {
                        // TODO(https://fxbug.dev/76987): Support configuring per-NIC forwarding.
                        responder_send!(responder, &mut Err(fidl_net_stack::Error::NotSupported));
                    }
                    StackRequest::GetDnsServerWatcher { watcher, control_handle: _ } => {
                        let () = watcher
                            .close_with_epitaph(fuchsia_zircon::Status::NOT_SUPPORTED)
                            .unwrap_or_else(|e| {
                                debug!("failed to close DNS server watcher {:?}", e)
                            });
                    }
                }
                Ok(worker)
            })
            .map_ok(|Self { ctx: _ }| ())
            .await
    }
}

impl<'a, C> LockedFidlWorker<'a, C>
where
    C: ethernet_worker::EthernetWorkerContext
        + InterfaceEventProducerFactory
        + InterfaceControlRunner,
    C: Clone,
{
    async fn fidl_add_ethernet_interface(
        self,
        _topological_path: String,
        device: fidl::endpoints::ClientEnd<fidl_fuchsia_hardware_ethernet::DeviceMarker>,
    ) -> Result<u64, fidl_net_stack::Error> {
        let Self { mut ctx, worker } = self;

        let (
            client,
            fidl_fuchsia_hardware_ethernet_ext::EthernetInfo {
                mtu,
                features,
                mac: fidl_fuchsia_hardware_ethernet_ext::MacAddress { octets: mac_octets },
            },
        ) = ethernet_worker::setup_ethernet(
            device.into_proxy().map_err(|_| fidl_net_stack::Error::InvalidArgs)?,
        )
        .await
        .map_err(|_| fidl_net_stack::Error::Internal)?;

        let client_stream = client.get_stream();

        let online = client
            .get_status()
            .await
            .map(|s| s.contains(ethernet_worker::DeviceStatus::ONLINE))
            .unwrap_or(false);
        let mac_addr =
            UnicastAddr::new(Mac::new(mac_octets)).ok_or(fidl_net_stack::Error::NotSupported)?;

        let id = {
            let Ctx { sync_ctx, non_sync_ctx } = &mut *ctx;
            let eth_id = netstack3_core::add_ethernet_device(sync_ctx, non_sync_ctx, mac_addr, mtu);
            let (interface_control_stop_sender, interface_control_stop_receiver) =
                futures::channel::oneshot::channel();
            let (control_sender, control_receiver) =
                interfaces_admin::OwnedControlHandle::new_channel();

            let devices: &mut Devices = non_sync_ctx.as_mut();
            devices
                .add_device(eth_id, |id| {
                    let device_class = if features.contains(fhardware_ethernet::Features::LOOPBACK)
                    {
                        finterfaces::DeviceClass::Loopback(finterfaces::Empty)
                    } else if features.contains(fhardware_ethernet::Features::SYNTHETIC) {
                        finterfaces::DeviceClass::Device(fhardware_network::DeviceClass::Virtual)
                    } else if features.contains(fhardware_ethernet::Features::WLAN_AP) {
                        finterfaces::DeviceClass::Device(fhardware_network::DeviceClass::WlanAp)
                    } else if features.contains(fhardware_ethernet::Features::WLAN) {
                        finterfaces::DeviceClass::Device(fhardware_network::DeviceClass::Wlan)
                    } else {
                        finterfaces::DeviceClass::Device(fhardware_network::DeviceClass::Ethernet)
                    };
                    let name = format!("eth{}", id);

                    // We do not support updating the device's mac-address, mtu, and
                    // features during it's lifetime, their cached states are hence
                    // not updated once initialized.
                    DeviceSpecificInfo::Ethernet(EthernetInfo {
                        common_info: CommonInfo {
                            mtu,
                            admin_enabled: true,
                            events: worker.ctx.create_interface_event_producer(
                                id,
                                super::InterfaceProperties { name: name.clone(), device_class },
                            ),
                            name,
                            control_hook: control_sender,
                        },
                        client,
                        mac: mac_addr,
                        features,
                        phy_up: online,
                        interface_control: EthernetInterfaceControl {
                            worker: self.worker.ctx.spawn_interface_control(
                                id,
                                interface_control_stop_receiver,
                                control_receiver,
                            ),
                            cancelation_sender: interface_control_stop_sender,
                        },
                    })
                })
                .unwrap_or_else(|| {
                    panic!("failed to store device with {:?} on devices map", eth_id)
                })
        };

        if online {
            ctx.enable_interface(id)?;
        }

        ethernet_worker::EthernetWorker::new(id, self.worker.ctx.clone()).spawn(client_stream);

        Ok(id)
    }
}

impl<'a, C> LockedFidlWorker<'a, C>
where
    C: LockableContext,
    C::NonSyncCtx: AsMut<Devices>,
{
    async fn fidl_del_ethernet_interface(mut self, id: u64) -> Result<(), fidl_net_stack::Error> {
        match self.ctx.non_sync_ctx.as_mut().remove_device(id) {
            Some(info) => {
                match info.into_info() {
                    devices::DeviceSpecificInfo::Ethernet(devices::EthernetInfo {
                        common_info: _,
                        client: _,
                        mac: _,
                        features: _,
                        phy_up: _,
                        interface_control: EthernetInterfaceControl { worker, cancelation_sender },
                    }) => {
                        cancelation_sender
                            .send(fnet_interfaces_admin::InterfaceRemovedReason::User)
                            .expect("failed to cancel interface control");
                        worker.await;
                    }
                    i @ devices::DeviceSpecificInfo::Loopback(_)
                    | i @ devices::DeviceSpecificInfo::Netdevice(_) => {
                        log::error!("unexpected device info {:?} for interface {}", i, id)
                    }
                }
                // TODO(rheacock): ensure that the core client deletes all data
                Ok(())
            }
            None => {
                // Invalid device ID
                Err(fidl_net_stack::Error::NotFound)
            }
        }
    }
}

impl<'a, C> LockedFidlWorker<'a, C>
where
    C: LockableContext,
    C::NonSyncCtx: AsRef<Devices>,
{
    fn fidl_add_interface_address(
        mut self,
        id: u64,
        addr: fidl_net::Subnet,
    ) -> Result<(), fidl_net_stack::Error> {
        let Ctx { sync_ctx, non_sync_ctx } = self.ctx.deref_mut();

        let device_info =
            non_sync_ctx.as_ref().get_device(id).ok_or(fidl_net_stack::Error::NotFound)?;
        let device_id = device_info.core_id();

        add_ip_addr_subnet(
            sync_ctx,
            non_sync_ctx,
            device_id,
            addr.try_into_core().map_err(IntoFidl::into_fidl)?,
        )
        .map_err(IntoFidl::into_fidl)
    }

    fn fidl_del_interface_address(
        mut self,
        id: u64,
        addr: fidl_net::Subnet,
    ) -> Result<(), fidl_net_stack::Error> {
        let Ctx { sync_ctx, non_sync_ctx } = self.ctx.deref_mut();

        let device_info =
            non_sync_ctx.as_ref().get_device(id).ok_or(fidl_net_stack::Error::NotFound)?;
        let device_id = device_info.core_id();
        let addr: SpecifiedAddr<_> = addr.addr.try_into_core().map_err(IntoFidl::into_fidl)?;

        del_ip_addr(sync_ctx, non_sync_ctx, device_id, addr.into()).map_err(IntoFidl::into_fidl)
    }

    fn fidl_get_forwarding_table(self) -> Vec<fidl_net_stack::ForwardingEntry> {
        let Ctx { sync_ctx, non_sync_ctx } = self.ctx.deref();

        get_all_routes(sync_ctx)
            .filter_map(|entry| match entry.try_into_fidl_with_ctx(&non_sync_ctx) {
                Ok(entry) => Some(entry),
                Err(e) => {
                    error!("Failed to map forwarding entry into FIDL: {:?}", e);
                    None
                }
            })
            .collect()
    }

    fn fidl_add_forwarding_entry(
        mut self,
        entry: ForwardingEntry,
    ) -> Result<(), fidl_net_stack::Error> {
        let Ctx { sync_ctx, non_sync_ctx } = self.ctx.deref_mut();

        let entry = match AddableEntryEither::try_from_fidl_with_ctx(&non_sync_ctx, entry) {
            Ok(entry) => entry,
            Err(e) => return Err(e.into()),
        };
        add_route(sync_ctx, non_sync_ctx, entry).map_err(IntoFidl::into_fidl)
    }
}

impl<'a, C> LockedFidlWorker<'a, C>
where
    C: LockableContext,
{
    fn fidl_del_forwarding_entry(
        mut self,
        subnet: fidl_net::Subnet,
    ) -> Result<(), fidl_net_stack::Error> {
        let Ctx { sync_ctx, non_sync_ctx } = self.ctx.deref_mut();

        if let Ok(subnet) = subnet.try_into_core() {
            del_route(sync_ctx, non_sync_ctx, subnet).map_err(IntoFidl::into_fidl)
        } else {
            Err(fidl_net_stack::Error::InvalidArgs)
        }
    }
}

impl<'a, C> LockedFidlWorker<'a, C>
where
    C: LockableContext,
    C::NonSyncCtx: DeviceStatusNotifier,
    C::NonSyncCtx: AsRef<Devices> + AsMut<Devices>,
{
    fn fidl_enable_interface(mut self, id: u64) -> Result<(), fidl_net_stack::Error> {
        self.ctx.update_device_state(id, |dev_info| {
            dev_info.info_mut().common_info_mut().admin_enabled = true;
        });
        self.ctx.enable_interface(id)
    }

    fn fidl_disable_interface(mut self, id: u64) -> Result<(), fidl_net_stack::Error> {
        self.ctx.update_device_state(id, |dev_info| {
            dev_info.info_mut().common_info_mut().admin_enabled = false;
        });
        self.ctx.disable_interface(id)
    }
}
