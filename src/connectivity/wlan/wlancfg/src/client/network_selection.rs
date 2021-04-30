// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{
        client::{
            scan::{self, ScanResultUpdate},
            types,
        },
        config_management::{
            self, ConnectFailure, Credential, FailureReason, SavedNetworksManager,
        },
        mode_management::iface_manager_api::IfaceManagerApi,
        util::sme_conversion::security_from_sme_protection,
    },
    async_trait::async_trait,
    fidl_fuchsia_wlan_internal as fidl_internal, fidl_fuchsia_wlan_policy as fidl_policy,
    fidl_fuchsia_wlan_sme as fidl_sme,
    fuchsia_cobalt::CobaltSender,
    fuchsia_inspect::Node as InspectNode,
    fuchsia_inspect_contrib::{
        inspect_insert, inspect_log,
        log::{InspectList, WriteInspect},
        nodes::{BoundedListNode as InspectBoundedListNode, NodeWriter},
    },
    fuchsia_zircon as zx,
    futures::lock::Mutex,
    log::{debug, error, info, trace, warn},
    rand::Rng,
    std::{collections::HashMap, convert::TryInto as _, sync::Arc},
    wlan_common::{channel::Channel, hasher::WlanHasher},
    wlan_inspect::wrappers::InspectWlanChan,
    wlan_metrics_registry::{
        ActiveScanRequestedForNetworkSelectionMetricDimensionActiveScanSsidsRequested as ActiveScanSsidsRequested,
        SavedNetworkInScanResultMetricDimensionBssCount,
        SavedNetworkInScanResultWithActiveScanMetricDimensionActiveScanSsidsObserved as ActiveScanSsidsObserved,
        ScanResultsReceivedMetricDimensionSavedNetworksCount,
        ACTIVE_SCAN_REQUESTED_FOR_NETWORK_SELECTION_METRIC_ID,
        LAST_SCAN_AGE_WHEN_SCAN_REQUESTED_METRIC_ID, SAVED_NETWORK_IN_SCAN_RESULT_METRIC_ID,
        SAVED_NETWORK_IN_SCAN_RESULT_WITH_ACTIVE_SCAN_METRIC_ID, SCAN_RESULTS_RECEIVED_METRIC_ID,
    },
};

const RECENT_FAILURE_WINDOW: zx::Duration = zx::Duration::from_seconds(60 * 5); // 5 minutes

// TODO(fxbug.dev/67791) Remove code or rework cache to be useful
// TODO(fxbug.dev/61992) Tweak duration
const STALE_SCAN_AGE: zx::Duration = zx::Duration::from_millis(50);

/// Above or at this RSSI, we'll give 5G networks a preference
const RSSI_CUTOFF_5G_PREFERENCE: i8 = -64;
/// The score boost for 5G networks that we are giving preference to.
const RSSI_5G_PREFERENCE_BOOST: i8 = 20;
/// The amount to decrease the score by for each failed connection attempt.
const SCORE_PENALTY_FOR_RECENT_FAILURE: i8 = 5;
/// This penalty is much higher than for a general failure because we are not likely to succeed
/// on a retry.
const SCORE_PENALTY_FOR_RECENT_CREDENTIAL_REJECTED: i8 = 30;

const INSPECT_EVENT_LIMIT_FOR_NETWORK_SELECTIONS: usize = 10;

pub struct NetworkSelector {
    saved_network_manager: Arc<SavedNetworksManager>,
    scan_result_cache: Arc<Mutex<ScanResultCache>>,
    cobalt_api: Arc<Mutex<CobaltSender>>,
    hasher: WlanHasher,
    _inspect_node_root: Arc<Mutex<InspectNode>>,
    inspect_node_for_network_selections: Arc<Mutex<InspectBoundedListNode>>,
}

struct ScanResultCache {
    updated_at: zx::Time,
    results: Vec<types::ScanResult>,
}

#[derive(Debug, PartialEq, Clone)]
struct InternalSavedNetworkData {
    network_id: types::NetworkIdentifier,
    credential: Credential,
    has_ever_connected: bool,
    recent_failures: Vec<ConnectFailure>,
}

#[derive(Debug, Clone, PartialEq)]
struct InternalBss<'a> {
    network_info: InternalSavedNetworkData,
    bss_info: &'a types::Bss,
    multiple_bss_candidates: bool,
    hasher: WlanHasher,
}

impl InternalBss<'_> {
    /// This function scores a BSS based on 3 factors: (1) RSSI (2) whether the BSS is 2.4 or 5 GHz
    /// and (3) recent failures to connect to this BSS. No single factor is enough to decide which
    /// BSS to connect to.
    fn score(&self) -> i8 {
        let mut score = self.bss_info.rssi;
        let channel = Channel::from_fidl(self.bss_info.channel);

        // If the network is 5G and has a strong enough RSSI, give it a bonus
        if channel.is_5ghz() && score >= RSSI_CUTOFF_5G_PREFERENCE {
            score = score.saturating_add(RSSI_5G_PREFERENCE_BOOST);
        }

        // Count failures for rejected credentials higher since we probably won't succeed another
        // try with the same credentials.
        let failure_score: i8 = self
            .network_info
            .recent_failures
            .iter()
            .filter(|failure| failure.bssid == self.bss_info.bssid)
            .map(|failure| {
                if failure.reason == FailureReason::CredentialRejected {
                    SCORE_PENALTY_FOR_RECENT_CREDENTIAL_REJECTED
                } else {
                    SCORE_PENALTY_FOR_RECENT_FAILURE
                }
            })
            .sum();

        return score.saturating_sub(failure_score);
    }

    fn recent_failure_count(&self) -> u64 {
        self.network_info
            .recent_failures
            .iter()
            .filter(|failure| failure.bssid == self.bss_info.bssid)
            .count()
            .try_into()
            .unwrap_or_else(|e| {
                error!("{}", e);
                u64::MAX
            })
    }

    fn saved_security_type_to_string(&self) -> String {
        match self.network_info.network_id.type_ {
            fidl_policy::SecurityType::None => "open",
            fidl_policy::SecurityType::Wep => "WEP",
            fidl_policy::SecurityType::Wpa => "WPA",
            fidl_policy::SecurityType::Wpa2 => "WPA2",
            fidl_policy::SecurityType::Wpa3 => "WPA3",
        }
        .to_string()
    }

    fn to_string_without_pii(&self) -> String {
        let channel = Channel::from_fidl(self.bss_info.channel);
        let rssi = self.bss_info.rssi;
        let recent_failure_count = self.recent_failure_count();
        format!(
            "{}({:4}), {}, {:>4}dBm, chan {:8}, score {:4}{}{}{}",
            self.hasher.hash_ssid(&self.network_info.network_id.ssid),
            self.saved_security_type_to_string(),
            self.hasher.hash_mac_addr(&self.bss_info.bssid),
            rssi,
            channel,
            self.score(),
            if !self.bss_info.compatible { ", NOT compatible" } else { "" },
            if recent_failure_count > 0 {
                format!(", {} recent failures", recent_failure_count)
            } else {
                "".to_string()
            },
            if !self.network_info.has_ever_connected { ", never used yet" } else { "" },
        )
    }
}
impl<'a> WriteInspect for InternalBss<'a> {
    fn write_inspect(&self, writer: &mut NodeWriter<'_>, key: &str) {
        inspect_insert!(writer, var key: {
            ssid_hash: self.hasher.hash_ssid(&self.network_info.network_id.ssid),
            bssid_hash: self.hasher.hash_mac_addr(&self.bss_info.bssid),
            rssi: self.bss_info.rssi,
            score: self.score(),
            security_type_saved: self.saved_security_type_to_string(),
            channel: InspectWlanChan(&self.bss_info.channel),
            compatible: self.bss_info.compatible,
            recent_failure_count: self.recent_failure_count(),
            saved_network_has_ever_connected: self.network_info.has_ever_connected,
        });
    }
}

impl NetworkSelector {
    pub fn new(
        saved_network_manager: Arc<SavedNetworksManager>,
        cobalt_api: CobaltSender,
        inspect_node: InspectNode,
    ) -> Self {
        let inspect_node_for_network_selection = InspectBoundedListNode::new(
            inspect_node.create_child("network_selection"),
            INSPECT_EVENT_LIMIT_FOR_NETWORK_SELECTIONS,
        );
        Self {
            saved_network_manager,
            scan_result_cache: Arc::new(Mutex::new(ScanResultCache {
                updated_at: zx::Time::ZERO,
                results: Vec::new(),
            })),
            cobalt_api: Arc::new(Mutex::new(cobalt_api)),
            hasher: WlanHasher::new(rand::thread_rng().gen::<u64>().to_le_bytes()),
            _inspect_node_root: Arc::new(Mutex::new(inspect_node)),
            inspect_node_for_network_selections: Arc::new(Mutex::new(
                inspect_node_for_network_selection,
            )),
        }
    }

    pub fn generate_scan_result_updater(&self, wpa3_supported: bool) -> NetworkSelectorScanUpdater {
        NetworkSelectorScanUpdater {
            scan_result_cache: Arc::clone(&self.scan_result_cache),
            saved_network_manager: Arc::clone(&self.saved_network_manager),
            cobalt_api: Arc::clone(&self.cobalt_api),
            wpa3_supported,
        }
    }

    async fn perform_scan(&self, iface_manager: Arc<Mutex<dyn IfaceManagerApi + Send>>) {
        // Get the scan age.
        let scan_result_guard = self.scan_result_cache.lock().await;
        let last_scan_result_time = scan_result_guard.updated_at;
        drop(scan_result_guard);
        let scan_age = zx::Time::get_monotonic() - last_scan_result_time;

        // Log a metric for scan age, to help us optimize the STALE_SCAN_AGE
        if last_scan_result_time != zx::Time::ZERO {
            let mut cobalt_api_guard = self.cobalt_api.lock().await;
            let cobalt_api = &mut *cobalt_api_guard;
            cobalt_api.log_elapsed_time(
                LAST_SCAN_AGE_WHEN_SCAN_REQUESTED_METRIC_ID,
                Vec::<u32>::new(),
                scan_age.into_micros(),
            );
            drop(cobalt_api_guard);
        }

        // Determine if a new scan is warranted
        if scan_age >= STALE_SCAN_AGE {
            if last_scan_result_time != zx::Time::ZERO {
                info!("Scan results are {}s old, triggering a scan", scan_age.into_seconds());
            }

            // Clear out the old scan results
            let mut scan_result_guard = self.scan_result_cache.lock().await;
            scan_result_guard.results = vec![];
            drop(scan_result_guard);

            let mut cobalt_api_clone = self.cobalt_api.lock().await.clone();
            let potentially_hidden_saved_networks =
                config_management::select_subset_potentially_hidden_networks(
                    self.saved_network_manager.get_networks().await,
                );

            let wpa3_supported =
                iface_manager.lock().await.has_wpa3_capable_client().await.unwrap_or_else(|e| {
                    error!("Failed to determine WPA3 support. Assuming no WPA3 support. {}", e);
                    false
                });

            scan::perform_scan(
                iface_manager,
                self.saved_network_manager.clone(),
                None,
                self.generate_scan_result_updater(wpa3_supported),
                scan::LocationSensorUpdater { wpa3_supported },
                |_| {
                    let active_scan_request_count_metric =
                        match potentially_hidden_saved_networks.len() {
                            0 => ActiveScanSsidsRequested::Zero,
                            1 => ActiveScanSsidsRequested::One,
                            2..=4 => ActiveScanSsidsRequested::TwoToFour,
                            5..=10 => ActiveScanSsidsRequested::FiveToTen,
                            11..=20 => ActiveScanSsidsRequested::ElevenToTwenty,
                            21..=50 => ActiveScanSsidsRequested::TwentyOneToFifty,
                            51..=100 => ActiveScanSsidsRequested::FiftyOneToOneHundred,
                            101..=usize::MAX => ActiveScanSsidsRequested::OneHundredAndOneOrMore,
                            _ => unreachable!(),
                        };
                    cobalt_api_clone.log_event(
                        ACTIVE_SCAN_REQUESTED_FOR_NETWORK_SELECTION_METRIC_ID,
                        active_scan_request_count_metric,
                    );

                    if potentially_hidden_saved_networks.is_empty() {
                        None
                    } else {
                        Some(potentially_hidden_saved_networks)
                    }
                },
            )
            .await;
        } else {
            info!("Using cached scan results from {}s ago", scan_age.into_seconds());
        }
    }

    /// Select the best available network, based on the current saved networks and the most
    /// recent scan results provided to this module.
    /// Only networks that are both saved and visible in the most recent scan results are eligible
    /// for consideration. Among those, the "best" network based on compatibility and quality (e.g.
    /// RSSI, recent failures) is selected.
    pub(crate) async fn find_best_connection_candidate(
        &self,
        iface_manager: Arc<Mutex<dyn IfaceManagerApi + Send>>,
        ignore_list: &Vec<types::NetworkIdentifier>,
    ) -> Option<types::ConnectionCandidate> {
        self.perform_scan(iface_manager.clone()).await;
        let saved_networks = load_saved_networks(Arc::clone(&self.saved_network_manager)).await;
        let wpa3_supported =
            iface_manager.lock().await.has_wpa3_capable_client().await.unwrap_or_else(|e| {
                error!("Failed to determine WPA3 support. Assuming no WPA3 support. {}", e);
                false
            });
        let scan_result_guard = self.scan_result_cache.lock().await;
        let networks = merge_saved_networks_and_scan_data(
            saved_networks,
            &scan_result_guard.results,
            wpa3_supported,
            &self.hasher,
        )
        .await;

        let mut inspect_node = self.inspect_node_for_network_selections.lock().await;
        match select_best_connection_candidate(networks, ignore_list, &mut inspect_node) {
            Some((selected, channel, bssid)) => {
                Some(augment_bss_with_active_scan(selected, channel, bssid, iface_manager).await)
            }
            None => None,
        }
    }

    /// Find a suitable BSS for the given network.
    pub(crate) async fn find_connection_candidate_for_network(
        &self,
        sme_proxy: fidl_sme::ClientSmeProxy,
        network: types::NetworkIdentifier,
        wpa3_supported: bool,
    ) -> Option<types::ConnectionCandidate> {
        // TODO: check if we have recent enough scan results that we can pull from instead?
        let scan_results =
            scan::perform_directed_active_scan(&sme_proxy, &network.ssid, None).await;

        match scan_results {
            Err(()) => None,
            Ok(scan_results) => {
                let saved_networks =
                    load_saved_networks(Arc::clone(&self.saved_network_manager)).await;
                let networks = merge_saved_networks_and_scan_data(
                    saved_networks,
                    &scan_results,
                    wpa3_supported,
                    &self.hasher,
                )
                .await;
                let ignore_list = vec![];
                let mut inspect_node = self.inspect_node_for_network_selections.lock().await;
                select_best_connection_candidate(networks, &ignore_list, &mut inspect_node).map(
                    |(candidate, _, _)| {
                        // Strip out the information about passive vs active scan, because we can't know
                        // if this network would have been observed in a passive scan (since we never
                        // performed a passive scan).
                        types::ConnectionCandidate { observed_in_passive_scan: None, ..candidate }
                    },
                )
            }
        }
    }
}

/// Merge the saved networks and scan results into a vector of BSSs that correspond to a saved
/// network.
async fn merge_saved_networks_and_scan_data<'a>(
    saved_networks: HashMap<types::NetworkIdentifier, InternalSavedNetworkData>,
    scan_results: &'a Vec<types::ScanResult>,
    wpa3_supported: bool,
    hasher: &WlanHasher,
) -> Vec<InternalBss<'a>> {
    let mut merged_networks = vec![];
    for scan_result in scan_results {
        // TODO(fxbug.dev/70965): use detailed security type for this matching
        if let Some(type_) =
            security_from_sme_protection(scan_result.security_type_detailed, wpa3_supported)
        {
            let id = types::NetworkIdentifier { ssid: scan_result.ssid.clone(), type_ };
            if let Some(saved_network_info) = saved_networks.get(&id) {
                let multiple_bss_candidates = scan_result.entries.len() > 1;
                for bss in &scan_result.entries {
                    merged_networks.push(InternalBss {
                        bss_info: bss,
                        multiple_bss_candidates,
                        network_info: saved_network_info.clone(),
                        hasher: hasher.clone(),
                    });
                }
            }
        }
    }
    merged_networks
}

/// Insert all saved networks into a hashmap with this module's internal data representation
async fn load_saved_networks(
    saved_network_manager: Arc<SavedNetworksManager>,
) -> HashMap<types::NetworkIdentifier, InternalSavedNetworkData> {
    let mut networks: HashMap<types::NetworkIdentifier, InternalSavedNetworkData> = HashMap::new();
    for saved_network in saved_network_manager.get_networks().await.into_iter() {
        let recent_failures = saved_network
            .perf_stats
            .failure_list
            .get_recent(zx::Time::get_monotonic() - RECENT_FAILURE_WINDOW);

        trace!(
            "Adding saved network to hashmap{}",
            if recent_failures.len() > 0 { " with some failures" } else { "" }
        );
        let id = types::NetworkIdentifier {
            ssid: saved_network.ssid.clone(),
            type_: saved_network.security_type.into(),
        };
        // We allow networks saved as WPA to be also used as WPA2 or WPA2 to be used for WPA3
        if let Some(security_type) = upgrade_security(&saved_network.security_type) {
            if let Some(_) = networks.insert(
                types::NetworkIdentifier { ssid: saved_network.ssid.clone(), type_: security_type },
                InternalSavedNetworkData {
                    network_id: id.clone(),
                    credential: saved_network.credential.clone(),
                    has_ever_connected: saved_network.has_ever_connected,
                    recent_failures: recent_failures.clone(),
                },
            ) {
                warn!("Replaced saved network with automatically upgrade security version");
            };
        }
        if let Some(_) = networks.insert(
            id.clone(),
            InternalSavedNetworkData {
                network_id: id,
                credential: saved_network.credential,
                has_ever_connected: saved_network.has_ever_connected,
                recent_failures: recent_failures,
            },
        ) {
            warn!("Duplicate saved network found");
        };
    }
    networks
}

pub fn upgrade_security(security: &config_management::SecurityType) -> Option<types::SecurityType> {
    match security {
        config_management::SecurityType::Wpa => Some(types::SecurityType::Wpa2),
        config_management::SecurityType::Wpa2 => Some(types::SecurityType::Wpa3),
        _ => None,
    }
}

pub struct NetworkSelectorScanUpdater {
    scan_result_cache: Arc<Mutex<ScanResultCache>>,
    saved_network_manager: Arc<SavedNetworksManager>,
    cobalt_api: Arc<Mutex<CobaltSender>>,
    wpa3_supported: bool,
}
#[async_trait]
impl ScanResultUpdate for NetworkSelectorScanUpdater {
    async fn update_scan_results(&mut self, scan_results: &Vec<types::ScanResult>) {
        // Update internal scan result cache
        let scan_results_clone = scan_results.clone();
        let mut scan_result_guard = self.scan_result_cache.lock().await;
        scan_result_guard.results = scan_results_clone;
        scan_result_guard.updated_at = zx::Time::get_monotonic();
        drop(scan_result_guard);

        // Record metrics for this scan
        let saved_networks = load_saved_networks(Arc::clone(&self.saved_network_manager)).await;
        let mut cobalt_api_guard = self.cobalt_api.lock().await;
        let cobalt_api = &mut *cobalt_api_guard;
        record_metrics_on_scan(scan_results, saved_networks, cobalt_api, self.wpa3_supported);
        drop(cobalt_api_guard);
    }
}

fn select_best_connection_candidate<'a>(
    bss_list: Vec<InternalBss<'a>>,
    ignore_list: &Vec<types::NetworkIdentifier>,
    inspect_node: &mut InspectBoundedListNode,
) -> Option<(types::ConnectionCandidate, types::WlanChan, types::Bssid)> {
    info!("Selecting from {} BSSs found for saved networks", bss_list.len());

    let selected = bss_list
        .iter()
        .inspect(|bss| {
            info!("{}", bss.to_string_without_pii());
        })
        .filter(|bss| {
            // Filter out incompatible BSSs
            if !bss.bss_info.compatible {
                trace!("BSS is incompatible, filtering: {:?}", bss);
                return false;
            };
            // Filter out networks we've been told to ignore
            if ignore_list.contains(&bss.network_info.network_id) {
                trace!("Network is ignored, filtering: {:?}", bss);
                return false;
            }
            true
        })
        .max_by(|bss_a, bss_b| bss_a.score().partial_cmp(&bss_b.score()).unwrap());

    // Log the candidates into Inspect
    inspect_log!(inspect_node, candidates: InspectList(&bss_list), selected?: selected);

    selected.map(|bss| {
        info!("Selected BSS:");
        info!("{}", bss.to_string_without_pii());
        (
            types::ConnectionCandidate {
                network: bss.network_info.network_id.clone(),
                credential: bss.network_info.credential.clone(),
                observed_in_passive_scan: Some(bss.bss_info.observed_in_passive_scan),
                bss: bss.bss_info.bss_desc.clone(),
                multiple_bss_candidates: Some(bss.multiple_bss_candidates),
            },
            bss.bss_info.channel,
            bss.bss_info.bssid,
        )
    })
}

/// If a BSS was discovered via a passive scan, we need to perform an active scan on it to discover
/// all the information potentially needed by the SME layer.
async fn augment_bss_with_active_scan(
    selected_network: types::ConnectionCandidate,
    channel: types::WlanChan,
    bssid: types::Bssid,
    iface_manager: Arc<Mutex<dyn IfaceManagerApi + Send>>,
) -> types::ConnectionCandidate {
    // This internal function encapsulates all the logic and has a Result<> return type, allowing us
    // to use the `?` operator inside it to reduce nesting.
    async fn get_enhanced_bss_description(
        selected_network: &types::ConnectionCandidate,
        channel: types::WlanChan,
        bssid: types::Bssid,
        iface_manager: Arc<Mutex<dyn IfaceManagerApi + Send>>,
    ) -> Result<Option<Box<fidl_internal::BssDescription>>, ()> {
        // Make sure the scan is needed
        match selected_network.observed_in_passive_scan {
            Some(true) => info!("Performing directed active scan on selected network"),
            Some(false) => {
                debug!("Network already discovered via active scan.");
                return Err(());
            }
            None => {
                error!("Unexpected 'None' value for 'observed_in_passive_scan'.");
                return Err(());
            }
        }

        // Get an SME proxy
        let mut iface_manager_guard = iface_manager.lock().await;
        let sme_proxy = iface_manager_guard.get_sme_proxy_for_scan().await.map_err(|e| {
            info!("Failed to get an SME proxy for scan: {:?}", e);
        })?;
        drop(iface_manager_guard);

        // Perform the scan
        let mut directed_scan_result = scan::perform_directed_active_scan(
            &sme_proxy,
            &selected_network.network.ssid,
            Some(vec![channel.primary]),
        )
        .await
        .map_err(|()| {
            info!("Failed to perform active scan to augment BSS info.");
        })?;

        // Find the bss in the results
        let bss_desc = directed_scan_result
            .drain(..)
            .find_map(|mut network| {
                if network.ssid == selected_network.network.ssid {
                    for bss in network.entries.drain(..) {
                        if bss.bssid == bssid {
                            return Some(bss.bss_desc);
                        }
                    }
                }
                None
            })
            .ok_or_else(|| {
                info!("BSS info will lack active scan augmentation, proceeding anyway.");
            })?;

        Ok(bss_desc)
    }

    match get_enhanced_bss_description(&selected_network, channel, bssid, iface_manager).await {
        Ok(new_bss_desc) => types::ConnectionCandidate { bss: new_bss_desc, ..selected_network },
        Err(()) => selected_network,
    }
}

fn record_metrics_on_scan(
    scan_results: &Vec<types::ScanResult>,
    saved_networks: HashMap<types::NetworkIdentifier, InternalSavedNetworkData>,
    cobalt_api: &mut CobaltSender,
    wpa3_supported: bool,
) {
    let mut num_saved_networks_observed = 0;
    let mut num_actively_scanned_networks = 0;

    for scan_result in scan_results {
        // TODO(fxbug.dev/70965): use detailed security type for this matching
        if let Some(type_) =
            security_from_sme_protection(scan_result.security_type_detailed, wpa3_supported)
        {
            let id = types::NetworkIdentifier { ssid: scan_result.ssid.clone(), type_ };
            if let Some(_) = saved_networks.get(&id) {
                // This saved network was present in scan results.
                num_saved_networks_observed += 1;

                // Check if the network was found via active scan.
                if scan_result.entries.iter().any(|bss| bss.observed_in_passive_scan == false) {
                    num_actively_scanned_networks += 1;
                };

                // Record how many BSSs are visible in the scan results for this saved network.
                let num_bss = match scan_result.entries.len() {
                    0 => unreachable!(), // The ::Zero enum exists, but we shouldn't get a scan result with no BSS
                    1 => SavedNetworkInScanResultMetricDimensionBssCount::One,
                    2..=4 => SavedNetworkInScanResultMetricDimensionBssCount::TwoToFour,
                    5..=10 => SavedNetworkInScanResultMetricDimensionBssCount::FiveToTen,
                    11..=20 => SavedNetworkInScanResultMetricDimensionBssCount::ElevenToTwenty,
                    21..=usize::MAX => {
                        SavedNetworkInScanResultMetricDimensionBssCount::TwentyOneOrMore
                    }
                    _ => unreachable!(),
                };
                cobalt_api.log_event(SAVED_NETWORK_IN_SCAN_RESULT_METRIC_ID, num_bss);
            }
        }
    }

    let saved_network_count_metric = match num_saved_networks_observed {
        0 => ScanResultsReceivedMetricDimensionSavedNetworksCount::Zero,
        1 => ScanResultsReceivedMetricDimensionSavedNetworksCount::One,
        2..=4 => ScanResultsReceivedMetricDimensionSavedNetworksCount::TwoToFour,
        5..=20 => ScanResultsReceivedMetricDimensionSavedNetworksCount::FiveToTwenty,
        21..=40 => ScanResultsReceivedMetricDimensionSavedNetworksCount::TwentyOneToForty,
        41..=usize::MAX => ScanResultsReceivedMetricDimensionSavedNetworksCount::FortyOneOrMore,
        _ => unreachable!(),
    };
    cobalt_api.log_event(SCAN_RESULTS_RECEIVED_METRIC_ID, saved_network_count_metric);

    let actively_scanned_networks_metrics = match num_actively_scanned_networks {
        0 => ActiveScanSsidsObserved::Zero,
        1 => ActiveScanSsidsObserved::One,
        2..=4 => ActiveScanSsidsObserved::TwoToFour,
        5..=10 => ActiveScanSsidsObserved::FiveToTen,
        11..=20 => ActiveScanSsidsObserved::ElevenToTwenty,
        21..=50 => ActiveScanSsidsObserved::TwentyOneToFifty,
        51..=100 => ActiveScanSsidsObserved::FiftyOneToOneHundred,
        101..=usize::MAX => ActiveScanSsidsObserved::OneHundredAndOneOrMore,
        _ => unreachable!(),
    };
    cobalt_api.log_event(
        SAVED_NETWORK_IN_SCAN_RESULT_WITH_ACTIVE_SCAN_METRIC_ID,
        actively_scanned_networks_metrics,
    );
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            access_point::state_machine as ap_fsm,
            util::{
                logger::set_logger_for_test,
                testing::{
                    create_mock_cobalt_sender_and_receiver, generate_channel,
                    generate_random_bss_desc, generate_random_channel,
                    poll_for_and_validate_sme_scan_request_and_send_results,
                    validate_sme_scan_request_and_send_results,
                },
            },
        },
        anyhow::Error,
        cobalt_client::traits::AsEventCode,
        fidl::endpoints::create_proxy,
        fidl_fuchsia_cobalt::CobaltEvent,
        fidl_fuchsia_wlan_common as fidl_common, fidl_fuchsia_wlan_sme as fidl_sme,
        fuchsia_async as fasync,
        fuchsia_cobalt::cobalt_event_builder::CobaltEventExt,
        fuchsia_inspect::{self as inspect, assert_data_tree},
        futures::{
            channel::{mpsc, oneshot},
            prelude::*,
            task::Poll,
        },
        pin_utils::pin_mut,
        rand::Rng,
        std::{convert::TryInto, sync::Arc},
        test_case::test_case,
        wlan_common::assert_variant,
    };

    struct TestValues {
        network_selector: Arc<NetworkSelector>,
        saved_network_manager: Arc<SavedNetworksManager>,
        cobalt_events: mpsc::Receiver<CobaltEvent>,
        iface_manager: Arc<Mutex<FakeIfaceManager>>,
        sme_stream: fidl_sme::ClientSmeRequestStream,
        inspector: inspect::Inspector,
    }

    async fn test_setup() -> TestValues {
        set_logger_for_test();

        // setup modules
        let (cobalt_api, cobalt_events) = create_mock_cobalt_sender_and_receiver();
        let saved_network_manager = Arc::new(SavedNetworksManager::new_for_test().await.unwrap());
        let inspector = inspect::Inspector::new();
        let inspect_node = inspector.root().create_child("net_select_test");

        let network_selector = Arc::new(NetworkSelector::new(
            Arc::clone(&saved_network_manager),
            cobalt_api,
            inspect_node,
        ));
        let (client_sme, remote) =
            create_proxy::<fidl_sme::ClientSmeMarker>().expect("error creating proxy");
        let iface_manager = Arc::new(Mutex::new(FakeIfaceManager::new(client_sme)));

        TestValues {
            network_selector,
            saved_network_manager,
            cobalt_events,
            iface_manager,
            sme_stream: remote.into_stream().expect("failed to create stream"),
            inspector,
        }
    }

    struct FakeIfaceManager {
        pub sme_proxy: fidl_fuchsia_wlan_sme::ClientSmeProxy,
    }

    impl FakeIfaceManager {
        pub fn new(proxy: fidl_fuchsia_wlan_sme::ClientSmeProxy) -> Self {
            FakeIfaceManager { sme_proxy: proxy }
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

        // Many tests use wpa3 networks expecting them to be used normally, so by default this
        // is true.
        async fn has_wpa3_capable_client(&mut self) -> Result<bool, Error> {
            Ok(true)
        }

        async fn set_country(
            &mut self,
            _country_code: Option<[u8; types::REGION_CODE_LEN]>,
        ) -> Result<(), Error> {
            unimplemented!()
        }
    }

    #[fasync::run_singlethreaded(test)]
    async fn saved_networks_are_loaded() {
        let test_values = test_setup().await;

        // check there are 0 saved networks to start with
        let networks = load_saved_networks(Arc::clone(&test_values.saved_network_manager)).await;
        assert_eq!(networks.len(), 0);

        // create some identifiers
        let test_id_1 = types::NetworkIdentifier {
            ssid: "foo".as_bytes().to_vec(),
            type_: types::SecurityType::Wpa3,
        };
        let credential_1 = Credential::Password("foo_pass".as_bytes().to_vec());
        let bssid_1 = [0; 6];
        let ssid_2 = "bar".as_bytes().to_vec();
        let test_id_2 =
            types::NetworkIdentifier { ssid: ssid_2.clone(), type_: types::SecurityType::Wpa };
        let credential_2 = Credential::Password("bar_pass".as_bytes().to_vec());
        let bssid_2 = [1, 2, 3, 4, 5, 6];

        // insert some new saved networks
        assert!(test_values
            .saved_network_manager
            .store(test_id_1.clone().into(), credential_1.clone())
            .await
            .unwrap()
            .is_none());

        assert!(test_values
            .saved_network_manager
            .store(test_id_2.clone().into(), credential_2.clone())
            .await
            .unwrap()
            .is_none());

        // mark the first one as having connected
        test_values
            .saved_network_manager
            .record_connect_result(
                test_id_1.clone().into(),
                &credential_1.clone(),
                bssid_1,
                fidl_sme::ConnectResultCode::Success,
                None,
            )
            .await;

        // mark the second one as having a failure
        test_values
            .saved_network_manager
            .record_connect_result(
                test_id_2.clone().into(),
                &credential_2.clone(),
                bssid_2,
                fidl_sme::ConnectResultCode::CredentialRejected,
                None,
            )
            .await;

        // check these networks were loaded
        let mut expected_hashmap = HashMap::new();
        let _ = expected_hashmap.insert(
            test_id_1.clone(),
            InternalSavedNetworkData {
                network_id: test_id_1,
                credential: credential_1,
                has_ever_connected: true,
                recent_failures: Vec::new(),
            },
        );
        let connect_failures =
            get_connect_failures(test_id_2.clone(), &test_values.saved_network_manager).await;
        let internal_data_2 = InternalSavedNetworkData {
            network_id: test_id_2.clone(),
            credential: credential_2.clone(),
            has_ever_connected: false,
            recent_failures: connect_failures,
        };
        let _ = expected_hashmap.insert(test_id_2.clone(), internal_data_2.clone());
        // Networks saved as WPA can be used to auto connect to WPA2 networks
        let _ = expected_hashmap.insert(
            types::NetworkIdentifier { ssid: ssid_2.clone(), type_: types::SecurityType::Wpa2 },
            internal_data_2.clone(),
        );
        let networks = load_saved_networks(Arc::clone(&test_values.saved_network_manager)).await;
        assert_eq!(networks, expected_hashmap);
    }

    #[fasync::run_singlethreaded(test)]
    async fn scan_results_are_stored() {
        let mut test_values = test_setup().await;
        let network_selector = test_values.network_selector;

        // check there are 0 scan results to start with
        let guard = network_selector.scan_result_cache.lock().await;
        assert_eq!(guard.results.len(), 0);
        drop(guard);

        // provide some new scan results
        let mock_scan_results = vec![generate_random_scan_result(), generate_random_scan_result()];
        let mut updater = network_selector.generate_scan_result_updater(true);
        updater.update_scan_results(&mock_scan_results).await;

        // check that the scan results are stored
        let guard = network_selector.scan_result_cache.lock().await;
        assert_eq!(guard.results, mock_scan_results);

        // check there are some metric events for the incoming scan results
        // note: the actual metrics are checked in unit tests for the metric recording function
        assert!(test_values.cobalt_events.try_next().unwrap().is_some());
    }

    #[fasync::run_singlethreaded(test)]
    async fn scan_results_merged_with_saved_networks() {
        // create some identifiers
        let test_ssid_1 = "foo".as_bytes().to_vec();
        let test_security_1 = types::SecurityTypeDetailed::Wpa3Personal;
        let test_id_1 = types::NetworkIdentifier {
            ssid: test_ssid_1.clone(),
            type_: types::SecurityType::Wpa3,
        };
        let credential_1 = Credential::Password("foo_pass".as_bytes().to_vec());
        let test_ssid_2 = "bar".as_bytes().to_vec();
        let test_security_2 = types::SecurityTypeDetailed::Wpa1;
        let test_id_2 =
            types::NetworkIdentifier { ssid: test_ssid_2.clone(), type_: types::SecurityType::Wpa };
        let credential_2 = Credential::Password("bar_pass".as_bytes().to_vec());

        // build some scan results
        let mock_scan_results = vec![
            types::ScanResult {
                ssid: test_ssid_1.clone(),
                security_type_detailed: test_security_1.clone(),
                entries: vec![generate_random_bss(), generate_random_bss(), generate_random_bss()],
                compatibility: types::Compatibility::Supported,
            },
            types::ScanResult {
                ssid: test_ssid_2.clone(),
                security_type_detailed: test_security_2.clone(),
                entries: vec![generate_random_bss()],
                compatibility: types::Compatibility::DisallowedNotSupported,
            },
        ];

        // create some connect failures, 3 GeneralFailures for BSSID 1 and 1 CredentialsRejected
        // for BSSID 2
        let bssid_1 = mock_scan_results[0].entries[0].bssid;
        let bssid_2 = mock_scan_results[0].entries[1].bssid;
        let recent_failures = vec![
            connect_failure_with_bssid(bssid_1),
            connect_failure_with_bssid(bssid_1),
            connect_failure_with_bssid(bssid_1),
            ConnectFailure {
                bssid: bssid_2,
                time: zx::Time::get_monotonic(),
                reason: FailureReason::CredentialRejected,
            },
        ];

        // create the saved networks hashmap
        let mut saved_networks = HashMap::new();
        let _ = saved_networks.insert(
            test_id_1.clone(),
            InternalSavedNetworkData {
                network_id: test_id_1.clone(),
                credential: credential_1.clone(),
                has_ever_connected: true,
                recent_failures: recent_failures.clone(),
            },
        );
        let _ = saved_networks.insert(
            test_id_2.clone(),
            InternalSavedNetworkData {
                network_id: test_id_2.clone(),
                credential: credential_2.clone(),
                has_ever_connected: false,
                recent_failures: Vec::new(),
            },
        );

        // build our expected result
        let hasher = WlanHasher::new(rand::thread_rng().gen::<u64>().to_le_bytes());
        let expected_internal_data_1 = InternalSavedNetworkData {
            network_id: test_id_1.clone(),
            credential: credential_1.clone(),
            has_ever_connected: true,
            recent_failures: recent_failures.clone(),
        };
        let expected_result = vec![
            InternalBss {
                network_info: expected_internal_data_1.clone(),
                bss_info: &mock_scan_results[0].entries[0],
                multiple_bss_candidates: true,
                hasher: hasher.clone(),
            },
            InternalBss {
                network_info: expected_internal_data_1.clone(),
                bss_info: &mock_scan_results[0].entries[1],
                multiple_bss_candidates: true,
                hasher: hasher.clone(),
            },
            InternalBss {
                network_info: expected_internal_data_1,
                bss_info: &mock_scan_results[0].entries[2],
                multiple_bss_candidates: true,
                hasher: hasher.clone(),
            },
            InternalBss {
                network_info: InternalSavedNetworkData {
                    network_id: test_id_2.clone(),
                    credential: credential_2.clone(),
                    has_ever_connected: false,
                    recent_failures: Vec::new(),
                },
                bss_info: &mock_scan_results[1].entries[0],
                multiple_bss_candidates: false,
                hasher: hasher.clone(),
            },
        ];

        // validate the function works
        let result =
            merge_saved_networks_and_scan_data(saved_networks, &mock_scan_results, true, &hasher)
                .await;
        assert_eq!(result, expected_result);
    }

    #[test_case(types::Bss {
            rssi: -8,
            channel: generate_channel(1),
            ..generate_random_bss()
        },
        -8; "2.4GHz BSS score is RSSI")]
    #[test_case(types::Bss {
            rssi: -49,
            channel: generate_channel(36),
            ..generate_random_bss()
        },
        -29; "5GHz score is (RSSI + mod), when above threshold")]
    #[test_case(types::Bss {
            rssi: -71,
            channel: generate_channel(36),
            ..generate_random_bss()
        },
        -71; "5GHz score is RSSI, when below threshold")]
    fn scoring_test(bss: types::Bss, expected_score: i8) {
        let mut rng = rand::thread_rng();

        let network_id = types::NetworkIdentifier {
            ssid: "test".as_bytes().to_vec(),
            type_: types::SecurityType::Wpa3,
        };
        let internal_bss = InternalBss {
            network_info: InternalSavedNetworkData {
                network_id: network_id,
                credential: Credential::None,
                has_ever_connected: rng.gen::<bool>(),
                recent_failures: vec![],
            },
            bss_info: &bss,
            multiple_bss_candidates: false,
            hasher: WlanHasher::new(rand::thread_rng().gen::<u64>().to_le_bytes()),
        };

        assert_eq!(internal_bss.score(), expected_score)
    }

    #[test]
    fn test_score_bss_prefers_less_failures() {
        let bss_info_worse =
            types::Bss { rssi: -60, channel: generate_channel(3), ..generate_random_bss() };
        let bss_info_better =
            types::Bss { rssi: -60, channel: generate_channel(3), ..generate_random_bss() };
        let (_test_id, mut internal_data) = generate_random_saved_network();
        // Add many test failures for the worse BSS and one for the better BSS
        let mut failures = vec![connect_failure_with_bssid(bss_info_worse.bssid); 12];
        failures.push(connect_failure_with_bssid(bss_info_better.bssid));
        internal_data.recent_failures = failures;
        let bss_worse = InternalBss {
            network_info: internal_data.clone(),
            bss_info: &bss_info_worse,
            multiple_bss_candidates: true,
            hasher: WlanHasher::new(rand::thread_rng().gen::<u64>().to_le_bytes()),
        };
        let bss_better = InternalBss {
            network_info: internal_data,
            bss_info: &bss_info_better,
            multiple_bss_candidates: true,
            hasher: WlanHasher::new(rand::thread_rng().gen::<u64>().to_le_bytes()),
        };
        // Check that the better BSS has a higher score than the worse BSS.
        assert!(bss_better.score() > bss_worse.score());
    }

    #[test]
    fn test_score_bss_prefers_stronger_with_failures() {
        // Test test that if one network has a few network failures but is 5 Ghz instead of 2.4,
        // the 5 GHz network has a higher score.
        let bss_info_worse =
            types::Bss { rssi: -35, channel: generate_channel(3), ..generate_random_bss() };
        let bss_info_better =
            types::Bss { rssi: -35, channel: generate_channel(36), ..generate_random_bss() };
        let (_test_id, mut internal_data) = generate_random_saved_network();
        // Set the failure list to have 0 failures for the worse BSS and 4 failures for the
        // stronger BSS.
        internal_data.recent_failures = vec![connect_failure_with_bssid(bss_info_better.bssid); 2];
        let bss_worse = InternalBss {
            network_info: internal_data.clone(),
            bss_info: &bss_info_worse,
            multiple_bss_candidates: false,
            hasher: WlanHasher::new(rand::thread_rng().gen::<u64>().to_le_bytes()),
        };
        let bss_better = InternalBss {
            network_info: internal_data,
            bss_info: &bss_info_better,
            multiple_bss_candidates: false,
            hasher: WlanHasher::new(rand::thread_rng().gen::<u64>().to_le_bytes()),
        };
        assert!(bss_better.score() > bss_worse.score());
    }

    #[test]
    fn test_score_credentials_rejected_worse() {
        // If two BSS are identical other than one failed to connect with wrong credentials and
        // the other failed with a few connect failurs, the one with wrong credentials has a lower
        // score.
        let bss_info_worse =
            types::Bss { rssi: -30, channel: generate_channel(44), ..generate_random_bss() };
        let bss_info_better =
            types::Bss { rssi: -30, channel: generate_channel(44), ..generate_random_bss() };
        let (_test_id, mut internal_data) = generate_random_saved_network();
        // Add many test failures for the worse BSS and one for the better BSS
        let mut failures = vec![connect_failure_with_bssid(bss_info_better.bssid); 4];
        failures.push(ConnectFailure {
            bssid: bss_info_worse.bssid,
            time: zx::Time::get_monotonic(),
            reason: FailureReason::CredentialRejected,
        });
        internal_data.recent_failures = failures;

        let bss_worse = InternalBss {
            network_info: internal_data.clone(),
            bss_info: &bss_info_worse,
            multiple_bss_candidates: true,
            hasher: WlanHasher::new(rand::thread_rng().gen::<u64>().to_le_bytes()),
        };
        let bss_better = InternalBss {
            network_info: internal_data,
            bss_info: &bss_info_better,
            multiple_bss_candidates: true,
            hasher: WlanHasher::new(rand::thread_rng().gen::<u64>().to_le_bytes()),
        };

        assert!(bss_better.score() > bss_worse.score());
    }

    #[test]
    fn select_best_connection_candidate_sorts_by_score() {
        // generate Inspect nodes
        let inspector = inspect::Inspector::new();
        let mut inspect_node =
            InspectBoundedListNode::new(inspector.root().create_child("test"), 10);
        // build networks list
        let test_id_1 = types::NetworkIdentifier {
            ssid: "foo".as_bytes().to_vec(),
            type_: types::SecurityType::Wpa3,
        };
        let credential_1 = Credential::Password("foo_pass".as_bytes().to_vec());
        let test_id_2 = types::NetworkIdentifier {
            ssid: "bar".as_bytes().to_vec(),
            type_: types::SecurityType::Wpa,
        };
        let credential_2 = Credential::Password("bar_pass".as_bytes().to_vec());

        let mut networks = vec![];

        let bss_info1 = types::Bss {
            compatible: true,
            rssi: -14,
            channel: generate_channel(36),
            ..generate_random_bss()
        };
        networks.push(InternalBss {
            network_info: InternalSavedNetworkData {
                network_id: test_id_1.clone(),
                credential: credential_1.clone(),
                has_ever_connected: true,
                recent_failures: Vec::new(),
            },
            bss_info: &bss_info1,
            multiple_bss_candidates: true,
            hasher: WlanHasher::new(rand::thread_rng().gen::<u64>().to_le_bytes()),
        });

        let bss_info2 = types::Bss {
            compatible: true,
            rssi: -10,
            channel: generate_channel(1),
            ..generate_random_bss()
        };
        networks.push(InternalBss {
            network_info: InternalSavedNetworkData {
                network_id: test_id_1.clone(),
                credential: credential_1.clone(),
                has_ever_connected: true,
                recent_failures: Vec::new(),
            },
            bss_info: &bss_info2,
            multiple_bss_candidates: true,
            hasher: WlanHasher::new(rand::thread_rng().gen::<u64>().to_le_bytes()),
        });

        let bss_info3 = types::Bss {
            compatible: true,
            rssi: -8,
            channel: generate_channel(1),
            ..generate_random_bss()
        };
        networks.push(InternalBss {
            network_info: InternalSavedNetworkData {
                network_id: test_id_2.clone(),
                credential: credential_2.clone(),
                has_ever_connected: true,
                recent_failures: Vec::new(),
            },
            bss_info: &bss_info3,
            multiple_bss_candidates: false,
            hasher: WlanHasher::new(rand::thread_rng().gen::<u64>().to_le_bytes()),
        });

        // there's a network on 5G, it should get a boost and be selected
        assert_eq!(
            select_best_connection_candidate(networks.clone(), &vec![], &mut inspect_node),
            Some((
                types::ConnectionCandidate {
                    network: test_id_1.clone(),
                    credential: credential_1.clone(),
                    bss: bss_info1.bss_desc.clone(),
                    observed_in_passive_scan: Some(bss_info1.observed_in_passive_scan),
                    multiple_bss_candidates: Some(true),
                },
                bss_info1.channel,
                bss_info1.bssid
            ))
        );

        // make the 5GHz network into a 2.4GHz network
        let mut modified_network = networks[0].clone();
        let modified_bss_info =
            types::Bss { channel: generate_channel(6), ..modified_network.bss_info.clone() };
        modified_network.bss_info = &modified_bss_info;
        networks[0] = modified_network;

        // all networks are 2.4GHz, strongest RSSI network returned
        assert_eq!(
            select_best_connection_candidate(networks.clone(), &vec![], &mut inspect_node),
            Some((
                types::ConnectionCandidate {
                    network: test_id_2.clone(),
                    credential: credential_2.clone(),
                    bss: networks[2].bss_info.bss_desc.clone(),
                    observed_in_passive_scan: Some(networks[2].bss_info.observed_in_passive_scan),
                    multiple_bss_candidates: Some(false),
                },
                networks[2].bss_info.channel,
                networks[2].bss_info.bssid
            ))
        );
    }

    #[test]
    fn select_best_connection_candidate_sorts_by_failure_count() {
        // generate Inspect nodes
        let inspector = inspect::Inspector::new();
        let mut inspect_node =
            InspectBoundedListNode::new(inspector.root().create_child("test"), 10);
        // build networks list
        let test_id_1 = types::NetworkIdentifier {
            ssid: "foo".as_bytes().to_vec(),
            type_: types::SecurityType::Wpa3,
        };
        let credential_1 = Credential::Password("foo_pass".as_bytes().to_vec());
        let test_id_2 = types::NetworkIdentifier {
            ssid: "bar".as_bytes().to_vec(),
            type_: types::SecurityType::Wpa,
        };
        let credential_2 = Credential::Password("bar_pass".as_bytes().to_vec());

        let mut networks = vec![];

        let bss_info1 = types::Bss {
            compatible: true,
            rssi: -34,
            channel: generate_channel(3),
            ..generate_random_bss()
        };
        networks.push(InternalBss {
            network_info: InternalSavedNetworkData {
                network_id: test_id_1.clone(),
                credential: credential_1.clone(),
                has_ever_connected: true,
                recent_failures: Vec::new(),
            },
            bss_info: &bss_info1,
            multiple_bss_candidates: false,
            hasher: WlanHasher::new(rand::thread_rng().gen::<u64>().to_le_bytes()),
        });

        let bss_info2 = types::Bss {
            compatible: true,
            rssi: -50,
            channel: generate_channel(3),
            ..generate_random_bss()
        };
        networks.push(InternalBss {
            network_info: InternalSavedNetworkData {
                network_id: test_id_2.clone(),
                credential: credential_2.clone(),
                has_ever_connected: true,
                recent_failures: Vec::new(),
            },
            bss_info: &bss_info2,
            multiple_bss_candidates: false,
            hasher: WlanHasher::new(rand::thread_rng().gen::<u64>().to_le_bytes()),
        });

        // stronger network returned
        assert_eq!(
            select_best_connection_candidate(networks.clone(), &vec![], &mut inspect_node),
            Some((
                types::ConnectionCandidate {
                    network: test_id_1.clone(),
                    credential: credential_1.clone(),
                    bss: bss_info1.bss_desc.clone(),
                    observed_in_passive_scan: Some(networks[0].bss_info.observed_in_passive_scan),
                    multiple_bss_candidates: Some(false),
                },
                bss_info1.channel,
                bss_info1.bssid
            ))
        );

        // mark the stronger network as having some failures
        let num_failures = 4;
        networks[0].network_info.recent_failures =
            vec![connect_failure_with_bssid(bss_info1.bssid); num_failures];
        networks[1].network_info.recent_failures =
            vec![connect_failure_with_bssid(bss_info1.bssid); num_failures];

        // weaker network (with no failures) returned
        assert_eq!(
            select_best_connection_candidate(networks.clone(), &vec![], &mut inspect_node),
            Some((
                types::ConnectionCandidate {
                    network: test_id_2.clone(),
                    credential: credential_2.clone(),
                    bss: bss_info2.bss_desc.clone(),
                    observed_in_passive_scan: Some(networks[1].bss_info.observed_in_passive_scan),
                    multiple_bss_candidates: Some(false),
                },
                bss_info2.channel,
                bss_info2.bssid
            ))
        );

        // give them both the same number of failures
        networks[1].network_info.recent_failures =
            vec![connect_failure_with_bssid(bss_info2.bssid.clone()); num_failures];

        // stronger network returned
        assert_eq!(
            select_best_connection_candidate(networks.clone(), &vec![], &mut inspect_node),
            Some((
                types::ConnectionCandidate {
                    network: test_id_1.clone(),
                    credential: credential_1.clone(),
                    bss: bss_info1.bss_desc.clone(),
                    observed_in_passive_scan: Some(networks[0].bss_info.observed_in_passive_scan),
                    multiple_bss_candidates: Some(false),
                },
                bss_info1.channel,
                bss_info1.bssid
            ))
        );
    }

    #[test]
    fn select_best_connection_candidate_incompatible() {
        // generate Inspect nodes
        let inspector = inspect::Inspector::new();
        let mut inspect_node =
            InspectBoundedListNode::new(inspector.root().create_child("test"), 10);
        // build networks list
        let test_id_1 = types::NetworkIdentifier {
            ssid: "foo".as_bytes().to_vec(),
            type_: types::SecurityType::Wpa3,
        };
        let credential_1 = Credential::Password("foo_pass".as_bytes().to_vec());
        let test_id_2 = types::NetworkIdentifier {
            ssid: "bar".as_bytes().to_vec(),
            type_: types::SecurityType::Wpa,
        };
        let credential_2 = Credential::Password("bar_pass".as_bytes().to_vec());

        let mut networks = vec![];

        let bss_info1 = types::Bss {
            compatible: true,
            rssi: -14,
            channel: generate_channel(1),
            ..generate_random_bss()
        };
        networks.push(InternalBss {
            network_info: InternalSavedNetworkData {
                network_id: test_id_1.clone(),
                credential: credential_1.clone(),
                has_ever_connected: true,
                recent_failures: Vec::new(),
            },
            bss_info: &bss_info1,
            multiple_bss_candidates: true,
            hasher: WlanHasher::new(rand::thread_rng().gen::<u64>().to_le_bytes()),
        });

        let bss_info2 = types::Bss {
            compatible: false,
            rssi: -10,
            channel: generate_channel(1),
            ..generate_random_bss()
        };
        networks.push(InternalBss {
            network_info: InternalSavedNetworkData {
                network_id: test_id_1.clone(),
                credential: credential_1.clone(),
                has_ever_connected: true,
                recent_failures: Vec::new(),
            },
            bss_info: &bss_info2,
            multiple_bss_candidates: true,
            hasher: WlanHasher::new(rand::thread_rng().gen::<u64>().to_le_bytes()),
        });

        let bss_info3 = types::Bss {
            compatible: true,
            rssi: -12,
            channel: generate_channel(1),
            ..generate_random_bss()
        };
        networks.push(InternalBss {
            network_info: InternalSavedNetworkData {
                network_id: test_id_2.clone(),
                credential: credential_2.clone(),
                has_ever_connected: true,
                recent_failures: Vec::new(),
            },
            bss_info: &bss_info3,
            multiple_bss_candidates: false,
            hasher: WlanHasher::new(rand::thread_rng().gen::<u64>().to_le_bytes()),
        });

        // stronger network returned
        assert_eq!(
            select_best_connection_candidate(networks.clone(), &vec![], &mut inspect_node),
            Some((
                types::ConnectionCandidate {
                    network: test_id_2.clone(),
                    credential: credential_2.clone(),
                    bss: bss_info3.bss_desc.clone(),
                    observed_in_passive_scan: Some(networks[2].bss_info.observed_in_passive_scan),
                    multiple_bss_candidates: Some(false),
                },
                bss_info3.channel,
                bss_info3.bssid
            ))
        );

        // mark the stronger network as incompatible
        let mut modified_network = networks[2].clone();
        let modified_bss_info =
            types::Bss { compatible: false, ..modified_network.bss_info.clone() };
        modified_network.bss_info = &modified_bss_info;
        networks[2] = modified_network;

        // other network returned
        assert_eq!(
            select_best_connection_candidate(networks.clone(), &vec![], &mut inspect_node),
            Some((
                types::ConnectionCandidate {
                    network: test_id_1.clone(),
                    credential: credential_1.clone(),
                    bss: networks[0].bss_info.bss_desc.clone(),
                    observed_in_passive_scan: Some(networks[0].bss_info.observed_in_passive_scan),
                    multiple_bss_candidates: Some(true),
                },
                networks[0].bss_info.channel,
                networks[0].bss_info.bssid
            ))
        );
    }

    #[test]
    fn select_best_connection_candidate_ignore_list() {
        // generate Inspect nodes
        let inspector = inspect::Inspector::new();
        let mut inspect_node =
            InspectBoundedListNode::new(inspector.root().create_child("test"), 10);
        // build networks list
        let test_id_1 = types::NetworkIdentifier {
            ssid: "foo".as_bytes().to_vec(),
            type_: types::SecurityType::Wpa3,
        };
        let credential_1 = Credential::Password("foo_pass".as_bytes().to_vec());
        let test_id_2 = types::NetworkIdentifier {
            ssid: "bar".as_bytes().to_vec(),
            type_: types::SecurityType::Wpa,
        };
        let credential_2 = Credential::Password("bar_pass".as_bytes().to_vec());

        let mut networks = vec![];

        let bss_info1 = types::Bss { compatible: true, rssi: -100, ..generate_random_bss() };
        networks.push(InternalBss {
            network_info: InternalSavedNetworkData {
                network_id: test_id_1.clone(),
                credential: credential_1.clone(),
                has_ever_connected: true,
                recent_failures: Vec::new(),
            },
            bss_info: &bss_info1,
            multiple_bss_candidates: false,
            hasher: WlanHasher::new(rand::thread_rng().gen::<u64>().to_le_bytes()),
        });

        let bss_info2 = types::Bss { compatible: true, rssi: -12, ..generate_random_bss() };
        networks.push(InternalBss {
            network_info: InternalSavedNetworkData {
                network_id: test_id_2.clone(),
                credential: credential_2.clone(),
                has_ever_connected: true,
                recent_failures: Vec::new(),
            },
            bss_info: &bss_info2,
            multiple_bss_candidates: false,
            hasher: WlanHasher::new(rand::thread_rng().gen::<u64>().to_le_bytes()),
        });

        // stronger network returned
        assert_eq!(
            select_best_connection_candidate(networks.clone(), &vec![], &mut inspect_node),
            Some((
                types::ConnectionCandidate {
                    network: test_id_2.clone(),
                    credential: credential_2.clone(),
                    bss: bss_info2.bss_desc.clone(),
                    observed_in_passive_scan: Some(networks[1].bss_info.observed_in_passive_scan),
                    multiple_bss_candidates: Some(false),
                },
                bss_info2.channel,
                bss_info2.bssid
            ))
        );

        // ignore the stronger network, other network returned
        assert_eq!(
            select_best_connection_candidate(
                networks.clone(),
                &vec![test_id_2.clone()],
                &mut inspect_node
            ),
            Some((
                types::ConnectionCandidate {
                    network: test_id_1.clone(),
                    credential: credential_1.clone(),
                    bss: bss_info1.bss_desc.clone(),
                    observed_in_passive_scan: Some(networks[0].bss_info.observed_in_passive_scan),
                    multiple_bss_candidates: Some(false),
                },
                bss_info1.channel,
                bss_info1.bssid
            ))
        );
    }

    #[test]
    fn select_best_connection_candidate_logs_to_inspect() {
        // generate Inspect nodes
        let inspector = inspect::Inspector::new();
        let mut inspect_node =
            InspectBoundedListNode::new(inspector.root().create_child("test"), 10);
        // build networks list
        let test_id_1 = types::NetworkIdentifier {
            ssid: "foo".as_bytes().to_vec(),
            type_: types::SecurityType::Wpa3,
        };
        let credential_1 = Credential::Password("foo_pass".as_bytes().to_vec());
        let test_id_2 = types::NetworkIdentifier {
            ssid: "bar".as_bytes().to_vec(),
            type_: types::SecurityType::Wpa,
        };
        let credential_2 = Credential::Password("bar_pass".as_bytes().to_vec());

        let mut networks = vec![];

        let bss_info1 = types::Bss {
            compatible: true,
            rssi: -14,
            channel: generate_channel(1),
            ..generate_random_bss()
        };
        networks.push(InternalBss {
            network_info: InternalSavedNetworkData {
                network_id: test_id_1.clone(),
                credential: credential_1.clone(),
                has_ever_connected: true,
                recent_failures: Vec::new(),
            },
            bss_info: &bss_info1,
            multiple_bss_candidates: true,
            hasher: WlanHasher::new(rand::thread_rng().gen::<u64>().to_le_bytes()),
        });

        let bss_info2 = types::Bss {
            compatible: false,
            rssi: -10,
            channel: generate_channel(1),
            ..generate_random_bss()
        };
        networks.push(InternalBss {
            network_info: InternalSavedNetworkData {
                network_id: test_id_1.clone(),
                credential: credential_1.clone(),
                has_ever_connected: true,
                recent_failures: Vec::new(),
            },
            bss_info: &bss_info2,
            multiple_bss_candidates: true,
            hasher: WlanHasher::new(rand::thread_rng().gen::<u64>().to_le_bytes()),
        });

        let bss_info3 = types::Bss {
            compatible: true,
            rssi: -12,
            channel: generate_channel(1),
            ..generate_random_bss()
        };
        networks.push(InternalBss {
            network_info: InternalSavedNetworkData {
                network_id: test_id_2.clone(),
                credential: credential_2.clone(),
                has_ever_connected: true,
                recent_failures: Vec::new(),
            },
            bss_info: &bss_info3,
            multiple_bss_candidates: false,
            hasher: WlanHasher::new(rand::thread_rng().gen::<u64>().to_le_bytes()),
        });

        // stronger network returned
        assert_eq!(
            select_best_connection_candidate(networks.clone(), &vec![], &mut inspect_node),
            Some((
                types::ConnectionCandidate {
                    network: test_id_2.clone(),
                    credential: credential_2.clone(),
                    bss: bss_info3.bss_desc.clone(),
                    observed_in_passive_scan: Some(networks[2].bss_info.observed_in_passive_scan),
                    multiple_bss_candidates: Some(false),
                },
                bss_info3.channel,
                bss_info3.bssid
            ))
        );

        assert_data_tree!(inspector, root: {
            test: {
                "0": {
                    "@time": inspect::testing::AnyProperty,
                    "candidates": {
                        "0": contains {
                            score: inspect::testing::AnyProperty,
                        },
                        "1": contains {
                            score: inspect::testing::AnyProperty,
                        },
                        "2": contains {
                            score: inspect::testing::AnyProperty,
                        },
                    },
                    "selected": {
                        ssid_hash: networks[2].hasher.hash_ssid(&networks[2].network_info.network_id.ssid),
                        bssid_hash: networks[2].hasher.hash_mac_addr(&networks[2].bss_info.bssid),
                        rssi: i64::from(networks[2].bss_info.rssi),
                        score: i64::from(networks[2].score()),
                        security_type_saved: networks[2].saved_security_type_to_string(),
                        channel: {
                            cbw: inspect::testing::AnyProperty,
                            primary: u64::from(networks[2].bss_info.channel.primary),
                            secondary80: u64::from(networks[2].bss_info.channel.secondary80),
                        },
                        compatible: networks[2].bss_info.compatible,
                        recent_failure_count: networks[2].recent_failure_count(),
                        saved_network_has_ever_connected: networks[2].network_info.has_ever_connected,
                    },
                }
            },
        });
    }

    #[fasync::run_singlethreaded(test)]
    async fn perform_scan_cache_is_fresh() {
        let mut test_values = test_setup().await;
        let network_selector = test_values.network_selector;

        // Set the scan result cache to be fresher than STALE_SCAN_AGE
        let mut scan_result_guard = network_selector.scan_result_cache.lock().await;
        let last_scan_age = zx::Duration::from_millis(1);
        assert!(last_scan_age < STALE_SCAN_AGE);
        scan_result_guard.updated_at = zx::Time::get_monotonic() - last_scan_age;
        drop(scan_result_guard);

        network_selector.perform_scan(test_values.iface_manager).await;

        // Metric logged for scan age
        let metric = test_values.cobalt_events.try_next().unwrap().unwrap();
        let expected_metric =
            CobaltEvent::builder(LAST_SCAN_AGE_WHEN_SCAN_REQUESTED_METRIC_ID).as_elapsed_time(0);
        // We need to individually check each field, since the elapsed time is non-deterministic
        assert_eq!(metric.metric_id, expected_metric.metric_id);
        assert_eq!(metric.event_codes, expected_metric.event_codes);
        assert_eq!(metric.component, expected_metric.component);
        assert_variant!(
            metric.payload, fidl_fuchsia_cobalt::EventPayload::ElapsedMicros(elapsed_micros) => {
                let elapsed_time = zx::Duration::from_micros(elapsed_micros.try_into().unwrap());
                assert!(elapsed_time < STALE_SCAN_AGE);
            }
        );

        // No scan performed
        assert!(test_values.sme_stream.next().await.is_none());
    }

    #[test]
    fn perform_scan_cache_is_stale() {
        let mut exec = fasync::Executor::new().expect("failed to create an executor");
        let mut test_values = exec.run_singlethreaded(test_setup());
        let network_selector = test_values.network_selector;
        let test_start_time = zx::Time::get_monotonic();

        // Set the scan result cache to be older than STALE_SCAN_AGE
        let mut scan_result_guard =
            exec.run_singlethreaded(network_selector.scan_result_cache.lock());
        scan_result_guard.updated_at =
            zx::Time::get_monotonic() - (STALE_SCAN_AGE + zx::Duration::from_seconds(1));
        drop(scan_result_guard);

        // Kick off scan
        let scan_fut = network_selector.perform_scan(test_values.iface_manager);
        pin_mut!(scan_fut);
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Metric logged for scan age
        let metric = test_values.cobalt_events.try_next().unwrap().unwrap();
        let expected_metric =
            CobaltEvent::builder(LAST_SCAN_AGE_WHEN_SCAN_REQUESTED_METRIC_ID).as_elapsed_time(0);
        assert_eq!(metric.metric_id, expected_metric.metric_id);
        assert_eq!(metric.event_codes, expected_metric.event_codes);
        assert_eq!(metric.component, expected_metric.component);
        assert_variant!(
            metric.payload, fidl_fuchsia_cobalt::EventPayload::ElapsedMicros(elapsed_micros) => {
                let elapsed_time = zx::Duration::from_micros(elapsed_micros.try_into().unwrap());
                assert!(elapsed_time > STALE_SCAN_AGE);
            }
        );

        // Check that a scan request was sent to the sme and send back results
        let expected_scan_request = fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {});
        validate_sme_scan_request_and_send_results(
            &mut exec,
            &mut test_values.sme_stream,
            &expected_scan_request,
            vec![],
        );
        // Process scan
        exec.run_singlethreaded(&mut scan_fut);

        // Check scan results were updated
        let scan_result_guard = exec.run_singlethreaded(network_selector.scan_result_cache.lock());
        assert!(scan_result_guard.updated_at > test_start_time);
        assert!(scan_result_guard.updated_at < zx::Time::get_monotonic());
        drop(scan_result_guard);
    }

    #[test]
    fn perform_scan_error_doesnt_use_stale_results() {
        let mut exec = fasync::Executor::new().expect("failed to create an executor");
        let mut test_values = exec.run_singlethreaded(test_setup());
        let network_selector = test_values.network_selector;

        // Set the scan result cache to be older than STALE_SCAN_AGE
        let mut scan_result_guard =
            exec.run_singlethreaded(network_selector.scan_result_cache.lock());
        scan_result_guard.updated_at =
            zx::Time::get_monotonic() - (STALE_SCAN_AGE + zx::Duration::from_seconds(1));
        // Add some stale/old results to the cache
        scan_result_guard.results = vec![types::ScanResult {
            ssid: "foo".as_bytes().to_vec(),
            security_type_detailed: types::SecurityTypeDetailed::Wpa2Wpa3Personal,
            entries: vec![],
            compatibility: types::Compatibility::Supported,
        }];
        drop(scan_result_guard);

        // Kick off scan
        let scan_fut = network_selector.perform_scan(test_values.iface_manager);
        pin_mut!(scan_fut);
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Check that a scan request was sent to the sme and send back an error
        assert_variant!(
            exec.run_until_stalled(&mut test_values.sme_stream.next()),
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
        // Process scan
        exec.run_singlethreaded(&mut scan_fut);

        // Check there are no scan results presents for use
        let scan_result_guard = exec.run_singlethreaded(network_selector.scan_result_cache.lock());
        assert_eq!(scan_result_guard.results.len(), 0);
        drop(scan_result_guard);
    }

    #[test]
    fn augment_bss_with_active_scan_doesnt_run_on_actively_found_networks() {
        let mut exec = fasync::Executor::new().expect("failed to create an executor");
        let test_values = exec.run_singlethreaded(test_setup());

        let test_id_1 = types::NetworkIdentifier {
            ssid: "foo".as_bytes().to_vec(),
            type_: types::SecurityType::Wpa3,
        };
        let credential_1 = Credential::Password("foo_pass".as_bytes().to_vec());
        let bss_info1 = types::Bss {
            compatible: true,
            rssi: -14,
            channel: generate_channel(36),
            ..generate_random_bss()
        };
        let connect_req = types::ConnectionCandidate {
            network: test_id_1.clone(),
            credential: credential_1.clone(),
            bss: bss_info1.bss_desc.clone(),
            observed_in_passive_scan: Some(false), // was actively scanned
            multiple_bss_candidates: Some(false),
        };

        let fut = augment_bss_with_active_scan(
            connect_req.clone(),
            bss_info1.channel,
            bss_info1.bssid,
            test_values.iface_manager.clone(),
        );
        pin_mut!(fut);

        // The connect_req comes out the other side with no change
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Ready(req) => {
            assert_eq!(req, connect_req)}
        );
    }

    #[test]
    fn augment_bss_with_active_scan_runs_on_passively_found_networks() {
        let mut exec = fasync::Executor::new().expect("failed to create an executor");
        let mut test_values = exec.run_singlethreaded(test_setup());

        let test_id_1 = types::NetworkIdentifier {
            ssid: "foo".as_bytes().to_vec(),
            type_: types::SecurityType::Wpa3,
        };
        let credential_1 = Credential::Password("foo_pass".as_bytes().to_vec());
        let bss_info1 = types::Bss {
            compatible: true,
            rssi: -14,
            channel: generate_channel(36),
            ..generate_random_bss()
        };
        let connect_req = types::ConnectionCandidate {
            network: test_id_1.clone(),
            credential: credential_1.clone(),
            bss: bss_info1.bss_desc.clone(),
            observed_in_passive_scan: Some(true), // was passively scanned
            multiple_bss_candidates: Some(true),
        };

        let fut = augment_bss_with_active_scan(
            connect_req.clone(),
            bss_info1.channel,
            bss_info1.bssid,
            test_values.iface_manager.clone(),
        );
        pin_mut!(fut);

        // Progress the future until a scan request is sent
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check that a scan request was sent to the sme and send back results
        let expected_scan_request = fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
            ssids: vec![test_id_1.ssid.clone()],
            channels: vec![36],
        });
        let new_bss_desc = generate_random_bss_desc();
        let mock_scan_results = vec![
            fidl_sme::BssInfo {
                bssid: [0, 0, 0, 0, 0, 0], // Not the same BSSID
                ssid: test_id_1.ssid.clone(),
                rssi_dbm: 10,
                snr_db: 10,
                channel: fidl_common::WlanChan {
                    primary: 1,
                    cbw: fidl_common::Cbw::Cbw20,
                    secondary80: 0,
                },
                protection: fidl_sme::Protection::Wpa3Enterprise,
                compatible: true,
                bss_desc: generate_random_bss_desc(),
            },
            fidl_sme::BssInfo {
                bssid: bss_info1.bssid.clone(),
                ssid: test_id_1.ssid.clone(),
                rssi_dbm: 0,
                snr_db: 0,
                channel: fidl_common::WlanChan {
                    primary: 1,
                    cbw: fidl_common::Cbw::Cbw20,
                    secondary80: 0,
                },
                protection: fidl_sme::Protection::Wpa3Enterprise,
                compatible: true,
                bss_desc: new_bss_desc.clone(),
            },
        ];
        validate_sme_scan_request_and_send_results(
            &mut exec,
            &mut test_values.sme_stream,
            &expected_scan_request,
            mock_scan_results,
        );

        // The connect_req comes out the other side with the new bss_desc
        assert_eq!(
            exec.run_singlethreaded(fut),
            types::ConnectionCandidate {
                bss: new_bss_desc,
                // observed_in_passive_scan should still be true, since the network was found in a
                // passive scan prior to the directed active scan augmentation.
                observed_in_passive_scan: Some(true),
                // multiple_bss_candidates should still be true, even if only one bss was found in
                // the active scan, because we had found multiple BSSs prior to the active scan.
                multiple_bss_candidates: Some(true),
                ..connect_req
            }
        );
    }

    #[test]
    fn find_best_connection_candidate_end_to_end() {
        let mut exec = fasync::Executor::new().expect("failed to create an executor");
        let mut test_values = exec.run_singlethreaded(test_setup());
        let network_selector = test_values.network_selector;

        // create some identifiers
        let test_id_1 = types::NetworkIdentifier {
            ssid: "foo".as_bytes().to_vec(),
            type_: types::SecurityType::Wpa3,
        };
        let credential_1 = Credential::Password("foo_pass".as_bytes().to_vec());
        let bss_desc1 = generate_random_bss_desc();
        let bss_desc1_active = generate_random_bss_desc();
        let test_id_2 = types::NetworkIdentifier {
            ssid: "bar".as_bytes().to_vec(),
            type_: types::SecurityType::Wpa,
        };
        let credential_2 = Credential::Password("bar_pass".as_bytes().to_vec());
        let bss_desc2 = generate_random_bss_desc();
        let bss_desc2_active = generate_random_bss_desc();

        // insert some new saved networks
        assert!(exec
            .run_singlethreaded(
                test_values
                    .saved_network_manager
                    .store(test_id_1.clone().into(), credential_1.clone()),
            )
            .unwrap()
            .is_none());
        assert!(exec
            .run_singlethreaded(
                test_values
                    .saved_network_manager
                    .store(test_id_2.clone().into(), credential_2.clone()),
            )
            .unwrap()
            .is_none());

        // Mark them as having connected. Make connection passive so that no active scans are made.
        exec.run_singlethreaded(test_values.saved_network_manager.record_connect_result(
            test_id_1.clone().into(),
            &credential_1.clone(),
            [0, 0, 0, 0, 0, 0],
            fidl_sme::ConnectResultCode::Success,
            Some(fidl_common::ScanType::Passive),
        ));
        exec.run_singlethreaded(test_values.saved_network_manager.record_connect_result(
            test_id_2.clone().into(),
            &credential_2.clone(),
            [0, 0, 0, 0, 0, 0],
            fidl_sme::ConnectResultCode::Success,
            Some(fidl_common::ScanType::Passive),
        ));

        // Kick off network selection
        let ignore_list = vec![];
        let network_selection_fut = network_selector
            .find_best_connection_candidate(test_values.iface_manager.clone(), &ignore_list);
        pin_mut!(network_selection_fut);
        assert_variant!(exec.run_until_stalled(&mut network_selection_fut), Poll::Pending);

        // Check that a scan request was sent to the sme and send back results
        let expected_scan_request = fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {});
        let mock_scan_results = vec![
            fidl_sme::BssInfo {
                bssid: [0, 0, 0, 0, 0, 0],
                ssid: test_id_1.ssid.clone(),
                rssi_dbm: 10,
                snr_db: 10,
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
                bssid: [0, 0, 0, 0, 0, 0],
                ssid: test_id_2.ssid.clone(),
                rssi_dbm: 0,
                snr_db: 0,
                channel: fidl_common::WlanChan {
                    primary: 1,
                    cbw: fidl_common::Cbw::Cbw20,
                    secondary80: 0,
                },
                protection: fidl_sme::Protection::Wpa1,
                compatible: true,
                bss_desc: bss_desc2.clone(),
            },
        ];
        validate_sme_scan_request_and_send_results(
            &mut exec,
            &mut test_values.sme_stream,
            &expected_scan_request,
            mock_scan_results.clone(),
        );

        // Process scan results
        assert_variant!(exec.run_until_stalled(&mut network_selection_fut), Poll::Pending);

        // An additional directed active scan should be made for the selected network
        let expected_scan_request = fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
            ssids: vec![test_id_1.ssid.clone()],
            channels: vec![1],
        });
        let mock_active_scan_results = vec![fidl_sme::BssInfo {
            bssid: [0, 0, 0, 0, 0, 0],
            ssid: test_id_1.ssid.clone(),
            rssi_dbm: 10,
            snr_db: 10,
            channel: fidl_common::WlanChan {
                primary: 1,
                cbw: fidl_common::Cbw::Cbw20,
                secondary80: 0,
            },
            protection: fidl_sme::Protection::Wpa3Enterprise,
            compatible: true,
            bss_desc: bss_desc1_active.clone(),
        }];
        poll_for_and_validate_sme_scan_request_and_send_results(
            &mut exec,
            &mut network_selection_fut,
            &mut test_values.sme_stream,
            &expected_scan_request,
            mock_active_scan_results,
        );

        // Check that we pick a network
        let results = exec.run_singlethreaded(&mut network_selection_fut);
        assert_eq!(
            results,
            Some(types::ConnectionCandidate {
                network: test_id_1.clone(),
                credential: credential_1.clone(),
                bss: bss_desc1_active.clone(),
                observed_in_passive_scan: Some(true),
                multiple_bss_candidates: Some(false)
            })
        );

        // Set the scan result cache's age so it is guaranteed to be old enough to trigger another
        // passive scan. Without this manual adjustment, the test timing is such that sometimes the
        // cache is fresh enough to use (therefore no new passive scan is performed).
        let mut scan_result_guard =
            exec.run_singlethreaded(network_selector.scan_result_cache.lock());
        scan_result_guard.updated_at =
            zx::Time::get_monotonic() - (STALE_SCAN_AGE + zx::Duration::from_millis(1));
        drop(scan_result_guard);

        // Ignore that network, check that we pick the other one
        let ignore_list = vec![test_id_1.clone()];
        let network_selection_fut = network_selector
            .find_best_connection_candidate(test_values.iface_manager.clone(), &ignore_list);
        pin_mut!(network_selection_fut);
        assert_variant!(exec.run_until_stalled(&mut network_selection_fut), Poll::Pending);

        // Check that a scan request was sent to the sme and send back results
        let expected_scan_request = fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {});
        validate_sme_scan_request_and_send_results(
            &mut exec,
            &mut test_values.sme_stream,
            &expected_scan_request,
            mock_scan_results,
        );

        // Process scan results
        assert_variant!(exec.run_until_stalled(&mut network_selection_fut), Poll::Pending);

        // An additional directed active scan should be made for the selected network
        let expected_scan_request = fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
            ssids: vec![test_id_2.ssid.clone()],
            channels: vec![1],
        });
        let mock_active_scan_results = vec![fidl_sme::BssInfo {
            bssid: [0, 0, 0, 0, 0, 0],
            ssid: test_id_2.ssid.clone(),
            rssi_dbm: 10,
            snr_db: 10,
            channel: fidl_common::WlanChan {
                primary: 1,
                cbw: fidl_common::Cbw::Cbw20,
                secondary80: 0,
            },
            protection: fidl_sme::Protection::Wpa1,
            compatible: true,
            bss_desc: bss_desc2_active.clone(),
        }];
        poll_for_and_validate_sme_scan_request_and_send_results(
            &mut exec,
            &mut network_selection_fut,
            &mut test_values.sme_stream,
            &expected_scan_request,
            mock_active_scan_results,
        );

        let results = exec.run_singlethreaded(&mut network_selection_fut);
        assert_eq!(
            results,
            Some(types::ConnectionCandidate {
                network: test_id_2.clone(),
                credential: credential_2.clone(),
                bss: bss_desc2_active.clone(),
                observed_in_passive_scan: Some(true),
                multiple_bss_candidates: Some(false)
            })
        );

        // Check the network selections were logged
        assert_data_tree!(test_values.inspector, root: {
            net_select_test: {
                network_selection: {
                    "0": {
                        "@time": inspect::testing::AnyProperty,
                        "candidates": {
                            "0": contains {
                                bssid_hash: inspect::testing::AnyProperty,
                                score: inspect::testing::AnyProperty,
                            },
                            "1": contains {
                                bssid_hash: inspect::testing::AnyProperty,
                                score: inspect::testing::AnyProperty,
                            },
                        },
                        "selected": contains {
                            bssid_hash: inspect::testing::AnyProperty,
                            score: inspect::testing::AnyProperty,
                        },
                    },
                    "1": {
                        "@time": inspect::testing::AnyProperty,
                        "candidates": {
                            "0": contains {
                                bssid_hash: inspect::testing::AnyProperty,
                                score: inspect::testing::AnyProperty,
                            },
                            "1": contains {
                                bssid_hash: inspect::testing::AnyProperty,
                                score: inspect::testing::AnyProperty,
                            },
                        },
                        "selected": contains {
                            bssid_hash: inspect::testing::AnyProperty,
                            score: inspect::testing::AnyProperty,
                        },
                    }
                }
            },
        });
    }

    #[test]
    fn find_best_connection_candidate_wpa_wpa2() {
        // Check that if we see a WPA2 network and have WPA and WPA3 credentials saved for it, we
        // could choose the WPA credential but not the WPA3 credential. In other words we can
        // upgrade saved networks to higher security but not downgrade.
        let mut exec = fasync::Executor::new().expect("failed to create executor");
        let test_values = exec.run_singlethreaded(test_setup());
        let network_selector = test_values.network_selector;

        // Save networks with WPA and WPA3 security, same SSIDs, and different passwords.
        let ssid = "foo".as_bytes().to_vec();
        let wpa_network_id =
            types::NetworkIdentifier { ssid: ssid.clone(), type_: types::SecurityType::Wpa };
        let credential = Credential::Password("foo_password".as_bytes().to_vec());
        assert!(exec
            .run_singlethreaded(
                test_values
                    .saved_network_manager
                    .store(wpa_network_id.clone().into(), credential.clone()),
            )
            .expect("Failed to save network")
            .is_none());
        let wpa3_network_id =
            types::NetworkIdentifier { ssid: ssid.clone(), type_: types::SecurityType::Wpa3 };
        let wpa3_credential = Credential::Password("wpa3_only_password".as_bytes().to_vec());
        assert!(exec
            .run_singlethreaded(
                test_values
                    .saved_network_manager
                    .store(wpa3_network_id.clone().into(), wpa3_credential.clone()),
            )
            .expect("Failed to save network")
            .is_none());

        // Record passive connects so that the test will not active scan.
        exec.run_singlethreaded(test_values.saved_network_manager.record_connect_result(
            wpa_network_id.clone().into(),
            &credential,
            [0, 0, 0, 0, 0, 0],
            fidl_sme::ConnectResultCode::Success,
            Some(fidl_common::ScanType::Passive),
        ));
        exec.run_singlethreaded(test_values.saved_network_manager.record_connect_result(
            wpa3_network_id.clone().into(),
            &wpa3_credential,
            [0, 0, 0, 0, 0, 0],
            fidl_sme::ConnectResultCode::Success,
            Some(fidl_common::ScanType::Passive),
        ));

        let mixed_scan_results = vec![types::ScanResult {
            ssid: ssid,
            security_type_detailed: types::SecurityTypeDetailed::Wpa2Personal,
            entries: vec![types::Bss {
                compatible: true,
                observed_in_passive_scan: false, // mark this as active, to avoid an additional scan
                ..generate_random_bss()
            }],
            compatibility: types::Compatibility::Supported,
        }];
        let mut updater = network_selector.generate_scan_result_updater(true);
        exec.run_singlethreaded(updater.update_scan_results(&mixed_scan_results));

        // Set the scan cache's "updated_at" field to the future so that a scan won't be triggered.
        {
            let mut cache_guard =
                exec.run_singlethreaded(network_selector.scan_result_cache.lock());
            cache_guard.updated_at = zx::Time::INFINITE;
        }

        // Check that we choose the config saved as WPA
        let ignore_list = Vec::new();
        let network_selection_fut = network_selector
            .find_best_connection_candidate(test_values.iface_manager.clone(), &ignore_list);
        pin_mut!(network_selection_fut);
        assert_variant!(
            exec.run_until_stalled(&mut network_selection_fut),
            Poll::Ready(Some(connection_candidate)) => {
                let expected_candidate = types::ConnectionCandidate {
                    // The network ID should match network config for recording connect results.
                    network: wpa_network_id.clone(),
                    credential,
                    bss: mixed_scan_results[0].entries[0].bss_desc.clone(),
                    observed_in_passive_scan: Some(
                        mixed_scan_results[0].entries[0].observed_in_passive_scan
                    ),
                    multiple_bss_candidates: Some(false),
                };
                assert_eq!(connection_candidate, expected_candidate);
            }
        );
        // If the best network ID is ignored, there is no best connection candidate.
        let ignore_list = vec![wpa_network_id];
        let network_selection_fut = network_selector
            .find_best_connection_candidate(test_values.iface_manager.clone(), &ignore_list);
        pin_mut!(network_selection_fut);
        assert_variant!(exec.run_until_stalled(&mut network_selection_fut), Poll::Ready(None));
    }

    #[test]
    fn find_connection_candidate_for_network_end_to_end() {
        let mut exec = fasync::Executor::new().expect("failed to create an executor");
        let mut test_values = exec.run_singlethreaded(test_setup());
        let network_selector = test_values.network_selector;

        // create identifiers
        let test_id_1 = types::NetworkIdentifier {
            ssid: "foo".as_bytes().to_vec(),
            type_: types::SecurityType::Wpa3,
        };
        let credential_1 = Credential::Password("foo_pass".as_bytes().to_vec());
        let bss_desc_1 = generate_random_bss_desc();

        // insert saved networks
        assert!(exec
            .run_singlethreaded(
                test_values
                    .saved_network_manager
                    .store(test_id_1.clone().into(), credential_1.clone()),
            )
            .unwrap()
            .is_none());

        // get the sme proxy
        let mut iface_manager_inner = exec.run_singlethreaded(test_values.iface_manager.lock());
        let sme_proxy =
            exec.run_singlethreaded(iface_manager_inner.get_sme_proxy_for_scan()).unwrap();
        drop(iface_manager_inner);

        // Kick off network selection
        let network_selection_fut = network_selector.find_connection_candidate_for_network(
            sme_proxy,
            test_id_1.clone(),
            true,
        );
        pin_mut!(network_selection_fut);
        assert_variant!(exec.run_until_stalled(&mut network_selection_fut), Poll::Pending);

        // Check that a scan request was sent to the sme and send back results
        let expected_scan_request = fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
            ssids: vec![test_id_1.ssid.clone()],
            channels: vec![],
        });
        let mock_scan_results = vec![
            fidl_sme::BssInfo {
                bssid: [0, 0, 0, 0, 0, 0],
                ssid: test_id_1.ssid.clone(),
                rssi_dbm: 10,
                snr_db: 10,
                channel: fidl_common::WlanChan {
                    primary: 1,
                    cbw: fidl_common::Cbw::Cbw20,
                    secondary80: 0,
                },
                // This network is WPA3, but should still match against the desired WPA2 network
                protection: fidl_sme::Protection::Wpa3Personal,
                compatible: true,
                bss_desc: bss_desc_1.clone(),
            },
            fidl_sme::BssInfo {
                bssid: [0, 0, 0, 0, 0, 0],
                ssid: "other ssid".as_bytes().to_vec(),
                rssi_dbm: 0,
                snr_db: 0,
                channel: fidl_common::WlanChan {
                    primary: 1,
                    cbw: fidl_common::Cbw::Cbw20,
                    secondary80: 0,
                },
                protection: fidl_sme::Protection::Wpa1,
                compatible: true,
                bss_desc: generate_random_bss_desc(),
            },
        ];
        validate_sme_scan_request_and_send_results(
            &mut exec,
            &mut test_values.sme_stream,
            &expected_scan_request,
            mock_scan_results,
        );

        // Check that we pick a network
        let results = exec.run_singlethreaded(&mut network_selection_fut);
        assert_eq!(
            results,
            Some(types::ConnectionCandidate {
                network: test_id_1.clone(),
                credential: credential_1.clone(),
                bss: bss_desc_1,
                // This code path can't know if the network would have been observed in a passive
                // scan, since it never performs a passive scan.
                observed_in_passive_scan: None,
                multiple_bss_candidates: Some(false),
            })
        );
    }

    #[test]
    fn find_connection_candidate_for_network_end_to_end_with_failure() {
        let mut exec = fasync::Executor::new().expect("failed to create an executor");
        let mut test_values = exec.run_singlethreaded(test_setup());
        let network_selector = test_values.network_selector;

        // create identifiers
        let test_id_1 = types::NetworkIdentifier {
            ssid: "foo".as_bytes().to_vec(),
            type_: types::SecurityType::Wpa3,
        };

        // get the sme proxy
        let mut iface_manager_inner = exec.run_singlethreaded(test_values.iface_manager.lock());
        let sme_proxy =
            exec.run_singlethreaded(iface_manager_inner.get_sme_proxy_for_scan()).unwrap();
        drop(iface_manager_inner);

        // Kick off network selection
        let network_selection_fut =
            network_selector.find_connection_candidate_for_network(sme_proxy, test_id_1, true);
        pin_mut!(network_selection_fut);
        assert_variant!(exec.run_until_stalled(&mut network_selection_fut), Poll::Pending);

        // Return an error on the scan
        assert_variant!(
            exec.run_until_stalled(&mut test_values.sme_stream.next()),
            Poll::Ready(Some(Ok(fidl_sme::ClientSmeRequest::Scan {
                txn, req: _, control_handle: _
            }))) => {
                // Send failed scan response.
                let (_stream, ctrl) = txn
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl.send_on_error(&mut fidl_sme::ScanError {
                    code: fidl_sme::ScanErrorCode::InternalError,
                    message: "Failed to scan".to_string()
                }).expect("failed to send scan error");
            }
        );

        // Check that nothing is returned
        let results = exec.run_singlethreaded(&mut network_selection_fut);
        assert_eq!(results, None);
    }

    fn generate_random_bss() -> types::Bss {
        let mut rng = rand::thread_rng();
        let bss = (0..6).map(|_| rng.gen::<u8>()).collect::<Vec<u8>>();
        types::Bss {
            bssid: bss.as_slice().try_into().unwrap(),
            rssi: rng.gen_range(-100, 20),
            channel: generate_random_channel(),
            timestamp_nanos: 0,
            snr_db: rng.gen_range(-20, 50),
            observed_in_passive_scan: rng.gen::<bool>(),
            compatible: rng.gen::<bool>(),
            bss_desc: generate_random_bss_desc(),
        }
    }

    fn generate_random_scan_result() -> types::ScanResult {
        let mut rng = rand::thread_rng();
        let ssid = format!("scan result rand {}", rng.gen::<i32>()).as_bytes().to_vec();
        types::ScanResult {
            ssid: ssid,
            security_type_detailed: types::SecurityTypeDetailed::Wpa1,
            entries: vec![generate_random_bss(), generate_random_bss()],
            compatibility: match rng.gen_range(0, 2) {
                0 => types::Compatibility::Supported,
                1 => types::Compatibility::DisallowedNotSupported,
                2 => types::Compatibility::DisallowedInsecure,
                _ => panic!(),
            },
        }
    }

    fn generate_random_saved_network() -> (types::NetworkIdentifier, InternalSavedNetworkData) {
        let mut rng = rand::thread_rng();
        let net_id = types::NetworkIdentifier {
            ssid: format!("saved network rand {}", rng.gen::<i32>()).as_bytes().to_vec(),
            type_: types::SecurityType::Wpa,
        };
        (
            net_id.clone(),
            InternalSavedNetworkData {
                network_id: net_id,
                credential: Credential::Password(
                    format!("password {}", rng.gen::<i32>()).as_bytes().to_vec(),
                ),
                has_ever_connected: false,
                recent_failures: Vec::new(),
            },
        )
    }

    #[fasync::run_singlethreaded(test)]
    async fn recorded_metrics_on_scan() {
        let (mut cobalt_api, mut cobalt_events) = create_mock_cobalt_sender_and_receiver();

        // create some identifiers
        let test_ssid_1 = "foo".as_bytes().to_vec();
        let test_security_1 = types::SecurityTypeDetailed::Wpa3Personal;
        let test_id_1 = types::NetworkIdentifier {
            ssid: test_ssid_1.clone(),
            type_: types::SecurityType::Wpa3,
        };
        let test_ssid_2 = "bar".as_bytes().to_vec();
        let test_security_2 = types::SecurityTypeDetailed::Wpa1;
        let test_id_2 =
            types::NetworkIdentifier { ssid: test_ssid_2.clone(), type_: types::SecurityType::Wpa };

        let mock_scan_results = vec![
            types::ScanResult {
                ssid: test_ssid_1.clone(),
                security_type_detailed: test_security_1.clone(),
                entries: vec![
                    types::Bss { observed_in_passive_scan: true, ..generate_random_bss() },
                    types::Bss { observed_in_passive_scan: true, ..generate_random_bss() },
                    types::Bss { observed_in_passive_scan: false, ..generate_random_bss() },
                ],
                compatibility: types::Compatibility::Supported,
            },
            types::ScanResult {
                ssid: test_ssid_2.clone(),
                security_type_detailed: test_security_2.clone(),
                entries: vec![types::Bss {
                    observed_in_passive_scan: true,
                    ..generate_random_bss()
                }],
                compatibility: types::Compatibility::Supported,
            },
            generate_random_scan_result(),
            generate_random_scan_result(),
            generate_random_scan_result(),
            generate_random_scan_result(),
            generate_random_scan_result(),
        ];

        let mut mock_saved_networks = HashMap::new();
        let _ = mock_saved_networks.insert(
            test_id_1.clone(),
            InternalSavedNetworkData {
                network_id: test_id_1.clone(),
                credential: Credential::Password("foo_pass".as_bytes().to_vec()),
                has_ever_connected: false,
                recent_failures: Vec::new(),
            },
        );
        let _ = mock_saved_networks.insert(
            test_id_2.clone(),
            InternalSavedNetworkData {
                network_id: test_id_2.clone(),
                credential: Credential::Password("bar_pass".as_bytes().to_vec()),
                has_ever_connected: false,
                recent_failures: Vec::new(),
            },
        );
        let random_saved_net = generate_random_saved_network();
        let _ = mock_saved_networks.insert(random_saved_net.0, random_saved_net.1);
        let random_saved_net = generate_random_saved_network();
        let _ = mock_saved_networks.insert(random_saved_net.0, random_saved_net.1);
        let random_saved_net = generate_random_saved_network();
        let _ = mock_saved_networks.insert(random_saved_net.0, random_saved_net.1);

        record_metrics_on_scan(&mock_scan_results, mock_saved_networks, &mut cobalt_api, true);

        // Three BSSs present for network 1 in scan results
        assert_eq!(
            cobalt_events.try_next().unwrap(),
            Some(
                CobaltEvent::builder(SAVED_NETWORK_IN_SCAN_RESULT_METRIC_ID)
                    .with_event_code(
                        SavedNetworkInScanResultMetricDimensionBssCount::TwoToFour.as_event_code()
                    )
                    .as_event()
            )
        );
        // One BSS present for network 2 in scan results
        assert_eq!(
            cobalt_events.try_next().unwrap(),
            Some(
                CobaltEvent::builder(SAVED_NETWORK_IN_SCAN_RESULT_METRIC_ID)
                    .with_event_code(
                        SavedNetworkInScanResultMetricDimensionBssCount::One.as_event_code()
                    )
                    .as_event()
            )
        );
        // Total of two saved networks in the scan results
        assert_eq!(
            cobalt_events.try_next().unwrap(),
            Some(
                CobaltEvent::builder(SCAN_RESULTS_RECEIVED_METRIC_ID)
                    .with_event_code(
                        ScanResultsReceivedMetricDimensionSavedNetworksCount::TwoToFour
                            .as_event_code()
                    )
                    .as_event()
            )
        );
        // One saved networks that was discovered via active scan
        assert_eq!(
            cobalt_events.try_next().unwrap(),
            Some(
                CobaltEvent::builder(SAVED_NETWORK_IN_SCAN_RESULT_WITH_ACTIVE_SCAN_METRIC_ID)
                    .with_event_code(ActiveScanSsidsObserved::One.as_event_code())
                    .as_event()
            )
        );
        // No more metrics
        assert!(cobalt_events.try_next().is_err());
    }

    #[fasync::run_singlethreaded(test)]
    async fn recorded_metrics_on_scan_no_saved_networks() {
        let (mut cobalt_api, mut cobalt_events) = create_mock_cobalt_sender_and_receiver();

        let mock_scan_results = vec![
            generate_random_scan_result(),
            generate_random_scan_result(),
            generate_random_scan_result(),
            generate_random_scan_result(),
            generate_random_scan_result(),
        ];

        let mock_saved_networks = HashMap::new();

        record_metrics_on_scan(&mock_scan_results, mock_saved_networks, &mut cobalt_api, true);

        // No saved networks in scan results
        assert_eq!(
            cobalt_events.try_next().unwrap(),
            Some(
                CobaltEvent::builder(SCAN_RESULTS_RECEIVED_METRIC_ID)
                    .with_event_code(
                        ScanResultsReceivedMetricDimensionSavedNetworksCount::Zero.as_event_code()
                    )
                    .as_event()
            )
        );
        // Also no saved networks that were discovered via active scan
        assert_eq!(
            cobalt_events.try_next().unwrap(),
            Some(
                CobaltEvent::builder(SAVED_NETWORK_IN_SCAN_RESULT_WITH_ACTIVE_SCAN_METRIC_ID)
                    .with_event_code(ActiveScanSsidsObserved::Zero.as_event_code())
                    .as_event()
            )
        );
        // No more metrics
        assert!(cobalt_events.try_next().is_err());
    }

    /// Get the connect failures of the specified network identifier for a test. This is used
    /// because we currently can't
    async fn get_connect_failures(
        id: types::NetworkIdentifier,
        saved_networks_manager: &SavedNetworksManager,
    ) -> Vec<ConnectFailure> {
        saved_networks_manager
            .lookup(id.into())
            .await
            .get(0)
            .expect("failed to get config")
            .perf_stats
            .failure_list
            .get_recent(zx::Time::get_monotonic() - RECENT_FAILURE_WINDOW)
    }

    fn connect_failure_with_bssid(bssid: types::Bssid) -> ConnectFailure {
        ConnectFailure {
            reason: FailureReason::GeneralFailure,
            time: zx::Time::INFINITE,
            bssid: bssid,
        }
    }
}
