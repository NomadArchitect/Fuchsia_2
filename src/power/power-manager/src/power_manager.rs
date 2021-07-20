// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::node::Node;
use anyhow::{Context, Error};
use fuchsia_component::server::{ServiceFs, ServiceObjLocal};
use fuchsia_inspect::component;
use futures::{
    future::LocalBoxFuture,
    stream::{FuturesUnordered, StreamExt},
};
use serde_json as json;
use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::rc::Rc;

// nodes
use crate::{
    activity_handler, cpu_control_handler, cpu_stats_handler, crash_report_handler,
    dev_control_handler, driver_manager_handler, input_settings_handler, lid_shutdown,
    shutdown_watcher, system_profile_handler, system_shutdown_handler, temperature_handler,
    thermal_limiter, thermal_policy, thermal_shutdown,
};

/// Path to the node config JSON file.
const NODE_CONFIG_PATH: &'static str = "/pkg/config/power_manager/node_config.json";

pub struct PowerManager {
    nodes: HashMap<String, Rc<dyn Node>>,
}

impl PowerManager {
    pub fn new() -> Self {
        Self { nodes: HashMap::new() }
    }

    /// Perform the node initialization and begin running the PowerManager.
    pub async fn run(&mut self) -> Result<(), Error> {
        // Create a new ServiceFs to handle incoming service requests for the various services that
        // the PowerManager hosts.
        let mut fs = ServiceFs::new_local();

        // Allow our services to be discovered.
        fs.take_and_serve_directory_handle()?;

        // Required call to serve the inspect tree
        let inspector = component::inspector();
        inspect_runtime::serve(inspector, &mut fs)?;

        // Create the nodes according to the config file
        let node_futures = FuturesUnordered::new();
        self.create_nodes_from_config(&mut fs, &node_futures).await?;

        // Run the ServiceFs (handles incoming request streams) and node futures. This future never
        // completes.
        futures::stream::select(fs, node_futures).collect::<()>().await;

        Ok(())
    }

    /// Create the nodes by reading and parsing the node config JSON file.
    async fn create_nodes_from_config<'a, 'b, 'c>(
        &mut self,
        service_fs: &'a mut ServiceFs<ServiceObjLocal<'b, ()>>,
        node_futures: &FuturesUnordered<LocalBoxFuture<'c, ()>>,
    ) -> Result<(), Error> {
        let json_data: json::Value =
            json::from_reader(BufReader::new(File::open(NODE_CONFIG_PATH)?))?;
        self.create_nodes(json_data, service_fs, node_futures).await
    }

    /// Creates the nodes using the specified JSON object, adding them to the `nodes` HashMap.
    async fn create_nodes<'a, 'b, 'c>(
        &mut self,
        json_data: json::Value,
        service_fs: &'a mut ServiceFs<ServiceObjLocal<'b, ()>>,
        node_futures: &FuturesUnordered<LocalBoxFuture<'c, ()>>,
    ) -> Result<(), Error> {
        // Iterate through each object in the top-level array, which represents configuration for a
        // single node
        for node_config in json_data.as_array().unwrap().iter() {
            let node = self
                .create_node(node_config.clone(), service_fs, node_futures)
                .await
                .with_context(|| format!("Failed creating node {}", node_config["name"]))?;
            self.nodes.insert(node_config["name"].as_str().unwrap().to_string(), node);
        }
        Ok(())
    }

    /// Uses the supplied `json_data` to construct a single node, where `json_data` is the JSON
    /// object corresponding to a single node configuration.
    async fn create_node<'a, 'b, 'c>(
        &mut self,
        json_data: json::Value,
        service_fs: &'a mut ServiceFs<ServiceObjLocal<'b, ()>>,
        node_futures: &FuturesUnordered<LocalBoxFuture<'c, ()>>,
    ) -> Result<Rc<dyn Node>, Error> {
        Ok(match json_data["type"].as_str().unwrap() {
            "ActivityHandler" => {
                activity_handler::ActivityHandlerBuilder::new_from_json(json_data, &self.nodes)
                    .build(node_futures)?
            }
            "CrashReportHandler" => {
                crash_report_handler::CrashReportHandlerBuilder::new().build()?
            }
            "CpuControlHandler" => {
                cpu_control_handler::CpuControlHandlerBuilder::new_from_json(json_data, &self.nodes)
                    .build()
                    .await?
            }
            "CpuStatsHandler" => {
                cpu_stats_handler::CpuStatsHandlerBuilder::new_from_json(json_data, &self.nodes)
                    .build()
                    .await?
            }
            "DeviceControlHandler" => {
                dev_control_handler::DeviceControlHandlerBuilder::new_from_json(
                    json_data,
                    &self.nodes,
                )
                .build()?
            }
            "DriverManagerHandler" => {
                driver_manager_handler::DriverManagerHandlerBuilder::new_from_json(
                    json_data,
                    &self.nodes,
                    service_fs,
                )
                .build()
                .await?
            }
            "InputSettingsHandler" => {
                input_settings_handler::InputSettingsHandlerBuilder::new_from_json(
                    json_data,
                    &self.nodes,
                )
                .build(node_futures)?
            }
            "LidShutdown" => {
                lid_shutdown::LidShutdownBuilder::new_from_json(json_data, &self.nodes)
                    .build(node_futures)
                    .await?
            }
            "SystemProfileHandler" => {
                system_profile_handler::SystemProfileHandlerBuilder::new_from_json(
                    json_data,
                    &self.nodes,
                    service_fs,
                )
                .build()?
            }
            "SystemShutdownHandler" => {
                system_shutdown_handler::SystemShutdownHandlerBuilder::new_from_json(
                    json_data,
                    &self.nodes,
                    service_fs,
                )
                .build()?
            }
            "ShutdownWatcher" => shutdown_watcher::ShutdownWatcherBuilder::new_from_json(
                json_data,
                &self.nodes,
                service_fs,
            )
            .build()?,
            "TemperatureHandler" => temperature_handler::TemperatureHandlerBuilder::new_from_json(
                json_data,
                &self.nodes,
            )
            .build()?,
            "ThermalLimiter" => thermal_limiter::ThermalLimiterBuilder::new_from_json(
                json_data,
                &self.nodes,
                service_fs,
            )
            .build()?,
            "ThermalPolicy" => {
                thermal_policy::ThermalPolicyBuilder::new_from_json(json_data, &self.nodes)
                    .build(node_futures)?
            }
            "ThermalShutdown" => {
                thermal_shutdown::ThermalShutdownBuilder::new_from_json(json_data, &self.nodes)
                    .build(node_futures)?
            }
            unknown => panic!("Unknown node type: {}", unknown),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuchsia_async as fasync;
    use std::fs;

    /// Tests that well-formed configuration JSON does not cause an unexpected panic in the
    /// `create_nodes` function. With this test JSON, we expect a panic with the message stated
    /// below indicating a node with the given type doesn't exist. By this point the JSON parsing
    /// will have already been validated.
    #[fasync::run_singlethreaded(test)]
    #[should_panic(expected = "Unknown node type: test_type")]
    async fn test_create_nodes() {
        let json_data = json::json!([
            {
                "type": "test_type",
                "name": "test_name"
            },
        ]);
        let mut power_manager = PowerManager::new();
        let node_futures = FuturesUnordered::new();
        power_manager
            .create_nodes(json_data, &mut ServiceFs::new_local(), &node_futures)
            .await
            .unwrap();
    }

    /// Finds all of the node config files under the test package's "/config/data" directory. The
    /// node config files are identified by a suffix of "node_config.json". Returns an iterator of
    /// tuples where the first element is the path to the config file and the second is a
    /// json::Value vector representing the config file JSON array.
    fn get_node_config_files() -> impl Iterator<Item = (String, Vec<json::Value>)> {
        let node_config_file_paths = fs::read_dir("/config/data")
            .unwrap()
            .filter(|f| {
                f.as_ref().unwrap().file_name().into_string().unwrap().ends_with("node_config.json")
            })
            .map(|f| f.unwrap().path());

        node_config_file_paths.map(|file| {
            (
                file.to_str().unwrap().to_string(),
                json::from_reader(BufReader::new(File::open(file).unwrap())).unwrap(),
            )
        })
    }

    #[test]
    fn test_config_files() -> Result<(), Error> {
        let config_files = get_node_config_files().collect::<Vec<_>>();
        assert!(config_files.len() > 0, "No config files found");

        for (file_path, config_file) in config_files {
            cpu_control_handler::tests::test_config_file(&config_file)
                .context(format!("cpu_control_handler check failed for {}", file_path))?;
            driver_manager_handler::tests::test_config_file(&config_file)
                .context(format!("driver_manager_handler check failed for {}", file_path))?;
        }

        Ok(())
    }
}
