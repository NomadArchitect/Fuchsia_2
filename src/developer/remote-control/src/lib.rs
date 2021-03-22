// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::{format_err, Context as _, Error},
    fidl_fuchsia_developer_remotecontrol as rcs, fidl_fuchsia_device as fdevice,
    fidl_fuchsia_diagnostics::Selector,
    fidl_fuchsia_hwinfo as hwinfo, fidl_fuchsia_io as io, fidl_fuchsia_net as fnet,
    fidl_fuchsia_net_stack as fnetstack, fuchsia_zircon as zx,
    futures::prelude::*,
    std::cell::RefCell,
    std::rc::Rc,
    tracing::*,
};

mod service_discovery;

const HUB_ROOT: &str = "/discovery_root";

pub struct RemoteControlService {
    netstack_proxy: fnetstack::StackProxy,
    name_provider_proxy: fdevice::NameProviderProxy,
    hwinfo_proxy: hwinfo::DeviceProxy,
    boot_timestamp_nanos: u64,
    ids: RefCell<Vec<u64>>,
}

impl RemoteControlService {
    pub fn new() -> Result<Self, Error> {
        let (netstack_proxy, name_provider_proxy, hwinfo_proxy) = Self::construct_proxies()?;
        let boot_timestamp =
            fuchsia_runtime::utc_time().into_nanos() - zx::Time::get_monotonic().into_nanos();
        return Ok(Self::new_with_proxies_and_boot_time(
            netstack_proxy,
            name_provider_proxy,
            hwinfo_proxy,
            boot_timestamp as u64,
        ));
    }

    pub fn new_with_proxies_and_boot_time(
        netstack_proxy: fnetstack::StackProxy,
        name_provider_proxy: fdevice::NameProviderProxy,
        hwinfo_proxy: hwinfo::DeviceProxy,
        boot_timestamp_nanos: u64,
    ) -> Self {
        return Self {
            netstack_proxy,
            name_provider_proxy,
            hwinfo_proxy,
            boot_timestamp_nanos,
            ids: RefCell::new(vec![]),
        };
    }

    pub async fn serve_stream(
        self: Rc<Self>,
        mut stream: rcs::RemoteControlRequestStream,
    ) -> Result<(), Error> {
        while let Some(request) = stream.try_next().await.context("next RemoteControl request")? {
            match request {
                rcs::RemoteControlRequest::AddId { id, responder } => {
                    self.ids.borrow_mut().push(id);
                    responder.send()?;
                }
                rcs::RemoteControlRequest::IdentifyHost { responder } => {
                    self.clone().identify_host(responder).await?;
                }
                rcs::RemoteControlRequest::Connect { selector, service_chan, responder } => {
                    responder
                        .send(&mut self.clone().connect_to_service(selector, service_chan).await)?;
                }
                rcs::RemoteControlRequest::Select { selector, responder } => {
                    responder.send(&mut self.clone().select(selector).await)?;
                }
                rcs::RemoteControlRequest::OpenHub { server, responder } => {
                    responder.send(
                        &mut io_util::connect_in_namespace(
                            HUB_ROOT,
                            server.into_channel(),
                            io::OPEN_RIGHT_READABLE,
                        )
                        .map_err(|i| i.into_raw()),
                    )?;
                }
            }
        }
        Ok(())
    }

    fn construct_proxies(
    ) -> Result<(fnetstack::StackProxy, fdevice::NameProviderProxy, hwinfo::DeviceProxy), Error>
    {
        let netstack_proxy =
            fuchsia_component::client::connect_to_service::<fnetstack::StackMarker>()
                .map_err(|s| format_err!("Failed to connect to NetStack service: {}", s))?;
        let name_provider_proxy =
            fuchsia_component::client::connect_to_service::<fdevice::NameProviderMarker>()
                .map_err(|s| format_err!("Failed to connect to NameProviderService: {}", s))?;
        let hwinfo_proxy = fuchsia_component::client::connect_to_service::<hwinfo::DeviceMarker>()
            .map_err(|s| format_err!("Failed to connect to DeviceProxyy: {}", s))?;
        return Ok((netstack_proxy, name_provider_proxy, hwinfo_proxy));
    }

    async fn connect_with_matcher(
        self: &Rc<Self>,
        selector: &Selector,
        service_chan: zx::Channel,
        matcher_fut: impl Future<Output = Result<Vec<service_discovery::PathEntry>, Error>>,
    ) -> Result<rcs::ServiceMatch, rcs::ConnectError> {
        let paths = matcher_fut.await.map_err(|err| {
            warn!(?selector, %err, "error looking for matching services for selector");
            rcs::ConnectError::ServiceDiscoveryFailed
        })?;
        if paths.is_empty() {
            return Err(rcs::ConnectError::NoMatchingServices);
        } else if paths.len() > 1 {
            // TODO(jwing): we should be able to communicate this to the FE somehow.
            warn!(
                ?paths,
                "Selector must match exactly one service. Provided selector matched all of the following");
            return Err(rcs::ConnectError::MultipleMatchingServices);
        }
        let svc_match = paths.get(0).unwrap();
        let hub_path = svc_match.hub_path.to_str().unwrap();
        info!(hub_path, "attempting to connect");
        io_util::connect_in_namespace(hub_path, service_chan, io::OPEN_RIGHT_READABLE).map_err(
            |err| {
                error!(?selector, %err, "error connecting to selector");
                rcs::ConnectError::ServiceConnectFailed
            },
        )?;

        Ok(svc_match.into())
    }

    pub async fn connect_to_service(
        self: &Rc<Self>,
        selector: Selector,
        service_chan: zx::Channel,
    ) -> Result<rcs::ServiceMatch, rcs::ConnectError> {
        self.connect_with_matcher(
            &selector,
            service_chan,
            service_discovery::get_matching_paths(HUB_ROOT, &selector),
        )
        .await
    }

    async fn select_with_matcher(
        self: &Rc<Self>,
        selector: &Selector,
        matcher_fut: impl Future<Output = Result<Vec<service_discovery::PathEntry>, Error>>,
    ) -> Result<Vec<rcs::ServiceMatch>, rcs::SelectError> {
        let paths = matcher_fut.await.map_err(|err| {
            warn!(?selector, %err, "error looking for matching services for selector");
            rcs::SelectError::ServiceDiscoveryFailed
        })?;

        Ok(paths.iter().map(|p| p.into()).collect::<Vec<rcs::ServiceMatch>>())
    }

    pub async fn select(
        self: &Rc<Self>,
        selector: Selector,
    ) -> Result<Vec<rcs::ServiceMatch>, rcs::SelectError> {
        self.select_with_matcher(
            &selector,
            service_discovery::get_matching_paths(HUB_ROOT, &selector),
        )
        .await
    }

    pub async fn identify_host(
        self: &Rc<Self>,
        responder: rcs::RemoteControlIdentifyHostResponder,
    ) -> Result<(), Error> {
        let mut ilist = match self.netstack_proxy.list_interfaces().await {
            Ok(l) => l,
            Err(err) => {
                error!(%err, "Getting interface list failed");
                responder
                    .send(&mut Err(rcs::IdentifyHostError::ListInterfacesFailed))
                    .context("sending IdentifyHost error response")?;
                return Ok(());
            }
        };

        let serial_number = match self.hwinfo_proxy.get_info().await {
            Ok(info) => info.serial_number,
            Err(e) => {
                error!(%e, "DeviceProxy internal err");
                // This will fail on most targets (including the emulator), so
                // no need to propagate this to the client via the responder.
                None
            }
        };

        let result = ilist
            .iter_mut()
            .flat_map(|int| int.properties.addresses.drain(..))
            .collect::<Vec<fnet::Subnet>>();

        let nodename = match self.name_provider_proxy.get_device_name().await {
            Ok(result) => match result {
                Ok(name) => name,
                Err(err) => {
                    error!(%err, "NameProvider internal error");
                    responder
                        .send(&mut Err(rcs::IdentifyHostError::GetDeviceNameFailed))
                        .context("sending GetDeviceName error response")?;
                    return Ok(());
                }
            },
            Err(err) => {
                error!(%err, "Getting nodename failed");
                responder
                    .send(&mut Err(rcs::IdentifyHostError::GetDeviceNameFailed))
                    .context("sending GetDeviceName error response")?;
                return Ok(());
            }
        };

        // TODO(raggi): limit size to stay under message size limit.
        let ids = self.ids.borrow().clone();

        responder
            .send(&mut Ok(rcs::IdentifyHostResponse {
                nodename: Some(nodename),
                addresses: Some(result),
                boot_timestamp_nanos: Some(self.boot_timestamp_nanos),
                serial_number,
                ids: Some(ids),
                ..rcs::IdentifyHostResponse::EMPTY
            }))
            .context("sending IdentifyHost response")?;

        return Ok(());
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        fidl_fuchsia_developer_remotecontrol as rcs,
        fidl_fuchsia_hardware_ethernet::{Features, MacAddress},
        fidl_fuchsia_io::NodeMarker,
        fidl_fuchsia_net as fnet, fuchsia_async as fasync, fuchsia_zircon as zx,
        selectors::parse_selector,
        service_discovery::PathEntry,
        std::path::PathBuf,
    };

    const NODENAME: &'static str = "thumb-set-human-shred";
    const BOOT_TIME: u64 = 123456789000000000;
    const SERIAL: &'static str = "test_serial";

    const IPV4_ADDR: [u8; 4] = [127, 0, 0, 1];
    const IPV6_ADDR: [u8; 16] = [127, 1, 2, 3, 4, 5, 6, 7, 8, 9, 1, 2, 3, 4, 5, 6];

    fn setup_fake_hwinfo_service() -> hwinfo::DeviceProxy {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<hwinfo::DeviceMarker>().unwrap();
        fasync::Task::spawn(async move {
            while let Ok(req) = stream.try_next().await {
                match req {
                    Some(hwinfo::DeviceRequest::GetInfo { responder }) => {
                        let _ = responder.send(hwinfo::DeviceInfo {
                            serial_number: Some(String::from(SERIAL)),
                            ..hwinfo::DeviceInfo::EMPTY
                        });
                    }
                    _ => panic!("invalid request"),
                }
            }
        })
        .detach();

        proxy
    }

    fn setup_fake_name_provider_service() -> fdevice::NameProviderProxy {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fdevice::NameProviderMarker>().unwrap();

        fasync::Task::spawn(async move {
            while let Ok(req) = stream.try_next().await {
                match req {
                    Some(fdevice::NameProviderRequest::GetDeviceName { responder }) => {
                        let _ = responder.send(&mut Ok(String::from(NODENAME)));
                    }
                    _ => assert!(false),
                }
            }
        })
        .detach();

        proxy
    }

    fn setup_fake_netstack_service() -> fnetstack::StackProxy {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fnetstack::StackMarker>().unwrap();

        fasync::Task::spawn(async move {
            while let Ok(req) = stream.try_next().await {
                match req {
                    Some(fnetstack::StackRequest::ListInterfaces { responder }) => {
                        let mut resp = vec![fnetstack::InterfaceInfo {
                            id: 1,
                            properties: fnetstack::InterfaceProperties {
                                name: String::from("eth0"),
                                topopath: String::from("N/A"),
                                filepath: String::from("N/A"),
                                administrative_status: fnetstack::AdministrativeStatus::Enabled,
                                physical_status: fnetstack::PhysicalStatus::Up,
                                mtu: 1,
                                features: Features::empty(),
                                mac: Some(Box::new(MacAddress { octets: [1, 2, 3, 4, 5, 6] })),
                                addresses: vec![
                                    fnet::Subnet {
                                        addr: fnet::IpAddress::Ipv4(fnet::Ipv4Address {
                                            addr: IPV4_ADDR,
                                        }),
                                        prefix_len: 4,
                                    },
                                    fnet::Subnet {
                                        addr: fnet::IpAddress::Ipv6(fnet::Ipv6Address {
                                            addr: IPV6_ADDR,
                                        }),
                                        prefix_len: 6,
                                    },
                                ],
                            },
                        }];
                        let _ = responder.send(&mut resp.iter_mut());
                    }
                    _ => assert!(false),
                }
            }
        })
        .detach();

        proxy
    }

    fn make_rcs() -> Rc<RemoteControlService> {
        Rc::new(RemoteControlService::new_with_proxies_and_boot_time(
            setup_fake_netstack_service(),
            setup_fake_name_provider_service(),
            setup_fake_hwinfo_service(),
            BOOT_TIME,
        ))
    }

    fn setup_rcs_proxy() -> rcs::RemoteControlProxy {
        let service = make_rcs();

        let (rcs_proxy, stream) =
            fidl::endpoints::create_proxy_and_stream::<rcs::RemoteControlMarker>().unwrap();
        fasync::Task::local(async move {
            service.serve_stream(stream).await.unwrap();
        })
        .detach();

        return rcs_proxy;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_identify_host() -> Result<(), Error> {
        let rcs_proxy = setup_rcs_proxy();

        let resp = rcs_proxy.identify_host().await.unwrap().unwrap();

        assert_eq!(resp.serial_number.unwrap(), SERIAL);
        assert_eq!(resp.nodename.unwrap(), NODENAME);

        let addrs = resp.addresses.unwrap();
        assert_eq!(addrs.len(), 2);

        let v4 = &addrs[0];
        assert_eq!(v4.prefix_len, 4);
        assert_eq!(v4.addr, fnet::IpAddress::Ipv4(fnet::Ipv4Address { addr: IPV4_ADDR }));

        let v6 = &addrs[1];
        assert_eq!(v6.prefix_len, 6);
        assert_eq!(v6.addr, fnet::IpAddress::Ipv6(fnet::Ipv6Address { addr: IPV6_ADDR }));

        assert_eq!(resp.boot_timestamp_nanos.unwrap(), BOOT_TIME);

        Ok(())
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_ids_in_host_identify() -> Result<(), Error> {
        let rcs_proxy = setup_rcs_proxy();

        let ident = rcs_proxy.identify_host().await.unwrap().unwrap();
        assert_eq!(ident.ids, Some(vec![]));

        rcs_proxy.add_id(1234).await.unwrap();
        rcs_proxy.add_id(4567).await.unwrap();

        let ident = rcs_proxy.identify_host().await.unwrap().unwrap();
        let ids = ident.ids.unwrap();
        assert_eq!(ids.len(), 2);
        assert_eq!(1234u64, ids[0]);
        assert_eq!(4567u64, ids[1]);

        Ok(())
    }

    fn wildcard_selector() -> Selector {
        parse_selector("*:*:*").unwrap()
    }

    async fn no_paths_matcher() -> Result<Vec<PathEntry>, Error> {
        Ok(vec![])
    }

    async fn two_paths_matcher() -> Result<Vec<PathEntry>, Error> {
        Ok(vec![
            PathEntry {
                hub_path: PathBuf::from("/"),
                moniker: PathBuf::from("/a/b/c"),
                component_subdir: "out".to_string(),
                service: "myservice".to_string(),
            },
            PathEntry {
                hub_path: PathBuf::from("/"),
                moniker: PathBuf::from("/a/b/c"),
                component_subdir: "out".to_string(),
                service: "myservice2".to_string(),
            },
        ])
    }

    async fn single_path_matcher() -> Result<Vec<PathEntry>, Error> {
        Ok(vec![PathEntry {
            hub_path: PathBuf::from("/tmp"),
            moniker: PathBuf::from("/tmp"),
            component_subdir: "out".to_string(),
            service: "myservice".to_string(),
        }])
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_connect_no_matches() -> Result<(), Error> {
        let service = make_rcs();
        let (_, server_end) = zx::Channel::create().unwrap();

        let result = service
            .connect_with_matcher(&wildcard_selector(), server_end, no_paths_matcher())
            .await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), rcs::ConnectError::NoMatchingServices);
        Ok(())
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_connect_multiple_matches() -> Result<(), Error> {
        let service = make_rcs();
        let (_, server_end) = zx::Channel::create().unwrap();

        let result = service
            .connect_with_matcher(&wildcard_selector(), server_end, two_paths_matcher())
            .await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), rcs::ConnectError::MultipleMatchingServices);
        Ok(())
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_connect_single_match() -> Result<(), Error> {
        let service = make_rcs();
        let (client_end, server_end) = fidl::endpoints::create_endpoints::<NodeMarker>().unwrap();

        service
            .connect_with_matcher(
                &wildcard_selector(),
                server_end.into_channel(),
                single_path_matcher(),
            )
            .await
            .unwrap();

        // Make a dummy call to verify that the channel did get hooked up.
        assert!(client_end.into_proxy().unwrap().describe().await.is_ok());
        Ok(())
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_select_multiple_matches() -> Result<(), Error> {
        let service = make_rcs();

        let result =
            service.select_with_matcher(&wildcard_selector(), two_paths_matcher()).await.unwrap();

        assert_eq!(result.len(), 2);
        assert!(result.iter().any(|p| *p
            == rcs::ServiceMatch {
                moniker: vec!["a".to_string(), "b".to_string(), "c".to_string()],
                subdir: "out".to_string(),
                service: "myservice".to_string()
            }));
        assert!(result.iter().any(|p| *p
            == rcs::ServiceMatch {
                moniker: vec!["a".to_string(), "b".to_string(), "c".to_string()],
                subdir: "out".to_string(),
                service: "myservice2".to_string()
            }));
        Ok(())
    }
}
