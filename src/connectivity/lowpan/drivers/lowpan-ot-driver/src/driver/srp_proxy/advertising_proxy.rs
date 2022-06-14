// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;
use fidl::endpoints::create_endpoints;
use fidl_fuchsia_net_mdns::*;
use fuchsia_async::Task;
use fuchsia_component::client::connect_to_protocol;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::sync::Arc;

/// The advertising proxy handles taking hosts and services registered with the SRP server
/// and republishing them via local mDNS.
#[derive(Debug)]
pub struct AdvertisingProxy {
    inner: Arc<Mutex<AdvertisingProxyInner>>,
}

impl Drop for AdvertisingProxy {
    fn drop(&mut self) {
        // Make sure all advertised hosts get cleaned up.
        self.inner.lock().hosts.clear();
    }
}

#[derive(Debug)]
struct AdvertisingProxyInner {
    srp_domain: String,
    hosts: HashMap<CString, AdvertisingProxyHost>,
    mdns_proxy_host_publisher: ProxyHostPublisherProxy,
}

#[derive(Debug)]
pub struct AdvertisingProxyHost {
    services: HashMap<CString, AdvertisingProxyService>,
    service_publisher: ServiceInstancePublisherProxy,
}

#[derive(Debug)]
pub struct AdvertisingProxyService {
    txt_data: Vec<u8>,
    port: u16,
    priority: u16,
    weight: u16,

    #[allow(dead_code)]
    task: Task<Result>,
}

impl AdvertisingProxyService {
    fn is_up_to_date(&self, srp_service: &ot::SrpServerService) -> bool {
        !srp_service.is_deleted()
            && self.txt_data == srp_service.txt_data()
            && self.weight == srp_service.weight()
            && self.priority == srp_service.priority()
            && self.port == srp_service.port()
    }
}

impl AdvertisingProxy {
    pub fn new(instance: &ot::Instance) -> Result<AdvertisingProxy, anyhow::Error> {
        let inner = Arc::new(Mutex::new(AdvertisingProxyInner {
            srp_domain: instance.srp_server_get_domain().to_str()?.to_string(),
            hosts: Default::default(),
            mdns_proxy_host_publisher: connect_to_protocol::<ProxyHostPublisherMarker>()?,
        }));
        let ret = AdvertisingProxy { inner: inner.clone() };

        ret.inner.lock().publish_srp_all(instance)?;

        instance.srp_server_set_service_update_fn(Some(
            move |ot_instance: &ot::Instance,
                  update_id: ot::SrpServerServiceUpdateId,
                  host: &ot::SrpServerHost,
                  timeout: u32| {
                debug!(
                    "srp_server_set_service_update: Update for {:?}, timeout: {}",
                    host, timeout
                );
                let result = inner.lock().push_srp_host_changes(instance, host);

                if let Err(err) = &result {
                    warn!("srp_server_set_service_update: Error publishing {:?}: {:?}", host, err);
                } else {
                    debug!("srp_server_set_service_update: Finished publishing {:?}", host);
                }

                ot_instance.srp_server_handle_service_update_result(
                    update_id,
                    result.map_err(|_: anyhow::Error| ot::Error::Failed),
                );
            },
        ));

        info!("AdvertisingProxy Started");

        Ok(ret)
    }
}

impl AdvertisingProxyInner {
    pub fn publish_srp_all(&mut self, instance: &ot::Instance) -> Result<(), anyhow::Error> {
        for host in instance.srp_server_hosts() {
            if let Err(err) = self.push_srp_host_changes(instance, host) {
                warn!(
                    "Unable to fully publish SRP host {:?} to mDNS: {:?}",
                    host.full_name_cstr(),
                    err
                );
            }
        }

        Ok(())
    }

    /// Updates the mDNS service with the host and services from the SrpServerHost.
    pub fn push_srp_host_changes(
        &mut self,
        _instance: &ot::Instance,
        srp_host: &ot::SrpServerHost,
    ) -> Result<(), anyhow::Error> {
        if srp_host.is_deleted() {
            // Delete the host.
            info!(
                "No longer advertising host {:?} on {:?}",
                srp_host.full_name_cstr(),
                LOCAL_DOMAIN
            );

            self.hosts.remove(srp_host.full_name_cstr());
            return Ok(());
        }

        let host: &mut AdvertisingProxyHost = if let Some(host) =
            self.hosts.get_mut(srp_host.full_name_cstr())
        {
            // Use the existing host.
            debug!(
                "Updating advertisement of {:?} on {:?}",
                srp_host.full_name_cstr(),
                LOCAL_DOMAIN
            );

            host
        } else {
            // Add the host.
            let local_name = srp_host
                .full_name_cstr()
                .as_ref()
                .to_str()?
                .trim_end_matches(&self.srp_domain)
                .trim_end_matches('.');

            debug!(
                "Advertising host {:?} on {:?} as {:?}",
                srp_host.full_name_cstr(),
                LOCAL_DOMAIN,
                local_name
            );

            if local_name.len() > MAX_DNSSD_HOST_LEN {
                bail!("Host {:?} is too long (max {} chars)", local_name, MAX_DNSSD_HOST_LEN);
            }

            let (client, server) = create_endpoints::<ServiceInstancePublisherMarker>()
                .context("Failed to create FIDL endpoints")?;

            let mut addrs = srp_host
                .addresses()
                .iter()
                .filter_map(|x| {
                    if !net_types::ip::Ipv6Addr::from_bytes(x.octets()).is_unicast_link_local() {
                        Some(fidl_fuchsia_net::IpAddress::Ipv6(fidl_fuchsia_net::Ipv6Address {
                            addr: x.octets(),
                        }))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();

            let publish_proxy_host_future = self
                .mdns_proxy_host_publisher
                .publish_proxy_host(
                    &local_name,
                    &mut addrs.iter_mut(),
                    ProxyHostPublicationOptions {
                        perform_probe: Some(false),
                        ..ProxyHostPublicationOptions::EMPTY
                    },
                    server,
                )
                .map(|x| match x {
                    Ok(Ok(())) => {
                        debug!("publish_proxy_host: success");
                    }
                    Ok(Err(err)) => {
                        error!("publish_proxy_host: {:?}", err);
                    }
                    Err(err) => {
                        error!("publish_proxy_host: {:?}", err);
                    }
                });

            fuchsia_async::Task::spawn(publish_proxy_host_future).detach();

            self.hosts.insert(
                srp_host.full_name_cstr().to_owned(),
                AdvertisingProxyHost {
                    services: Default::default(),
                    service_publisher: client.into_proxy()?,
                },
            );
            self.hosts.get_mut(srp_host.full_name_cstr()).unwrap()
        };

        let services = &mut host.services;

        for srp_service in srp_host.find_services::<&CStr, &CStr>(
            ot::SrpServerServiceFlags::BASE_TYPE_SERVICE_ONLY,
            None,
            None,
        ) {
            // The service name as a Rust string slice from the SRP service.
            let service_name = srp_service.service_name_cstr().as_ref().to_str()?;

            // The service name without the domain, with a trailing period, like "_trel._udp.".
            let local_service_name = service_name.trim_end_matches(&self.srp_domain);

            // The instance name without the service name or domain,
            // without any trailing period, like "My-Service".
            let local_instance_name = srp_service
                .full_name_cstr()
                .as_ref()
                .to_str()?
                .trim_end_matches(service_name)
                .trim_end_matches('.');

            if srp_service.is_deleted() {
                // Delete the service.
                debug!(
                    "No longer advertising service {:?} on {:?}",
                    srp_service.full_name_cstr(),
                    LOCAL_DOMAIN
                );
                services.remove(srp_service.full_name_cstr());
                continue;
            }

            let is_up_to_date = services
                .get(srp_service.full_name_cstr())
                .map(|s| s.is_up_to_date(srp_service))
                .unwrap_or(false);

            if is_up_to_date {
                // Service is already up to date.
                continue;
            }

            debug!(
                "Advertising service {:?} on {:?} as {:?}",
                local_service_name, LOCAL_DOMAIN, local_instance_name
            );

            if local_service_name.len() > MAX_DNSSD_SERVICE_LEN {
                error!(
                    "Unable publish service instance {:?}: Service too long (max {} chars)",
                    local_service_name, MAX_DNSSD_SERVICE_LEN
                );
                continue;
            }

            if local_instance_name.len() > MAX_DNSSD_INSTANCE_LEN {
                error!(
                    "Unable publish service instance {:?}: Instance name too long (max {} chars)",
                    local_instance_name, MAX_DNSSD_INSTANCE_LEN
                );
                continue;
            }

            // (Re-)Add the service.

            let (client, server) = create_endpoints::<ServiceInstancePublicationResponder_Marker>()
                .context("Failed to create FIDL endpoints")?;

            let publish_init_future = host
                .service_publisher
                .publish_service_instance(
                    &local_service_name,
                    &local_instance_name,
                    ServiceInstancePublicationOptions {
                        perform_probe: Some(false),
                        ..ServiceInstancePublicationOptions::EMPTY
                    },
                    client,
                )
                .map(|x| match x {
                    Ok(Ok(())) => {
                        debug!("publish_service_instance: success");
                        Ok(())
                    }
                    Ok(Err(err)) => {
                        error!("publish_service_instance: {:?}", err);
                        Err(format_err!("publish_service_instance: {:?}", err))
                    }
                    Err(err) => {
                        error!("publish_service_instance: {:?}", err);
                        Err(format_err!("publish_service_instance: {:?}", err))
                    }
                });

            // Make copies of all of the pertinent data for the publication responder.
            let txt = srp_service.txt_entries().map(|x| x.unwrap().to_vec()).collect::<Vec<_>>();
            let port = srp_service.port();
            let weight = srp_service.weight();
            let priority = srp_service.priority();

            let publish_responder_future =
                server.into_stream().unwrap().map_err(Into::into).try_for_each(
                    move |ServiceInstancePublicationResponder_Request::OnPublication {
                              responder,
                              ..
                          }| {
                        let txt = txt.clone();
                        async move {
                            responder
                                .send(&mut Ok(ServiceInstancePublication {
                                    port: Some(port),
                                    text: Some(txt),
                                    srv_priority: Some(priority),
                                    srv_weight: Some(weight),
                                    ..ServiceInstancePublication::EMPTY
                                }))
                                .map_err(Into::into)
                        }
                    },
                );

            let future = futures::future::try_join(publish_init_future, publish_responder_future)
                .map_ok(|_| ());

            services.insert(
                srp_service.full_name_cstr().to_owned(),
                AdvertisingProxyService {
                    txt_data: srp_service.txt_data().to_vec(),
                    port: srp_service.port(),
                    priority: srp_service.priority(),
                    weight: srp_service.weight(),
                    task: fuchsia_async::Task::spawn(future),
                },
            );
        }

        Ok(())
    }
}
