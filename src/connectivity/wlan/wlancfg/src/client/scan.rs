// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Manages Scan requests for the Client Policy API.
use {
    crate::{
        client::types,
        config_management::{SavedNetworksManager, ScanResultType},
        mode_management::iface_manager_api::IfaceManagerApi,
    },
    anyhow::{format_err, Error},
    async_trait::async_trait,
    fidl_fuchsia_location_sensor as fidl_location_sensor, fidl_fuchsia_wlan_policy as fidl_policy,
    fidl_fuchsia_wlan_sme as fidl_sme,
    fuchsia_async::{self as fasync, DurationExt},
    fuchsia_component::client::connect_to_protocol,
    fuchsia_zircon as zx,
    futures::{lock::Mutex, prelude::*},
    log::{debug, error, info, warn},
    std::{collections::HashMap, sync::Arc},
    stream::FuturesUnordered,
    wlan_common::channel::Channel,
};

// Arbitrary count of networks (ssid/security pairs) to output per request
const OUTPUT_CHUNK_NETWORK_COUNT: usize = 5;
// Delay between scanning retries when the firmware returns "ShouldWait" error code
const SCAN_RETRY_DELAY_MS: i64 = 100;

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
struct SmeNetworkIdentifier {
    ssid: types::Ssid,
    protection: types::SecurityTypeDetailed,
}

/// Allows for consumption of updated scan results.
#[async_trait]
pub trait ScanResultUpdate: Sync + Send {
    async fn update_scan_results(&mut self, scan_results: &Vec<types::ScanResult>);
}

/// Requests a new SME scan and returns the results.
async fn sme_scan(
    sme_proxy: &fidl_sme::ClientSmeProxy,
    scan_request: fidl_sme::ScanRequest,
) -> Result<Vec<fidl_sme::BssInfo>, ()> {
    enum SmeScanError {
        ShouldRetryLater,
        Other,
    }

    async fn sme_scan_internal(
        sme_proxy: &fidl_sme::ClientSmeProxy,
        mut scan_request: fidl_sme::ScanRequest,
    ) -> Result<Vec<fidl_sme::BssInfo>, SmeScanError> {
        let (local, remote) = fidl::endpoints::create_proxy().map_err(|e| {
            error!("Failed to create FIDL proxy for scan: {:?}", e);
            SmeScanError::Other
        })?;
        let txn = {
            match sme_proxy.scan(&mut scan_request, remote) {
                Ok(()) => local,
                Err(error) => {
                    error!("Scan initiation error: {:?}", error);
                    return Err(SmeScanError::Other);
                }
            }
        };
        debug!("Sent scan request to SME successfully");
        let mut stream = txn.take_event_stream();
        let mut scanned_networks = vec![];
        while let Some(Ok(event)) = stream.next().await {
            match event {
                fidl_sme::ScanTransactionEvent::OnResult { aps: new_aps } => {
                    debug!("Received scan results from SME");
                    scanned_networks.extend(new_aps);
                }
                fidl_sme::ScanTransactionEvent::OnFinished {} => {
                    debug!("Finished getting scan results from SME");
                    return Ok(scanned_networks);
                }
                fidl_sme::ScanTransactionEvent::OnError { error } => {
                    error!("Scan error from SME: {:?}", error);
                    return Err(
                        if error.code == fidl_sme::ScanErrorCode::ShouldWait
                            || error.code == fidl_sme::ScanErrorCode::CanceledByDriverOrFirmware
                        {
                            SmeScanError::ShouldRetryLater
                        } else {
                            SmeScanError::Other
                        },
                    );
                }
            };
        }
        error!("SME closed scan result channel without sending OnFinished");
        Err(SmeScanError::Other)
    }

    match sme_scan_internal(sme_proxy, scan_request.clone()).await {
        Ok(results) => Ok(results),
        Err(SmeScanError::ShouldRetryLater) => {
            info!("Driver requested a delay before retrying scan");
            fasync::Timer::new(zx::Duration::from_millis(SCAN_RETRY_DELAY_MS).after_now()).await;
            sme_scan_internal(sme_proxy, scan_request.clone()).await.map_err(|_| ())
        }
        Err(_) => Err(()),
    }
}

/// Handles incoming scan requests by creating a new SME scan request.
/// For the output_iterator, returns scan results and/or errors.
/// On successful scan, also provides scan results to:
/// - Emergency Location Provider
/// - Network Selection Module
pub(crate) async fn perform_scan<F>(
    iface_manager: Arc<Mutex<dyn IfaceManagerApi + Send>>,
    saved_networks_manager: Arc<SavedNetworksManager>,
    mut output_iterator: Option<fidl::endpoints::ServerEnd<fidl_policy::ScanResultIteratorMarker>>,
    mut network_selector: impl ScanResultUpdate,
    mut location_sensor_updater: impl ScanResultUpdate,
    active_scan_decider: F,
) where
    F: FnOnce(&Vec<types::Ssid>) -> Option<Vec<types::NetworkIdentifier>>,
{
    let mut bss_by_network: HashMap<SmeNetworkIdentifier, Vec<types::Bss>> = HashMap::new();

    let sme_proxy = match iface_manager.lock().await.get_sme_proxy_for_scan().await {
        Ok(proxy) => proxy,
        Err(e) => {
            // The attempt to get an SME proxy failed. Send an error to the requester, return early.
            warn!("Failed to get an SME proxy for scan: {:?}", e);
            if let Some(output_iterator) = output_iterator {
                send_scan_error_over_fidl(
                    output_iterator,
                    fidl_policy::ScanErrorCode::GeneralError,
                )
                .await
                .unwrap_or_else(|e| error!("Failed to send scan error: {}", e));
            }
            return;
        }
    };

    // Perform an initial passive scan
    let scan_request = fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {});
    let sme_result = sme_scan(&sme_proxy, scan_request).await;
    match sme_result {
        Ok(results) => {
            insert_bss_to_network_bss_map(&mut bss_by_network, results, true);
        }
        Err(()) => {
            // The passive scan failed. Send an error to the requester and return early.
            if let Some(output_iterator) = output_iterator {
                send_scan_error_over_fidl(
                    output_iterator,
                    fidl_policy::ScanErrorCode::GeneralError,
                )
                .await
                .unwrap_or_else(|e| error!("Failed to send scan error: {}", e));
            }
            return;
        }
    };

    // Determine which active scans to perform by asking the active_scan_decider()
    let observed_ssid_list = bss_by_network.iter().map(|(id, _)| id.ssid.clone()).collect();
    if let Some(requested_active_scan_ids) = active_scan_decider(&observed_ssid_list) {
        let requested_active_scan_ssids =
            requested_active_scan_ids.iter().map(|id| id.ssid.clone()).collect();
        let scan_request = fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
            ssids: requested_active_scan_ssids,
            channels: vec![],
        });
        let sme_result = sme_scan(&sme_proxy, scan_request).await;
        match sme_result {
            Ok(results) => {
                record_directed_scan_results(
                    requested_active_scan_ids,
                    &results,
                    saved_networks_manager,
                )
                .await;
                insert_bss_to_network_bss_map(&mut bss_by_network, results, false);
            }
            Err(()) => {
                // There was an error in the active scan. For the FIDL interface, send an error. We
                // `.take()` the output_iterator here, so it won't be used for sending results below.
                if let Some(output_iterator) = output_iterator.take() {
                    send_scan_error_over_fidl(
                        output_iterator,
                        fidl_policy::ScanErrorCode::GeneralError,
                    )
                    .await
                    .unwrap_or_else(|e| error!("Failed to send scan error: {}", e));
                };
                info!("Proceeding with passive scan results for non-FIDL scan consumers");
            }
        }
    };

    let scan_results = network_bss_map_to_scan_result(bss_by_network);
    // TODO(b/182569380): use actual wpa3 support in this conversion rather than hardcoding 'false'
    let fidl_scan_results = scan_result_to_policy_scan_result(&scan_results, false);
    let mut scan_result_consumers = FuturesUnordered::new();

    // Send scan results to the location sensor
    scan_result_consumers.push(location_sensor_updater.update_scan_results(&scan_results));
    // Send scan results to the network selection module
    scan_result_consumers.push(network_selector.update_scan_results(&scan_results));
    // If the requester provided a channel, send the results to them
    if let Some(output_iterator) = output_iterator {
        let requester_fut = send_scan_results_over_fidl(output_iterator, &fidl_scan_results)
            .unwrap_or_else(|e| {
                error!("Failed to send scan results to requester: {:?}", e);
            });
        scan_result_consumers.push(Box::pin(requester_fut));
    }

    while let Some(_) = scan_result_consumers.next().await {}
}

/// Perform a directed active scan for a given network on given channels.
pub(crate) async fn perform_directed_active_scan(
    sme_proxy: &fidl_sme::ClientSmeProxy,
    ssid: &Vec<u8>,
    channels: Option<Vec<u8>>,
) -> Result<Vec<types::ScanResult>, ()> {
    let scan_request = fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
        ssids: vec![ssid.clone()],
        channels: channels.unwrap_or(vec![]),
    });

    let sme_result = sme_scan(sme_proxy, scan_request).await;
    sme_result.map(|results| {
        let mut bss_by_network: HashMap<SmeNetworkIdentifier, Vec<types::Bss>> = HashMap::new();
        insert_bss_to_network_bss_map(&mut bss_by_network, results, false);

        // The active scan targets a specific SSID, ensure only that SSID is present in results
        bss_by_network.retain(|network_id, _| network_id.ssid == *ssid);

        network_bss_map_to_scan_result(bss_by_network)
    })
}

/// Figure out which saved networks we actively scanned for and did not get results for, and update
/// their configs to update the rate at which we would actively scan for these networks.
async fn record_directed_scan_results(
    target_ids: Vec<types::NetworkIdentifier>,
    scan_results: &Vec<fidl_sme::BssInfo>,
    saved_networks_manager: Arc<SavedNetworksManager>,
) {
    let ids = scan_results
        .iter()
        .map(|result| types::NetworkIdentifierDetailed {
            ssid: result.ssid.clone(),
            security_type: result.protection,
        })
        .collect();
    // TODO(fxbug.dev/70965): modify this result to use the detailed security type
    saved_networks_manager.record_scan_result(ScanResultType::Directed(target_ids), ids).await;
}

/// The location sensor module uses scan results to help determine the
/// device's location, for use by the Emergency Location Provider.
pub struct LocationSensorUpdater {
    pub wpa3_supported: bool,
}
#[async_trait]
impl ScanResultUpdate for LocationSensorUpdater {
    async fn update_scan_results(&mut self, scan_results: &Vec<types::ScanResult>) {
        async fn send_results(scan_results: &Vec<fidl_policy::ScanResult>) -> Result<(), Error> {
            // Get an output iterator
            let (iter, server) =
                fidl::endpoints::create_endpoints::<fidl_policy::ScanResultIteratorMarker>()
                    .map_err(|err| format_err!("failed to create iterator: {:?}", err))?;
            let location_watcher_proxy =
                connect_to_protocol::<fidl_location_sensor::WlanBaseStationWatcherMarker>()
                    .map_err(|err| {
                        format_err!("failed to connect to location sensor service: {:?}", err)
                    })?;
            location_watcher_proxy
                .report_current_stations(iter)
                .map_err(|err| format_err!("failed to call location sensor service: {:?}", err))?;

            // Send results to the iterator
            send_scan_results_over_fidl(server, &scan_results).await
        }

        let scan_results = scan_result_to_policy_scan_result(scan_results, self.wpa3_supported);
        // Filter out any errors and just log a message.
        // No error recovery, we'll just try again next time a scan result comes in.
        if let Err(e) = send_results(&scan_results).await {
            // TODO(fxbug.dev/52700) Upgrade this to a "warn!" once the location sensor works.
            debug!("Failed to send scan results to location sensor: {:?}", e)
        } else {
            debug!("Updated location sensor")
        };
    }
}

/// Converts sme::BssInfo to our internal BSS type, then adds it to the provided bss_by_network map.
/// Only keeps the first unique instance of a BSSID
fn insert_bss_to_network_bss_map(
    bss_by_network: &mut HashMap<SmeNetworkIdentifier, Vec<types::Bss>>,
    new_bss: Vec<fidl_sme::BssInfo>,
    observed_in_passive_scan: bool,
) {
    for bss in new_bss.into_iter() {
        let entry = bss_by_network
            .entry(SmeNetworkIdentifier { ssid: bss.ssid.to_vec(), protection: bss.protection })
            .or_insert(vec![]);
        // Check if this BSSID is already in the hashmap
        if !entry.iter().any(|existing_bss| existing_bss.bssid == bss.bssid) {
            entry.push(types::Bss {
                bssid: bss.bssid,
                rssi: bss.rssi_dbm,
                snr_db: bss.snr_db,
                channel: bss.channel,
                timestamp_nanos: 0, // TODO(mnck): find where this comes from
                observed_in_passive_scan,
                compatible: bss.compatible,
                bss_desc: bss.bss_desc,
            });
        };
    }
}

fn network_bss_map_to_scan_result(
    mut bss_by_network: HashMap<SmeNetworkIdentifier, Vec<types::Bss>>,
) -> Vec<types::ScanResult> {
    let mut scan_results: Vec<types::ScanResult> = bss_by_network
        .drain()
        .map(|(SmeNetworkIdentifier { ssid, protection }, bss_infos)| {
            let compatibility = if bss_infos.iter().any(|bss| bss.compatible) {
                fidl_policy::Compatibility::Supported
            } else {
                fidl_policy::Compatibility::DisallowedNotSupported
            };
            types::ScanResult {
                ssid: ssid,
                security_type_detailed: protection,
                entries: bss_infos,
                compatibility: compatibility,
            }
        })
        .collect();

    scan_results.sort_by(|a, b| a.ssid.cmp(&b.ssid));
    return scan_results;
}

/// Convert the protection type we receive from the SME in scan results to the Policy layer
/// security type. This function should only be used when converting to results for the public
/// FIDL API, and not for internal use within Policy, where we should prefer the detailed SME
/// security types.
fn fidl_security_from_sme_protection(
    protection: fidl_sme::Protection,
    wpa3_supported: bool,
) -> Option<fidl_policy::SecurityType> {
    use fidl_policy::SecurityType;
    use fidl_sme::Protection::*;
    match protection {
        Wpa3Enterprise | Wpa3Personal | Wpa2Wpa3Personal => {
            Some(if wpa3_supported { SecurityType::Wpa3 } else { SecurityType::Wpa2 })
        }
        Wpa2Enterprise
        | Wpa2Personal
        | Wpa1Wpa2Personal
        | Wpa2PersonalTkipOnly
        | Wpa1Wpa2PersonalTkipOnly => Some(SecurityType::Wpa2),
        Wpa1 => Some(SecurityType::Wpa),
        Wep => Some(SecurityType::Wep),
        Open => Some(SecurityType::None),
        Unknown => None,
    }
}

fn scan_result_to_policy_scan_result(
    internal_results: &Vec<types::ScanResult>,
    wpa3_supported: bool,
) -> Vec<fidl_policy::ScanResult> {
    let scan_results: Vec<fidl_policy::ScanResult> = internal_results
        .iter()
        .filter_map(|internal| {
            if let Some(security) =
                fidl_security_from_sme_protection(internal.security_type_detailed, wpa3_supported)
            {
                Some(fidl_policy::ScanResult {
                    id: Some(types::NetworkIdentifier {
                        ssid: internal.ssid.clone(),
                        type_: security,
                    }),
                    entries: Some(
                        internal
                            .entries
                            .iter()
                            .map(|input| {
                                // Get the frequency. On error, default to Some(0) rather than None
                                // to protect against consumer code that expects this field to
                                // always be set.
                                let frequency = Channel::from_fidl(input.channel)
                                    .get_center_freq()
                                    .unwrap_or(0);
                                fidl_policy::Bss {
                                    bssid: Some(input.bssid),
                                    rssi: Some(input.rssi),
                                    frequency: Some(frequency.into()), // u16.into() -> u32
                                    timestamp_nanos: Some(input.timestamp_nanos),
                                    ..fidl_policy::Bss::EMPTY
                                }
                            })
                            .collect(),
                    ),
                    compatibility: if internal.entries.iter().any(|bss| bss.compatible) {
                        Some(fidl_policy::Compatibility::Supported)
                    } else {
                        Some(fidl_policy::Compatibility::DisallowedNotSupported)
                    },
                    ..fidl_policy::ScanResult::EMPTY
                })
            } else {
                debug!(
                    "Unknown security type present in scan results: {:?}",
                    internal.security_type_detailed
                );
                None
            }
        })
        .collect();

    return scan_results;
}

/// Send batches of results to the output iterator when getNext() is called on it.
/// Close the channel when no results are remaining.
async fn send_scan_results_over_fidl(
    output_iterator: fidl::endpoints::ServerEnd<fidl_policy::ScanResultIteratorMarker>,
    scan_results: &Vec<fidl_policy::ScanResult>,
) -> Result<(), Error> {
    let mut chunks = scan_results.chunks(OUTPUT_CHUNK_NETWORK_COUNT);
    let mut sent_some_results = false;
    // Wait to get a request for a chunk of scan results
    let (mut stream, ctrl) = output_iterator.into_stream_and_control_handle()?;
    loop {
        if let Some(fidl_policy::ScanResultIteratorRequest::GetNext { responder }) =
            stream.try_next().await?
        {
            sent_some_results = true;
            if let Some(chunk) = chunks.next() {
                let mut next_result: fidl_policy::ScanResultIteratorGetNextResult =
                    Ok(chunk.to_vec());
                responder.send(&mut next_result)?;
            } else {
                // When no results are left, send an empty vec and close the channel.
                let mut next_result: fidl_policy::ScanResultIteratorGetNextResult = Ok(vec![]);
                responder.send(&mut next_result)?;
                ctrl.shutdown();
                break;
            }
        } else {
            // This will happen if the iterator request stream was closed and we expected to send
            // another response.
            if sent_some_results {
                // Some consumers may not care about all scan results, e.g. if they find the
                // particular network they were looking for. This is not an error.
                debug!("Scan result consumer closed channel before consuming all scan results");
                return Ok(());
            } else {
                return Err(format_err!("Peer closed channel before receiving any scan results"));
            }
        }
    }
    Ok(())
}

/// On the next request for results, send an error to the output iterator and
/// shut it down.
async fn send_scan_error_over_fidl(
    output_iterator: fidl::endpoints::ServerEnd<fidl_policy::ScanResultIteratorMarker>,
    error_code: fidl_policy::ScanErrorCode,
) -> Result<(), fidl::Error> {
    // Wait to get a request for a chunk of scan results
    let (mut stream, ctrl) = output_iterator.into_stream_and_control_handle()?;
    if let Some(req) = stream.try_next().await? {
        let fidl_policy::ScanResultIteratorRequest::GetNext { responder } = req;
        let mut err: fidl_policy::ScanResultIteratorGetNextResult = Err(error_code);
        responder.send(&mut err)?;
        ctrl.shutdown();
    } else {
        // This will happen if the iterator request stream was closed and we expected to send
        // another response.
        info!("Peer closed channel for getting scan results unexpectedly");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            access_point::state_machine as ap_fsm,
            config_management::network_config::{Credential, PROB_HIDDEN_DEFAULT},
            util::{
                logger::set_logger_for_test,
                testing::{
                    generate_random_bss_desc, generate_random_bss_info,
                    validate_sme_scan_request_and_send_results,
                },
            },
        },
        anyhow::Error,
        fidl::endpoints::{create_proxy, Proxy},
        fidl_fuchsia_wlan_common as fidl_common, fuchsia_async as fasync, fuchsia_zircon as zx,
        futures::{channel::oneshot, lock::Mutex, task::Poll},
        pin_utils::pin_mut,
        std::sync::Arc,
        test_case::test_case,
        wlan_common::assert_variant,
    };

    const CENTER_FREQ_CHAN_1: u32 = 2412;
    const CENTER_FREQ_CHAN_8: u32 = 2447;
    const CENTER_FREQ_CHAN_11: u32 = 2462;

    struct FakeIfaceManager {
        pub sme_proxy: fidl_fuchsia_wlan_sme::ClientSmeProxy,
        pub wpa3_capable: bool,
    }

    impl FakeIfaceManager {
        pub fn new(proxy: fidl_fuchsia_wlan_sme::ClientSmeProxy) -> Self {
            FakeIfaceManager { sme_proxy: proxy, wpa3_capable: true }
        }
    }

    #[async_trait]
    impl IfaceManagerApi for FakeIfaceManager {
        async fn disconnect(
            &mut self,
            _network_id: fidl_fuchsia_wlan_policy::NetworkIdentifier,
            _reason: types::DisconnectReason,
        ) -> Result<(), Error> {
            unimplemented!()
        }

        async fn connect(
            &mut self,
            _connect_req: types::ConnectRequest,
        ) -> Result<oneshot::Receiver<()>, Error> {
            unimplemented!()
        }

        async fn record_idle_client(&mut self, _iface_id: u16) -> Result<(), Error> {
            unimplemented!()
        }

        async fn has_idle_client(&mut self) -> Result<bool, Error> {
            unimplemented!()
        }

        async fn handle_added_iface(&mut self, _iface_id: u16) -> Result<(), Error> {
            unimplemented!()
        }

        async fn handle_removed_iface(&mut self, _iface_id: u16) -> Result<(), Error> {
            unimplemented!()
        }

        async fn scan(
            &mut self,
            mut scan_request: fidl_sme::ScanRequest,
        ) -> Result<fidl_fuchsia_wlan_sme::ScanTransactionProxy, Error> {
            let (local, remote) = fidl::endpoints::create_proxy()?;
            let _ = self.sme_proxy.scan(&mut scan_request, remote);
            Ok(local)
        }

        async fn get_sme_proxy_for_scan(
            &mut self,
        ) -> Result<fidl_fuchsia_wlan_sme::ClientSmeProxy, Error> {
            Ok(self.sme_proxy.clone())
        }

        async fn stop_client_connections(
            &mut self,
            _reason: types::DisconnectReason,
        ) -> Result<(), Error> {
            unimplemented!()
        }

        async fn start_client_connections(&mut self) -> Result<(), Error> {
            unimplemented!()
        }

        async fn start_ap(
            &mut self,
            _config: ap_fsm::ApConfig,
        ) -> Result<oneshot::Receiver<()>, Error> {
            unimplemented!()
        }

        async fn stop_ap(&mut self, _ssid: Vec<u8>, _password: Vec<u8>) -> Result<(), Error> {
            unimplemented!()
        }

        async fn stop_all_aps(&mut self) -> Result<(), Error> {
            unimplemented!()
        }

        async fn has_wpa3_capable_client(&mut self) -> Result<bool, Error> {
            Ok(self.wpa3_capable)
        }

        async fn set_country(
            &mut self,
            _country_code: Option<[u8; types::REGION_CODE_LEN]>,
        ) -> Result<(), Error> {
            unimplemented!()
        }
    }

    /// Creates a Client wrapper.
    async fn create_iface_manager(
    ) -> (Arc<Mutex<FakeIfaceManager>>, fidl_sme::ClientSmeRequestStream) {
        set_logger_for_test();
        let (client_sme, remote) =
            create_proxy::<fidl_sme::ClientSmeMarker>().expect("error creating proxy");
        let iface_manager = Arc::new(Mutex::new(FakeIfaceManager::new(client_sme)));
        (iface_manager, remote.into_stream().expect("failed to create stream"))
    }

    /// Creates an SME proxy for tests.
    async fn create_sme_proxy() -> (fidl_sme::ClientSmeProxy, fidl_sme::ClientSmeRequestStream) {
        set_logger_for_test();
        let (client_sme, remote) =
            create_proxy::<fidl_sme::ClientSmeMarker>().expect("error creating proxy");
        (client_sme, remote.into_stream().expect("failed to create stream"))
    }

    struct MockScanResultConsumer {
        scan_results: Arc<Mutex<Option<Vec<types::ScanResult>>>>,
    }
    impl MockScanResultConsumer {
        fn new() -> (Self, Arc<Mutex<Option<Vec<types::ScanResult>>>>) {
            let scan_results = Arc::new(Mutex::new(None));
            (Self { scan_results: Arc::clone(&scan_results) }, scan_results)
        }
    }
    #[async_trait]
    impl ScanResultUpdate for MockScanResultConsumer {
        async fn update_scan_results(&mut self, scan_results: &Vec<types::ScanResult>) {
            let mut guard = self.scan_results.lock().await;
            *guard = Some(scan_results.clone());
        }
    }

    // Creates test data for the scan functions.
    struct MockScanData {
        passive_input_aps: Vec<fidl_sme::BssInfo>,
        passive_internal_aps: Vec<types::ScanResult>,
        passive_fidl_aps: Vec<fidl_policy::ScanResult>,
        active_input_aps: Vec<fidl_sme::BssInfo>,
        combined_internal_aps: Vec<types::ScanResult>,
        combined_fidl_aps: Vec<fidl_policy::ScanResult>,
    }
    fn create_scan_ap_data() -> MockScanData {
        let bss_desc1 = generate_random_bss_desc();
        let bss_desc2 = generate_random_bss_desc();
        let bss_desc3 = generate_random_bss_desc();
        let passive_input_aps = vec![
            fidl_sme::BssInfo {
                bssid: [0, 0, 0, 0, 0, 0],
                ssid: "duplicated ssid".as_bytes().to_vec(),
                rssi_dbm: 0,
                snr_db: 1,
                channel: fidl_common::WlanChan {
                    primary: 1,
                    cbw: fidl_common::Cbw::Cbw20,
                    secondary80: 0,
                },
                protection: fidl_sme::Protection::Wpa3Enterprise,
                compatible: true,
                bss_desc: bss_desc1.clone(),
            },
            fidl_sme::BssInfo {
                bssid: [1, 2, 3, 4, 5, 6],
                ssid: "unique ssid".as_bytes().to_vec(),
                rssi_dbm: 7,
                snr_db: 2,
                channel: fidl_common::WlanChan {
                    primary: 8,
                    cbw: fidl_common::Cbw::Cbw20,
                    secondary80: 0,
                },
                protection: fidl_sme::Protection::Wpa2Personal,
                compatible: true,
                bss_desc: bss_desc2.clone(),
            },
            fidl_sme::BssInfo {
                bssid: [7, 8, 9, 10, 11, 12],
                ssid: "duplicated ssid".as_bytes().to_vec(),
                rssi_dbm: 13,
                snr_db: 3,
                channel: fidl_common::WlanChan {
                    primary: 11,
                    cbw: fidl_common::Cbw::Cbw20,
                    secondary80: 0,
                },
                protection: fidl_sme::Protection::Wpa3Enterprise,
                compatible: false,
                bss_desc: bss_desc3.clone(),
            },
        ];
        // input_aps contains some duplicate SSIDs, which should be
        // grouped in the output.
        let passive_internal_aps = vec![
            types::ScanResult {
                ssid: "duplicated ssid".as_bytes().to_vec(),
                security_type_detailed: types::SecurityTypeDetailed::Wpa3Enterprise,
                entries: vec![
                    types::Bss {
                        bssid: [0, 0, 0, 0, 0, 0],
                        rssi: 0,
                        timestamp_nanos: 0,
                        snr_db: 1,
                        channel: fidl_common::WlanChan {
                            primary: 1,
                            cbw: fidl_common::Cbw::Cbw20,
                            secondary80: 0,
                        },
                        observed_in_passive_scan: true,
                        compatible: true,
                        bss_desc: bss_desc1.clone(),
                    },
                    types::Bss {
                        bssid: [7, 8, 9, 10, 11, 12],
                        rssi: 13,
                        timestamp_nanos: 0,
                        snr_db: 3,
                        channel: fidl_common::WlanChan {
                            primary: 11,
                            cbw: fidl_common::Cbw::Cbw20,
                            secondary80: 0,
                        },
                        observed_in_passive_scan: true,
                        compatible: false,
                        bss_desc: bss_desc3.clone(),
                    },
                ],
                compatibility: types::Compatibility::Supported,
            },
            types::ScanResult {
                ssid: "unique ssid".as_bytes().to_vec(),
                security_type_detailed: types::SecurityTypeDetailed::Wpa2Personal,
                entries: vec![types::Bss {
                    bssid: [1, 2, 3, 4, 5, 6],
                    rssi: 7,
                    timestamp_nanos: 0,
                    snr_db: 2,
                    channel: fidl_common::WlanChan {
                        primary: 8,
                        cbw: fidl_common::Cbw::Cbw20,
                        secondary80: 0,
                    },
                    observed_in_passive_scan: true,
                    compatible: true,
                    bss_desc: bss_desc2.clone(),
                }],
                compatibility: types::Compatibility::Supported,
            },
        ];
        let passive_fidl_aps = vec![
            fidl_policy::ScanResult {
                id: Some(fidl_policy::NetworkIdentifier {
                    ssid: "duplicated ssid".as_bytes().to_vec(),
                    type_: fidl_policy::SecurityType::Wpa2,
                }),
                entries: Some(vec![
                    fidl_policy::Bss {
                        bssid: Some([0, 0, 0, 0, 0, 0]),
                        rssi: Some(0),
                        frequency: Some(CENTER_FREQ_CHAN_1),
                        timestamp_nanos: Some(0),
                        ..fidl_policy::Bss::EMPTY
                    },
                    fidl_policy::Bss {
                        bssid: Some([7, 8, 9, 10, 11, 12]),
                        rssi: Some(13),
                        frequency: Some(CENTER_FREQ_CHAN_11),
                        timestamp_nanos: Some(0),
                        ..fidl_policy::Bss::EMPTY
                    },
                ]),
                compatibility: Some(fidl_policy::Compatibility::Supported),
                ..fidl_policy::ScanResult::EMPTY
            },
            fidl_policy::ScanResult {
                id: Some(fidl_policy::NetworkIdentifier {
                    ssid: "unique ssid".as_bytes().to_vec(),
                    type_: fidl_policy::SecurityType::Wpa2,
                }),
                entries: Some(vec![fidl_policy::Bss {
                    bssid: Some([1, 2, 3, 4, 5, 6]),
                    rssi: Some(7),
                    frequency: Some(CENTER_FREQ_CHAN_8),
                    timestamp_nanos: Some(0),
                    ..fidl_policy::Bss::EMPTY
                }]),
                compatibility: Some(fidl_policy::Compatibility::Supported),
                ..fidl_policy::ScanResult::EMPTY
            },
        ];

        let bss_desc4 = generate_random_bss_desc();
        let bss_desc5 = generate_random_bss_desc();
        let active_input_aps = vec![
            fidl_sme::BssInfo {
                bssid: [9, 9, 9, 9, 9, 9],
                ssid: "foo active ssid".as_bytes().to_vec(),
                rssi_dbm: 0,
                snr_db: 8,
                channel: fidl_common::WlanChan {
                    primary: 1,
                    cbw: fidl_common::Cbw::Cbw20,
                    secondary80: 0,
                },
                protection: fidl_sme::Protection::Wpa3Enterprise,
                compatible: true,
                bss_desc: bss_desc4.clone(),
            },
            fidl_sme::BssInfo {
                bssid: [8, 8, 8, 8, 8, 8],
                ssid: "misc ssid".as_bytes().to_vec(),
                rssi_dbm: 7,
                snr_db: 9,
                channel: fidl_common::WlanChan {
                    primary: 8,
                    cbw: fidl_common::Cbw::Cbw20,
                    secondary80: 0,
                },
                protection: fidl_sme::Protection::Wpa2Personal,
                compatible: true,
                bss_desc: bss_desc5.clone(),
            },
        ];
        let combined_internal_aps = vec![
            types::ScanResult {
                ssid: "duplicated ssid".as_bytes().to_vec(),
                security_type_detailed: types::SecurityTypeDetailed::Wpa3Enterprise,
                entries: vec![
                    types::Bss {
                        bssid: [0, 0, 0, 0, 0, 0],
                        rssi: 0,
                        timestamp_nanos: 0,
                        snr_db: 1,
                        channel: fidl_common::WlanChan {
                            primary: 1,
                            cbw: fidl_common::Cbw::Cbw20,
                            secondary80: 0,
                        },
                        observed_in_passive_scan: true,
                        compatible: true,
                        bss_desc: bss_desc1.clone(),
                    },
                    types::Bss {
                        bssid: [7, 8, 9, 10, 11, 12],
                        rssi: 13,
                        timestamp_nanos: 0,
                        snr_db: 3,
                        channel: fidl_common::WlanChan {
                            primary: 11,
                            cbw: fidl_common::Cbw::Cbw20,
                            secondary80: 0,
                        },
                        observed_in_passive_scan: true,
                        compatible: false,
                        bss_desc: bss_desc3.clone(),
                    },
                ],
                compatibility: types::Compatibility::Supported,
            },
            types::ScanResult {
                ssid: "foo active ssid".as_bytes().to_vec(),
                security_type_detailed: types::SecurityTypeDetailed::Wpa3Enterprise,
                entries: vec![types::Bss {
                    bssid: [9, 9, 9, 9, 9, 9],
                    rssi: 0,
                    timestamp_nanos: 0,
                    snr_db: 8,
                    channel: fidl_common::WlanChan {
                        primary: 1,
                        cbw: fidl_common::Cbw::Cbw20,
                        secondary80: 0,
                    },
                    observed_in_passive_scan: false,
                    compatible: true,
                    bss_desc: bss_desc4,
                }],
                compatibility: types::Compatibility::Supported,
            },
            types::ScanResult {
                ssid: "misc ssid".as_bytes().to_vec(),
                security_type_detailed: types::SecurityTypeDetailed::Wpa2Personal,
                entries: vec![types::Bss {
                    bssid: [8, 8, 8, 8, 8, 8],
                    rssi: 7,
                    timestamp_nanos: 0,
                    snr_db: 9,
                    channel: fidl_common::WlanChan {
                        primary: 8,
                        cbw: fidl_common::Cbw::Cbw20,
                        secondary80: 0,
                    },
                    observed_in_passive_scan: false,
                    compatible: true,
                    bss_desc: bss_desc5,
                }],
                compatibility: types::Compatibility::Supported,
            },
            types::ScanResult {
                ssid: "unique ssid".as_bytes().to_vec(),
                security_type_detailed: types::SecurityTypeDetailed::Wpa2Personal,
                entries: vec![types::Bss {
                    bssid: [1, 2, 3, 4, 5, 6],
                    rssi: 7,
                    timestamp_nanos: 0,
                    snr_db: 2,
                    channel: fidl_common::WlanChan {
                        primary: 8,
                        cbw: fidl_common::Cbw::Cbw20,
                        secondary80: 0,
                    },
                    observed_in_passive_scan: true,
                    compatible: true,
                    bss_desc: bss_desc2,
                }],
                compatibility: types::Compatibility::Supported,
            },
        ];
        let combined_fidl_aps = vec![
            fidl_policy::ScanResult {
                id: Some(fidl_policy::NetworkIdentifier {
                    ssid: "duplicated ssid".as_bytes().to_vec(),
                    type_: fidl_policy::SecurityType::Wpa2,
                }),
                entries: Some(vec![
                    fidl_policy::Bss {
                        bssid: Some([0, 0, 0, 0, 0, 0]),
                        rssi: Some(0),
                        frequency: Some(CENTER_FREQ_CHAN_1),
                        timestamp_nanos: Some(0),
                        ..fidl_policy::Bss::EMPTY
                    },
                    fidl_policy::Bss {
                        bssid: Some([7, 8, 9, 10, 11, 12]),
                        rssi: Some(13),
                        frequency: Some(CENTER_FREQ_CHAN_11),
                        timestamp_nanos: Some(0),
                        ..fidl_policy::Bss::EMPTY
                    },
                ]),
                compatibility: Some(fidl_policy::Compatibility::Supported),
                ..fidl_policy::ScanResult::EMPTY
            },
            fidl_policy::ScanResult {
                id: Some(fidl_policy::NetworkIdentifier {
                    ssid: "foo active ssid".as_bytes().to_vec(),
                    type_: fidl_policy::SecurityType::Wpa2,
                }),
                entries: Some(vec![fidl_policy::Bss {
                    bssid: Some([9, 9, 9, 9, 9, 9]),
                    rssi: Some(0),
                    frequency: Some(CENTER_FREQ_CHAN_1),
                    timestamp_nanos: Some(0),
                    ..fidl_policy::Bss::EMPTY
                }]),
                compatibility: Some(fidl_policy::Compatibility::Supported),
                ..fidl_policy::ScanResult::EMPTY
            },
            fidl_policy::ScanResult {
                id: Some(fidl_policy::NetworkIdentifier {
                    ssid: "misc ssid".as_bytes().to_vec(),
                    type_: fidl_policy::SecurityType::Wpa2,
                }),
                entries: Some(vec![fidl_policy::Bss {
                    bssid: Some([8, 8, 8, 8, 8, 8]),
                    rssi: Some(7),
                    frequency: Some(CENTER_FREQ_CHAN_8),
                    timestamp_nanos: Some(0),
                    ..fidl_policy::Bss::EMPTY
                }]),
                compatibility: Some(fidl_policy::Compatibility::Supported),
                ..fidl_policy::ScanResult::EMPTY
            },
            fidl_policy::ScanResult {
                id: Some(fidl_policy::NetworkIdentifier {
                    ssid: "unique ssid".as_bytes().to_vec(),
                    type_: fidl_policy::SecurityType::Wpa2,
                }),
                entries: Some(vec![fidl_policy::Bss {
                    bssid: Some([1, 2, 3, 4, 5, 6]),
                    rssi: Some(7),
                    frequency: Some(CENTER_FREQ_CHAN_8),
                    timestamp_nanos: Some(0),
                    ..fidl_policy::Bss::EMPTY
                }]),
                compatibility: Some(fidl_policy::Compatibility::Supported),
                ..fidl_policy::ScanResult::EMPTY
            },
        ];

        MockScanData {
            passive_input_aps,
            passive_internal_aps,
            passive_fidl_aps,
            active_input_aps,
            combined_internal_aps,
            combined_fidl_aps,
        }
    }

    #[test]
    fn sme_scan_with_passive_request() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let (sme_proxy, mut sme_stream) = exec.run_singlethreaded(create_sme_proxy());

        // Issue request to scan.
        let scan_request = fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {});
        let scan_fut = sme_scan(&sme_proxy, scan_request.clone());
        pin_mut!(scan_fut);

        // Request scan data from SME
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Create mock scan data
        let MockScanData {
            passive_input_aps: input_aps,
            passive_internal_aps: _,
            passive_fidl_aps: _,
            active_input_aps: _,
            combined_internal_aps: _,
            combined_fidl_aps: _,
        } = create_scan_ap_data();
        // Validate the SME received the scan_request and send back mock data
        validate_sme_scan_request_and_send_results(
            &mut exec,
            &mut sme_stream,
            &scan_request,
            input_aps.clone(),
        );

        // Check for results
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Ready(result) => {
            assert_eq!(result, Ok(input_aps));
        });

        // No further requests to the sme
        assert_variant!(exec.run_until_stalled(&mut sme_stream.next()), Poll::Pending);
    }

    #[test]
    fn sme_scan_with_active_request() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let (sme_proxy, mut sme_stream) = exec.run_singlethreaded(create_sme_proxy());

        // Issue request to scan.
        let scan_request = fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
            ssids: vec!["foo_ssid".as_bytes().to_vec(), "bar_ssid".as_bytes().to_vec()],
            channels: vec![1, 20],
        });
        let scan_fut = sme_scan(&sme_proxy, scan_request.clone());
        pin_mut!(scan_fut);

        // Request scan data from SME
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Create mock scan data
        let MockScanData {
            passive_input_aps: input_aps,
            passive_internal_aps: _,
            passive_fidl_aps: _,
            active_input_aps: _,
            combined_internal_aps: _,
            combined_fidl_aps: _,
        } = create_scan_ap_data();
        // Validate the SME received the scan_request and send back mock data
        validate_sme_scan_request_and_send_results(
            &mut exec,
            &mut sme_stream,
            &scan_request,
            input_aps.clone(),
        );

        // Check for results
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Ready(result) => {
            assert_eq!(result, Ok(input_aps));
        });

        // No further requests to the sme
        assert_variant!(exec.run_until_stalled(&mut sme_stream.next()), Poll::Pending);
    }

    #[test_case(fidl_sme::ScanErrorCode::InternalError; "SME scan error InternalError")]
    #[test_case(fidl_sme::ScanErrorCode::NotSupported; "SME scan error NotSupported")]
    fn sme_scan_error(error_code: fidl_sme::ScanErrorCode) {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let (sme_proxy, mut sme_stream) = exec.run_singlethreaded(create_sme_proxy());

        // Issue request to scan.
        let scan_request = fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {});
        let scan_fut = sme_scan(&sme_proxy, scan_request.clone());
        pin_mut!(scan_fut);

        // Request scan data from SME
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Check that a scan request was sent to the sme and send back an error
        assert_variant!(
            exec.run_until_stalled(&mut sme_stream.next()),
            Poll::Ready(Some(Ok(fidl_sme::ClientSmeRequest::Scan {
                txn, ..
            }))) => {
                // Send failed scan response.
                let (_stream, ctrl) = txn
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl.send_on_error(&mut fidl_sme::ScanError {
                    code: error_code,
                    message: "Failed to scan".to_string()
                })
                    .expect("failed to send scan error");
            }
        );

        // No retry expected, check for results on the scan request
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Ready(result) => {
            assert_eq!(result, Err(()));
        });

        // No further requests to the sme
        assert_variant!(exec.run_until_stalled(&mut sme_stream.next()), Poll::Pending);
    }

    #[test_case(fidl_sme::ScanErrorCode::ShouldWait, false; "SME scan error ShouldWait with failed retry")]
    #[test_case(fidl_sme::ScanErrorCode::ShouldWait, true; "SME scan error ShouldWait with successful retry")]
    #[test_case(fidl_sme::ScanErrorCode::CanceledByDriverOrFirmware, true; "SME scan error CanceledByDriverOrFirmware with successful retry")]
    fn sme_scan_error_with_retry(error_code: fidl_sme::ScanErrorCode, retry_succeeds: bool) {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let (sme_proxy, mut sme_stream) = exec.run_singlethreaded(create_sme_proxy());

        // Issue request to scan.
        let scan_request = fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {});
        let scan_fut = sme_scan(&sme_proxy, scan_request.clone());
        pin_mut!(scan_fut);

        // Request scan data from SME
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Check that a scan request was sent to the sme and send back an error
        assert_variant!(
            exec.run_until_stalled(&mut sme_stream.next()),
            Poll::Ready(Some(Ok(fidl_sme::ClientSmeRequest::Scan {
                txn, ..
            }))) => {
                // Send failed scan response.
                let (_stream, ctrl) = txn
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl.send_on_error(&mut fidl_sme::ScanError {
                    code: error_code,
                    message: "Failed to scan".to_string()
                })
                    .expect("failed to send scan error");
            }
        );

        // Advance the scanning future
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // There shouldn't yet be a request in the SME stream, since there should be
        // some delay before the scan is retried
        assert_variant!(exec.run_until_stalled(&mut sme_stream.next()), Poll::Pending);

        // Wake up the timer and advance the scanning future
        assert!(exec.wake_next_timer().is_some());
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Expect a retry: we should now see a new request in the SME stream
        if retry_succeeds {
            // Validate the SME received the scan_request and send back mock data
            let aps = vec![];
            validate_sme_scan_request_and_send_results(
                &mut exec,
                &mut sme_stream,
                &scan_request,
                aps,
            );

            // Check for results
            assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Ready(result) => {
                assert_eq!(result, Ok(vec![]));
            });
        } else {
            // Check that a scan request was sent to the sme and send back an error
            assert_variant!(
                exec.run_until_stalled(&mut sme_stream.next()),
                Poll::Ready(Some(Ok(fidl_sme::ClientSmeRequest::Scan {
                    txn, ..
                }))) => {
                    // Send failed scan response.
                    let (_stream, ctrl) = txn
                        .into_stream_and_control_handle().expect("error accessing control handle");
                    ctrl.send_on_error(&mut fidl_sme::ScanError {
                        code: error_code,
                        message: "Failed to scan".to_string()
                    })
                        .expect("failed to send scan error");
                }
            );

            // Check for results
            assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Ready(result) => {
                assert_eq!(result, Err(()));
            });
        }

        // No further requests to the sme
        assert_variant!(exec.run_until_stalled(&mut sme_stream.next()), Poll::Pending);
    }

    #[test]
    fn sme_scan_channel_closed() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let (sme_proxy, mut sme_stream) = exec.run_singlethreaded(create_sme_proxy());

        // Issue request to scan.
        let scan_request = fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {});
        let scan_fut = sme_scan(&sme_proxy, scan_request);
        pin_mut!(scan_fut);

        // Request scan data from SME
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Check that a scan request was sent to the sme and send back an error
        assert_variant!(
            exec.run_until_stalled(&mut sme_stream.next()),
            Poll::Ready(Some(Ok(fidl_sme::ClientSmeRequest::Scan {
                txn, ..
            }))) => {
                // Send failed scan response.
                txn.close_with_epitaph(zx::Status::OK).expect("Failed to close channel");
            }
        );

        // Check for results
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Ready(result) => {
            assert_eq!(result, Err(()));
        });

        // No further requests to the sme
        assert_variant!(exec.run_until_stalled(&mut sme_stream.next()), Poll::Pending);
    }

    #[test]
    fn basic_scan() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let (client, mut sme_stream) = exec.run_singlethreaded(create_iface_manager());
        let (network_selector, network_selector_results) = MockScanResultConsumer::new();
        let (location_sensor, location_sensor_results) = MockScanResultConsumer::new();
        let saved_networks_manager = create_saved_networks_manager(&mut exec);

        // Issue request to scan.
        let (iter, iter_server) =
            fidl::endpoints::create_proxy().expect("failed to create iterator");
        let scan_fut = perform_scan(
            client,
            saved_networks_manager,
            Some(iter_server),
            network_selector,
            location_sensor,
            |_| None,
        );
        pin_mut!(scan_fut);

        // Request a chunk of scan results. Progress until waiting on response from server side of
        // the iterator.
        let mut output_iter_fut = iter.get_next();
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut), Poll::Pending);
        // Progress scan handler forward so that it will respond to the iterator get next request.
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Create mock scan data and send it via the SME
        let expected_scan_request = fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {});
        let MockScanData {
            passive_input_aps: input_aps,
            passive_internal_aps: internal_aps,
            passive_fidl_aps: fidl_aps,
            active_input_aps: _,
            combined_internal_aps: _,
            combined_fidl_aps: _,
        } = create_scan_ap_data();
        validate_sme_scan_request_and_send_results(
            &mut exec,
            &mut sme_stream,
            &expected_scan_request,
            input_aps.clone(),
        );

        // Process response from SME
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Check for results
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut), Poll::Ready(result) => {
            let results = result.expect("Failed to get next scan results").unwrap();
            assert_eq!(results, fidl_aps);
        });

        // Request the next chunk of scan results. Progress until waiting on response from server side of
        // the iterator.
        let mut output_iter_fut = iter.get_next();

        // Process scan handler
        // Note: this will be Poll::Ready because the scan handler will exit after sending the final
        // scan results.
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Ready(()));

        // Check for results
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut), Poll::Ready(result) => {
            let results = result.expect("Failed to get next scan results").unwrap();
            assert_eq!(results, vec![]);
        });

        // Check both successful scan consumers got results
        assert_eq!(
            *exec.run_singlethreaded(network_selector_results.lock()),
            Some(internal_aps.clone())
        );
        assert_eq!(
            *exec.run_singlethreaded(location_sensor_results.lock()),
            Some(internal_aps.clone())
        );
    }

    #[test]
    fn scan_with_active_scan_decider() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let (client, mut sme_stream) = exec.run_singlethreaded(create_iface_manager());
        let (network_selector, network_selector_results) = MockScanResultConsumer::new();
        let (location_sensor, location_sensor_results) = MockScanResultConsumer::new();
        let saved_networks_manager = create_saved_networks_manager(&mut exec);

        // Create the passive and active scan info
        let MockScanData {
            passive_input_aps,
            passive_internal_aps,
            passive_fidl_aps: _,
            active_input_aps,
            combined_internal_aps,
            combined_fidl_aps,
        } = create_scan_ap_data();

        // Save the network that isn't seen in the scan so we can check its hidden probability.
        let unseen_ssid = b"unseen_id".to_vec();
        let unseen_active_id =
            types::NetworkIdentifier { ssid: unseen_ssid.clone(), type_: types::SecurityType::Wpa };
        let credential = Credential::Password(b"some-cred".to_vec());
        assert!(exec
            .run_singlethreaded(
                saved_networks_manager.store(unseen_active_id.clone().into(), credential),
            )
            .expect("failed to store network")
            .is_none());
        let config = exec
            .run_singlethreaded(saved_networks_manager.lookup(unseen_active_id.clone().into()))
            .pop()
            .expect("failed to lookup");
        assert_eq!(config.hidden_probability, PROB_HIDDEN_DEFAULT);

        // Issue request to scan.
        let (iter, iter_server) =
            fidl::endpoints::create_proxy().expect("failed to create iterator");
        let expected_passive_results: Vec<types::Ssid> =
            passive_internal_aps.iter().map(|result| result.ssid.clone()).collect();
        let active_ssid = b"foo active ssid".to_vec();
        let scan_fut = perform_scan(
            client,
            saved_networks_manager.clone(),
            Some(iter_server),
            network_selector,
            location_sensor,
            |passive_results| {
                assert_eq!(passive_results.clone().sort(), expected_passive_results.clone().sort());
                Some(vec![
                    types::NetworkIdentifier {
                        ssid: active_ssid.clone(),
                        type_: types::SecurityType::Wpa2,
                    },
                    unseen_active_id.clone(),
                ])
            },
        );
        pin_mut!(scan_fut);

        // Request a chunk of scan results. Progress until waiting on response from server side of
        // the iterator.
        let mut output_iter_fut = iter.get_next();
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut), Poll::Pending);
        // Progress scan handler forward so that it will respond to the iterator get next request.
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Respond to the first (passive) scan request
        let expected_scan_request = fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {});
        validate_sme_scan_request_and_send_results(
            &mut exec,
            &mut sme_stream,
            &expected_scan_request,
            passive_input_aps.clone(),
        );

        // Process response from SME
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Respond to the second (active) scan request
        let expected_scan_request = fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
            ssids: vec![active_ssid.clone(), unseen_ssid.clone()],
            channels: vec![],
        });
        validate_sme_scan_request_and_send_results(
            &mut exec,
            &mut sme_stream,
            &expected_scan_request,
            active_input_aps.clone(),
        );

        // Process response from SME
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Check for results
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut), Poll::Ready(result) => {
            let results = result.expect("Failed to get next scan results").unwrap();
            assert_eq!(results, combined_fidl_aps);
        });

        // Request the next chunk of scan results. Progress until waiting on response from server side of
        // the iterator.
        let mut output_iter_fut = iter.get_next();

        // Process scan handler
        // Note: this will be Poll::Ready because the scan handler will exit after sending the final
        // scan results.
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Ready(()));

        // Check for results
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut), Poll::Ready(result) => {
            let results = result.expect("Failed to get next scan results").unwrap();
            assert_eq!(results, vec![]);
        });

        // Check both successful scan consumers got results
        assert_eq!(
            *exec.run_singlethreaded(network_selector_results.lock()),
            Some(combined_internal_aps.clone())
        );
        assert_eq!(
            *exec.run_singlethreaded(location_sensor_results.lock()),
            Some(combined_internal_aps.clone())
        );

        // Verify that the network we actively scanned for but didn't see had its hidden
        // probability updated.
        let config = exec
            .run_singlethreaded(saved_networks_manager.lookup(unseen_active_id.clone().into()))
            .pop()
            .expect("Failed to get network config");
        assert!(config.hidden_probability < PROB_HIDDEN_DEFAULT);
    }

    #[test]
    fn insert_bss_to_network_bss_map_duplicated_bss() {
        let mut bss_by_network = HashMap::new();

        // Create some input data with duplicated BSSID and Network Identifiers
        let passive_bss_desc = generate_random_bss_desc();
        let passive_input_aps = vec![
            fidl_sme::BssInfo {
                bssid: [0, 0, 0, 0, 0, 0],
                ssid: "duplicated ssid".as_bytes().to_vec(),
                rssi_dbm: 0,
                snr_db: 1,
                channel: fidl_common::WlanChan {
                    primary: 1,
                    cbw: fidl_common::Cbw::Cbw20,
                    secondary80: 0,
                },
                protection: fidl_sme::Protection::Wpa3Enterprise,
                compatible: true,
                bss_desc: passive_bss_desc.clone(),
            },
            fidl_sme::BssInfo {
                bssid: [0, 0, 0, 0, 0, 0],
                ssid: "duplicated ssid".as_bytes().to_vec(),
                rssi_dbm: 13,
                snr_db: 3,
                channel: fidl_common::WlanChan {
                    primary: 14,
                    cbw: fidl_common::Cbw::Cbw20,
                    secondary80: 0,
                },
                protection: fidl_sme::Protection::Wpa3Enterprise,
                compatible: true,
                bss_desc: generate_random_bss_desc(),
            },
        ];

        let expected_id = SmeNetworkIdentifier {
            ssid: "duplicated ssid".as_bytes().to_vec(),
            protection: fidl_sme::Protection::Wpa3Enterprise,
        };

        // We should only see one entry for the duplicated BSSs in the passive scan results
        let expected_bss = vec![types::Bss {
            bssid: [0, 0, 0, 0, 0, 0],
            rssi: 0,
            timestamp_nanos: 0,
            snr_db: 1,
            channel: fidl_common::WlanChan {
                primary: 1,
                cbw: fidl_common::Cbw::Cbw20,
                secondary80: 0,
            },
            observed_in_passive_scan: true,
            compatible: true,
            bss_desc: passive_bss_desc.clone(),
        }];

        insert_bss_to_network_bss_map(&mut bss_by_network, passive_input_aps, true);
        assert_eq!(bss_by_network.len(), 1);
        assert_eq!(bss_by_network[&expected_id], expected_bss);

        // Create some input data with one duplicate BSSID and one new BSSID
        let active_bss_desc = generate_random_bss_desc();
        let active_input_aps = vec![
            fidl_sme::BssInfo {
                bssid: [0, 0, 0, 0, 0, 0],
                ssid: "duplicated ssid".as_bytes().to_vec(),
                rssi_dbm: 100,
                snr_db: 100,
                channel: fidl_common::WlanChan {
                    primary: 100,
                    cbw: fidl_common::Cbw::Cbw40,
                    secondary80: 0,
                },
                protection: fidl_sme::Protection::Wpa3Enterprise,
                compatible: true,
                bss_desc: generate_random_bss_desc(),
            },
            fidl_sme::BssInfo {
                bssid: [1, 2, 3, 4, 5, 6],
                ssid: "duplicated ssid".as_bytes().to_vec(),
                rssi_dbm: 101,
                snr_db: 101,
                channel: fidl_common::WlanChan {
                    primary: 101,
                    cbw: fidl_common::Cbw::Cbw40,
                    secondary80: 0,
                },
                protection: fidl_sme::Protection::Wpa3Enterprise,
                compatible: true,
                bss_desc: active_bss_desc.clone(),
            },
        ];

        // After the active scan, there should be a second bss included in the results
        let expected_bss = vec![
            types::Bss {
                bssid: [0, 0, 0, 0, 0, 0],
                rssi: 0,
                timestamp_nanos: 0,
                snr_db: 1,
                channel: fidl_common::WlanChan {
                    primary: 1,
                    cbw: fidl_common::Cbw::Cbw20,
                    secondary80: 0,
                },
                observed_in_passive_scan: true,
                compatible: true,
                bss_desc: passive_bss_desc.clone(),
            },
            types::Bss {
                bssid: [1, 2, 3, 4, 5, 6],
                rssi: 101,
                timestamp_nanos: 0,
                snr_db: 101,
                channel: fidl_common::WlanChan {
                    primary: 101,
                    cbw: fidl_common::Cbw::Cbw40,
                    secondary80: 0,
                },
                observed_in_passive_scan: false,
                compatible: true,
                bss_desc: active_bss_desc.clone(),
            },
        ];

        insert_bss_to_network_bss_map(&mut bss_by_network, active_input_aps, false);
        assert_eq!(bss_by_network.len(), 1);
        assert_eq!(bss_by_network[&expected_id], expected_bss);
    }

    #[test]
    fn scan_with_active_scan_decider_and_active_scan_failure() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let (client, mut sme_stream) = exec.run_singlethreaded(create_iface_manager());
        let (network_selector, network_selector_results) = MockScanResultConsumer::new();
        let (location_sensor, location_sensor_results) = MockScanResultConsumer::new();
        let saved_networks_manager = create_saved_networks_manager(&mut exec);

        // Create the passive and active scan info
        let MockScanData {
            passive_input_aps,
            passive_internal_aps,
            passive_fidl_aps: _,
            active_input_aps: _,
            combined_internal_aps: _,
            combined_fidl_aps: _,
        } = create_scan_ap_data();

        // Issue request to scan.
        let (iter, iter_server) =
            fidl::endpoints::create_proxy().expect("failed to create iterator");
        let expected_passive_results: Vec<types::Ssid> =
            passive_internal_aps.iter().map(|result| result.ssid.clone()).collect();
        let active_ssid = b"foo active ssid".to_vec();
        let scan_fut = perform_scan(
            client,
            saved_networks_manager,
            Some(iter_server),
            network_selector,
            location_sensor,
            |passive_results| {
                assert_eq!(passive_results.clone().sort(), expected_passive_results.clone().sort());
                Some(vec![types::NetworkIdentifier {
                    ssid: active_ssid.clone(),
                    type_: types::SecurityType::Wpa,
                }])
            },
        );
        pin_mut!(scan_fut);

        // Request a chunk of scan results. Progress until waiting on response from server side of
        // the iterator.
        let mut output_iter_fut = iter.get_next();
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut), Poll::Pending);
        // Progress scan handler forward so that it will respond to the iterator get next request.
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Respond to the first (passive) scan request
        let expected_scan_request = fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {});
        validate_sme_scan_request_and_send_results(
            &mut exec,
            &mut sme_stream,
            &expected_scan_request,
            passive_input_aps.clone(),
        );

        // Process response from SME
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Check that a scan request was sent to the sme and send back an error
        let expected_scan_request = fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
            ssids: vec![active_ssid.clone()],
            channels: vec![],
        });
        assert_variant!(
            exec.run_until_stalled(&mut sme_stream.next()),
            Poll::Ready(Some(Ok(fidl_sme::ClientSmeRequest::Scan {
                txn, req, ..
            }))) => {
                assert_eq!(req, expected_scan_request);
                // Send failed scan response.
                let (_stream, ctrl) = txn
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl.send_on_error(&mut fidl_sme::ScanError {
                    code: fidl_sme::ScanErrorCode::InternalError,
                    message: "Failed to scan".to_string()
                })
                    .expect("failed to send scan error");
            }
        );

        // Process scan handler
        // Note: this will be Poll::Ready because the scan handler will exit after sending the final
        // scan results.
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Ready(()));

        // Check the FIDL result -- this should be an error, since the active scan failed
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut), Poll::Ready(result) => {
            let result = result.expect("Failed to get next scan results").unwrap_err();
            assert_eq!(result, fidl_policy::ScanErrorCode::GeneralError);
        });

        // Check both scan consumers got just the passive scan results, since the active scan failed
        assert_eq!(
            *exec.run_singlethreaded(network_selector_results.lock()),
            Some(passive_internal_aps.clone())
        );
        assert_eq!(
            *exec.run_singlethreaded(location_sensor_results.lock()),
            Some(passive_internal_aps.clone())
        );
    }

    #[test]
    fn scan_iterator_never_polled() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let (client, mut sme_stream) = exec.run_singlethreaded(create_iface_manager());
        let (network_selector1, network_selector_results1) = MockScanResultConsumer::new();
        let (location_sensor1, location_sensor_results1) = MockScanResultConsumer::new();
        let (network_selector2, network_selector_results2) = MockScanResultConsumer::new();
        let (location_sensor2, location_sensor_results2) = MockScanResultConsumer::new();
        let saved_networks_manager = create_saved_networks_manager(&mut exec);

        // Issue request to scan.
        let (_iter, iter_server) =
            fidl::endpoints::create_proxy().expect("failed to create iterator");
        let scan_fut = perform_scan(
            client.clone(),
            saved_networks_manager.clone(),
            Some(iter_server),
            network_selector1,
            location_sensor1,
            |_| None,
        );
        pin_mut!(scan_fut);

        // Progress scan side forward without ever calling getNext() on the scan result iterator
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Create mock scan data and send it via the SME
        let expected_scan_request = fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {});
        let MockScanData {
            passive_input_aps: input_aps,
            passive_internal_aps: internal_aps,
            passive_fidl_aps: fidl_aps,
            active_input_aps: _,
            combined_internal_aps: _,
            combined_fidl_aps: _,
        } = create_scan_ap_data();
        validate_sme_scan_request_and_send_results(
            &mut exec,
            &mut sme_stream,
            &expected_scan_request,
            input_aps.clone(),
        );

        // Progress scan side forward without progressing the scan result iterator
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Issue a second request to scan, to make sure that everything is still
        // moving along even though the first scan result iterator was never progressed.
        let (iter2, iter_server2) =
            fidl::endpoints::create_proxy().expect("failed to create iterator");
        let scan_fut2 = perform_scan(
            client,
            saved_networks_manager,
            Some(iter_server2),
            network_selector2,
            location_sensor2,
            |_| None,
        );
        pin_mut!(scan_fut2);

        // Progress scan side forward
        assert_variant!(exec.run_until_stalled(&mut scan_fut2), Poll::Pending);

        // Create mock scan data and send it via the SME
        let expected_scan_request = fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {});
        validate_sme_scan_request_and_send_results(
            &mut exec,
            &mut sme_stream,
            &expected_scan_request,
            input_aps.clone(),
        );

        // Request the results on the second iterator
        let mut output_iter_fut2 = iter2.get_next();

        // Progress scan side forward
        assert_variant!(exec.run_until_stalled(&mut scan_fut2), Poll::Pending);

        // Ensure results are present on the iterator
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut2), Poll::Ready(result) => {
            let results = result.expect("Failed to get next scan results").unwrap();
            assert_eq!(results, fidl_aps);
        });

        // Check all successful scan consumers got results
        assert_eq!(
            *exec.run_singlethreaded(network_selector_results1.lock()),
            Some(internal_aps.clone())
        );
        assert_eq!(
            *exec.run_singlethreaded(location_sensor_results1.lock()),
            Some(internal_aps.clone())
        );
        assert_eq!(
            *exec.run_singlethreaded(network_selector_results2.lock()),
            Some(internal_aps.clone())
        );
        assert_eq!(
            *exec.run_singlethreaded(location_sensor_results2.lock()),
            Some(internal_aps.clone())
        );
    }

    #[test]
    fn scan_iterator_shut_down() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let (client, mut sme_stream) = exec.run_singlethreaded(create_iface_manager());
        let (network_selector, network_selector_results) = MockScanResultConsumer::new();
        let (location_sensor, location_sensor_results) = MockScanResultConsumer::new();
        let saved_networks_manager = create_saved_networks_manager(&mut exec);

        // Issue request to scan.
        let (iter, iter_server) =
            fidl::endpoints::create_proxy().expect("failed to create iterator");
        let scan_fut = perform_scan(
            client,
            saved_networks_manager,
            Some(iter_server),
            network_selector,
            location_sensor,
            |_| None,
        );
        pin_mut!(scan_fut);

        // Progress scan handler forward so that it will respond to the iterator get next request.
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Create mock scan data and send it via the SME
        let expected_scan_request = fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {});
        let MockScanData {
            passive_input_aps: input_aps,
            passive_internal_aps: internal_aps,
            passive_fidl_aps: _,
            active_input_aps: _,
            combined_internal_aps: _,
            combined_fidl_aps: _,
        } = create_scan_ap_data();
        validate_sme_scan_request_and_send_results(
            &mut exec,
            &mut sme_stream,
            &expected_scan_request,
            input_aps.clone(),
        );

        // Close the channel
        drop(iter.into_channel());

        // Process scan handler
        // Note: this will be Poll::Ready because the scan handler will exit since all the consumers are done
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Ready(()));

        // Check both successful scan consumers got results
        assert_eq!(
            *exec.run_singlethreaded(network_selector_results.lock()),
            Some(internal_aps.clone())
        );
        assert_eq!(
            *exec.run_singlethreaded(location_sensor_results.lock()),
            Some(internal_aps.clone())
        );
    }

    #[test]
    fn scan_error() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let (client, mut sme_stream) = exec.run_singlethreaded(create_iface_manager());
        let (network_selector, network_selector_results) = MockScanResultConsumer::new();
        let (location_sensor, location_sensor_results) = MockScanResultConsumer::new();
        let saved_networks_manager = create_saved_networks_manager(&mut exec);

        // Issue request to scan.
        let (iter, iter_server) =
            fidl::endpoints::create_proxy().expect("failed to create iterator");
        let scan_fut = perform_scan(
            client,
            saved_networks_manager,
            Some(iter_server),
            network_selector,
            location_sensor,
            |_| None,
        );
        pin_mut!(scan_fut);

        // Request a chunk of scan results. Progress until waiting on response from server side of
        // the iterator.
        let mut output_iter_fut = iter.get_next();
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut), Poll::Pending);
        // Progress scan handler forward so that it will respond to the iterator get next request.
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Check that a scan request was sent to the sme and send back an error
        assert_variant!(
            exec.run_until_stalled(&mut sme_stream.next()),
            Poll::Ready(Some(Ok(fidl_sme::ClientSmeRequest::Scan {
                txn, ..
            }))) => {
                // Send failed scan response.
                let (_stream, ctrl) = txn
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl.send_on_error(&mut fidl_sme::ScanError {
                    code: fidl_sme::ScanErrorCode::InternalError,
                    message: "Failed to scan".to_string()
                })
                    .expect("failed to send scan error");
            }
        );

        // Process SME result.
        // Note: this will be Poll::Ready, since the scan handler will quit after sending the error
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Ready(()));

        // the iterator should have an error on it
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut), Poll::Ready(result) => {
            let results = result.expect("Failed to get next scan results");
            assert_eq!(results, Err(fidl_policy::ScanErrorCode::GeneralError));
        });

        // Check both successful scan consumers have no results
        assert_eq!(*exec.run_singlethreaded(network_selector_results.lock()), None);
        assert_eq!(*exec.run_singlethreaded(location_sensor_results.lock()), None);
    }

    #[test]
    fn overlapping_scans() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let (client, mut sme_stream) = exec.run_singlethreaded(create_iface_manager());
        let (network_selector1, network_selector_results1) = MockScanResultConsumer::new();
        let (location_sensor1, location_sensor_results1) = MockScanResultConsumer::new();
        let (network_selector2, network_selector_results2) = MockScanResultConsumer::new();
        let (location_sensor2, location_sensor_results2) = MockScanResultConsumer::new();
        let saved_networks_manager = create_saved_networks_manager(&mut exec);

        let MockScanData {
            passive_input_aps,
            passive_internal_aps,
            passive_fidl_aps,
            active_input_aps,
            combined_internal_aps,
            combined_fidl_aps,
        } = create_scan_ap_data();

        // Create two sets of endpoints
        let (iter0, iter_server0) =
            fidl::endpoints::create_proxy().expect("failed to create iterator");
        let (iter1, iter_server1) =
            fidl::endpoints::create_proxy().expect("failed to create iterator");

        // Issue request to scan on both iterator.
        let scan_fut0 = perform_scan(
            client.clone(),
            saved_networks_manager.clone(),
            Some(iter_server0),
            network_selector1,
            location_sensor1,
            |_| None,
        );
        pin_mut!(scan_fut0);
        let active_ssid = b"foo active ssid".to_vec();
        let expected_passive_results: Vec<types::Ssid> =
            passive_internal_aps.iter().map(|result| result.ssid.clone()).collect();
        let scan_fut1 = perform_scan(
            client.clone(),
            saved_networks_manager,
            Some(iter_server1),
            network_selector2,
            location_sensor2,
            |passive_results| {
                assert_eq!(passive_results.clone().sort(), expected_passive_results.clone().sort());
                Some(vec![types::NetworkIdentifier {
                    ssid: active_ssid.clone(),
                    type_: types::SecurityType::Wpa3,
                }])
            },
        );
        pin_mut!(scan_fut1);

        // Request a chunk of scan results on both iterators. Progress until waiting on
        // response from server side of the iterator.
        let mut output_iter_fut0 = iter0.get_next();
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut0), Poll::Pending);
        let mut output_iter_fut1 = iter1.get_next();
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut1), Poll::Pending);

        // Progress first scan handler forward so that it will respond to the iterator get next request.
        assert_variant!(exec.run_until_stalled(&mut scan_fut0), Poll::Pending);

        // Check that a scan request was sent to the sme and send back results
        assert_variant!(
            exec.run_until_stalled(&mut sme_stream.next()),
            Poll::Ready(Some(Ok(fidl_sme::ClientSmeRequest::Scan {
                txn, ..
            }))) => {
                // Send the first AP
                let (_stream, ctrl) = txn
                    .into_stream_and_control_handle().expect("error accessing control handle");
                let mut aps = [passive_input_aps[0].clone()];
                ctrl.send_on_result(&mut aps.iter_mut())
                    .expect("failed to send scan data");
                // Process SME result.
                assert_variant!(exec.run_until_stalled(&mut scan_fut0), Poll::Pending);
                // The iterator should not have any data yet, until the sme is done
                assert_variant!(exec.run_until_stalled(&mut output_iter_fut0), Poll::Pending);

                // Progress second scan handler forward so that it will respond to the iterator get next request.
                assert_variant!(exec.run_until_stalled(&mut scan_fut1), Poll::Pending);
                // Check that the second scan request was sent to the sme and send back results
                let expected_scan_request = fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {});
                validate_sme_scan_request_and_send_results(&mut exec, &mut sme_stream, &expected_scan_request, passive_input_aps.clone()); // for output_iter_fut1
                // Process SME result.
                assert_variant!(exec.run_until_stalled(&mut scan_fut1), Poll::Pending);
                // The second request should now result in an active scan
                let expected_scan_request = fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
                    channels: vec![],
                    ssids: vec![active_ssid.clone()],
                });
                validate_sme_scan_request_and_send_results(&mut exec, &mut sme_stream, &expected_scan_request, active_input_aps.clone()); // for output_iter_fut1
                // Process SME result.
                assert_variant!(exec.run_until_stalled(&mut scan_fut1), Poll::Pending);// The second iterator should have all its data

                assert_variant!(exec.run_until_stalled(&mut output_iter_fut1), Poll::Ready(result) => {
                    let results = result.expect("Failed to get next scan results").unwrap();
                    assert_eq!(results.len(), combined_fidl_aps.len());
                    assert_eq!(results, combined_fidl_aps);
                });

                // Send the remaining APs for the first iterator
                let mut aps = passive_input_aps[1..].iter().map(|a| a.clone()).collect::<Vec<_>>();
                ctrl.send_on_result(&mut aps.iter_mut())
                    .expect("failed to send scan data");
                // Process SME result.
                assert_variant!(exec.run_until_stalled(&mut scan_fut0), Poll::Pending);
                // Send the end of data
                ctrl.send_on_finished()
                    .expect("failed to send scan data");
            }
        );

        // Process response from SME
        assert_variant!(exec.run_until_stalled(&mut scan_fut0), Poll::Pending);

        // The first iterator should have all its data
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut0), Poll::Ready(result) => {
            let results = result.expect("Failed to get next scan results").unwrap();
            assert_eq!(results.len(), passive_fidl_aps.len());
            assert_eq!(results, passive_fidl_aps);
        });

        // Check both successful scan consumers got results
        assert_eq!(
            *exec.run_singlethreaded(network_selector_results1.lock()),
            Some(passive_internal_aps.clone())
        );
        assert_eq!(
            *exec.run_singlethreaded(location_sensor_results1.lock()),
            Some(passive_internal_aps.clone())
        );
        assert_eq!(
            *exec.run_singlethreaded(network_selector_results2.lock()),
            Some(combined_internal_aps.clone())
        );
        assert_eq!(
            *exec.run_singlethreaded(location_sensor_results2.lock()),
            Some(combined_internal_aps.clone())
        );
    }

    #[test_case(true)]
    #[test_case(false)]
    fn perform_scan_wpa3_supported(wpa3_capable: bool) {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let (network_selector, network_selector_results) = MockScanResultConsumer::new();
        let (location_sensor, location_sensor_results) = MockScanResultConsumer::new();
        let saved_networks_manager = create_saved_networks_manager(&mut exec);
        set_logger_for_test();

        // Create a FakeIfaceManager that returns has_wpa3_iface() based on test value
        let (sme_proxy, remote) =
            create_proxy::<fidl_sme::ClientSmeMarker>().expect("error creating proxy");
        let mut sme_stream = remote.into_stream().expect("failed to create stream");
        let client = Arc::new(Mutex::new(FakeIfaceManager { sme_proxy, wpa3_capable }));

        // Issue request to scan.
        let (iter, iter_server) =
            fidl::endpoints::create_proxy().expect("failed to create iterator");
        let scan_fut = perform_scan(
            client,
            saved_networks_manager,
            Some(iter_server),
            network_selector,
            location_sensor,
            |_| None,
        );
        pin_mut!(scan_fut);

        // Request a chunk of scan results. Progress until waiting on response from server side of
        // the iterator.
        let mut output_iter_fut = iter.get_next();
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut), Poll::Pending);
        // Progress scan handler forward so that it will respond to the iterator get next request.
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Generate scan results
        let ssid = b"some_ssid".to_vec();
        let bss_info = fidl_sme::BssInfo {
            ssid: ssid.clone(),
            protection: fidl_sme::Protection::Wpa2Wpa3Personal,
            compatible: true,
            channel: fidl_common::WlanChan {
                primary: 8,
                cbw: fidl_common::Cbw::Cbw20,
                secondary80: 0,
            },
            ..generate_random_bss_info()
        };
        let scan_result_aps = vec![bss_info.clone()];
        let _type_ =
            if wpa3_capable { types::SecurityType::Wpa3 } else { types::SecurityType::Wpa2 };
        let expected_scan_results = vec![fidl_policy::ScanResult {
            id: Some(fidl_policy::NetworkIdentifier {
                ssid: ssid.clone(),
                // Note: for now, we must always present WPA2/3 networks as WPA2 over our external
                // interfaces (i.e. to FIDL consumers of scan results). See b/182209070 for more
                // information.
                // TODO(b/182569380): change this back to a variable `type_` based on WPA3 support.
                type_: types::SecurityType::Wpa2,
            }),
            entries: Some(vec![fidl_policy::Bss {
                bssid: Some(bss_info.bssid),
                rssi: Some(bss_info.rssi_dbm),
                // For now frequency and timestamp are set to 0 when converting types.
                frequency: Some(CENTER_FREQ_CHAN_8),
                timestamp_nanos: Some(0),
                ..fidl_policy::Bss::EMPTY
            }]),
            compatibility: Some(types::Compatibility::Supported),
            ..fidl_policy::ScanResult::EMPTY
        }];
        let expected_internal_scans = vec![types::ScanResult {
            ssid: ssid.clone(),
            security_type_detailed: types::SecurityTypeDetailed::Wpa2Wpa3Personal,
            compatibility: fidl_policy::Compatibility::Supported,
            entries: vec![types::Bss {
                bssid: bss_info.bssid.clone(),
                rssi: bss_info.rssi_dbm,
                snr_db: bss_info.snr_db,
                channel: bss_info.channel,
                timestamp_nanos: 0,
                observed_in_passive_scan: true,
                compatible: bss_info.compatible,
                bss_desc: bss_info.bss_desc,
            }],
        }];
        // Create mock scan data and send it via the SME
        let expected_scan_request = fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {});
        validate_sme_scan_request_and_send_results(
            &mut exec,
            &mut sme_stream,
            &expected_scan_request,
            scan_result_aps.clone(),
        );

        // Process response from SME
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Check for results
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut), Poll::Ready(result) => {
            let results = result.expect("Failed to get next scan results").unwrap();
            assert_eq!(results, expected_scan_results);
        });

        // Request the next chunk of scan results. Progress until waiting on response from server side of
        // the iterator.
        let mut output_iter_fut = iter.get_next();

        // Process scan handler
        // Note: this will be Poll::Ready because the scan handler will exit after sending the final
        // scan results.
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Ready(()));

        // Check for results
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut), Poll::Ready(result) => {
            let results = result.expect("Failed to get next scan results").unwrap();
            assert_eq!(results, vec![]);
        });

        // Check both successful scan consumers got results
        assert_eq!(
            *exec.run_singlethreaded(network_selector_results.lock()),
            Some(expected_internal_scans.clone())
        );
        assert_eq!(
            *exec.run_singlethreaded(location_sensor_results.lock()),
            Some(expected_internal_scans.clone())
        );
    }

    // TODO(fxbug.dev/54255): Separate test case for "empty final vector not consumed" vs "partial ap list"
    // consumed.
    #[test]
    fn partial_scan_result_consumption_has_no_error() {
        set_logger_for_test();
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let MockScanData {
            passive_input_aps: _,
            passive_internal_aps: _,
            passive_fidl_aps: _,
            active_input_aps: _,
            combined_internal_aps: _,
            combined_fidl_aps,
        } = create_scan_ap_data();

        // Create an iterator and send scan results
        let (iter, iter_server) =
            fidl::endpoints::create_proxy().expect("failed to create iterator");
        let send_fut = send_scan_results_over_fidl(iter_server, &combined_fidl_aps);
        pin_mut!(send_fut);

        // Request a chunk of scan results.
        let mut output_iter_fut = iter.get_next();

        // Send first chunk of scan results
        assert_variant!(exec.run_until_stalled(&mut send_fut), Poll::Pending);

        // Make sure the first chunk of results were delivered
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut), Poll::Ready(result) => {
            let results = result.expect("Failed to get next scan results").unwrap();
            assert_eq!(results, combined_fidl_aps);
        });

        // Close the channel without getting remaining results
        // Note: as of the writing of this test, the "remaining results" are just the final message
        // with an empty vector of networks that signify the end of results. That final empty vector
        // is still considered part of the results, so this test successfully exercises the
        // "partial results read" path.
        drop(output_iter_fut);
        drop(iter);

        // This should not result in error, since some results were consumed
        assert_variant!(exec.run_until_stalled(&mut send_fut), Poll::Ready(Ok(())));
    }

    #[test]
    fn no_scan_result_consumption_has_error() {
        set_logger_for_test();
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let MockScanData {
            passive_input_aps: _,
            passive_internal_aps: _,
            passive_fidl_aps: _,
            active_input_aps: _,
            combined_internal_aps: _,
            combined_fidl_aps,
        } = create_scan_ap_data();

        // Create an iterator and send scan results
        let (iter, iter_server) =
            fidl::endpoints::create_proxy().expect("failed to create iterator");
        let send_fut = send_scan_results_over_fidl(iter_server, &combined_fidl_aps);
        pin_mut!(send_fut);

        // Close the channel without getting results
        drop(iter);

        // This should result in error, since no results were consumed
        assert_variant!(exec.run_until_stalled(&mut send_fut), Poll::Ready(Err(_)));
    }

    #[test]
    fn directed_active_scan_filters_desired_network() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let (sme_proxy, mut sme_stream) = exec.run_singlethreaded(create_sme_proxy());

        // Issue request to scan.
        let desired_ssid = "test_ssid".as_bytes().to_vec();
        let desired_channels = vec![1, 36];
        let scan_fut =
            perform_directed_active_scan(&sme_proxy, &desired_ssid, Some(desired_channels.clone()));
        pin_mut!(scan_fut);

        // Generate scan results
        let scan_result_aps = vec![
            fidl_sme::BssInfo {
                ssid: desired_ssid.clone(),
                protection: fidl_sme::Protection::Wpa3Enterprise,
                ..generate_random_bss_info()
            },
            fidl_sme::BssInfo {
                ssid: desired_ssid.clone(),
                protection: fidl_sme::Protection::Wpa2Wpa3Personal,
                ..generate_random_bss_info()
            },
            fidl_sme::BssInfo {
                ssid: desired_ssid.clone(),
                protection: fidl_sme::Protection::Wpa2Wpa3Personal,
                ..generate_random_bss_info()
            },
            fidl_sme::BssInfo {
                ssid: "other ssid".as_bytes().to_vec(),
                protection: fidl_sme::Protection::Wpa2Personal,
                ..generate_random_bss_info()
            },
        ];

        // Progress scan handler forward so that it will respond to the iterator get next request.
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Respond to the scan request
        let expected_scan_request = fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
            ssids: vec![desired_ssid.clone()],
            channels: desired_channels,
        });
        validate_sme_scan_request_and_send_results(
            &mut exec,
            &mut sme_stream,
            &expected_scan_request,
            scan_result_aps.clone(),
        );

        // Check for results
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Ready(result) => {
            let mut result = result.unwrap();
            // Two networks with the desired SSID are present
            assert_eq!(result.len(), 2);
            result.sort_by_key(|r| r.security_type_detailed.clone());
            // One network is WPA2WPA3
            assert_eq!(result[0].ssid, desired_ssid.clone());
            assert_eq!(result[0].security_type_detailed, types::SecurityTypeDetailed::Wpa2Wpa3Personal);
            // Two BSSs for this network
            assert_eq!(result[0].entries.len(), 2);
            // Other network is WPA3
            assert_eq!(result[1].ssid, desired_ssid.clone());
            assert_eq!(result[1].security_type_detailed, types::SecurityTypeDetailed::Wpa3Enterprise);
            // One BSS for this network
            assert_eq!(result[1].entries.len(), 1);
        });
    }

    // TODO(fxbug.dev/52700) Ignore this test until the location sensor module exists.
    #[ignore]
    #[test]
    fn scan_observer_sends_to_location_sensor() {
        set_logger_for_test();
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let mut location_sensor_updater = LocationSensorUpdater { wpa3_supported: true };
        let MockScanData {
            passive_input_aps: _,
            passive_internal_aps: internal_aps,
            passive_fidl_aps: _,
            active_input_aps: _,
            combined_internal_aps: _,
            combined_fidl_aps: _,
        } = create_scan_ap_data();
        let fut = location_sensor_updater.update_scan_results(&internal_aps);
        exec.run_singlethreaded(fut);
        panic!("Need to reach into location sensor and check it got data")
    }

    fn create_saved_networks_manager(exec: &mut fasync::TestExecutor) -> Arc<SavedNetworksManager> {
        let saved_networks_manager = exec
            .run_singlethreaded(SavedNetworksManager::new_for_test())
            .expect("failed to create saved networks manager");
        Arc::new(saved_networks_manager)
    }

    #[test]
    fn sme_protection_converts_to_policy_security() {
        use {super::fidl_sme::Protection, super::types::SecurityType};
        let wpa3_supported = true;
        let wpa3_not_supported = false;
        let test_pairs = vec![
            // Below are pairs when WPA3 is supported.
            (Protection::Wpa3Enterprise, wpa3_supported, Some(SecurityType::Wpa3)),
            (Protection::Wpa3Personal, wpa3_supported, Some(SecurityType::Wpa3)),
            (Protection::Wpa2Wpa3Personal, wpa3_supported, Some(SecurityType::Wpa3)),
            (Protection::Wpa2Enterprise, wpa3_supported, Some(SecurityType::Wpa2)),
            (Protection::Wpa2Personal, wpa3_supported, Some(SecurityType::Wpa2)),
            (Protection::Wpa1Wpa2Personal, wpa3_supported, Some(SecurityType::Wpa2)),
            (Protection::Wpa2PersonalTkipOnly, wpa3_supported, Some(SecurityType::Wpa2)),
            (Protection::Wpa1Wpa2PersonalTkipOnly, wpa3_supported, Some(SecurityType::Wpa2)),
            (Protection::Wpa1, wpa3_supported, Some(SecurityType::Wpa)),
            (Protection::Wep, wpa3_supported, Some(SecurityType::Wep)),
            (Protection::Open, wpa3_supported, Some(SecurityType::None)),
            (Protection::Unknown, wpa3_supported, None),
            // Below are pairs when WPA3 is not supported.
            (Protection::Wpa3Enterprise, wpa3_not_supported, Some(SecurityType::Wpa2)),
            (Protection::Wpa3Personal, wpa3_not_supported, Some(SecurityType::Wpa2)),
            (Protection::Wpa2Wpa3Personal, wpa3_not_supported, Some(SecurityType::Wpa2)),
            (Protection::Wpa2Enterprise, wpa3_not_supported, Some(SecurityType::Wpa2)),
            (Protection::Wpa2Personal, wpa3_not_supported, Some(SecurityType::Wpa2)),
            (Protection::Wpa1Wpa2Personal, wpa3_not_supported, Some(SecurityType::Wpa2)),
            (Protection::Wpa2PersonalTkipOnly, wpa3_not_supported, Some(SecurityType::Wpa2)),
            (Protection::Wpa1Wpa2PersonalTkipOnly, wpa3_not_supported, Some(SecurityType::Wpa2)),
            (Protection::Wpa1, wpa3_not_supported, Some(SecurityType::Wpa)),
            (Protection::Wep, wpa3_not_supported, Some(SecurityType::Wep)),
            (Protection::Open, wpa3_not_supported, Some(SecurityType::None)),
            (Protection::Unknown, wpa3_not_supported, None),
        ];
        for (input, wpa3_capable, output) in test_pairs {
            assert_eq!(fidl_security_from_sme_protection(input, wpa3_capable), output);
        }
    }
}
