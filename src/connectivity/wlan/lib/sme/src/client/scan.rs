// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::client::{inspect, DeviceInfo, Ssid},
    fidl_fuchsia_wlan_common as fidl_common, fidl_fuchsia_wlan_internal as fidl_internal,
    fidl_fuchsia_wlan_mlme::{self as fidl_mlme, ScanRequest, ScanResultCode},
    fidl_fuchsia_wlan_sme as fidl_sme,
    fuchsia_inspect::NumericProperty,
    log::{error, warn},
    std::{
        collections::{hash_map, HashMap, HashSet},
        mem,
        sync::Arc,
    },
    wlan_common::{
        bss::BssDescription,
        channel::{Cbw, Channel},
        ie::IesMerger,
    },
};

const PASSIVE_SCAN_CHANNEL_MS: u32 = 200;
const ACTIVE_SCAN_PROBE_DELAY_MS: u32 = 5;
const ACTIVE_SCAN_CHANNEL_MS: u32 = 75;

type BssId = [u8; 6];

// A "user"-initiated scan request for the purpose of discovering available networks
#[derive(Debug, PartialEq)]
pub struct DiscoveryScan<T> {
    tokens: Vec<T>,
    scan_request: fidl_sme::ScanRequest,
}

impl<T> DiscoveryScan<T> {
    pub fn new(token: T, scan_request: fidl_sme::ScanRequest) -> Self {
        Self { tokens: vec![token], scan_request }
    }

    pub fn matches(&self, scan: &DiscoveryScan<T>) -> bool {
        self.scan_request == scan.scan_request
    }

    pub fn merges(&mut self, mut scan: DiscoveryScan<T>) {
        self.tokens.append(&mut scan.tokens)
    }
}

pub struct ScanScheduler<D> {
    // The currently running scan. We assume that MLME can handle a single concurrent scan
    // regardless of its own state.
    current: ScanState<D>,
    // Pending discovery requests from the user
    pending_discovery: Vec<DiscoveryScan<D>>,
    device_info: Arc<DeviceInfo>,
    last_mlme_txn_id: u64,
}

#[derive(Debug)]
enum ScanState<D> {
    NotScanning,
    ScanningToDiscover {
        cmd: DiscoveryScan<D>,
        mlme_txn_id: u64,
        bss_map: HashMap<BssId, (fidl_internal::BssDescription, IesMerger)>,
    },
}

// A reaction to MLME's ScanConfirm event
pub enum ScanResult<D> {
    // No reaction: the scan results are not relevant anymore, or we received
    // an unexpected ScanConfirm.
    None,
    // "Discovery" scan has finished, either successfully or not.
    // SME is expected to forward the result to the user.
    DiscoveryFinished { tokens: Vec<D>, result: Result<Vec<BssDescription>, ScanResultCode> },
}

impl<D> ScanScheduler<D> {
    pub fn new(device_info: Arc<DeviceInfo>) -> Self {
        ScanScheduler {
            current: ScanState::NotScanning,
            pending_discovery: Vec::new(),
            device_info,
            last_mlme_txn_id: 0,
        }
    }

    // Initiate a "discovery" scan. The scan might or might not begin immediately.
    // The request can be merged with any pending or ongoing requests.
    // If a ScanRequest is returned, the caller is responsible for forwarding it to MLME.
    pub fn enqueue_scan_to_discover(&mut self, s: DiscoveryScan<D>) -> Option<ScanRequest> {
        if let ScanState::ScanningToDiscover { cmd, .. } = &mut self.current {
            if cmd.matches(&s) {
                cmd.merges(s);
                return None;
            }
        }
        if let Some(scan_cmd) = self.pending_discovery.iter_mut().find(|cmd| cmd.matches(&s)) {
            scan_cmd.merges(s);
            return None;
        }
        self.pending_discovery.push(s);
        self.start_next_scan()
    }

    // Should be called for every OnScanResult event received from MLME.
    pub fn on_mlme_scan_result(
        &mut self,
        msg: fidl_mlme::ScanResult,
        sme_inspect: &Arc<inspect::SmeTree>,
    ) {
        if !self.matching_mlme_txn_id(msg.txn_id) {
            error!("rejecting scan result with wrong txn id");
            return;
        }
        match &mut self.current {
            ScanState::ScanningToDiscover { bss_map, .. } => {
                maybe_insert_bss(bss_map, msg.bss, sme_inspect);
            }
            ScanState::NotScanning => {}
        }
    }

    // Should be called for every OnScanEnd event received from MLME.
    // The caller is expected to take action based on the returned ScanResult.
    // If a ScanRequest is returned, the caller is responsible for forwarding it to MLME.
    pub fn on_mlme_scan_end(
        &mut self,
        msg: fidl_mlme::ScanEnd,
        sme_inspect: &Arc<inspect::SmeTree>,
    ) -> (ScanResult<D>, Option<ScanRequest>) {
        if !self.matching_mlme_txn_id(msg.txn_id) {
            return (ScanResult::None, None);
        }
        let old_state = mem::replace(&mut self.current, ScanState::NotScanning);
        let result = match old_state {
            ScanState::NotScanning => ScanResult::None,
            ScanState::ScanningToDiscover { cmd, bss_map, .. } => ScanResult::DiscoveryFinished {
                tokens: cmd.tokens,
                result: match msg.code {
                    ScanResultCode::Success => Ok(convert_bss_map(bss_map, None, sme_inspect)),
                    other => Err(other),
                },
            },
        };
        let request = self.start_next_scan();
        (result, request)
    }

    fn matching_mlme_txn_id(&self, incoming_txn_id: u64) -> bool {
        match &self.current {
            ScanState::NotScanning => false,
            ScanState::ScanningToDiscover { mlme_txn_id, .. } => *mlme_txn_id == incoming_txn_id,
        }
    }

    fn start_next_scan(&mut self) -> Option<ScanRequest> {
        let has_pending = !self.pending_discovery.is_empty();
        (matches!(self.current, ScanState::NotScanning) && has_pending).then(|| {
            self.last_mlme_txn_id += 1;
            let scan_cmd = self.pending_discovery.remove(0);
            let request =
                new_discovery_scan_request(self.last_mlme_txn_id, &scan_cmd, &self.device_info);
            self.current = ScanState::ScanningToDiscover {
                cmd: scan_cmd,
                mlme_txn_id: self.last_mlme_txn_id,
                bss_map: HashMap::new(),
            };
            request
        })
    }
}

fn maybe_insert_bss(
    bss_map: &mut HashMap<BssId, (fidl_internal::BssDescription, IesMerger)>,
    mut fidl_bss: fidl_internal::BssDescription,
    sme_inspect: &Arc<inspect::SmeTree>,
) {
    let mut ies = vec![];
    std::mem::swap(&mut ies, &mut fidl_bss.ies);

    match bss_map.entry(fidl_bss.bssid) {
        hash_map::Entry::Occupied(mut entry) => {
            let (ref mut existing_bss, ref mut ies_merger) = entry.get_mut();
            ies_merger.merge(&ies[..]);
            if ies_merger.buffer_overflow() {
                warn!(
                    "Not merging some IEs due to running out of buffer. BSSID: {:?}",
                    sme_inspect.hasher.hash_mac_addr(&fidl_bss.bssid)
                );
            }
            *existing_bss = fidl_bss;
        }
        hash_map::Entry::Vacant(entry) => {
            entry.insert((fidl_bss, IesMerger::new(ies)));
        }
    }
}

fn convert_bss_map(
    bss_map: HashMap<BssId, (fidl_internal::BssDescription, IesMerger)>,
    ssid_filter: Option<Ssid>,
    sme_inspect: &Arc<inspect::SmeTree>,
) -> Vec<BssDescription> {
    bss_map
        .into_iter()
        .filter_map(|(_bssid, (mut bss, mut ies_merger))| {
            sme_inspect.scan_merge_ie_failures.add(ies_merger.merge_ie_failures() as u64);

            let mut ies = ies_merger.finalize();
            std::mem::swap(&mut ies, &mut bss.ies);
            let bss = BssDescription::from_fidl(bss).ok();
            if bss.is_none() {
                sme_inspect.scan_discard_fidl_bss.add(1);
            }
            bss
        })
        .filter(|v| ssid_filter.as_deref().map(|ssid| v.ssid() == ssid).unwrap_or(true))
        .collect()
}

const WILDCARD_BSS_ID: [u8; 6] = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff];

fn new_scan_request(
    mlme_txn_id: u64,
    scan_request: fidl_sme::ScanRequest,
    ssid: Vec<u8>,
    device_info: &DeviceInfo,
) -> ScanRequest {
    let scan_req = ScanRequest {
        txn_id: mlme_txn_id,
        bss_type_selector: fidl_internal::BSS_TYPE_SELECTOR_INFRASTRUCTURE,
        bssid: WILDCARD_BSS_ID.clone(),
        ssid,
        scan_type: fidl_mlme::ScanTypes::Passive,
        probe_delay: 0,
        channel_list: Some(get_channels_to_scan(&device_info, &scan_request)),
        min_channel_time: PASSIVE_SCAN_CHANNEL_MS,
        max_channel_time: PASSIVE_SCAN_CHANNEL_MS,
        ssid_list: None,
    };
    match scan_request {
        fidl_sme::ScanRequest::Active(active_scan_params) => ScanRequest {
            scan_type: fidl_mlme::ScanTypes::Active,
            probe_delay: ACTIVE_SCAN_PROBE_DELAY_MS,
            min_channel_time: ACTIVE_SCAN_CHANNEL_MS,
            max_channel_time: ACTIVE_SCAN_CHANNEL_MS,
            ssid_list: if active_scan_params.ssids.len() > 0 {
                Some(active_scan_params.ssids)
            } else {
                None
            },
            ..scan_req
        },
        fidl_sme::ScanRequest::Passive(_) => scan_req,
    }
}

fn new_discovery_scan_request<T>(
    mlme_txn_id: u64,
    discovery_scan: &DiscoveryScan<T>,
    device_info: &DeviceInfo,
) -> ScanRequest {
    new_scan_request(mlme_txn_id, discovery_scan.scan_request.clone(), vec![], device_info)
}

/// Get channels to scan depending on device's capability and scan type. If scan type is passive,
/// or if scan type is active but the device handles DFS channels, then the channels returned by
/// this function are the intersection of device's supported channels and Fuchsia supported
/// channels. If scan type is active and the device doesn't handle DFS channels, then the return
/// value excludes DFS channels.
///
/// Example:
///
/// Suppose that Fuchsia supported channels are [1, 2, 52], and 1, 2 are non-DFS channels while
/// 112 is DFS channel. Also suppose that device's supported channels are [1, 52] as parameter
/// to DeviceInfo below.
///
/// ScanType | Device handles DFS | Return values
/// ---------+--------------------+-----------------
/// Passive  | Y                  | [1, 52]
/// Passive  | N                  | [1, 52]
/// Active   | Y                  | [1, 52]
/// Active   | N                  | [1]
fn get_channels_to_scan(device_info: &DeviceInfo, scan_request: &fidl_sme::ScanRequest) -> Vec<u8> {
    let mut device_supported_channels: HashSet<u8> = HashSet::new();
    for band in &device_info.bands {
        device_supported_channels.extend(&band.channels);
    }
    let supports_dfs = device_info.driver_features.contains(&fidl_common::DriverFeature::Dfs);

    SUPPORTED_CHANNELS
        .iter()
        .filter(|channel| device_supported_channels.contains(channel))
        .filter(|channel| {
            if let &fidl_sme::ScanRequest::Passive(_) = scan_request {
                return true;
            };
            if supports_dfs {
                return true;
            };
            !Channel::new(**channel, Cbw::Cbw20).is_dfs()
        })
        .filter(|chan| {
            // If this is an active scan and there are any channels specified by the caller,
            // only include those channels.
            if let &fidl_sme::ScanRequest::Active(ref options) = scan_request {
                if !options.channels.is_empty() {
                    return options.channels.contains(chan);
                }
            }
            true
        })
        .map(|chan| *chan)
        .collect()
}

// TODO(65792): Evaluate options for where and how we select what channels to scan.
// Firmware will reject channels if they are not allowed by the current regulatory region.
const SUPPORTED_CHANNELS: &[u8] = &[
    // 5GHz UNII-1
    36, 40, 44, 48, // 5GHz UNII-2 Middle
    52, 56, 60, 64, // 5GHz UNII-2 Extended
    100, 104, 108, 112, 116, 120, 124, 128, 132, 136, 140, 144, // 5GHz UNII-3
    149, 153, 157, 161, 165, // 5GHz
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, // 2GHz
];

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_utils;
    use fuchsia_inspect::Inspector;
    use itertools;
    use wlan_common::{assert_variant, fake_fidl_bss, hasher::WlanHasher};

    const CLIENT_ADDR: [u8; 6] = [0x7A, 0xE7, 0x76, 0xD9, 0xF2, 0x67];

    fn passive_discovery_scan(token: i32) -> DiscoveryScan<i32> {
        DiscoveryScan::new(token, fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {}))
    }

    #[test]
    fn discovery_scan() {
        let mut sched = create_sched();
        let (_inspector, sme_inspect) = sme_inspect();
        let req = sched
            .enqueue_scan_to_discover(passive_discovery_scan(10))
            .expect("expected a ScanRequest");
        let txn_id = req.txn_id;
        sched.on_mlme_scan_result(
            fidl_mlme::ScanResult {
                txn_id,
                bss: fidl_internal::BssDescription {
                    bssid: [1; 6],
                    ..fake_fidl_bss!(Open, ssid: b"foo".to_vec())
                },
            },
            &sme_inspect,
        );
        sched.on_mlme_scan_result(
            fidl_mlme::ScanResult {
                txn_id: txn_id + 100, // mismatching transaction id
                bss: fidl_internal::BssDescription {
                    bssid: [2; 6],
                    ..fake_fidl_bss!(Open, ssid: b"bar".to_vec())
                },
            },
            &sme_inspect,
        );
        sched.on_mlme_scan_result(
            fidl_mlme::ScanResult {
                txn_id,
                bss: fidl_internal::BssDescription {
                    bssid: [3; 6],
                    ..fake_fidl_bss!(Open, ssid: b"qux".to_vec())
                },
            },
            &sme_inspect,
        );
        let (result, req) = sched.on_mlme_scan_end(
            fidl_mlme::ScanEnd { txn_id, code: fidl_mlme::ScanResultCode::Success },
            &sme_inspect,
        );
        assert!(req.is_none());
        let (tokens, result) = assert_variant!(result,
            ScanResult::DiscoveryFinished { tokens, result } => (tokens, result),
            "expected discovery scan to be completed"
        );
        assert_eq!(vec![10], tokens);
        let mut ssid_list = result
            .expect("expected a successful scan result")
            .into_iter()
            .map(|bss| bss.ssid().to_vec())
            .collect::<Vec<_>>();
        ssid_list.sort();
        assert_eq!(vec![b"foo".to_vec(), b"qux".to_vec()], ssid_list);
    }

    #[test]
    fn discovery_scan_deduplicate_bssid() {
        let mut sched = create_sched();
        let (_inspector, sme_inspect) = sme_inspect();
        let req = sched
            .enqueue_scan_to_discover(passive_discovery_scan(10))
            .expect("expected a ScanRequest");
        let txn_id = req.txn_id;
        sched.on_mlme_scan_result(
            fidl_mlme::ScanResult {
                txn_id,
                bss: fidl_internal::BssDescription {
                    bssid: [1; 6],
                    ..fake_fidl_bss!(Open, ssid: b"bar".to_vec())
                },
            },
            &sme_inspect,
        );
        // A new scan result with the same BSSID replaces the previous result.
        sched.on_mlme_scan_result(
            fidl_mlme::ScanResult {
                txn_id,
                bss: fidl_internal::BssDescription {
                    bssid: [1; 6],
                    ..fake_fidl_bss!(Open, ssid: b"baz".to_vec())
                },
            },
            &sme_inspect,
        );
        let (result, req) = sched.on_mlme_scan_end(
            fidl_mlme::ScanEnd { txn_id, code: fidl_mlme::ScanResultCode::Success },
            &sme_inspect,
        );
        assert!(req.is_none());
        let (tokens, result) = assert_variant!(result,
            ScanResult::DiscoveryFinished { tokens, result } => (tokens, result),
            "expected discovery scan to be completed"
        );
        assert_eq!(vec![10], tokens);
        let mut ssid_list = result
            .expect("expected a successful scan result")
            .into_iter()
            .map(|bss| bss.ssid().to_vec())
            .collect::<Vec<_>>();
        ssid_list.sort();
        assert_eq!(vec![b"baz".to_vec()], ssid_list);
    }

    #[test]
    fn discovery_scan_merge_ies() {
        let mut sched = create_sched();
        let (_inspector, sme_inspect) = sme_inspect();
        let req = sched
            .enqueue_scan_to_discover(passive_discovery_scan(10))
            .expect("expected a ScanRequest");
        let txn_id = req.txn_id;

        let mut bss = fake_fidl_bss!(Open, ssid: b"ssid".to_vec());
        // Add an extra IE so we can distinguish this result.
        let ie_marker1 = &[0xdd, 0x07, 0xee, 0xee, 0xee, 0xee, 0xee, 0xee, 0xee];
        bss.ies.extend_from_slice(ie_marker1);
        sched.on_mlme_scan_result(fidl_mlme::ScanResult { txn_id, bss }, &sme_inspect);

        let mut bss = fake_fidl_bss!(Open, ssid: b"ssid".to_vec());
        // Add an extra IE so we can distinguish this result.
        let ie_marker2 = &[0xdd, 0x07, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff];
        bss.ies.extend_from_slice(ie_marker2);
        sched.on_mlme_scan_result(fidl_mlme::ScanResult { txn_id, bss }, &sme_inspect);
        let (result, req) = sched.on_mlme_scan_end(
            fidl_mlme::ScanEnd { txn_id, code: fidl_mlme::ScanResultCode::Success },
            &sme_inspect,
        );
        assert!(req.is_none());
        let (tokens, result) = assert_variant!(result,
            ScanResult::DiscoveryFinished { tokens, result } => (tokens, result),
            "expected discovery scan to be completed"
        );
        assert_eq!(vec![10], tokens);

        let result = result.expect("expected a successful scan result");
        assert_eq!(result.len(), 1);
        // Verify that both IEs are processed.
        assert!(slice_contains(&result[0].ies[..], ie_marker1));
        assert!(slice_contains(&result[0].ies[..], ie_marker2));
    }

    fn slice_contains(slice: &[u8], subslice: &[u8]) -> bool {
        let slice_str = itertools::join(slice, ",");
        let subslice_str = itertools::join(subslice, ",");
        slice_str.contains(&subslice_str)
    }

    #[test]
    fn test_passive_discovery_scan_args() {
        let mut sched = create_sched();
        let req = sched
            .enqueue_scan_to_discover(passive_discovery_scan(10))
            .expect("expected a ScanRequest");

        assert_eq!(req.scan_type, fidl_mlme::ScanTypes::Passive);
        assert_eq!(req.ssid, Vec::<u8>::new());
    }

    #[test]
    fn test_active_discovery_scan_args_empty() {
        let device_info = device_info_with_channel(vec![1, 36, 165]);
        let mut sched: ScanScheduler<i32> = ScanScheduler::new(Arc::new(device_info));
        let scan_cmd = DiscoveryScan::new(
            10,
            fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
                ssids: vec![],
                channels: vec![],
            }),
        );
        let req = sched.enqueue_scan_to_discover(scan_cmd).expect("expected a ScanRequest");

        assert_eq!(req.scan_type, fidl_mlme::ScanTypes::Active);
        assert_eq!(req.ssid, Vec::<u8>::new());
        assert_eq!(req.ssid_list, None);
        assert_eq!(req.channel_list, Some(vec![36, 165, 1]));
    }

    #[test]
    fn test_active_discovery_scan_args_filled() {
        let device_info = device_info_with_channel(vec![1, 36, 165]);
        let mut sched: ScanScheduler<i32> = ScanScheduler::new(Arc::new(device_info));
        let ssid1 = "ssid1".as_bytes().to_vec();
        let ssid2 = "ssid2".as_bytes().to_vec();
        let scan_cmd = DiscoveryScan::new(
            10,
            fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
                ssids: vec![ssid1.clone(), ssid2.clone()],
                channels: vec![1, 20, 100],
            }),
        );
        let req = sched.enqueue_scan_to_discover(scan_cmd).expect("expected a ScanRequest");

        assert_eq!(req.scan_type, fidl_mlme::ScanTypes::Active);
        assert_eq!(req.ssid, Vec::<u8>::new());
        assert_eq!(req.ssid_list, Some(vec![ssid1, ssid2]));
        assert_eq!(req.channel_list, Some(vec![1]));
    }

    #[test]
    fn test_discovery_scans_dedupe_single_group() {
        let mut sched = create_sched();
        let (_inspector, sme_inspect) = sme_inspect();

        // Post one scan command, expect a message to MLME
        let mlme_req = sched
            .enqueue_scan_to_discover(passive_discovery_scan(10))
            .expect("expected a ScanRequest");
        let txn_id = mlme_req.txn_id;

        // Report a scan result
        sched.on_mlme_scan_result(
            fidl_mlme::ScanResult {
                txn_id,
                bss: fidl_internal::BssDescription {
                    bssid: [1; 6],
                    ..fake_fidl_bss!(Open, ssid: b"foo".to_vec())
                },
            },
            &sme_inspect,
        );

        // Post another command. It should not issue another request to the MLME since
        // there is already an on-going one
        assert!(sched.enqueue_scan_to_discover(passive_discovery_scan(20)).is_none());

        // Report another scan result and the end of the scan transaction
        sched.on_mlme_scan_result(
            fidl_mlme::ScanResult {
                txn_id,
                bss: fidl_internal::BssDescription {
                    bssid: [2; 6],
                    ..fake_fidl_bss!(Open, ssid: b"bar".to_vec())
                },
            },
            &sme_inspect,
        );
        let (result, req) = sched.on_mlme_scan_end(
            fidl_mlme::ScanEnd { txn_id, code: fidl_mlme::ScanResultCode::Success },
            &sme_inspect,
        );

        // We don't expect another request to the MLME
        assert!(req.is_none());

        // Expect a discovery result with both tokens and both SSIDs
        assert_discovery_scan_result(result, vec![10, 20], vec![b"bar".to_vec(), b"foo".to_vec()]);
    }

    #[test]
    fn test_discovery_scans_dedupe_multiple_groups() {
        let mut sched = create_sched();
        let (_inspector, sme_inspect) = sme_inspect();

        // Post a passive scan command, expect a message to MLME
        let mlme_req = sched
            .enqueue_scan_to_discover(passive_discovery_scan(10))
            .expect("expected a ScanRequest");
        let txn_id = mlme_req.txn_id;

        // Post an active scan command, which should be enqueued until the previous one finishes
        let scan_cmd = DiscoveryScan::new(
            20,
            fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
                ssids: vec![],
                channels: vec![],
            }),
        );
        assert!(sched.enqueue_scan_to_discover(scan_cmd).is_none());

        // Post a passive scan command. It should be merged with the ongoing one and so should not
        // issue another request to MLME
        assert!(sched.enqueue_scan_to_discover(passive_discovery_scan(30)).is_none());

        // Post an active scan command. It should be merged with the active scan command that's
        // still enqueued, and so should not issue another request to MLME
        let scan_cmd = DiscoveryScan::new(
            40,
            fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
                ssids: vec![],
                channels: vec![],
            }),
        );
        assert!(sched.enqueue_scan_to_discover(scan_cmd).is_none());

        // Report scan result and scan end
        sched.on_mlme_scan_result(
            fidl_mlme::ScanResult {
                txn_id,
                bss: fidl_internal::BssDescription {
                    bssid: [1; 6],
                    ..fake_fidl_bss!(Open, ssid: b"foo".to_vec())
                },
            },
            &sme_inspect,
        );
        let (result, mlme_req) = sched.on_mlme_scan_end(
            fidl_mlme::ScanEnd { txn_id, code: fidl_mlme::ScanResultCode::Success },
            &sme_inspect,
        );

        // Expect discovery result with 1st and 3rd tokens
        assert_discovery_scan_result(result, vec![10, 30], vec![b"foo".to_vec()]);

        // Next mlme_req should be an active scan request
        assert!(mlme_req.is_some());
        let mlme_req = mlme_req.unwrap();
        assert_eq!(mlme_req.scan_type, fidl_mlme::ScanTypes::Active);
        let txn_id = mlme_req.txn_id;

        // Report scan result and scan end
        sched.on_mlme_scan_result(
            fidl_mlme::ScanResult {
                txn_id,
                bss: fidl_internal::BssDescription {
                    bssid: [2; 6],
                    ..fake_fidl_bss!(Open, ssid: b"bar".to_vec())
                },
            },
            &sme_inspect,
        );
        let (result, mlme_req) = sched.on_mlme_scan_end(
            fidl_mlme::ScanEnd { txn_id, code: fidl_mlme::ScanResultCode::Success },
            &sme_inspect,
        );

        // Expect discovery result with 2nd and 3rd tokens
        assert_discovery_scan_result(result, vec![20, 40], vec![b"bar".to_vec()]);

        // We don't expect another request to the MLME
        assert!(mlme_req.is_none());
    }

    fn assert_discovery_scan_result(
        result: ScanResult<i32>,
        expected_tokens: Vec<i32>,
        expected_ssids: Vec<Vec<u8>>,
    ) {
        let (tokens, result) = assert_variant!(result,
            ScanResult::DiscoveryFinished { tokens, result } => (tokens, result),
            "expected discovery scan to be completed"
        );
        assert_eq!(tokens, expected_tokens);
        let mut ssid_list = result
            .expect("expected a successful scan result")
            .into_iter()
            .map(|bss| bss.ssid().to_vec())
            .collect::<Vec<_>>();
        ssid_list.sort();
        assert_eq!(ssid_list, expected_ssids);
    }

    fn create_sched() -> ScanScheduler<i32> {
        ScanScheduler::new(Arc::new(test_utils::fake_device_info(CLIENT_ADDR)))
    }

    fn device_info_with_channel(channels: Vec<u8>) -> DeviceInfo {
        DeviceInfo {
            bands: vec![fidl_mlme::BandCapabilities {
                channels,
                ..test_utils::fake_5ghz_band_capabilities()
            }],
            ..test_utils::fake_device_info(CLIENT_ADDR)
        }
    }

    fn sme_inspect() -> (Inspector, Arc<inspect::SmeTree>) {
        let inspector = Inspector::new();
        let hasher = WlanHasher::new([88, 77, 66, 55, 44, 33, 22, 11]);
        let sme_inspect = Arc::new(inspect::SmeTree::new(inspector.root(), hasher));
        (inspector, sme_inspect)
    }
}
