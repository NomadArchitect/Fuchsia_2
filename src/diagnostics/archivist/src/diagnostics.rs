// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::archive::EventFileGroupStatsMap,
    anyhow::Error,
    fuchsia_component::server::{ServiceFs, ServiceObjTrait},
    fuchsia_inspect::{
        component, health::Reporter, ExponentialHistogramParams, HistogramProperty, Inspector,
        LinearHistogramParams, Node, NumericProperty, UintExponentialHistogramProperty,
        UintLinearHistogramProperty, UintProperty,
    },
    fuchsia_zircon::{self as zx, Duration},
    futures::FutureExt,
    lazy_static::lazy_static,
    parking_lot::Mutex,
    std::collections::BTreeMap,
    std::sync::Arc,
};

lazy_static! {
    static ref GROUPS: Arc<Mutex<Groups>> = Arc::new(Mutex::new(Groups::new(
        component::inspector().root().create_child("archived_events")
    )));
}

enum GroupData {
    Node(Node),
    Count(UintProperty),
}

struct Groups {
    node: Node,
    children: Vec<GroupData>,
}

impl Groups {
    fn new(node: Node) -> Self {
        Groups { node, children: vec![] }
    }

    fn replace(&mut self, stats: &EventFileGroupStatsMap) {
        self.children.clear();
        for (name, stat) in stats {
            let node = self.node.create_child(name);
            let files = node.create_uint("file_count", stat.file_count as u64);
            let size = node.create_uint("size_in_bytes", stat.size);

            self.children.push(GroupData::Node(node));
            self.children.push(GroupData::Count(files));
            self.children.push(GroupData::Count(size));
        }
    }
}

pub fn init() {
    component::health().set_starting_up();
}

pub fn root() -> &'static Node {
    component::inspector().root()
}

pub fn serve(service_fs: &mut ServiceFs<impl ServiceObjTrait>) -> Result<(), Error> {
    component::inspector().root().record_lazy_child("inspect_stats", move || {
        async move {
            let inspector = Inspector::new();
            if let Some(stats) = component::inspector().stats() {
                inspector.root().record_uint("maximum_size", stats.maximum_size as u64);
                inspector.root().record_uint("current_size", stats.maximum_size as u64);
                inspector
                    .root()
                    .record_uint("total_dynamic_children", stats.total_dynamic_children as u64);
            }
            Ok(inspector)
        }
        .boxed()
    });
    inspect_runtime::serve(component::inspector(), service_fs)?;
    Ok(())
}

pub(crate) fn set_group_stats(stats: &EventFileGroupStatsMap) {
    GROUPS.lock().replace(stats);
}

pub struct AccessorStats {
    /// Inspect node for tracking usage/health metrics of diagnostics platform.
    pub archive_accessor_node: Node,

    /// Metrics aggregated across all client connections.
    pub global_stats: Arc<GlobalAccessorStats>,

    /// Global stats tracking the usages of StreamDiagnostics for
    /// exfiltrating inspect data.
    pub global_inspect_stats: Arc<GlobalConnectionStats>,

    /// Global stats tracking the usages of StreamDiagnostics for
    /// exfiltrating lifecycle data.
    pub global_lifecycle_stats: Arc<GlobalConnectionStats>,

    /// Global stats tracking the usages of StreamDiagnostics for
    /// exfiltrating logs.
    pub global_logs_stats: Arc<GlobalConnectionStats>,
}

pub struct GlobalAccessorStats {
    /// Property tracking number of opening connections to any archive_accessor instance.
    pub archive_accessor_connections_opened: UintProperty,
    /// Property tracking number of closing connections to any archive_accessor instance.
    pub archive_accessor_connections_closed: UintProperty,
    /// Number of requests to a single ArchiveAccessor to StreamDiagnostics, starting a
    /// new inspect ReaderServer.
    pub stream_diagnostics_requests: UintProperty,
}

impl AccessorStats {
    pub fn new(mut archive_accessor_node: Node) -> Self {
        let archive_accessor_connections_opened =
            archive_accessor_node.create_uint("archive_accessor_connections_opened", 0);
        let archive_accessor_connections_closed =
            archive_accessor_node.create_uint("archive_accessor_connections_closed", 0);

        let stream_diagnostics_requests =
            archive_accessor_node.create_uint("stream_diagnostics_requests", 0);

        let global_inspect_stats =
            Arc::new(GlobalConnectionStats::for_inspect(&mut archive_accessor_node));

        let global_lifecycle_stats =
            Arc::new(GlobalConnectionStats::for_lifecycle(&mut archive_accessor_node));

        let global_logs_stats =
            Arc::new(GlobalConnectionStats::for_logs(&mut archive_accessor_node));

        AccessorStats {
            archive_accessor_node,
            global_stats: Arc::new(GlobalAccessorStats {
                archive_accessor_connections_opened,
                archive_accessor_connections_closed,
                stream_diagnostics_requests,
            }),
            global_inspect_stats,
            global_lifecycle_stats,
            global_logs_stats,
        }
    }
}

// Exponential histograms for time in microseconds contains power-of-two intervals
const EXPONENTIAL_HISTOGRAM_USEC_FLOOR: u64 = 0;
const EXPONENTIAL_HISTOGRAM_USEC_STEP: u64 = 1;
const EXPONENTIAL_HISTOGRAM_USEC_MULTIPLIER: u64 = 2;
const EXPONENTIAL_HISTOGRAM_USEC_BUCKETS: u64 = 26;

// Linear histogram for max snapshot size in bytes requested by clients.
// Divide configs into 10kb buckets, from 0mb to 1mb.
const LINEAR_HISTOGRAM_BYTES_FLOOR: u64 = 0;
const LINEAR_HISTOGRAM_BYTES_STEP: u64 = 10000;
const LINEAR_HISTOGRAM_BYTES_BUCKETS: u64 = 100;

// Linear histogram tracking percent of schemas truncated for a given snapshot.
// Divide configs into 5% buckets, from 0% to 100%.
const LINEAR_HISTOGRAM_TRUNCATION_PERCENT_FLOOR: u64 = 0;
const LINEAR_HISTOGRAM_TRUNCATION_PERCENT_STEP: u64 = 5;
const LINEAR_HISTOGRAM_TRUNCATION_PERCENT_BUCKETS: u64 = 20;

pub struct GlobalConnectionStats {
    /// The name of the diagnostics source being tracked by this struct.
    diagnostics_source: &'static str,
    /// Weak clone of the node that stores stats, used for on-demand population.
    connection_node: Node,
    /// Number of DiagnosticsServers created in response to an StreamDiagnostics
    /// client request.
    reader_servers_constructed: UintProperty,
    /// Number of DiagnosticsServers destroyed in response to falling out of scope.
    reader_servers_destroyed: UintProperty,
    /// Property tracking number of opening connections to any batch iterator instance.
    batch_iterator_connections_opened: UintProperty,
    /// Property tracking number of closing connections to any batch iterator instance.
    batch_iterator_connections_closed: UintProperty,
    /// Property tracking number of times a future to retrieve diagnostics data for a component
    /// timed out.
    component_timeouts_count: UintProperty,
    /// Number of times "GetNext" was called
    batch_iterator_get_next_requests: UintProperty,
    /// Number of times a "GetNext" response was sent
    batch_iterator_get_next_responses: UintProperty,
    /// Number of times "GetNext" resulted in an error
    batch_iterator_get_next_errors: UintProperty,
    /// Number of items returned in batches from "GetNext"
    batch_iterator_get_next_result_count: UintProperty,
    /// Number of items returned in batches from "GetNext" that contained errors
    batch_iterator_get_next_result_errors: UintProperty,
    /// Number of times a diagnostics schema had to be truncated because it would otherwise
    /// cause a component to exceed its configured size budget.
    schema_truncation_count: UintProperty,

    /// Histogram of processing times for overall "GetNext" requests.
    batch_iterator_get_next_time_usec: UintExponentialHistogramProperty,
    /// Optional histogram of processing times for individual components in GetNext
    component_time_usec: Mutex<Option<UintExponentialHistogramProperty>>,
    /// Histogram of max aggregated snapshot sizes for overall Snapshot requests.
    max_snapshot_sizes_bytes: UintLinearHistogramProperty,
    /// Percentage of schemas in a single snapshot that got truncated.
    snapshot_schema_truncation_percentage: UintLinearHistogramProperty,
    /// Longest processing times for individual components, with timestamps.
    processing_time_tracker: Mutex<Option<ProcessingTimeTracker>>,
}

impl GlobalConnectionStats {
    // TODO(fxbug.dev/54442): Consider encoding prefix as node name and represent the same
    //              named properties under different nodes for each diagnostics source.
    pub fn for_inspect(archive_accessor_node: &mut Node) -> Self {
        GlobalConnectionStats::new(archive_accessor_node, "inspect")
    }

    // TODO(fxbug.dev/54442): Consider encoding prefix as node name and represent the same
    //              named properties under different nodes for each diagnostics source.
    pub fn for_lifecycle(archive_accessor_node: &mut Node) -> Self {
        GlobalConnectionStats::new(archive_accessor_node, "lifecycle")
    }

    // TODO(fxbug.dev/54442): Consider encoding prefix as node name and represent the same
    //              named properties under different nodes for each diagnostics source.
    pub fn for_logs(archive_accessor_node: &mut Node) -> Self {
        GlobalConnectionStats::new(archive_accessor_node, "logs")
    }

    fn new(connection_node: &mut Node, diagnostics_source: &'static str) -> Self {
        let reader_servers_constructed = connection_node
            .create_uint(format!("{}_reader_servers_constructed", diagnostics_source), 0);
        let reader_servers_destroyed = connection_node
            .create_uint(format!("{}_reader_servers_destroyed", diagnostics_source), 0);
        let batch_iterator_connections_opened = connection_node
            .create_uint(format!("{}_batch_iterator_connections_opened", diagnostics_source), 0);
        let batch_iterator_connections_closed = connection_node
            .create_uint(format!("{}_batch_iterator_connections_closed", diagnostics_source), 0);
        let component_timeouts_count = connection_node
            .create_uint(format!("{}_component_timeouts_count", diagnostics_source), 0);
        let batch_iterator_get_next_requests = connection_node
            .create_uint(format!("{}_batch_iterator_get_next_requests", diagnostics_source), 0);
        let batch_iterator_get_next_responses = connection_node
            .create_uint(format!("{}_batch_iterator_get_next_responses", diagnostics_source), 0);
        let batch_iterator_get_next_errors = connection_node
            .create_uint(format!("{}_batch_iterator_get_next_errors", diagnostics_source), 0);
        let batch_iterator_get_next_result_count = connection_node
            .create_uint(format!("{}_batch_iterator_get_next_result_count", diagnostics_source), 0);
        let batch_iterator_get_next_result_errors = connection_node.create_uint(
            format!("{}_batch_iterator_get_next_result_errors", diagnostics_source),
            0,
        );
        let batch_iterator_get_next_time_usec = connection_node.create_uint_exponential_histogram(
            format!("{}_batch_iterator_get_next_time_usec", diagnostics_source),
            ExponentialHistogramParams {
                floor: EXPONENTIAL_HISTOGRAM_USEC_FLOOR,
                initial_step: EXPONENTIAL_HISTOGRAM_USEC_STEP,
                step_multiplier: EXPONENTIAL_HISTOGRAM_USEC_MULTIPLIER,
                buckets: EXPONENTIAL_HISTOGRAM_USEC_BUCKETS as usize,
            },
        );

        let max_snapshot_sizes_bytes = connection_node.create_uint_linear_histogram(
            format!("{}_max_snapshot_sizes_bytes", diagnostics_source),
            LinearHistogramParams {
                floor: LINEAR_HISTOGRAM_BYTES_FLOOR,
                step_size: LINEAR_HISTOGRAM_BYTES_STEP,
                buckets: LINEAR_HISTOGRAM_BYTES_BUCKETS as usize,
            },
        );

        let snapshot_schema_truncation_percentage = connection_node.create_uint_linear_histogram(
            format!("{}_snapshot_schema_truncation_percentage", diagnostics_source),
            LinearHistogramParams {
                floor: LINEAR_HISTOGRAM_TRUNCATION_PERCENT_FLOOR,
                step_size: LINEAR_HISTOGRAM_TRUNCATION_PERCENT_STEP,
                buckets: LINEAR_HISTOGRAM_TRUNCATION_PERCENT_BUCKETS as usize,
            },
        );

        let schema_truncation_count = connection_node
            .create_uint(format!("{}_schema_truncation_count", diagnostics_source), 0);

        GlobalConnectionStats {
            diagnostics_source,
            connection_node: connection_node.clone_weak(),
            reader_servers_constructed,
            reader_servers_destroyed,
            batch_iterator_connections_opened,
            batch_iterator_connections_closed,
            component_timeouts_count,
            batch_iterator_get_next_requests,
            batch_iterator_get_next_responses,
            batch_iterator_get_next_errors,
            batch_iterator_get_next_result_count,
            batch_iterator_get_next_result_errors,
            batch_iterator_get_next_time_usec,
            max_snapshot_sizes_bytes,
            snapshot_schema_truncation_percentage,
            schema_truncation_count,
            component_time_usec: Mutex::new(None),
            processing_time_tracker: Mutex::new(None),
        }
    }

    pub fn add_timeout(&self) {
        self.component_timeouts_count.add(1);
    }

    pub fn record_percent_truncated_schemas(&self, percent_truncated_schemas: u64) {
        self.snapshot_schema_truncation_percentage.insert(percent_truncated_schemas);
    }

    pub fn record_max_snapshot_size_config(&self, max_snapshot_size_config: u64) {
        self.max_snapshot_sizes_bytes.insert(max_snapshot_size_config);
    }

    /// Record the duration of a whole request to GetNext.
    pub fn record_batch_duration(&self, duration: Duration) {
        let micros = duration.into_micros();
        if micros >= 0 {
            self.batch_iterator_get_next_time_usec.insert(micros as u64);
        }
    }

    /// Record the duration of obtaining data from a single component.
    pub fn record_component_duration(&self, moniker: &str, duration: Duration) {
        let nanos = duration.into_nanos();
        if nanos >= 0 {
            // Lazily initialize stats that may not be needed for all diagnostics types.

            let mut component_time_usec = self.component_time_usec.lock();
            if component_time_usec.is_none() {
                *component_time_usec =
                    Some(self.connection_node.create_uint_exponential_histogram(
                        format!("{}_component_time_usec", self.diagnostics_source),
                        ExponentialHistogramParams {
                            floor: EXPONENTIAL_HISTOGRAM_USEC_FLOOR,
                            initial_step: EXPONENTIAL_HISTOGRAM_USEC_STEP,
                            step_multiplier: EXPONENTIAL_HISTOGRAM_USEC_MULTIPLIER,
                            buckets: EXPONENTIAL_HISTOGRAM_USEC_BUCKETS as usize,
                        },
                    ));
            }

            let mut processing_time_tracker = self.processing_time_tracker.lock();
            if processing_time_tracker.is_none() {
                *processing_time_tracker =
                    Some(ProcessingTimeTracker::new(self.connection_node.create_child(format!(
                        "{}_longest_processing_times",
                        self.diagnostics_source
                    ))));
            }

            component_time_usec.as_ref().unwrap().insert(nanos as u64 / 1000);
            processing_time_tracker.as_mut().unwrap().track(moniker, nanos as u64);
        }
    }
}

const PROCESSING_TIME_COMPONENT_COUNT_LIMIT: usize = 20;

/// Holds stats on the longest processing times for individual components' data.
struct ProcessingTimeTracker {
    /// The node holding all properties for the tracker.
    node: Node,
    /// Map from component moniker to a tuple of its time and a node containing the stats about it.
    longest_times_by_component: BTreeMap<String, (u64, Node)>,
    /// The shortest time seen so far. If a new component is being
    /// recorded and its time is greater than this, we need to pop the
    /// entry containing this time.
    shortest_time_ns: u64,
}

impl ProcessingTimeTracker {
    fn new(node: Node) -> Self {
        Self { node, longest_times_by_component: BTreeMap::new(), shortest_time_ns: u64::MAX }
    }
    fn track(&mut self, moniker: &str, time_ns: u64) {
        let at_capacity =
            self.longest_times_by_component.len() >= PROCESSING_TIME_COMPONENT_COUNT_LIMIT;

        // Do nothing if the container it as the limit and the new time doesn't need to get
        // inserted.
        if at_capacity && time_ns < self.shortest_time_ns {
            return;
        }

        let parent_node = &self.node;

        let make_entry = || {
            let n = parent_node.create_child(moniker.to_string());
            n.record_int("@time", zx::Time::get_monotonic().into_nanos());
            n.record_double("duration_seconds", time_ns as f64 / 1e9);
            (time_ns, n)
        };

        self.longest_times_by_component
            .entry(moniker.to_string())
            .and_modify(move |v| {
                if v.0 < time_ns {
                    *v = make_entry();
                }
            })
            .or_insert_with(make_entry);

        // Repeatedly find the key for the smallest time and remove it until we are under the
        // limit.
        while self.longest_times_by_component.len() > PROCESSING_TIME_COMPONENT_COUNT_LIMIT {
            let mut key = "".to_string();
            for (k, (val, _)) in &self.longest_times_by_component {
                if *val == self.shortest_time_ns {
                    key = k.clone();
                    break;
                }
            }
            self.longest_times_by_component.remove(&key);
            self.shortest_time_ns = self
                .longest_times_by_component
                .values()
                .map(|v| v.0)
                .min()
                .unwrap_or(std::u64::MAX);
        }

        self.shortest_time_ns = std::cmp::min(self.shortest_time_ns, time_ns);
    }
}

pub struct ConnectionStats {
    /// Inspect node for tracking usage/health metrics of a single connection to a batch iterator.
    _batch_iterator_connection_node: Node,

    /// Global stats for connections to the BatchIterator protocol.
    global_stats: Arc<GlobalConnectionStats>,

    /// Property tracking number of requests to the BatchIterator instance this struct is tracking.
    batch_iterator_get_next_requests: UintProperty,
    /// Property tracking number of responses from the BatchIterator instance this struct is tracking.
    batch_iterator_get_next_responses: UintProperty,
    /// Property tracking number of times the batch iterator has served a terminal batch signalling that
    /// the client has reached the end of the iterator and should terminate their connection.
    batch_iterator_terminal_responses: UintProperty,
}

impl ConnectionStats {
    pub fn open_connection(&self) {
        self.global_stats.batch_iterator_connections_opened.add(1);
    }

    pub fn close_connection(&self) {
        self.global_stats.batch_iterator_connections_closed.add(1);
    }

    pub fn global_stats(&self) -> &Arc<GlobalConnectionStats> {
        &self.global_stats
    }

    pub fn add_request(self: &Arc<Self>) {
        self.global_stats.batch_iterator_get_next_requests.add(1);
        self.batch_iterator_get_next_requests.add(1);
    }

    pub fn add_response(self: &Arc<Self>) {
        self.global_stats.batch_iterator_get_next_responses.add(1);
        self.batch_iterator_get_next_responses.add(1);
    }

    pub fn add_terminal(self: &Arc<Self>) {
        self.batch_iterator_terminal_responses.add(1);
    }

    pub fn add_result(&self) {
        self.global_stats.batch_iterator_get_next_result_count.add(1);
    }

    pub fn add_error(&self) {
        self.global_stats.batch_iterator_get_next_errors.add(1);
    }

    pub fn add_result_error(&self) {
        self.global_stats.batch_iterator_get_next_result_errors.add(1);
    }

    pub fn add_schema_truncated(&self) {
        self.global_stats.schema_truncation_count.add(1);
    }

    pub fn for_inspect(archive_accessor_stats: Arc<AccessorStats>) -> Self {
        let global_inspect = archive_accessor_stats.global_inspect_stats.clone();
        ConnectionStats::new(archive_accessor_stats, global_inspect, "inspect")
    }

    pub fn for_lifecycle(archive_accessor_stats: Arc<AccessorStats>) -> Self {
        let global_lifecycle = archive_accessor_stats.global_lifecycle_stats.clone();
        ConnectionStats::new(archive_accessor_stats, global_lifecycle, "lifecycle")
    }

    pub fn for_logs(archive_accessor_stats: Arc<AccessorStats>) -> Self {
        let global_logs = archive_accessor_stats.global_logs_stats.clone();
        ConnectionStats::new(archive_accessor_stats, global_logs, "logs")
    }

    fn new(
        archive_accessor_stats: Arc<AccessorStats>,
        global_stats: Arc<GlobalConnectionStats>,
        prefix: &str,
    ) -> Self {
        // we'll decrement these on drop
        global_stats.reader_servers_constructed.add(1);

        // TODO(fxbug.dev/59454) add this to a "list node" instead of using numeric suffixes
        let batch_iterator_connection_node =
            archive_accessor_stats.archive_accessor_node.create_child(
                fuchsia_inspect::unique_name(&format!("{}_batch_iterator_connection", prefix)),
            );

        let batch_iterator_get_next_requests = batch_iterator_connection_node
            .create_uint(format!("{}_batch_iterator_get_next_requests", prefix), 0);
        let batch_iterator_get_next_responses = batch_iterator_connection_node
            .create_uint(format!("{}_batch_iterator_get_next_responses", prefix), 0);
        let batch_iterator_terminal_responses = batch_iterator_connection_node
            .create_uint(format!("{}_batch_iterator_terminal_responses", prefix), 0);

        ConnectionStats {
            _batch_iterator_connection_node: batch_iterator_connection_node,
            global_stats,
            batch_iterator_get_next_requests,
            batch_iterator_get_next_responses,
            batch_iterator_terminal_responses,
        }
    }
}

impl Drop for ConnectionStats {
    fn drop(&mut self) {
        self.global_stats.reader_servers_destroyed.add(1);
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crate::archive::EventFileGroupStats,
        fuchsia_inspect::{assert_inspect_tree, health::Reporter, testing::AnyProperty, Inspector},
        std::iter::FromIterator,
    };

    #[test]
    fn health() {
        component::health().set_ok();
        assert_inspect_tree!(component::inspector(),
        root: {
            "fuchsia.inspect.Health": {
                status: "OK",
                start_timestamp_nanos: AnyProperty,
            }
        });

        component::health().set_unhealthy("Bad state");
        assert_inspect_tree!(component::inspector(),
        root: contains {
            "fuchsia.inspect.Health": {
                status: "UNHEALTHY",
                message: "Bad state",
                start_timestamp_nanos: AnyProperty,
            }
        });

        component::health().set_ok();
        assert_inspect_tree!(component::inspector(),
        root: contains {
            "fuchsia.inspect.Health": {
                status: "OK",
                start_timestamp_nanos: AnyProperty,
            }
        });
    }

    #[test]
    fn group_stats() {
        let inspector = Inspector::new();
        let mut group = Groups::new(inspector.root().create_child("archived_events"));
        group.replace(&EventFileGroupStatsMap::from_iter(vec![
            ("a/b".to_string(), EventFileGroupStats { file_count: 1, size: 2 }),
            ("c/d".to_string(), EventFileGroupStats { file_count: 3, size: 4 }),
        ]));

        assert_inspect_tree!(inspector,
        root: contains {
            archived_events: {
               "a/b": {
                    file_count: 1u64,
                    size_in_bytes: 2u64
               },
               "c/d": {
                   file_count: 3u64,
                   size_in_bytes: 4u64
               }
            }
        });
    }

    #[test]
    fn processing_time_tracker() {
        let inspector = Inspector::new();
        let mut tracker = ProcessingTimeTracker::new(inspector.root().create_child("test"));

        tracker.track("a", 1e9 as u64);
        assert_inspect_tree!(inspector,
        root: {
            test: {
                a: {
                    "@time": AnyProperty,
                    duration_seconds: 1f64
                }
            }
        });

        tracker.track("a", 5e8 as u64);
        assert_inspect_tree!(inspector,
        root: {
            test: {
                a: {
                    "@time": AnyProperty,
                    duration_seconds: 1f64
                }
            }
        });

        tracker.track("a", 5500e6 as u64);
        assert_inspect_tree!(inspector,
        root: {
            test: {
                a: {
                    "@time": AnyProperty,
                    duration_seconds: 5.5f64
                }
            }
        });

        for time in 0..60 {
            tracker.track(&format!("b{}", time), time * 1e9 as u64);
        }

        assert_inspect_tree!(inspector,
        root: {
            test: {
                b40: { "@time": AnyProperty, duration_seconds: 40f64 },
                b41: { "@time": AnyProperty, duration_seconds: 41f64 },
                b42: { "@time": AnyProperty, duration_seconds: 42f64 },
                b43: { "@time": AnyProperty, duration_seconds: 43f64 },
                b44: { "@time": AnyProperty, duration_seconds: 44f64 },
                b45: { "@time": AnyProperty, duration_seconds: 45f64 },
                b46: { "@time": AnyProperty, duration_seconds: 46f64 },
                b47: { "@time": AnyProperty, duration_seconds: 47f64 },
                b48: { "@time": AnyProperty, duration_seconds: 48f64 },
                b49: { "@time": AnyProperty, duration_seconds: 49f64 },
                b50: { "@time": AnyProperty, duration_seconds: 50f64 },
                b51: { "@time": AnyProperty, duration_seconds: 51f64 },
                b52: { "@time": AnyProperty, duration_seconds: 52f64 },
                b53: { "@time": AnyProperty, duration_seconds: 53f64 },
                b54: { "@time": AnyProperty, duration_seconds: 54f64 },
                b55: { "@time": AnyProperty, duration_seconds: 55f64 },
                b56: { "@time": AnyProperty, duration_seconds: 56f64 },
                b57: { "@time": AnyProperty, duration_seconds: 57f64 },
                b58: { "@time": AnyProperty, duration_seconds: 58f64 },
                b59: { "@time": AnyProperty, duration_seconds: 59f64 },
            }
        });
    }
}
