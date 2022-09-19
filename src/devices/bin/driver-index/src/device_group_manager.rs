// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::match_common::{
        collect_node_names_from_composite_rules, get_composite_rules_from_composite_driver,
        node_to_device_property,
    },
    crate::resolved_driver::ResolvedDriver,
    bind::compiler::Symbol,
    bind::interpreter::match_bind::{match_bind, DeviceProperties, MatchBindData, PropertyKey},
    fidl_fuchsia_driver_framework as fdf, fidl_fuchsia_driver_index as fdi,
    fuchsia_zircon::{zx_status_t, Status},
    std::collections::{BTreeMap, HashMap, HashSet},
};

#[derive(Debug, Eq, Hash, PartialEq)]
pub struct DeviceGroupNodePropertyCondition {
    condition: fdf::Condition,
    values: Vec<Symbol>,
}

type DeviceGroupNodeProperties = BTreeMap<PropertyKey, DeviceGroupNodePropertyCondition>;

struct MatchedComposite {
    pub info: fdi::MatchedCompositeInfo,
    pub names: Vec<String>,
}

struct DeviceGroupInfo {
    pub nodes: Vec<fdf::DeviceGroupNode>,
    pub matched: Option<MatchedComposite>,
}

// The DeviceGroupManager struct is responsible of managing a list of device groups
// for matching.
pub struct DeviceGroupManager {
    // This maps a list of device groups to the nodes that they belong to.
    pub device_group_nodes: HashMap<DeviceGroupNodeProperties, Vec<fdi::MatchedDeviceGroupInfo>>,

    // Contains all the topological path of all device groups that have been added. This
    // list is to ensure that we don't add multiple device groups with the same topological
    // path. The value stores:
    // The transformation for the device group.
    // The matched composite info of a composite driver that matched
    // to the device group at the topological path (or None if it did not match any).
    // We store this info here rather than the device_group_nodes items since these have a 1:1
    // mapping to the device group, putting them with the device_group_nodes would lead
    // to a lot of duplicates leading to wasted space.
    device_group_list: HashMap<String, DeviceGroupInfo>,
}

impl DeviceGroupManager {
    pub fn new() -> Self {
        DeviceGroupManager { device_group_nodes: HashMap::new(), device_group_list: HashMap::new() }
    }

    pub fn add_device_group(
        &mut self,
        group: fdf::DeviceGroup,
        composite_drivers: Vec<&ResolvedDriver>,
    ) -> fdi::DriverIndexAddDeviceGroupResult {
        let topological_path = group.topological_path.ok_or(Status::INVALID_ARGS.into_raw())?;
        let nodes = group.nodes.ok_or(Status::INVALID_ARGS.into_raw())?;

        if self.device_group_list.contains_key(&topological_path) {
            return Err(Status::ALREADY_EXISTS.into_raw());
        }

        if nodes.is_empty() {
            return Err(Status::INVALID_ARGS.into_raw());
        }

        // Collect device group nodes in a separate vector before adding them to the device group
        // manager. This is to ensure that we add the nodes after they're all verified to be valid.
        // TODO(fxb/105562): Update tests so that we can verify that transformation exists in
        // each node.
        let mut device_group_nodes: Vec<(DeviceGroupNodeProperties, fdi::MatchedDeviceGroupInfo)> =
            vec![];
        for (node_idx, node) in nodes.iter().enumerate() {
            let properties = convert_to_device_properties(&node.properties)?;
            let device_group_info = fdi::MatchedDeviceGroupInfo {
                topological_path: Some(topological_path.clone()),
                node_index: Some(node_idx as u32),
                num_nodes: Some(nodes.len() as u32),
                ..fdi::MatchedDeviceGroupInfo::EMPTY
            };

            device_group_nodes.push((properties, device_group_info));
        }

        // Add each node and its device group to the node map.
        for (properties, group_info) in device_group_nodes {
            self.device_group_nodes
                .entry(properties)
                .and_modify(|device_groups| device_groups.push(group_info.clone()))
                .or_insert(vec![group_info]);
        }

        for composite_driver in composite_drivers {
            let matched_composite = match_composite_transformation(composite_driver, &nodes)?;
            if let Some(matched_composite) = matched_composite {
                // Found a match so we can set this in our map.
                self.device_group_list.insert(
                    topological_path.clone(),
                    DeviceGroupInfo {
                        nodes,
                        matched: Some(MatchedComposite {
                            info: matched_composite.info.clone(),
                            names: matched_composite.names.clone(),
                        }),
                    },
                );
                return Ok((matched_composite.info, matched_composite.names));
            }
        }

        self.device_group_list.insert(topological_path, DeviceGroupInfo { nodes, matched: None });
        Err(Status::NOT_FOUND.into_raw())
    }

    // Match the given device properties to all the nodes. Returns a list of device groups for all the
    // nodes that match.
    pub fn match_device_group_nodes(
        &self,
        properties: &DeviceProperties,
    ) -> Option<fdi::MatchedDriver> {
        let mut device_groups: Vec<fdi::MatchedDeviceGroupInfo> = vec![];
        for (node_props, group_list) in self.device_group_nodes.iter() {
            if match_node(&node_props, properties) {
                device_groups.extend_from_slice(group_list.as_slice());
            }
        }

        if device_groups.is_empty() {
            return None;
        }

        // Put in the matched composite info for this device group
        // that we have stored in our device_group_list.
        let mut device_groups_result = vec![];
        for device_group in device_groups {
            if let Some(device_group) = self.device_group_add_composite_info(device_group) {
                device_groups_result.push(device_group);
            }
        }

        if device_groups_result.is_empty() {
            return None;
        }

        Some(fdi::MatchedDriver::DeviceGroupNode(fdi::MatchedDeviceGroupNodeInfo {
            device_groups: Some(device_groups_result),
            ..fdi::MatchedDeviceGroupNodeInfo::EMPTY
        }))
    }

    pub fn new_driver_available(&mut self, resolved_driver: ResolvedDriver) {
        for dev_group in self.device_group_list.values_mut() {
            if dev_group.matched.is_some() {
                continue;
            }
            let matched_composite_result =
                match_composite_transformation(&resolved_driver, &dev_group.nodes);
            if let Ok(Some(matched_composite)) = matched_composite_result {
                dev_group.matched = Some(MatchedComposite {
                    info: matched_composite.info,
                    names: matched_composite.names,
                });
            }
        }
    }

    fn device_group_add_composite_info(
        &self,
        mut info: fdi::MatchedDeviceGroupInfo,
    ) -> Option<fdi::MatchedDeviceGroupInfo> {
        if let Some(topological_path) = &info.topological_path {
            let list_value = self.device_group_list.get(topological_path);
            if let Some(device_group) = list_value {
                // TODO(fxb/107371): Only return device groups that have a matched composite.
                if let Some(matched) = &device_group.matched {
                    info.composite = Some(matched.info.clone());
                    info.node_names = Some(matched.names.clone());
                }

                return Some(info);
            }
        }

        return None;
    }
}

fn convert_to_device_properties(
    node_properties: &Vec<fdf::DeviceGroupProperty>,
) -> Result<DeviceGroupNodeProperties, zx_status_t> {
    if node_properties.is_empty() {
        return Err(Status::INVALID_ARGS.into_raw());
    }

    let mut device_properties = BTreeMap::new();
    for property in node_properties {
        let key = match &property.key {
            fdf::NodePropertyKey::IntValue(i) => PropertyKey::NumberKey(i.clone().into()),
            fdf::NodePropertyKey::StringValue(s) => PropertyKey::StringKey(s.clone()),
        };

        // Check if the properties contain duplicate keys.
        if device_properties.contains_key(&key) {
            return Err(Status::INVALID_ARGS.into_raw());
        }

        let first_val = property.values.first().ok_or(Status::INVALID_ARGS.into_raw())?;
        let values = property
            .values
            .iter()
            .map(|val| {
                // Check that the properties are all the same type.
                if std::mem::discriminant(first_val) != std::mem::discriminant(val) {
                    return Err(Status::INVALID_ARGS.into_raw());
                }
                Ok(node_property_to_symbol(val))
            })
            .collect::<Result<Vec<Symbol>, zx_status_t>>()?;

        device_properties.insert(
            key,
            DeviceGroupNodePropertyCondition { condition: property.condition, values: values },
        );
    }
    Ok(device_properties)
}

fn match_node(
    node_properties: &DeviceGroupNodeProperties,
    device_properties: &DeviceProperties,
) -> bool {
    for (key, node_prop_values) in node_properties.iter() {
        let dev_prop_contains_value = match device_properties.get(key) {
            Some(val) => node_prop_values.values.contains(val),
            None => false,
        };

        let evaluate_condition = match node_prop_values.condition {
            fdf::Condition::Accept => {
                // If the node property accepts a false boolean value and the property is
                // missing from the device properties, then we should evaluate the condition
                // as true.
                dev_prop_contains_value
                    || node_prop_values.values.contains(&Symbol::BoolValue(false))
            }
            fdf::Condition::Reject => !dev_prop_contains_value,
        };

        if !evaluate_condition {
            return false;
        }
    }

    true
}

fn node_property_to_symbol(value: &fdf::NodePropertyValue) -> Symbol {
    match value {
        fdf::NodePropertyValue::IntValue(i) => {
            bind::compiler::Symbol::NumberValue(i.clone().into())
        }
        fdf::NodePropertyValue::StringValue(s) => bind::compiler::Symbol::StringValue(s.clone()),
        fdf::NodePropertyValue::EnumValue(s) => bind::compiler::Symbol::EnumValue(s.clone()),
        fdf::NodePropertyValue::BoolValue(b) => bind::compiler::Symbol::BoolValue(b.clone()),
    }
}

fn match_composite_transformation<'a>(
    composite_driver: &'a ResolvedDriver,
    nodes: &'a Vec<fdf::DeviceGroupNode>,
) -> Result<Option<MatchedComposite>, i32> {
    // The device group must have at least 1 node to match a composite driver.
    if nodes.len() < 1 {
        return Ok(None);
    }

    let composite = get_composite_rules_from_composite_driver(composite_driver)?;

    // Both the composite driver and the transformation need the exact same number of nodes.
    if composite.additional_nodes.len() + 1 != nodes.len() {
        return Ok(None);
    }

    // First check the primary nodes match.
    let primary_matches = node_matches_composite_driver(
        &nodes[0],
        &composite.primary_node.instructions,
        &composite.symbol_table,
    );

    if !primary_matches {
        return Ok(None);
    }

    // The remaining nodes in the transformation can match the
    // additional nodes in the bind rules in any order.
    //
    // This logic has one issue that we are accepting as a tradeoff for simplicity:
    // If a transformation node can match to multiple bind rule
    // additional nodes, it is going to take the first one, even if there is a less strict
    // node that it can take. This can lead to false negative matches.
    //
    // Example:
    // transform[1] can match both additional_nodes[0] and additional_nodes[1]
    // transform[2] can only match additional_nodes[0]
    //
    // This algorithm will return false because it matches up transform[1] with
    // additional_nodes[0], and so transform[2] can't match the remaining nodes
    // [additional_nodes[1]].
    //
    // If we were smarter here we could match up transform[1] with additional_nodes[1]
    // and transform[2] with additional_nodes[0] to return a positive match.
    // TODO(fxb/107176): Disallow ambiguity with device group matching. We should log
    // a warning and return false if a device group node matches with multiple composite
    // driver nodes, and vice versa.
    let mut unmatched_additional_indices =
        (0..composite.additional_nodes.len()).collect::<HashSet<_>>();

    let primary_name: String = composite.symbol_table[&composite.primary_node.name_id].clone();
    let mut names = vec![primary_name];

    for i in 1..nodes.len() {
        let mut matched = None;
        let mut matched_name: Option<String> = None;
        for &j in &unmatched_additional_indices {
            let matches = node_matches_composite_driver(
                &nodes[i],
                &composite.additional_nodes[j].instructions,
                &composite.symbol_table,
            );
            if matches {
                matched = Some(j);
                matched_name =
                    Some(composite.symbol_table[&composite.additional_nodes[j].name_id].clone());
                break;
            }
        }

        if matched == None {
            return Ok(None);
        }

        unmatched_additional_indices.remove(&matched.unwrap());
        names.push(matched_name.unwrap());
    }

    let info = fdi::MatchedCompositeInfo {
        node_index: None,
        num_nodes: Some((composite.additional_nodes.len() + 1) as u32),
        composite_name: Some(composite.symbol_table[&composite.device_name_id].clone()),
        node_names: Some(collect_node_names_from_composite_rules(composite)),
        driver_info: Some(composite_driver.create_matched_driver_info()),
        ..fdi::MatchedCompositeInfo::EMPTY
    };

    return Ok(Some(MatchedComposite { info, names }));
}

fn node_matches_composite_driver(
    node: &fdf::DeviceGroupNode,
    bind_rules_node: &Vec<u8>,
    symbol_table: &HashMap<u32, String>,
) -> bool {
    match node_to_device_property(&node.transformation) {
        Err(_) => false,
        Ok(props) => {
            let match_bind_data = MatchBindData { symbol_table, instructions: bind_rules_node };
            match_bind(match_bind_data, &props).unwrap_or(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolved_driver::DriverPackageType;
    use bind::compiler::{
        CompiledBindRules, CompositeBindRules, CompositeNode, Symbol, SymbolicInstruction,
        SymbolicInstructionInfo,
    };
    use bind::interpreter::decode_bind_rules::DecodedRules;
    use bind::parser::bind_library::ValueType;
    use fuchsia_async as fasync;

    fn create_driver_with_rules<'a>(
        device_name: &str,
        primary_node: (&str, Vec<SymbolicInstructionInfo<'a>>),
        additionals: Vec<(&str, Vec<SymbolicInstructionInfo<'a>>)>,
    ) -> ResolvedDriver {
        let mut additional_nodes = vec![];
        for additional in additionals {
            additional_nodes
                .push(CompositeNode { name: additional.0.to_string(), instructions: additional.1 });
        }
        let bind_rules = CompositeBindRules {
            device_name: device_name.to_string(),
            symbol_table: HashMap::new(),
            primary_node: CompositeNode {
                name: primary_node.0.to_string(),
                instructions: primary_node.1,
            },
            additional_nodes: additional_nodes,
            enable_debug: false,
        };

        let bytecode = CompiledBindRules::CompositeBind(bind_rules).encode_to_bytecode().unwrap();
        let rules = DecodedRules::new(bytecode).unwrap();

        ResolvedDriver {
            component_url: url::Url::parse("fuchsia-pkg://fuchsia.com/package#driver/my-driver.cm")
                .unwrap(),
            v1_driver_path: None,
            bind_rules: rules,
            bind_bytecode: vec![],
            colocate: false,
            device_category: vec![],
            device_sub_category: vec![],
            fallback: false,
            package_type: DriverPackageType::Base,
            package_hash: None,
        }
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_property_match_node() {
        let node_properties_1 = vec![
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::IntValue(1),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::IntValue(200)],
            },
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::IntValue(3),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::BoolValue(true)],
            },
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::StringValue("killdeer".to_string()),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::StringValue("plover".to_string())],
            },
        ];

        let node_transformation_1 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(2)),
            value: Some(fdf::NodePropertyValue::BoolValue(false)),
            ..fdf::NodeProperty::EMPTY
        }];

        let node_properties_2 = vec![
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::StringValue("killdeer".to_string()),
                condition: fdf::Condition::Reject,
                values: vec![fdf::NodePropertyValue::StringValue("plover".to_string())],
            },
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::StringValue("flycatcher".to_string()),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::EnumValue("flycatcher.phoebe".to_string())],
            },
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::StringValue("yellowlegs".to_string()),
                condition: fdf::Condition::Reject,
                values: vec![fdf::NodePropertyValue::BoolValue(true)],
            },
        ];

        let node_transformation_2 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(3)),
            value: Some(fdf::NodePropertyValue::BoolValue(true)),
            ..fdf::NodeProperty::EMPTY
        }];

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Err(Status::NOT_FOUND.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    topological_path: Some("test/path".to_string()),
                    nodes: Some(vec![
                        fdf::DeviceGroupNode {
                            properties: node_properties_1,
                            transformation: node_transformation_1,
                        },
                        fdf::DeviceGroupNode {
                            properties: node_properties_2,
                            transformation: node_transformation_2,
                        },
                    ]),
                    ..fdf::DeviceGroup::EMPTY
                },
                vec![]
            )
        );

        // Match node 1.
        let mut device_properties_1: DeviceProperties = HashMap::new();
        device_properties_1.insert(PropertyKey::NumberKey(1), Symbol::NumberValue(200));
        device_properties_1.insert(
            PropertyKey::StringKey("kingfisher".to_string()),
            Symbol::StringValue("kookaburra".to_string()),
        );
        device_properties_1.insert(PropertyKey::NumberKey(3), Symbol::BoolValue(true));
        device_properties_1.insert(
            PropertyKey::StringKey("killdeer".to_string()),
            Symbol::StringValue("plover".to_string()),
        );

        let expected_device_group = fdi::MatchedDeviceGroupInfo {
            topological_path: Some("test/path".to_string()),
            node_index: Some(0),
            num_nodes: Some(2),
            ..fdi::MatchedDeviceGroupInfo::EMPTY
        };
        assert_eq!(
            Some(fdi::MatchedDriver::DeviceGroupNode(fdi::MatchedDeviceGroupNodeInfo {
                device_groups: Some(vec![expected_device_group]),
                ..fdi::MatchedDeviceGroupNodeInfo::EMPTY
            })),
            device_group_manager.match_device_group_nodes(&device_properties_1)
        );

        // Match node 2.
        let mut device_properties_2: DeviceProperties = HashMap::new();
        device_properties_2
            .insert(PropertyKey::StringKey("yellowlegs".to_string()), Symbol::BoolValue(false));
        device_properties_2.insert(
            PropertyKey::StringKey("killdeer".to_string()),
            Symbol::StringValue("lapwing".to_string()),
        );
        device_properties_2.insert(
            PropertyKey::StringKey("flycatcher".to_string()),
            Symbol::EnumValue("flycatcher.phoebe".to_string()),
        );

        let expected_device_group_2 = fdi::MatchedDeviceGroupInfo {
            topological_path: Some("test/path".to_string()),
            node_index: Some(1),
            num_nodes: Some(2),
            ..fdi::MatchedDeviceGroupInfo::EMPTY
        };
        assert_eq!(
            Some(fdi::MatchedDriver::DeviceGroupNode(fdi::MatchedDeviceGroupNodeInfo {
                device_groups: Some(vec![expected_device_group_2]),
                ..fdi::MatchedDeviceGroupNodeInfo::EMPTY
            })),
            device_group_manager.match_device_group_nodes(&device_properties_2)
        );
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_property_match_bool_edgecase() {
        let node_properties = vec![
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::IntValue(1),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::IntValue(200)],
            },
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::IntValue(3),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::BoolValue(false)],
            },
        ];

        let node_transformation = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(3)),
            value: Some(fdf::NodePropertyValue::BoolValue(true)),
            ..fdf::NodeProperty::EMPTY
        }];

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Err(Status::NOT_FOUND.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    topological_path: Some("test/path".to_string()),
                    nodes: Some(vec![fdf::DeviceGroupNode {
                        properties: node_properties,
                        transformation: node_transformation,
                    }]),
                    ..fdf::DeviceGroup::EMPTY
                },
                vec![]
            )
        );

        // Match node.
        let mut device_properties: DeviceProperties = HashMap::new();
        device_properties.insert(PropertyKey::NumberKey(1), Symbol::NumberValue(200));

        let expected_device_group = fdi::MatchedDeviceGroupInfo {
            topological_path: Some("test/path".to_string()),
            node_index: Some(0),
            num_nodes: Some(1),
            ..fdi::MatchedDeviceGroupInfo::EMPTY
        };
        assert_eq!(
            Some(fdi::MatchedDriver::DeviceGroupNode(fdi::MatchedDeviceGroupNodeInfo {
                device_groups: Some(vec![expected_device_group]),
                ..fdi::MatchedDeviceGroupNodeInfo::EMPTY
            })),
            device_group_manager.match_device_group_nodes(&device_properties)
        );
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_multiple_group_match() {
        let node_properties_1 = vec![
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::IntValue(1),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::IntValue(200)],
            },
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::IntValue(3),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::BoolValue(true)],
            },
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::StringValue("killdeer".to_string()),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::StringValue("plover".to_string())],
            },
        ];

        let node_transformation_1 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(2)),
            value: Some(fdf::NodePropertyValue::BoolValue(false)),
            ..fdf::NodeProperty::EMPTY
        }];

        let node_properties_2 = vec![
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::StringValue("killdeer".to_string()),
                condition: fdf::Condition::Reject,
                values: vec![fdf::NodePropertyValue::StringValue("plover".to_string())],
            },
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::StringValue("flycatcher".to_string()),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::EnumValue("flycatcher.phoebe".to_string())],
            },
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::StringValue("yellowlegs".to_string()),
                condition: fdf::Condition::Reject,
                values: vec![fdf::NodePropertyValue::BoolValue(true)],
            },
        ];

        let node_properties_2_rearranged = vec![
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::StringValue("flycatcher".to_string()),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::EnumValue("flycatcher.phoebe".to_string())],
            },
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::StringValue("killdeer".to_string()),
                condition: fdf::Condition::Reject,
                values: vec![fdf::NodePropertyValue::StringValue("plover".to_string())],
            },
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::StringValue("yellowlegs".to_string()),
                condition: fdf::Condition::Reject,
                values: vec![fdf::NodePropertyValue::BoolValue(true)],
            },
        ];

        let node_transformation_2 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(3)),
            value: Some(fdf::NodePropertyValue::BoolValue(true)),
            ..fdf::NodeProperty::EMPTY
        }];

        let node_properties_3 = vec![fdf::DeviceGroupProperty {
            key: fdf::NodePropertyKey::StringValue("cormorant".to_string()),
            condition: fdf::Condition::Accept,
            values: vec![fdf::NodePropertyValue::BoolValue(true)],
        }];

        let node_transformation_3 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::StringValue("anhinga".to_string())),
            value: Some(fdf::NodePropertyValue::BoolValue(false)),
            ..fdf::NodeProperty::EMPTY
        }];

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Err(Status::NOT_FOUND.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    topological_path: Some("test/path".to_string()),
                    nodes: Some(vec![
                        fdf::DeviceGroupNode {
                            properties: node_properties_1,
                            transformation: node_transformation_1,
                        },
                        fdf::DeviceGroupNode {
                            properties: node_properties_2,
                            transformation: node_transformation_2.clone(),
                        },
                    ]),
                    ..fdf::DeviceGroup::EMPTY
                },
                vec![]
            )
        );

        assert_eq!(
            Err(Status::NOT_FOUND.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    topological_path: Some("test/path2".to_string()),
                    nodes: Some(vec![
                        fdf::DeviceGroupNode {
                            properties: node_properties_2_rearranged,
                            transformation: node_transformation_2,
                        },
                        fdf::DeviceGroupNode {
                            properties: node_properties_3,
                            transformation: node_transformation_3,
                        },
                    ]),
                    ..fdf::DeviceGroup::EMPTY
                },
                vec![]
            )
        );

        // Match node.
        let mut device_properties: DeviceProperties = HashMap::new();
        device_properties
            .insert(PropertyKey::StringKey("yellowlegs".to_string()), Symbol::BoolValue(false));
        device_properties.insert(
            PropertyKey::StringKey("killdeer".to_string()),
            Symbol::StringValue("lapwing".to_string()),
        );
        device_properties.insert(
            PropertyKey::StringKey("flycatcher".to_string()),
            Symbol::EnumValue("flycatcher.phoebe".to_string()),
        );
        let match_result =
            device_group_manager.match_device_group_nodes(&device_properties).unwrap();

        assert!(if let fdi::MatchedDriver::DeviceGroupNode(matched_node_info) = match_result {
            let matched_device_groups = matched_node_info.device_groups.unwrap();
            assert_eq!(2, matched_device_groups.len());

            assert!(matched_device_groups.contains(&fdi::MatchedDeviceGroupInfo {
                topological_path: Some("test/path".to_string()),
                node_index: Some(1),
                num_nodes: Some(2),
                ..fdi::MatchedDeviceGroupInfo::EMPTY
            }));

            assert!(matched_device_groups.contains(&fdi::MatchedDeviceGroupInfo {
                topological_path: Some("test/path2".to_string()),
                node_index: Some(0),
                num_nodes: Some(2),
                ..fdi::MatchedDeviceGroupInfo::EMPTY
            }));

            true
        } else {
            false
        });
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_multiple_group_nodes_match() {
        let node_properties_1 = vec![
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::IntValue(1),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::IntValue(200)],
            },
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::StringValue("killdeer".to_string()),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::StringValue("plover".to_string())],
            },
        ];

        let node_transformation_1 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(2)),
            value: Some(fdf::NodePropertyValue::BoolValue(false)),
            ..fdf::NodeProperty::EMPTY
        }];

        let node_properties_2 = vec![
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::StringValue("killdeer".to_string()),
                condition: fdf::Condition::Reject,
                values: vec![fdf::NodePropertyValue::StringValue("plover".to_string())],
            },
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::StringValue("flycatcher".to_string()),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::EnumValue("flycatcher.phoebe".to_string())],
            },
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::StringValue("yellowlegs".to_string()),
                condition: fdf::Condition::Reject,
                values: vec![fdf::NodePropertyValue::BoolValue(true)],
            },
        ];

        let node_transformation_2 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(3)),
            value: Some(fdf::NodePropertyValue::BoolValue(true)),
            ..fdf::NodeProperty::EMPTY
        }];

        let node_properties_1_rearranged = vec![
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::StringValue("killdeer".to_string()),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::StringValue("plover".to_string())],
            },
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::IntValue(1),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::IntValue(200)],
            },
        ];

        let node_properties_3 = vec![fdf::DeviceGroupProperty {
            key: fdf::NodePropertyKey::StringValue("cormorant".to_string()),
            condition: fdf::Condition::Accept,
            values: vec![fdf::NodePropertyValue::BoolValue(true)],
        }];

        let node_transformation_3 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(3)),
            value: Some(fdf::NodePropertyValue::BoolValue(false)),
            ..fdf::NodeProperty::EMPTY
        }];

        let node_properties_4 = vec![fdf::DeviceGroupProperty {
            key: fdf::NodePropertyKey::IntValue(1),
            condition: fdf::Condition::Accept,
            values: vec![
                fdf::NodePropertyValue::IntValue(10),
                fdf::NodePropertyValue::IntValue(200),
            ],
        }];

        let node_transformation_4 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(2)),
            value: Some(fdf::NodePropertyValue::BoolValue(true)),
            ..fdf::NodeProperty::EMPTY
        }];

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Err(Status::NOT_FOUND.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    topological_path: Some("test/path".to_string()),
                    nodes: Some(vec![
                        fdf::DeviceGroupNode {
                            properties: node_properties_1,
                            transformation: node_transformation_1.clone(),
                        },
                        fdf::DeviceGroupNode {
                            properties: node_properties_2,
                            transformation: node_transformation_2,
                        },
                    ]),
                    ..fdf::DeviceGroup::EMPTY
                },
                vec![]
            )
        );

        assert_eq!(
            Err(Status::NOT_FOUND.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    topological_path: Some("test/path2".to_string()),
                    nodes: Some(vec![
                        fdf::DeviceGroupNode {
                            properties: node_properties_3,
                            transformation: node_transformation_3,
                        },
                        fdf::DeviceGroupNode {
                            properties: node_properties_1_rearranged,
                            transformation: node_transformation_1,
                        },
                    ]),
                    ..fdf::DeviceGroup::EMPTY
                },
                vec![]
            )
        );

        assert_eq!(
            Err(Status::NOT_FOUND.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    topological_path: Some("test/path3".to_string()),
                    nodes: Some(vec![fdf::DeviceGroupNode {
                        properties: node_properties_4,
                        transformation: node_transformation_4,
                    }]),
                    ..fdf::DeviceGroup::EMPTY
                },
                vec![]
            )
        );

        // Match node.
        let mut device_properties: DeviceProperties = HashMap::new();
        device_properties.insert(PropertyKey::NumberKey(1), Symbol::NumberValue(200));
        device_properties.insert(
            PropertyKey::StringKey("killdeer".to_string()),
            Symbol::StringValue("plover".to_string()),
        );
        let match_result =
            device_group_manager.match_device_group_nodes(&device_properties).unwrap();

        assert!(if let fdi::MatchedDriver::DeviceGroupNode(matched_node_info) = match_result {
            let matched_device_groups = matched_node_info.device_groups.unwrap();
            assert_eq!(3, matched_device_groups.len());

            assert!(matched_device_groups.contains(&fdi::MatchedDeviceGroupInfo {
                topological_path: Some("test/path".to_string()),
                node_index: Some(0),
                num_nodes: Some(2),
                ..fdi::MatchedDeviceGroupInfo::EMPTY
            }));

            assert!(matched_device_groups.contains(&fdi::MatchedDeviceGroupInfo {
                topological_path: Some("test/path2".to_string()),
                node_index: Some(1),
                num_nodes: Some(2),
                ..fdi::MatchedDeviceGroupInfo::EMPTY
            }));

            assert!(matched_device_groups.contains(&fdi::MatchedDeviceGroupInfo {
                topological_path: Some("test/path3".to_string()),
                node_index: Some(0),
                num_nodes: Some(1),
                ..fdi::MatchedDeviceGroupInfo::EMPTY
            }));

            true
        } else {
            false
        });
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_property_mismatch() {
        let node_properties_1 = vec![
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::IntValue(1),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::IntValue(200)],
            },
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::IntValue(3),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::BoolValue(true)],
            },
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::StringValue("killdeer".to_string()),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::StringValue("plover".to_string())],
            },
        ];

        let node_transformation_1 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(2)),
            value: Some(fdf::NodePropertyValue::BoolValue(false)),
            ..fdf::NodeProperty::EMPTY
        }];

        let node_properties_2 = vec![
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::StringValue("killdeer".to_string()),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::StringValue("plover".to_string())],
            },
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::StringValue("yellowlegs".to_string()),
                condition: fdf::Condition::Reject,
                values: vec![fdf::NodePropertyValue::BoolValue(false)],
            },
        ];

        let node_transformation_2 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(3)),
            value: Some(fdf::NodePropertyValue::BoolValue(true)),
            ..fdf::NodeProperty::EMPTY
        }];

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Err(Status::NOT_FOUND.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    topological_path: Some("test/path".to_string()),
                    nodes: Some(vec![
                        fdf::DeviceGroupNode {
                            properties: node_properties_1,
                            transformation: node_transformation_1,
                        },
                        fdf::DeviceGroupNode {
                            properties: node_properties_2,
                            transformation: node_transformation_2,
                        },
                    ]),
                    ..fdf::DeviceGroup::EMPTY
                },
                vec![]
            )
        );

        let mut device_properties: DeviceProperties = HashMap::new();
        device_properties.insert(PropertyKey::NumberKey(1), Symbol::NumberValue(200));
        device_properties.insert(
            PropertyKey::StringKey("kingfisher".to_string()),
            Symbol::StringValue("bee-eater".to_string()),
        );
        device_properties
            .insert(PropertyKey::StringKey("yellowlegs".to_string()), Symbol::BoolValue(false));
        device_properties.insert(
            PropertyKey::StringKey("killdeer".to_string()),
            Symbol::StringValue("plover".to_string()),
        );

        assert_eq!(None, device_group_manager.match_device_group_nodes(&device_properties));
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_property_match_list() {
        let node_properties_1 = vec![
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::IntValue(10),
                condition: fdf::Condition::Reject,
                values: vec![
                    fdf::NodePropertyValue::IntValue(200),
                    fdf::NodePropertyValue::IntValue(150),
                ],
            },
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::StringValue("plover".to_string()),
                condition: fdf::Condition::Accept,
                values: vec![
                    fdf::NodePropertyValue::StringValue("killdeer".to_string()),
                    fdf::NodePropertyValue::StringValue("lapwing".to_string()),
                ],
            },
        ];

        let node_transformation_1 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(1)),
            value: Some(fdf::NodePropertyValue::IntValue(100)),
            ..fdf::NodeProperty::EMPTY
        }];

        let node_properties_2 = vec![
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::IntValue(11),
                condition: fdf::Condition::Reject,
                values: vec![
                    fdf::NodePropertyValue::IntValue(20),
                    fdf::NodePropertyValue::IntValue(10),
                ],
            },
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::StringValue("dunlin".to_string()),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::BoolValue(true)],
            },
        ];

        let node_transformation_2 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(3)),
            value: Some(fdf::NodePropertyValue::BoolValue(true)),
            ..fdf::NodeProperty::EMPTY
        }];

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Err(Status::NOT_FOUND.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    topological_path: Some("test/path".to_string()),
                    nodes: Some(vec![
                        fdf::DeviceGroupNode {
                            properties: node_properties_1,
                            transformation: node_transformation_1,
                        },
                        fdf::DeviceGroupNode {
                            properties: node_properties_2,
                            transformation: node_transformation_2,
                        },
                    ]),
                    ..fdf::DeviceGroup::EMPTY
                },
                vec![]
            )
        );

        // Match node 1.
        let mut device_properties_1: DeviceProperties = HashMap::new();
        device_properties_1.insert(PropertyKey::NumberKey(10), Symbol::NumberValue(20));
        device_properties_1.insert(
            PropertyKey::StringKey("plover".to_string()),
            Symbol::StringValue("lapwing".to_string()),
        );

        let expected_device_group_1 = fdi::MatchedDeviceGroupInfo {
            topological_path: Some("test/path".to_string()),
            node_index: Some(0),
            num_nodes: Some(2),
            ..fdi::MatchedDeviceGroupInfo::EMPTY
        };
        assert_eq!(
            Some(fdi::MatchedDriver::DeviceGroupNode(fdi::MatchedDeviceGroupNodeInfo {
                device_groups: Some(vec![expected_device_group_1]),
                ..fdi::MatchedDeviceGroupNodeInfo::EMPTY
            })),
            device_group_manager.match_device_group_nodes(&device_properties_1)
        );

        // Match node 2.
        let mut device_properties_2: DeviceProperties = HashMap::new();
        device_properties_2.insert(PropertyKey::NumberKey(5), Symbol::NumberValue(20));
        device_properties_2
            .insert(PropertyKey::StringKey("dunlin".to_string()), Symbol::BoolValue(true));

        let expected_device_group_2 = fdi::MatchedDeviceGroupInfo {
            topological_path: Some("test/path".to_string()),
            node_index: Some(1),
            num_nodes: Some(2),
            ..fdi::MatchedDeviceGroupInfo::EMPTY
        };
        assert_eq!(
            Some(fdi::MatchedDriver::DeviceGroupNode(fdi::MatchedDeviceGroupNodeInfo {
                device_groups: Some(vec![expected_device_group_2]),
                ..fdi::MatchedDeviceGroupNodeInfo::EMPTY
            })),
            device_group_manager.match_device_group_nodes(&device_properties_2)
        );
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_property_mismatch_list() {
        let node_properties_1 = vec![
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::IntValue(10),
                condition: fdf::Condition::Reject,
                values: vec![
                    fdf::NodePropertyValue::IntValue(200),
                    fdf::NodePropertyValue::IntValue(150),
                ],
            },
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::StringValue("plover".to_string()),
                condition: fdf::Condition::Accept,
                values: vec![
                    fdf::NodePropertyValue::StringValue("killdeer".to_string()),
                    fdf::NodePropertyValue::StringValue("lapwing".to_string()),
                ],
            },
        ];

        let node_transformation_1 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(1)),
            value: Some(fdf::NodePropertyValue::IntValue(100)),
            ..fdf::NodeProperty::EMPTY
        }];

        let node_properties_2 = vec![
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::IntValue(11),
                condition: fdf::Condition::Reject,
                values: vec![
                    fdf::NodePropertyValue::IntValue(20),
                    fdf::NodePropertyValue::IntValue(10),
                ],
            },
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::IntValue(2),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::BoolValue(true)],
            },
        ];

        let node_transformation_2 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(3)),
            value: Some(fdf::NodePropertyValue::BoolValue(true)),
            ..fdf::NodeProperty::EMPTY
        }];

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Err(Status::NOT_FOUND.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    topological_path: Some("test/path".to_string()),
                    nodes: Some(vec![
                        fdf::DeviceGroupNode {
                            properties: node_properties_1,
                            transformation: node_transformation_1,
                        },
                        fdf::DeviceGroupNode {
                            properties: node_properties_2,
                            transformation: node_transformation_2,
                        },
                    ]),
                    ..fdf::DeviceGroup::EMPTY
                },
                vec![]
            )
        );

        // Match node 1.
        let mut device_properties_1: DeviceProperties = HashMap::new();
        device_properties_1.insert(PropertyKey::NumberKey(10), Symbol::NumberValue(200));
        device_properties_1.insert(
            PropertyKey::StringKey("plover".to_string()),
            Symbol::StringValue("lapwing".to_string()),
        );
        assert_eq!(None, device_group_manager.match_device_group_nodes(&device_properties_1));

        // Match node 2.
        let mut device_properties_2: DeviceProperties = HashMap::new();
        device_properties_2.insert(PropertyKey::NumberKey(11), Symbol::NumberValue(10));
        device_properties_2.insert(PropertyKey::NumberKey(2), Symbol::BoolValue(true));

        assert_eq!(None, device_group_manager.match_device_group_nodes(&device_properties_2));
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_property_multiple_value_types() {
        let node_properties = vec![fdf::DeviceGroupProperty {
            key: fdf::NodePropertyKey::IntValue(10),
            condition: fdf::Condition::Reject,
            values: vec![
                fdf::NodePropertyValue::IntValue(200),
                fdf::NodePropertyValue::BoolValue(false),
            ],
        }];

        let node_transformation = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(1)),
            value: Some(fdf::NodePropertyValue::IntValue(100)),
            ..fdf::NodeProperty::EMPTY
        }];

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Err(Status::INVALID_ARGS.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    topological_path: Some("test/path".to_string()),
                    nodes: Some(vec![fdf::DeviceGroupNode {
                        properties: node_properties,
                        transformation: node_transformation,
                    }]),
                    ..fdf::DeviceGroup::EMPTY
                },
                vec![]
            )
        );

        assert!(device_group_manager.device_group_nodes.is_empty());
        assert!(device_group_manager.device_group_list.is_empty());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_property_duplicate_key() {
        let node_properties = vec![
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::IntValue(10),
                condition: fdf::Condition::Reject,
                values: vec![fdf::NodePropertyValue::IntValue(200)],
            },
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::IntValue(10),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::IntValue(10)],
            },
        ];

        let node_transformation = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(3)),
            value: Some(fdf::NodePropertyValue::BoolValue(true)),
            ..fdf::NodeProperty::EMPTY
        }];

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Err(Status::INVALID_ARGS.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    topological_path: Some("test/path".to_string()),
                    nodes: Some(vec![fdf::DeviceGroupNode {
                        properties: node_properties,
                        transformation: node_transformation,
                    },]),
                    ..fdf::DeviceGroup::EMPTY
                },
                vec![]
            )
        );

        assert!(device_group_manager.device_group_nodes.is_empty());
        assert!(device_group_manager.device_group_list.is_empty());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_missing_node_properties() {
        let node_properties = vec![
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::IntValue(10),
                condition: fdf::Condition::Reject,
                values: vec![fdf::NodePropertyValue::IntValue(200)],
            },
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::IntValue(10),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::IntValue(10)],
            },
        ];

        let node_transformation_1 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(3)),
            value: Some(fdf::NodePropertyValue::BoolValue(true)),
            ..fdf::NodeProperty::EMPTY
        }];

        let node_transformation_2 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(10)),
            value: Some(fdf::NodePropertyValue::BoolValue(false)),
            ..fdf::NodeProperty::EMPTY
        }];

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Err(Status::INVALID_ARGS.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    topological_path: Some("test/path".to_string()),
                    nodes: Some(vec![
                        fdf::DeviceGroupNode {
                            properties: node_properties,
                            transformation: node_transformation_1,
                        },
                        fdf::DeviceGroupNode {
                            properties: vec![],
                            transformation: node_transformation_2
                        },
                    ]),
                    ..fdf::DeviceGroup::EMPTY
                },
                vec![]
            )
        );

        assert!(device_group_manager.device_group_nodes.is_empty());
        assert!(device_group_manager.device_group_list.is_empty());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_missing_device_group_fields() {
        let node_properties = vec![
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::IntValue(10),
                condition: fdf::Condition::Reject,
                values: vec![fdf::NodePropertyValue::IntValue(200)],
            },
            fdf::DeviceGroupProperty {
                key: fdf::NodePropertyKey::IntValue(10),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::IntValue(10)],
            },
        ];

        let node_transformation_1 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(3)),
            value: Some(fdf::NodePropertyValue::BoolValue(true)),
            ..fdf::NodeProperty::EMPTY
        }];

        let node_transformation_2 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(1)),
            value: Some(fdf::NodePropertyValue::BoolValue(false)),
            ..fdf::NodeProperty::EMPTY
        }];

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Err(Status::INVALID_ARGS.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    topological_path: None,
                    nodes: Some(vec![
                        fdf::DeviceGroupNode {
                            properties: node_properties,
                            transformation: node_transformation_1,
                        },
                        fdf::DeviceGroupNode {
                            properties: vec![],
                            transformation: node_transformation_2,
                        },
                    ]),
                    ..fdf::DeviceGroup::EMPTY
                },
                vec![]
            )
        );
        assert!(device_group_manager.device_group_nodes.is_empty());
        assert!(device_group_manager.device_group_list.is_empty());

        assert_eq!(
            Err(Status::INVALID_ARGS.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    topological_path: Some("test/path".to_string()),
                    nodes: None,
                    ..fdf::DeviceGroup::EMPTY
                },
                vec![]
            )
        );

        assert!(device_group_manager.device_group_nodes.is_empty());
        assert!(device_group_manager.device_group_list.is_empty());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_composite_match() {
        let primary_node_properties = vec![fdf::DeviceGroupProperty {
            key: fdf::NodePropertyKey::IntValue(1),
            condition: fdf::Condition::Accept,
            values: vec![fdf::NodePropertyValue::IntValue(200)],
        }];

        let additional_node_properties_1 = vec![fdf::DeviceGroupProperty {
            key: fdf::NodePropertyKey::IntValue(1),
            condition: fdf::Condition::Accept,
            values: vec![fdf::NodePropertyValue::IntValue(10)],
        }];

        let additional_node_properties_2 = vec![fdf::DeviceGroupProperty {
            key: fdf::NodePropertyKey::IntValue(10),
            condition: fdf::Condition::Accept,
            values: vec![fdf::NodePropertyValue::BoolValue(true)],
        }];

        let primary_key_1 = "whimbrel";
        let primary_val_1 = "sanderling";

        let additional_a_key_1 = 100;
        let additional_a_val_1 = 50;

        let additional_b_key_1 = "curlew";
        let additional_b_val_1 = 500;

        let device_name = "mimid";
        let primary_name = "catbird";
        let additional_a_name = "mockingbird";
        let additional_b_name = "lapwing";

        let primary_device_group_node = fdf::DeviceGroupNode {
            properties: primary_node_properties,
            transformation: vec![fdf::NodeProperty {
                key: Some(fdf::NodePropertyKey::StringValue(primary_key_1.to_string())),
                value: Some(fdf::NodePropertyValue::StringValue(primary_val_1.to_string())),
                ..fdf::NodeProperty::EMPTY
            }],
        };

        let primary_node_inst = vec![SymbolicInstructionInfo {
            location: None,
            instruction: SymbolicInstruction::AbortIfNotEqual {
                lhs: Symbol::Key(primary_key_1.to_string(), ValueType::Str),
                rhs: Symbol::StringValue(primary_val_1.to_string()),
            },
        }];

        let additional_device_group_node_a = fdf::DeviceGroupNode {
            properties: additional_node_properties_1,
            transformation: vec![fdf::NodeProperty {
                key: Some(fdf::NodePropertyKey::IntValue(additional_a_key_1)),
                value: Some(fdf::NodePropertyValue::IntValue(additional_a_val_1)),
                ..fdf::NodeProperty::EMPTY
            }],
        };

        let additional_node_a_inst = vec![
            SymbolicInstructionInfo {
                location: None,
                instruction: SymbolicInstruction::AbortIfNotEqual {
                    lhs: Symbol::DeprecatedKey(additional_a_key_1),
                    rhs: Symbol::NumberValue(additional_a_val_1.clone().into()),
                },
            },
            SymbolicInstructionInfo {
                location: None,
                instruction: SymbolicInstruction::AbortIfEqual {
                    lhs: Symbol::Key("NA".to_string(), ValueType::Number),
                    rhs: Symbol::NumberValue(500),
                },
            },
        ];

        let additional_device_group_node_b = fdf::DeviceGroupNode {
            properties: additional_node_properties_2,
            transformation: vec![fdf::NodeProperty {
                key: Some(fdf::NodePropertyKey::StringValue(additional_b_key_1.to_string())),
                value: Some(fdf::NodePropertyValue::IntValue(additional_b_val_1)),
                ..fdf::NodeProperty::EMPTY
            }],
        };

        let additional_node_b_inst = vec![SymbolicInstructionInfo {
            location: None,
            instruction: SymbolicInstruction::AbortIfNotEqual {
                lhs: Symbol::Key(additional_b_key_1.to_string(), ValueType::Number),
                rhs: Symbol::NumberValue(additional_b_val_1.clone().into()),
            },
        }];

        let composite_driver = create_driver_with_rules(
            device_name,
            (primary_name, primary_node_inst),
            vec![
                (additional_a_name, additional_node_a_inst),
                (additional_b_name, additional_node_b_inst),
            ],
        );

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Ok((
                fdi::MatchedCompositeInfo {
                    node_index: None,
                    num_nodes: Some(3),
                    composite_name: Some(device_name.to_string()),
                    node_names: Some(vec![
                        primary_name.to_string(),
                        additional_a_name.to_string(),
                        additional_b_name.to_string()
                    ]),
                    driver_info: Some(composite_driver.clone().create_matched_driver_info()),
                    ..fdi::MatchedCompositeInfo::EMPTY
                },
                vec![
                    primary_name.to_string(),
                    additional_b_name.to_string(),
                    additional_a_name.to_string()
                ]
            )),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    topological_path: Some("test/path".to_string()),
                    nodes: Some(vec![
                        primary_device_group_node,
                        additional_device_group_node_b,
                        additional_device_group_node_a,
                    ]),
                    ..fdf::DeviceGroup::EMPTY
                },
                vec![&composite_driver]
            )
        );

        // Match additional node A, the last node in the device group at index 2.
        let mut device_properties_1: DeviceProperties = HashMap::new();
        device_properties_1.insert(PropertyKey::NumberKey(1), Symbol::NumberValue(10));

        let expected_device_group = fdi::MatchedDeviceGroupInfo {
            topological_path: Some("test/path".to_string()),
            node_index: Some(2),
            num_nodes: Some(3),
            node_names: Some(vec![
                primary_name.to_string(),
                additional_b_name.to_string(),
                additional_a_name.to_string(),
            ]),
            composite: Some(fdi::MatchedCompositeInfo {
                node_index: None,
                num_nodes: Some(3),
                composite_name: Some(device_name.to_string()),
                node_names: Some(vec![
                    primary_name.to_string(),
                    additional_a_name.to_string(),
                    additional_b_name.to_string(),
                ]),
                driver_info: Some(composite_driver.clone().create_matched_driver_info()),
                ..fdi::MatchedCompositeInfo::EMPTY
            }),
            ..fdi::MatchedDeviceGroupInfo::EMPTY
        };
        assert_eq!(
            Some(fdi::MatchedDriver::DeviceGroupNode(fdi::MatchedDeviceGroupNodeInfo {
                device_groups: Some(vec![expected_device_group]),
                ..fdi::MatchedDeviceGroupNodeInfo::EMPTY
            })),
            device_group_manager.match_device_group_nodes(&device_properties_1)
        );
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_composite_mismatch() {
        let primary_node_properties = vec![fdf::DeviceGroupProperty {
            key: fdf::NodePropertyKey::IntValue(1),
            condition: fdf::Condition::Accept,
            values: vec![fdf::NodePropertyValue::IntValue(200)],
        }];

        let additional_node_properties_1 = vec![fdf::DeviceGroupProperty {
            key: fdf::NodePropertyKey::IntValue(1),
            condition: fdf::Condition::Accept,
            values: vec![fdf::NodePropertyValue::IntValue(10)],
        }];

        let additional_node_properties_2 = vec![fdf::DeviceGroupProperty {
            key: fdf::NodePropertyKey::IntValue(10),
            condition: fdf::Condition::Accept,
            values: vec![fdf::NodePropertyValue::BoolValue(false)],
        }];

        let primary_key_1 = "whimbrel";
        let primary_val_1 = "sanderling";

        let additional_a_key_1 = 100;
        let additional_a_val_1 = 50;

        let additional_b_key_1 = "curlew";
        let additional_b_val_1 = 500;

        let device_name = "mimid";
        let primary_name = "catbird";
        let additional_a_name = "mockingbird";
        let additional_b_name = "lapwing";

        let primary_node_inst = vec![SymbolicInstructionInfo {
            location: None,
            instruction: SymbolicInstruction::AbortIfNotEqual {
                lhs: Symbol::Key(primary_key_1.to_string(), ValueType::Str),
                rhs: Symbol::StringValue(primary_val_1.to_string()),
            },
        }];

        let primary_device_group_node = fdf::DeviceGroupNode {
            properties: primary_node_properties,
            transformation: vec![fdf::NodeProperty {
                key: Some(fdf::NodePropertyKey::StringValue(primary_key_1.to_string())),
                value: Some(fdf::NodePropertyValue::StringValue(primary_val_1.to_string())),
                ..fdf::NodeProperty::EMPTY
            }],
        };

        let additional_node_a_inst = vec![
            SymbolicInstructionInfo {
                location: None,
                instruction: SymbolicInstruction::AbortIfNotEqual {
                    lhs: Symbol::Key(additional_b_key_1.to_string(), ValueType::Number),
                    rhs: Symbol::NumberValue(additional_b_val_1.clone().into()),
                },
            },
            SymbolicInstructionInfo {
                location: None,
                // This does not exist in our transform so we expect it to not match.
                instruction: SymbolicInstruction::AbortIfNotEqual {
                    lhs: Symbol::Key("NA".to_string(), ValueType::Number),
                    rhs: Symbol::NumberValue(500),
                },
            },
        ];

        let additional_device_group_node_a = fdf::DeviceGroupNode {
            properties: additional_node_properties_1,
            transformation: vec![fdf::NodeProperty {
                key: Some(fdf::NodePropertyKey::StringValue(additional_b_key_1.to_string())),
                value: Some(fdf::NodePropertyValue::IntValue(additional_b_val_1)),
                ..fdf::NodeProperty::EMPTY
            }],
        };

        let additional_node_b_inst = vec![SymbolicInstructionInfo {
            location: None,
            instruction: SymbolicInstruction::AbortIfNotEqual {
                lhs: Symbol::DeprecatedKey(additional_a_key_1.clone()),
                rhs: Symbol::NumberValue(additional_a_val_1.clone().into()),
            },
        }];

        let additional_device_group_node_b = fdf::DeviceGroupNode {
            properties: additional_node_properties_2,
            transformation: vec![fdf::NodeProperty {
                key: Some(fdf::NodePropertyKey::IntValue(additional_a_key_1)),
                value: Some(fdf::NodePropertyValue::IntValue(additional_a_val_1)),
                ..fdf::NodeProperty::EMPTY
            }],
        };

        let composite_driver = create_driver_with_rules(
            device_name,
            (primary_name, primary_node_inst),
            vec![
                (additional_a_name, additional_node_a_inst),
                (additional_b_name, additional_node_b_inst),
            ],
        );

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Err(Status::NOT_FOUND.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    topological_path: Some("test/path".to_string()),
                    nodes: Some(vec![
                        primary_device_group_node,
                        additional_device_group_node_a,
                        additional_device_group_node_b
                    ]),
                    ..fdf::DeviceGroup::EMPTY
                },
                vec![&composite_driver]
            )
        );
    }
}
