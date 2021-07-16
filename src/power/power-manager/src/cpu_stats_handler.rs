// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::error::PowerManagerError;
use crate::log_if_err;
use crate::message::{Message, MessageReturn};
use crate::node::Node;
use crate::types::Nanoseconds;
use crate::utils::connect_proxy;
use anyhow::{format_err, Error};
use async_trait::async_trait;
use fidl_fuchsia_kernel as fstats;
use fuchsia_inspect::{self as inspect};
use fuchsia_inspect_contrib::{inspect_log, nodes::BoundedListNode};
use log::*;
use serde_json as json;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// Node: CpuStatsHandler
///
/// Summary: Provides CPU statistic information by interfacing with the Kernel Stats service. That
///          information includes the number of CPU cores and CPU load information.
///
/// Handles Messages:
///     - GetNumCpus
///     - GetCpuLoads
///
/// Sends Messages: N/A
///
/// FIDL dependencies:
///     - fuchsia.kernel.Stats: the node connects to this service to query kernel information

/// The Kernel Stats service that we'll be communicating with
const CPU_STATS_SVC: &'static str = "/svc/fuchsia.kernel.Stats";

/// A builder for constructing the CpuStatsHandler node
pub struct CpuStatsHandlerBuilder<'a> {
    stats_svc_proxy: Option<fstats::StatsProxy>,
    inspect_root: Option<&'a inspect::Node>,
}

impl<'a> CpuStatsHandlerBuilder<'a> {
    pub fn new() -> Self {
        Self { stats_svc_proxy: None, inspect_root: None }
    }

    pub fn new_from_json(_json_data: json::Value, _nodes: &HashMap<String, Rc<dyn Node>>) -> Self {
        Self::new()
    }

    #[cfg(test)]
    pub fn with_proxy(mut self, proxy: fstats::StatsProxy) -> Self {
        self.stats_svc_proxy = Some(proxy);
        self
    }

    #[cfg(test)]
    pub fn with_inspect_root(mut self, root: &'a inspect::Node) -> Self {
        self.inspect_root = Some(root);
        self
    }

    pub async fn build(self) -> Result<Rc<CpuStatsHandler>, Error> {
        // Optionally use the default proxy
        let proxy = if self.stats_svc_proxy.is_none() {
            connect_proxy::<fstats::StatsMarker>(&CPU_STATS_SVC.to_string())?
        } else {
            self.stats_svc_proxy.unwrap()
        };

        // Optionally use the default inspect root node
        let inspect_root = self.inspect_root.unwrap_or(inspect::component::inspector().root());

        let node = Rc::new(CpuStatsHandler {
            stats_svc_proxy: proxy,
            cpu_idle_stats: RefCell::new(Default::default()),
            inspect: InspectData::new(inspect_root, "CpuStatsHandler".to_string()),
        });

        // Seed the idle stats
        node.cpu_idle_stats.replace(node.get_idle_stats().await?);

        Ok(node)
    }
}

/// The CpuStatsHandler node
pub struct CpuStatsHandler {
    /// A proxy handle to communicate with the Kernel Stats service
    stats_svc_proxy: fstats::StatsProxy,

    /// Cached CPU idle states from the most recent call
    cpu_idle_stats: RefCell<CpuIdleStats>,

    /// A struct for managing Component Inspection data
    inspect: InspectData,
}

/// A record to store the total time spent idle for each CPU in the system at a moment in time
#[derive(Default, Debug)]
struct CpuIdleStats {
    /// Time the record was taken
    timestamp: Nanoseconds,

    /// Vector containing the total time since boot that each CPU has spent has spent idle. The
    /// length of the vector is equal to the number of CPUs in the system at the time of the record.
    idle_times: Vec<Nanoseconds>,
}

impl CpuStatsHandler {
    /// Calls out to the Kernel Stats service to retrieve the latest CPU stats
    async fn get_cpu_stats(&self) -> Result<fstats::CpuStats, Error> {
        fuchsia_trace::duration!("power_manager", "CpuStatsHandler::get_cpu_stats");
        let result = self
            .stats_svc_proxy
            .get_cpu_stats()
            .await
            .map_err(|e| format_err!("get_cpu_stats IPC failed: {}", e));

        log_if_err!(result, "Failed to get CPU stats");
        fuchsia_trace::instant!(
            "power_manager",
            "CpuStatsHandler::get_cpu_stats_result",
            fuchsia_trace::Scope::Thread,
            "result" => format!("{:?}", result).as_str()
        );

        Ok(result?)
    }

    async fn handle_get_num_cpus(&self) -> Result<MessageReturn, PowerManagerError> {
        fuchsia_trace::duration!("power_manager", "CpuStatsHandler::handle_get_num_cpus");
        let stats = self.get_cpu_stats().await?;
        Ok(MessageReturn::GetNumCpus(stats.actual_num_cpus as u32))
    }

    async fn handle_get_cpu_loads(&self) -> Result<MessageReturn, PowerManagerError> {
        fuchsia_trace::duration!("power_manager", "CpuStatsHandler::handle_get_cpu_loads");
        let new_stats = self.get_idle_stats().await?;
        let loads = Self::calculate_cpu_loads(&self.cpu_idle_stats.borrow(), &new_stats);
        self.cpu_idle_stats.replace(new_stats);

        // Log the total load to Inspect / tracing
        let total_load: f32 = loads.iter().sum();
        self.inspect.log_total_cpu_load(total_load as f64);
        fuchsia_trace::instant!(
            "power_manager",
            "CpuStatsHandler::total_cpu_load",
            fuchsia_trace::Scope::Thread,
            "load" => total_load as f64
        );

        Ok(MessageReturn::GetCpuLoads(loads))
    }

    /// Gets the CPU idle stats, then populates them into the CpuIdleStats struct format that we
    /// can more easily use for calculations.
    async fn get_idle_stats(&self) -> Result<CpuIdleStats, Error> {
        fuchsia_trace::duration!("power_manager", "CpuStatsHandler::get_idle_stats");
        let mut idle_stats: CpuIdleStats = Default::default();
        let cpu_stats = self.get_cpu_stats().await?;
        let per_cpu_stats =
            cpu_stats.per_cpu_stats.ok_or(format_err!("Received null per_cpu_stats"))?;

        idle_stats.timestamp = crate::utils::get_current_timestamp();
        for i in 0..cpu_stats.actual_num_cpus as usize {
            idle_stats.idle_times.push(Nanoseconds(
                per_cpu_stats[i]
                    .idle_time
                    .ok_or(format_err!("Received null idle_time for CPU {}", i))?,
            ));
        }

        fuchsia_trace::instant!(
            "power_manager",
            "CpuStatsHandler::idle_stats_result",
            fuchsia_trace::Scope::Thread,
            "idle_stats" => format!("{:?}", idle_stats).as_str()
        );

        Ok(idle_stats)
    }

    /// Calculates the load of all CPUs in the system. Per-CPU load is measured as a value from 0.0
    /// to 1.0.
    ///     old_idle: the starting idle stats
    ///     new_idle: the ending idle stats
    fn calculate_cpu_loads(old_idle: &CpuIdleStats, new_idle: &CpuIdleStats) -> Vec<f32> {
        fuchsia_trace::duration!("power_manager", "CpuStatsHandler::calculate_cpu_loads");
        if old_idle.idle_times.len() != new_idle.idle_times.len() {
            error!(
                "Number of CPUs changed (old={}; new={})",
                old_idle.idle_times.len(),
                new_idle.idle_times.len()
            );
            return vec![0.0];
        }

        let total_time_delta = new_idle.timestamp - old_idle.timestamp;
        if total_time_delta.0 <= 0 {
            error!(
                "Expected positive total_time_delta, got: {:?} (start={:?}; end={:?})",
                total_time_delta, old_idle.timestamp, new_idle.timestamp
            );
            return vec![0.0];
        }

        old_idle
            .idle_times
            .iter()
            .zip(new_idle.idle_times.iter())
            .map(|(old_idle_time, new_idle_time)| {
                let busy_time = total_time_delta.0 - (new_idle_time.0 - old_idle_time.0);
                busy_time as f32 / total_time_delta.0 as f32
            })
            .collect()
    }
}

const NUM_INSPECT_LOAD_SAMPLES: usize = 10;

struct InspectData {
    cpu_loads: RefCell<BoundedListNode>,
}

impl InspectData {
    fn new(parent: &inspect::Node, name: String) -> Self {
        // Create a local root node and properties
        let root = parent.create_child(name);
        let cpu_loads = RefCell::new(BoundedListNode::new(
            root.create_child("cpu_loads"),
            NUM_INSPECT_LOAD_SAMPLES,
        ));

        // Pass ownership of the new node to the parent node, otherwise it'll be dropped
        parent.record(root);

        InspectData { cpu_loads }
    }

    fn log_total_cpu_load(&self, load: f64) {
        inspect_log!(self.cpu_loads.borrow_mut(), load: load);
    }
}

#[async_trait(?Send)]
impl Node for CpuStatsHandler {
    fn name(&self) -> String {
        "CpuStatsHandler".to_string()
    }

    async fn handle_message(&self, msg: &Message) -> Result<MessageReturn, PowerManagerError> {
        match msg {
            Message::GetNumCpus => self.handle_get_num_cpus().await,
            Message::GetCpuLoads => self.handle_get_cpu_loads().await,
            _ => Err(PowerManagerError::Unsupported),
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::types::Seconds;
    use async_utils::PollExt;
    use fuchsia_async as fasync;
    use fuchsia_zircon::DurationNum;
    use futures::TryStreamExt;
    use inspect::assert_data_tree;
    use std::collections::VecDeque;
    use std::ops::Add;

    const TEST_NUM_CORES: u32 = 4;

    /// Generates CpuStats for an input vector of idle times, using the length of the idle times
    /// vector to determine the number of CPUs.
    fn idle_times_to_cpu_stats(idle_times: &Vec<Nanoseconds>) -> fstats::CpuStats {
        let mut per_cpu_stats = Vec::new();
        for (i, idle_time) in idle_times.iter().enumerate() {
            per_cpu_stats.push(fstats::PerCpuStats {
                cpu_number: Some(i as u32),
                flags: None,
                idle_time: Some(idle_time.0),
                reschedules: None,
                context_switches: None,
                irq_preempts: None,
                yields: None,
                ints: None,
                timer_ints: None,
                timers: None,
                page_faults: None,
                exceptions: None,
                syscalls: None,
                reschedule_ipis: None,
                generic_ipis: None,
                ..fstats::PerCpuStats::EMPTY
            });
        }

        fstats::CpuStats {
            actual_num_cpus: idle_times.len() as u64,
            per_cpu_stats: Some(per_cpu_stats),
        }
    }

    fn setup_fake_service(
        mut get_idle_times: impl FnMut() -> Vec<Nanoseconds> + 'static,
    ) -> fstats::StatsProxy {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fstats::StatsMarker>().unwrap();

        fasync::Task::local(async move {
            while let Ok(req) = stream.try_next().await {
                match req {
                    Some(fstats::StatsRequest::GetCpuStats { responder }) => {
                        let mut cpu_stats = idle_times_to_cpu_stats(&get_idle_times());
                        let _ = responder.send(&mut cpu_stats);
                    }
                    _ => assert!(false),
                }
            }
        })
        .detach();

        proxy
    }

    /// Creates a test CpuStatsHandler node, with the provided closure giving per-CPU idle times
    /// that will be reported in CpuStats. The number of CPUs is implied by the length of the
    /// closure's returned Vec.
    pub async fn setup_test_node(
        get_idle_times: impl FnMut() -> Vec<Nanoseconds> + 'static,
    ) -> Rc<CpuStatsHandler> {
        CpuStatsHandlerBuilder::new()
            .with_proxy(setup_fake_service(get_idle_times))
            .build()
            .await
            .unwrap()
    }

    /// Creates a test CpuStatsHandler that reports zero idle times, with `TEST_NUM_CORES` CPUs.
    pub async fn setup_simple_test_node() -> Rc<CpuStatsHandler> {
        setup_test_node(|| vec![Nanoseconds(0); TEST_NUM_CORES as usize]).await
    }

    /// This test creates a CpuStatsHandler node and sends it the 'GetNumCpus' message. The
    /// test verifies that the message returns successfully and the expected number of CPUs
    /// are reported (in the test configuration, it should report `TEST_NUM_CORES`).
    #[fasync::run_singlethreaded(test)]
    async fn test_get_num_cpus() {
        let node = setup_simple_test_node().await;
        let num_cpus = node.handle_message(&Message::GetNumCpus).await.unwrap();
        if let MessageReturn::GetNumCpus(n) = num_cpus {
            assert_eq!(n, TEST_NUM_CORES);
        } else {
            assert!(false);
        }
    }

    /// Tests that the node correctly calculates CPU loads for all CPUs as a response to the
    /// 'GetCpuLoads' message.
    #[test]
    fn test_handle_get_cpu_loads() {
        // Use executor so we can advance the fake time
        let mut executor = fasync::TestExecutor::new_with_fake_time().unwrap();

        // Fake idle times that will be fed into the node. These idle times mean the node will first
        // see idle times of 0 for both CPUs, then idle times of 1s and 2s on the next poll.
        let mut fake_idle_times: VecDeque<Vec<Nanoseconds>> = vec![
            vec![Seconds(0.0).into(), Seconds(0.0).into()],
            vec![Seconds(1.0).into(), Seconds(2.0).into()],
        ]
        .into();

        let node = executor
            .run_until_stalled(&mut Box::pin(setup_test_node(move || {
                fake_idle_times.pop_front().unwrap()
            })))
            .unwrap();

        // Move fake time forward by 4s. This total time delta combined with the `fake_idle_times`
        // data mean the CPU loads should be reported as 0.75 and 0.25.
        executor.set_fake_time(executor.now().add(4.seconds()));
        executor
            .run_until_stalled(&mut Box::pin(async {
                match node.handle_message(&Message::GetCpuLoads).await {
                    Ok(MessageReturn::GetCpuLoads(loads)) => {
                        assert_eq!(loads, vec![0.75, 0.5]);
                    }
                    e => panic!("Unexpected message response: {:?}", e),
                }
            }))
            .unwrap();
    }

    /// Tests the calculate_cpu_loads function. Test values are used as input (representing
    /// the total time delta and idle times of four total CPUs), and the result is compared
    /// against expected output for these test values:
    ///     CPU 0: 20ns idle / 100ns total = 0.8 load
    ///     CPU 1: 40ns idle / 100ns total = 0.6 load
    ///     CPU 2: 60ns idle / 100ns total = 0.4 load
    ///     CPU 3: 80ns idle / 100ns total = 0.2 load
    #[test]
    fn test_calculate_cpu_loads() {
        let idle_sample_1 = CpuIdleStats {
            timestamp: Nanoseconds(0),
            idle_times: vec![Nanoseconds(0), Nanoseconds(0), Nanoseconds(0), Nanoseconds(0)],
        };

        let idle_sample_2 = CpuIdleStats {
            timestamp: Nanoseconds(100),
            idle_times: vec![Nanoseconds(20), Nanoseconds(40), Nanoseconds(60), Nanoseconds(80)],
        };

        let calculated_loads = CpuStatsHandler::calculate_cpu_loads(&idle_sample_1, &idle_sample_2);
        assert_eq!(calculated_loads, vec![0.8, 0.6, 0.4, 0.2])
    }

    /// Tests that an unsupported message is handled gracefully and an Unsupported error is returned
    #[fasync::run_singlethreaded(test)]
    async fn test_unsupported_msg() {
        let node = setup_simple_test_node().await;
        match node.handle_message(&Message::ReadTemperature).await {
            Err(PowerManagerError::Unsupported) => {}
            e => panic!("Unexpected return value: {:?}", e),
        }
    }

    /// Tests for the presence and correctness of dynamically-added inspect data
    #[fasync::run_singlethreaded(test)]
    async fn test_inspect_data() {
        let inspector = inspect::Inspector::new();
        let node = CpuStatsHandlerBuilder::new()
            .with_proxy(setup_fake_service(|| vec![Nanoseconds(0); TEST_NUM_CORES as usize]))
            .with_inspect_root(inspector.root())
            .build()
            .await
            .unwrap();

        // For each message, the node will query CPU load and log the sample into Inspect
        node.handle_message(&Message::GetCpuLoads).await.unwrap();

        assert_data_tree!(
            inspector,
            root: {
                CpuStatsHandler: {
                    cpu_loads: {
                        "0": {
                            load: TEST_NUM_CORES as f64,
                            "@time": inspect::testing::AnyProperty
                        }
                    }
                }
            }
        );
    }

    /// Tests that well-formed configuration JSON does not panic the `new_from_json` function.
    #[fasync::run_singlethreaded(test)]
    async fn test_new_from_json() {
        let json_data = json::json!({
            "type": "CpuStatsHandler",
            "name": "cpu_stats"
        });
        let _ = CpuStatsHandlerBuilder::new_from_json(json_data, &HashMap::new());
    }
}
