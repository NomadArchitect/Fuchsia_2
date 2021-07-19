// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{device::IfaceMap, service::get_iface_stats},
    fidl_fuchsia_wlan_stats as fidl_stats,
    fuchsia_inspect::{
        ArrayProperty, InspectType, Inspector, Node, NumericProperty, Property, UintProperty,
    },
    fuchsia_inspect_contrib::{
        auto_persist::{self, AutoPersist},
        nodes::BoundedListNode,
    },
    futures::FutureExt,
    log::error,
    parking_lot::Mutex,
    paste, rand,
    std::{
        collections::{HashMap, HashSet},
        sync::Arc,
    },
    wlan_common::hasher::WlanHasher,
    wlan_inspect::{IfaceTreeHolder, IfacesTrees},
};

pub const VMO_SIZE_BYTES: usize = 1000 * 1024;
const MAX_DEAD_IFACE_NODES: usize = 2;

/// Limit was chosen arbitrary. 20 events seem enough to log multiple PHY/iface create or
/// destroy events.
const DEVICE_EVENTS_LIMIT: usize = 20;

/// Limit for these stat events were chosen arbitrarily. At minimum, we just need to have
/// enough so they can be used to trigger bug reports.
const CONNECT_EVENTS_LIMIT: usize = 7;
const DISCONNECT_EVENTS_LIMIT: usize = 7;
const SCAN_EVENTS_LIMIT: usize = 20;
const SCAN_FAILURE_EVENTS_LIMIT: usize = 5;
const COUNTERS_EVENTS_LIMIT: usize = 60;

pub struct WlanstackTree {
    /// Root of the tree
    pub inspector: Inspector,
    /// Key used to hash privacy-sensitive values in the tree
    pub hasher: WlanHasher,
    /// "client_stats" node
    pub client_stats: ClientStatsNode,
    /// "device_events" subtree
    pub device_events: Mutex<AutoPersist<BoundedListNode>>,
    /// "iface-<n>" subtrees, where n is the iface ID.
    ifaces_trees: Mutex<IfacesTrees>,
    /// "active_iface" property, what's the currently active iface. This assumes only one iface
    /// is active at a time
    latest_active_client_iface: Mutex<Option<UintProperty>>,

    // Keep track of removed ifaces. Not an Inspect node/property.
    removed_ifaces: Arc<Mutex<HashSet<u16>>>,
}

impl WlanstackTree {
    pub fn new(
        inspector: Inspector,
        persistence_req_sender: auto_persist::PersistenceReqSender,
    ) -> Self {
        let client_stats = inspector.root().create_child("client_stats");
        let device_events = inspector.root().create_child("device_events");
        let device_events = AutoPersist::new(
            BoundedListNode::new(device_events, DEVICE_EVENTS_LIMIT),
            "wlanstack-device-events",
            persistence_req_sender.clone(),
        );
        let ifaces_trees = IfacesTrees::new(MAX_DEAD_IFACE_NODES);
        Self {
            inspector,
            // According to doc, `rand::random` uses ThreadRng, which is cryptographically secure:
            // https://docs.rs/rand/0.5.0/rand/rngs/struct.ThreadRng.html
            hasher: WlanHasher::new(rand::random::<u64>().to_le_bytes()),
            client_stats: ClientStatsNode::new(client_stats, persistence_req_sender),
            device_events: Mutex::new(device_events),
            ifaces_trees: Mutex::new(ifaces_trees),
            latest_active_client_iface: Mutex::new(None),
            removed_ifaces: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub fn create_iface_child(&self, iface_id: u16) -> Arc<IfaceTreeHolder> {
        self.removed_ifaces.lock().remove(&iface_id);
        self.ifaces_trees.lock().create_iface_child(self.inspector.root(), iface_id)
    }

    pub fn notify_iface_removed(&self, iface_id: u16) {
        self.ifaces_trees.lock().notify_iface_removed(iface_id);
        self.removed_ifaces.lock().insert(iface_id);
    }

    pub fn mark_active_client_iface(
        &self,
        iface_id: u16,
        iface_map: Arc<IfaceMap>,
        iface_tree_holder: Arc<IfaceTreeHolder>,
    ) {
        self.latest_active_client_iface
            .lock()
            .get_or_insert_with(|| {
                self.inspector.root().create_uint("latest_active_client_iface", iface_id as u64)
            })
            .set(iface_id as u64);

        // Note: "histograms" is a bit of a misnomer since this node also contains packet counters.
        //       However, as some children of this node are queried by another component, we can't
        //       just change its name.
        // TODO(fxbug.dev/65093) - Rename this node by following migration steps.
        let removed_ifaces = Arc::clone(&self.removed_ifaces);
        let histograms = iface_tree_holder.node.create_lazy_child("histograms", move || {
            {
                let iface_map = iface_map.clone();
                let removed_ifaces = Arc::clone(&removed_ifaces);
                async move {
                    let inspector = Inspector::new();
                    // Skip retrieving histograms for this iface if it's already removed because
                    // the call would not succeed anyway.
                    if removed_ifaces.lock().contains(&iface_id) {
                        return Ok(inspector);
                    }
                    match get_iface_stats(&iface_map, iface_id).await {
                        Ok(stats) => {
                            if let Some(mlme_stats) = &stats.lock().mlme_stats {
                                if let fidl_stats::MlmeStats::ClientMlmeStats(mlme_stats) =
                                    mlme_stats.as_ref()
                                {
                                    // TODO(fxbug.dev/64905): Fix the fields of PacketCounter
                                    let counters = inspector.root().create_child("packet_counters");
                                    counters.record_uint("rx_total", mlme_stats.rx_frame.in_.count);
                                    counters.record_uint("rx_drop", mlme_stats.rx_frame.drop.count);
                                    counters.record_uint("tx_total", mlme_stats.tx_frame.in_.count);
                                    counters.record_uint("tx_drop", mlme_stats.tx_frame.drop.count);

                                    let mut histograms = HistogramsSubtrees::new();
                                    histograms.log_per_antenna_snr_histograms(
                                        &mlme_stats.snr_histograms[..],
                                        &inspector,
                                    );
                                    histograms.log_per_antenna_rx_rate_histograms(
                                        &mlme_stats.rx_rate_index_histograms[..],
                                        &inspector,
                                    );
                                    histograms.log_per_antenna_noise_floor_histograms(
                                        &mlme_stats.noise_floor_histograms[..],
                                        &inspector,
                                    );
                                    histograms.log_per_antenna_rssi_histograms(
                                        &mlme_stats.rssi_histograms[..],
                                        &inspector,
                                    );

                                    inspector.root().record(counters);
                                    inspector.root().record(histograms);
                                }
                            }
                        }
                        Err(e) => error!(
                            "iface {} - unable to retrieve signal histograms for Inspect: {}",
                            iface_id, e
                        ),
                    }
                    Ok(inspector)
                }
            }
            .boxed()
        });
        iface_tree_holder.add_iface_subtree(Arc::new(histograms));
    }

    pub fn unmark_active_client_iface(&self, iface_id: u16) {
        let mut active_iface = self.latest_active_client_iface.lock();
        if let Some(property) = active_iface.as_ref() {
            if let Ok(id) = property.get() {
                if id == iface_id as u64 {
                    active_iface.take();
                }
            }
        }
    }
}

pub struct ClientStatsNode {
    _node: Node,
    pub connect: Mutex<AutoPersist<BoundedListNode>>,
    pub disconnect: Mutex<AutoPersist<BoundedListNode>>,
    /// Tracked so we know periods of time when there may be spotty data transfer.
    pub scan: Mutex<AutoPersist<BoundedListNode>>,
    pub scan_failures: Mutex<AutoPersist<BoundedListNode>>,
    pub counters: Mutex<BoundedListNode>,
}

impl ClientStatsNode {
    fn new(node: Node, persistence_req_sender: auto_persist::PersistenceReqSender) -> Self {
        let connect = node.create_child("connect");
        let disconnect = node.create_child("disconnect");
        let scan = node.create_child("scan");
        let scan_failures = node.create_child("scan_failures");
        let counters = node.create_child("counters");
        Self {
            _node: node,
            connect: Mutex::new(AutoPersist::new(
                BoundedListNode::new(connect, CONNECT_EVENTS_LIMIT),
                "wlanstack-connect-events",
                persistence_req_sender.clone(),
            )),
            disconnect: Mutex::new(AutoPersist::new(
                BoundedListNode::new(disconnect, DISCONNECT_EVENTS_LIMIT),
                "wlanstack-disconnect-events",
                persistence_req_sender.clone(),
            )),
            scan: Mutex::new(AutoPersist::new(
                BoundedListNode::new(scan, SCAN_EVENTS_LIMIT),
                "wlanstack-scan-events",
                persistence_req_sender.clone(),
            )),
            scan_failures: Mutex::new(AutoPersist::new(
                BoundedListNode::new(scan_failures, SCAN_FAILURE_EVENTS_LIMIT),
                "wlanstack-scan-failure-events",
                persistence_req_sender.clone(),
            )),
            counters: Mutex::new(BoundedListNode::new(counters, COUNTERS_EVENTS_LIMIT)),
        }
    }
}

struct HistogramsSubtrees {
    antenna_nodes: HashMap<fidl_stats::AntennaId, Node>,
}

impl InspectType for HistogramsSubtrees {}

macro_rules! fn_log_per_antenna_histograms {
    ($name:ident, $field:ident, $histogram_ty:ty, $sample:ident => $sample_index_expr:expr) => {
        paste::paste! {
            pub fn [<log_per_antenna_ $name _histograms>](
                &mut self,
                histograms: &[$histogram_ty],
                inspector: &Inspector
            ) {
                for histogram in histograms {
                    // Only antenna histograms are logged (STATION scope histograms are discarded)
                    let antenna_id = match &histogram.antenna_id {
                        Some(id) => **id,
                        None => continue,
                    };
                    let antenna_node = self.create_or_get_antenna_node(antenna_id, inspector);

                    let samples = &histogram.$field;
                    let histogram_prop_name = concat!(stringify!($name), "_histogram");
                    let histogram_prop =
                        antenna_node.create_int_array(histogram_prop_name, samples.len() * 2);
                    for (i, sample) in samples.iter().enumerate() {
                        let $sample = sample;
                        histogram_prop.set(i * 2, $sample_index_expr);
                        histogram_prop.set(i * 2 + 1, $sample.num_samples as i64);
                    }

                    let invalid_samples_name = concat!(stringify!($name), "_invalid_samples");
                    let invalid_samples =
                        antenna_node.create_uint(invalid_samples_name, histogram.invalid_samples);

                    antenna_node.record(histogram_prop);
                    antenna_node.record(invalid_samples);
                }
            }
        }
    };
}

impl HistogramsSubtrees {
    pub fn new() -> Self {
        Self { antenna_nodes: HashMap::new() }
    }

    // fn log_per_antenna_snr_histograms
    fn_log_per_antenna_histograms!(snr, snr_samples, fidl_stats::SnrHistogram,
                                   sample => sample.bucket_index as i64);
    // fn log_per_antenna_rx_rate_histograms
    fn_log_per_antenna_histograms!(rx_rate, rx_rate_index_samples,
                                   fidl_stats::RxRateIndexHistogram,
                                   sample => sample.bucket_index as i64);
    // fn log_per_antenna_noise_floor_histograms
    fn_log_per_antenna_histograms!(noise_floor, noise_floor_samples,
                                   fidl_stats::NoiseFloorHistogram,
                                   sample => sample.bucket_index as i64 - 255);
    // fn log_per_antenna_rssi_histograms
    fn_log_per_antenna_histograms!(rssi, rssi_samples, fidl_stats::RssiHistogram,
                                   sample => sample.bucket_index as i64 - 255);

    fn create_or_get_antenna_node(
        &mut self,
        antenna_id: fidl_stats::AntennaId,
        inspector: &Inspector,
    ) -> &mut Node {
        self.antenna_nodes.entry(antenna_id).or_insert_with(|| {
            let freq = match antenna_id.freq {
                fidl_stats::AntennaFreq::Antenna2G => "2Ghz",
                fidl_stats::AntennaFreq::Antenna5G => "5Ghz",
            };
            let node =
                inspector.root().create_child(format!("antenna{}_{}", antenna_id.index, freq));
            node.record_uint("antenna_index", antenna_id.index as u64);
            node.record_string("antenna_freq", freq);
            node
        })
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, crate::test_helper, fuchsia_inspect::assert_data_tree,
        wlan_common::assert_variant,
    };

    #[test]
    fn test_mark_unmark_active_client_iface_simple() {
        let (inspect_tree, _persistence_stream) = test_helper::fake_inspect_tree();
        let iface_map = IfaceMap::new();
        let iface_map = Arc::new(iface_map);

        inspect_tree.mark_active_client_iface(0, iface_map, inspect_tree.create_iface_child(0));
        assert_active_client_iface_eq(&inspect_tree, 0);
        inspect_tree.unmark_active_client_iface(0);
        assert_variant!(inspect_tree.latest_active_client_iface.lock().as_ref(), None);
    }

    #[test]
    fn test_mark_unmark_active_client_iface_interleave() {
        let (inspect_tree, _persistence_stream) = test_helper::fake_inspect_tree();
        let iface_map = IfaceMap::new();
        let iface_map = Arc::new(iface_map);

        inspect_tree.mark_active_client_iface(
            0,
            iface_map.clone(),
            inspect_tree.create_iface_child(0),
        );
        assert_active_client_iface_eq(&inspect_tree, 0);
        // We don't support two concurrent active client iface in practice. This test is
        // just us being paranoid about the unmark call coming later than the call to
        // mark the new iface.
        inspect_tree.mark_active_client_iface(
            1,
            iface_map.clone(),
            inspect_tree.create_iface_child(1),
        );
        assert_active_client_iface_eq(&inspect_tree, 1);

        // Stale unmark call should have no effect on the tree
        inspect_tree.unmark_active_client_iface(0);
        assert_active_client_iface_eq(&inspect_tree, 1);

        inspect_tree.unmark_active_client_iface(1);
        assert_variant!(inspect_tree.latest_active_client_iface.lock().as_ref(), None);
    }

    #[test]
    fn test_log_signal_histograms() {
        let snr_histograms = vec![fidl_stats::SnrHistogram {
            hist_scope: fidl_stats::HistScope::PerAntenna,
            antenna_id: Some(Box::new(fidl_stats::AntennaId {
                freq: fidl_stats::AntennaFreq::Antenna2G,
                index: 0,
            })),
            snr_samples: vec![fidl_stats::HistBucket { bucket_index: 30, num_samples: 999 }],
            invalid_samples: 11,
        }];
        let rx_rate_histograms = vec![
            fidl_stats::RxRateIndexHistogram {
                hist_scope: fidl_stats::HistScope::Station,
                antenna_id: None,
                rx_rate_index_samples: vec![fidl_stats::HistBucket {
                    bucket_index: 99,
                    num_samples: 1400,
                }],
                invalid_samples: 22,
            },
            fidl_stats::RxRateIndexHistogram {
                hist_scope: fidl_stats::HistScope::PerAntenna,
                antenna_id: Some(Box::new(fidl_stats::AntennaId {
                    freq: fidl_stats::AntennaFreq::Antenna5G,
                    index: 1,
                })),
                rx_rate_index_samples: vec![fidl_stats::HistBucket {
                    bucket_index: 100,
                    num_samples: 1500,
                }],
                invalid_samples: 33,
            },
        ];
        let noise_floor_histograms = vec![fidl_stats::NoiseFloorHistogram {
            hist_scope: fidl_stats::HistScope::PerAntenna,
            antenna_id: Some(Box::new(fidl_stats::AntennaId {
                freq: fidl_stats::AntennaFreq::Antenna2G,
                index: 0,
            })),
            noise_floor_samples: vec![fidl_stats::HistBucket {
                bucket_index: 200,
                num_samples: 999,
            }],
            invalid_samples: 44,
        }];
        let rssi_histograms = vec![fidl_stats::RssiHistogram {
            hist_scope: fidl_stats::HistScope::PerAntenna,
            antenna_id: Some(Box::new(fidl_stats::AntennaId {
                freq: fidl_stats::AntennaFreq::Antenna2G,
                index: 0,
            })),
            rssi_samples: vec![fidl_stats::HistBucket { bucket_index: 230, num_samples: 999 }],
            invalid_samples: 55,
        }];

        let inspector = Inspector::new();
        let mut histograms_subtrees = HistogramsSubtrees::new();
        histograms_subtrees.log_per_antenna_snr_histograms(&snr_histograms[..], &inspector);
        histograms_subtrees.log_per_antenna_rx_rate_histograms(&rx_rate_histograms[..], &inspector);
        histograms_subtrees
            .log_per_antenna_noise_floor_histograms(&noise_floor_histograms[..], &inspector);
        histograms_subtrees.log_per_antenna_rssi_histograms(&rssi_histograms[..], &inspector);

        assert_data_tree!(inspector, root: {
            antenna0_2Ghz: {
                antenna_index: 0u64,
                antenna_freq: "2Ghz",
                snr_histogram: vec![30i64, 999],
                snr_invalid_samples: 11u64,
                noise_floor_histogram: vec![-55i64, 999],
                noise_floor_invalid_samples: 44u64,
                rssi_histogram: vec![-25i64, 999],
                rssi_invalid_samples: 55u64,
            },
            antenna1_5Ghz: {
                antenna_index: 1u64,
                antenna_freq: "5Ghz",
                rx_rate_histogram: vec![100i64, 1500],
                rx_rate_invalid_samples: 33u64,
            },
        });
    }

    fn assert_active_client_iface_eq(inspect_tree: &WlanstackTree, iface_id: u64) {
        assert_variant!(inspect_tree.latest_active_client_iface.lock().as_ref(), Some(property) => {
            assert_variant!(property.get(), Ok(id) => {
                assert_eq!(id, iface_id);
            });
        });
    }
}
