// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::match_common::{get_composite_rules_from_composite_driver, node_to_device_property},
    crate::resolved_driver::ResolvedDriver,
    bind::compiler::symbol_table::{get_deprecated_key_identifier, get_deprecated_key_value},
    bind::compiler::Symbol,
    bind::interpreter::match_bind::{match_bind, DeviceProperties, MatchBindData, PropertyKey},
    fidl_fuchsia_driver_development as fdd, fidl_fuchsia_driver_framework as fdf,
    fidl_fuchsia_driver_index as fdi,
    fuchsia_zircon::{zx_status_t, Status},
    regex::Regex,
    std::collections::{BTreeMap, HashMap, HashSet},
};

const NAME_REGEX: &'static str = r"^[a-zA-Z0-9\-_]*$";

#[derive(Debug, Eq, Hash, PartialEq)]
pub struct BindRuleCondition {
    condition: fdf::Condition,
    values: Vec<Symbol>,
}

type DeviceGroupNodeBindRules = BTreeMap<PropertyKey, BindRuleCondition>;

struct MatchedComposite {
    pub info: fdi::MatchedCompositeInfo,
    pub names: Vec<String>,
    pub primary_index: u32,
}

struct DeviceGroupInfo {
    pub nodes: Vec<fdf::DeviceGroupNode>,

    // The composite driver matched to the device group.
    pub matched: Option<MatchedComposite>,
}

// The DeviceGroupManager struct is responsible of managing a list of device groups
// for matching.
pub struct DeviceGroupManager {
    // Maps a list of device groups to the bind rules of their nodes. This is to handle multiple
    // device groups that share a node with the same bind rules. Used for matching nodes.
    pub device_group_nodes: HashMap<DeviceGroupNodeBindRules, Vec<fdi::MatchedDeviceGroupInfo>>,

    // Maps device groups to the name. This list ensures that we don't add multiple groups with
    // the same name.
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
        // Get and validate the name.
        let name = group.name.ok_or(Status::INVALID_ARGS.into_raw())?;
        if let Ok(name_regex) = Regex::new(NAME_REGEX) {
            if !name_regex.is_match(&name) {
                log::error!(
                    "Invalid device group name. Name can only contain [A-Za-z0-9-_] characters"
                );
                return Err(Status::INVALID_ARGS.into_raw());
            }
        } else {
            log::warn!("Regex failure. Unable to validate device group name");
        }

        let nodes = group.nodes.ok_or(Status::INVALID_ARGS.into_raw())?;

        if self.device_group_list.contains_key(&name) {
            return Err(Status::ALREADY_EXISTS.into_raw());
        }

        if nodes.is_empty() {
            return Err(Status::INVALID_ARGS.into_raw());
        }

        // Collect device group nodes in a separate vector before adding them to the device group
        // manager. This is to ensure that we add the nodes after they're all verified to be valid.
        // TODO(fxb/105562): Update tests so that we can verify that bind_properties exists in
        // each node.
        let mut device_group_nodes: Vec<(DeviceGroupNodeBindRules, fdi::MatchedDeviceGroupInfo)> =
            vec![];
        for (node_idx, node) in nodes.iter().enumerate() {
            let properties = convert_fidl_to_bind_rules(&node.bind_rules)?;
            let device_group_info = fdi::MatchedDeviceGroupInfo {
                name: Some(name.clone()),
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
            let matched_composite = match_composite_bind_properties(composite_driver, &nodes)?;
            if let Some(matched_composite) = matched_composite {
                // Found a match so we can set this in our map.
                self.device_group_list.insert(
                    name.clone(),
                    DeviceGroupInfo {
                        nodes,
                        matched: Some(MatchedComposite {
                            info: matched_composite.info.clone(),
                            names: matched_composite.names.clone(),
                            primary_index: matched_composite.primary_index,
                        }),
                    },
                );
                return Ok((matched_composite.info, matched_composite.names));
            }
        }

        self.device_group_list.insert(name, DeviceGroupInfo { nodes, matched: None });
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
                match_composite_bind_properties(&resolved_driver, &dev_group.nodes);
            if let Ok(Some(matched_composite)) = matched_composite_result {
                dev_group.matched = Some(matched_composite);
            }
        }
    }

    pub fn get_node_groups(&self, name_filter: Option<String>) -> Vec<fdd::NodeGroupInfo> {
        if let Some(name) = name_filter {
            match self.device_group_list.get(&name) {
                Some(item) => return vec![to_node_group_info(&name, item)],
                None => return vec![],
            }
        };

        let node_groups = self
            .device_group_list
            .iter()
            .map(|(name, device_group_info)| to_node_group_info(name, device_group_info))
            .collect::<Vec<_>>();

        return node_groups;
    }

    fn device_group_add_composite_info(
        &self,
        mut info: fdi::MatchedDeviceGroupInfo,
    ) -> Option<fdi::MatchedDeviceGroupInfo> {
        if let Some(name) = &info.name {
            let list_value = self.device_group_list.get(name);
            if let Some(device_group) = list_value {
                // TODO(fxb/107371): Only return device groups that have a matched composite.
                if let Some(matched) = &device_group.matched {
                    info.composite = Some(matched.info.clone());
                    info.node_names = Some(matched.names.clone());
                    info.primary_index = Some(matched.primary_index);
                }

                return Some(info);
            }
        }

        return None;
    }
}

fn to_node_group_info(name: &str, device_group_info: &DeviceGroupInfo) -> fdd::NodeGroupInfo {
    match &device_group_info.matched {
        Some(matched_driver) => {
            let driver = match &matched_driver.info.driver_info {
                Some(driver_info) => driver_info.url.clone().or(driver_info.driver_url.clone()),
                None => None,
            };
            fdd::NodeGroupInfo {
                name: Some(name.to_string()),
                driver,
                primary_index: Some(matched_driver.primary_index),
                node_names: Some(matched_driver.names.clone()),
                nodes: Some(device_group_info.nodes.clone()),
                ..fdd::NodeGroupInfo::EMPTY
            }
        }
        None => fdd::NodeGroupInfo {
            name: Some(name.to_string()),
            nodes: Some(device_group_info.nodes.clone()),
            ..fdd::NodeGroupInfo::EMPTY
        },
    }
}

fn convert_fidl_to_bind_rules(
    fidl_bind_rules: &Vec<fdf::BindRule>,
) -> Result<DeviceGroupNodeBindRules, zx_status_t> {
    if fidl_bind_rules.is_empty() {
        return Err(Status::INVALID_ARGS.into_raw());
    }

    let mut bind_rules = BTreeMap::new();
    for fidl_rule in fidl_bind_rules {
        let key = match &fidl_rule.key {
            fdf::NodePropertyKey::IntValue(i) => PropertyKey::NumberKey(i.clone().into()),
            fdf::NodePropertyKey::StringValue(s) => PropertyKey::StringKey(s.clone()),
        };

        // Check if the properties contain duplicate keys.
        if bind_rules.contains_key(&key) {
            return Err(Status::INVALID_ARGS.into_raw());
        }

        let first_val = fidl_rule.values.first().ok_or(Status::INVALID_ARGS.into_raw())?;
        let values = fidl_rule
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

        bind_rules
            .insert(key, BindRuleCondition { condition: fidl_rule.condition, values: values });
    }
    Ok(bind_rules)
}

fn match_node(bind_rules: &DeviceGroupNodeBindRules, device_properties: &DeviceProperties) -> bool {
    for (key, node_prop_values) in bind_rules.iter() {
        let mut dev_prop_contains_value = match device_properties.get(key) {
            Some(val) => node_prop_values.values.contains(val),
            None => false,
        };

        // If the properties don't contain the key, try to convert it to a deprecated
        // key and check the properties with it.
        if !dev_prop_contains_value && !device_properties.contains_key(key) {
            let deprecated_key = match key {
                PropertyKey::NumberKey(int_key) => get_deprecated_key_identifier(*int_key as u32)
                    .map(|key| PropertyKey::StringKey(key)),
                PropertyKey::StringKey(str_key) => {
                    get_deprecated_key_value(str_key).map(|key| PropertyKey::NumberKey(key as u64))
                }
            };

            if let Some(key) = deprecated_key {
                dev_prop_contains_value = match device_properties.get(&key) {
                    Some(val) => node_prop_values.values.contains(val),
                    None => false,
                };
            }
        }

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

fn match_composite_bind_properties<'a>(
    composite_driver: &'a ResolvedDriver,
    nodes: &'a Vec<fdf::DeviceGroupNode>,
) -> Result<Option<MatchedComposite>, i32> {
    // The device group must have at least 1 node to match a composite driver.
    if nodes.len() < 1 {
        return Ok(None);
    }

    let composite = get_composite_rules_from_composite_driver(composite_driver)?;

    // The composite driver bind rules should have a total node count of more than or equal to the
    // total node count of the device group. This is to account for optional nodes in the
    // composite driver bind rules.
    if composite.optional_nodes.len() + composite.additional_nodes.len() + 1 < nodes.len() {
        return Ok(None);
    }

    // First find a matching primary node.
    let mut primary_index = 0;
    let mut primary_matches = false;
    for i in 0..nodes.len() {
        primary_matches = node_matches_composite_driver(
            &nodes[i],
            &composite.primary_node.instructions,
            &composite.symbol_table,
        );
        if primary_matches {
            primary_index = i as u32;
            break;
        }
    }

    if !primary_matches {
        return Ok(None);
    }

    // The remaining nodes in the bind_properties can match the
    // additional nodes in the bind rules in any order.
    //
    // This logic has one issue that we are accepting as a tradeoff for simplicity:
    // If a bind_properties node can match to multiple bind rule
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
    let mut unmatched_optional_indices =
        (0..composite.optional_nodes.len()).collect::<HashSet<_>>();

    let mut names = vec![];

    for i in 0..nodes.len() {
        if i == primary_index as usize {
            names.push(composite.symbol_table[&composite.primary_node.name_id].clone());
            continue;
        }

        let mut matched = None;
        let mut matched_name: Option<String> = None;
        let mut from_optional = false;

        // First check if any of the additional nodes match it.
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

        // If no additional nodes matched it, then look in the optional nodes.
        if matched.is_none() {
            for &j in &unmatched_optional_indices {
                let matches = node_matches_composite_driver(
                    &nodes[i],
                    &composite.optional_nodes[j].instructions,
                    &composite.symbol_table,
                );
                if matches {
                    from_optional = true;
                    matched = Some(j);
                    matched_name =
                        Some(composite.symbol_table[&composite.optional_nodes[j].name_id].clone());
                    break;
                }
            }
        }

        if matched.is_none() {
            return Ok(None);
        }

        if from_optional {
            unmatched_optional_indices.remove(&matched.unwrap());
        } else {
            unmatched_additional_indices.remove(&matched.unwrap());
        }

        names.push(matched_name.unwrap());
    }

    // If we didn't consume all of the additional nodes in the bind rules then this is not a match.
    if !unmatched_additional_indices.is_empty() {
        return Ok(None);
    }

    let info = fdi::MatchedCompositeInfo {
        node_index: None,
        num_nodes: None,
        composite_name: Some(composite.symbol_table[&composite.device_name_id].clone()),
        node_names: None,
        driver_info: Some(composite_driver.create_matched_driver_info()),
        ..fdi::MatchedCompositeInfo::EMPTY
    };
    return Ok(Some(MatchedComposite { info, names, primary_index }));
}

fn node_matches_composite_driver(
    node: &fdf::DeviceGroupNode,
    bind_rules_node: &Vec<u8>,
    symbol_table: &HashMap<u32, String>,
) -> bool {
    match node_to_device_property(&node.bind_properties) {
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
        optionals: Vec<(&str, Vec<SymbolicInstructionInfo<'a>>)>,
    ) -> ResolvedDriver {
        let mut additional_nodes = vec![];
        let mut optional_nodes = vec![];
        for additional in additionals {
            additional_nodes
                .push(CompositeNode { name: additional.0.to_string(), instructions: additional.1 });
        }
        for optional in optionals {
            optional_nodes
                .push(CompositeNode { name: optional.0.to_string(), instructions: optional.1 });
        }
        let bind_rules = CompositeBindRules {
            device_name: device_name.to_string(),
            symbol_table: HashMap::new(),
            primary_node: CompositeNode {
                name: primary_node.0.to_string(),
                instructions: primary_node.1,
            },
            additional_nodes: additional_nodes,
            optional_nodes: optional_nodes,
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
            device_categories: vec![],
            fallback: false,
            package_type: DriverPackageType::Base,
            package_hash: None,
        }
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_property_match_node() {
        let bind_rules_1 = vec![
            fdf::BindRule {
                key: fdf::NodePropertyKey::IntValue(1),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::IntValue(200)],
            },
            fdf::BindRule {
                key: fdf::NodePropertyKey::IntValue(3),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::BoolValue(true)],
            },
            fdf::BindRule {
                key: fdf::NodePropertyKey::StringValue("killdeer".to_string()),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::StringValue("plover".to_string())],
            },
        ];

        let bind_properties_1 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(2)),
            value: Some(fdf::NodePropertyValue::BoolValue(false)),
            ..fdf::NodeProperty::EMPTY
        }];

        let bind_rules_2 = vec![
            fdf::BindRule {
                key: fdf::NodePropertyKey::StringValue("killdeer".to_string()),
                condition: fdf::Condition::Reject,
                values: vec![fdf::NodePropertyValue::StringValue("plover".to_string())],
            },
            fdf::BindRule {
                key: fdf::NodePropertyKey::StringValue("flycatcher".to_string()),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::EnumValue("flycatcher.phoebe".to_string())],
            },
            fdf::BindRule {
                key: fdf::NodePropertyKey::StringValue("yellowlegs".to_string()),
                condition: fdf::Condition::Reject,
                values: vec![fdf::NodePropertyValue::BoolValue(true)],
            },
        ];

        let bind_properties_2 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(3)),
            value: Some(fdf::NodePropertyValue::BoolValue(true)),
            ..fdf::NodeProperty::EMPTY
        }];

        let nodes = Some(vec![
            fdf::DeviceGroupNode { bind_rules: bind_rules_1, bind_properties: bind_properties_1 },
            fdf::DeviceGroupNode { bind_rules: bind_rules_2, bind_properties: bind_properties_2 },
        ]);

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Err(Status::NOT_FOUND.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    name: Some("test_group".to_string()),
                    nodes: nodes.clone(),
                    ..fdf::DeviceGroup::EMPTY
                },
                vec![]
            )
        );

        assert_eq!(1, device_group_manager.get_node_groups(None).len());
        assert_eq!(0, device_group_manager.get_node_groups(Some("not_there".to_string())).len());
        let node_groups = device_group_manager.get_node_groups(Some("test_group".to_string()));
        assert_eq!(1, node_groups.len());
        let node_group = &node_groups[0];
        let expected_node_group = fdd::NodeGroupInfo {
            name: Some("test_group".to_string()),
            nodes,
            ..fdd::NodeGroupInfo::EMPTY
        };

        assert_eq!(&expected_node_group, node_group);

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
            name: Some("test_group".to_string()),
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
            name: Some("test_group".to_string()),
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
        let bind_rules = vec![
            fdf::BindRule {
                key: fdf::NodePropertyKey::IntValue(1),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::IntValue(200)],
            },
            fdf::BindRule {
                key: fdf::NodePropertyKey::IntValue(3),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::BoolValue(false)],
            },
        ];

        let bind_properties = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(3)),
            value: Some(fdf::NodePropertyValue::BoolValue(true)),
            ..fdf::NodeProperty::EMPTY
        }];

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Err(Status::NOT_FOUND.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    name: Some("test_group".to_string()),
                    nodes: Some(vec![fdf::DeviceGroupNode {
                        bind_rules: bind_rules,
                        bind_properties: bind_properties,
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
            name: Some("test_group".to_string()),
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
    async fn test_deprecated_keys_match() {
        let bind_rules = vec![
            fdf::BindRule {
                key: fdf::NodePropertyKey::StringValue("fuchsia.BIND_PROTOCOL".to_string()),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::IntValue(200)],
            },
            fdf::BindRule {
                key: fdf::NodePropertyKey::IntValue(0x0201), // "fuchsia.BIND_USB_PID"
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::IntValue(10)],
            },
        ];

        let bind_properties = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(0x01)),
            value: Some(fdf::NodePropertyValue::IntValue(50)),
            ..fdf::NodeProperty::EMPTY
        }];

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Err(Status::NOT_FOUND.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    name: Some("test_group".to_string()),
                    nodes: Some(vec![fdf::DeviceGroupNode {
                        bind_rules: bind_rules,
                        bind_properties: bind_properties,
                    }]),
                    ..fdf::DeviceGroup::EMPTY
                },
                vec![]
            )
        );

        // Match node.
        let mut device_properties: DeviceProperties = HashMap::new();
        device_properties.insert(
            PropertyKey::NumberKey(1), /* "fuchsia.BIND_PROTOCOL" */
            Symbol::NumberValue(200),
        );
        device_properties.insert(
            PropertyKey::StringKey("fuchsia.BIND_USB_PID".to_string()),
            Symbol::NumberValue(10),
        );

        let expected_device_group = fdi::MatchedDeviceGroupInfo {
            name: Some("test_group".to_string()),
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
        let bind_rules_1 = vec![
            fdf::BindRule {
                key: fdf::NodePropertyKey::IntValue(1),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::IntValue(200)],
            },
            fdf::BindRule {
                key: fdf::NodePropertyKey::IntValue(3),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::BoolValue(true)],
            },
            fdf::BindRule {
                key: fdf::NodePropertyKey::StringValue("killdeer".to_string()),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::StringValue("plover".to_string())],
            },
        ];

        let bind_properties_1 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(2)),
            value: Some(fdf::NodePropertyValue::BoolValue(false)),
            ..fdf::NodeProperty::EMPTY
        }];

        let bind_rules_2 = vec![
            fdf::BindRule {
                key: fdf::NodePropertyKey::StringValue("killdeer".to_string()),
                condition: fdf::Condition::Reject,
                values: vec![fdf::NodePropertyValue::StringValue("plover".to_string())],
            },
            fdf::BindRule {
                key: fdf::NodePropertyKey::StringValue("flycatcher".to_string()),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::EnumValue("flycatcher.phoebe".to_string())],
            },
            fdf::BindRule {
                key: fdf::NodePropertyKey::StringValue("yellowlegs".to_string()),
                condition: fdf::Condition::Reject,
                values: vec![fdf::NodePropertyValue::BoolValue(true)],
            },
        ];

        let bind_rules_2_rearranged = vec![
            fdf::BindRule {
                key: fdf::NodePropertyKey::StringValue("flycatcher".to_string()),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::EnumValue("flycatcher.phoebe".to_string())],
            },
            fdf::BindRule {
                key: fdf::NodePropertyKey::StringValue("killdeer".to_string()),
                condition: fdf::Condition::Reject,
                values: vec![fdf::NodePropertyValue::StringValue("plover".to_string())],
            },
            fdf::BindRule {
                key: fdf::NodePropertyKey::StringValue("yellowlegs".to_string()),
                condition: fdf::Condition::Reject,
                values: vec![fdf::NodePropertyValue::BoolValue(true)],
            },
        ];

        let bind_properties_2 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(3)),
            value: Some(fdf::NodePropertyValue::BoolValue(true)),
            ..fdf::NodeProperty::EMPTY
        }];

        let bind_rules_3 = vec![fdf::BindRule {
            key: fdf::NodePropertyKey::StringValue("cormorant".to_string()),
            condition: fdf::Condition::Accept,
            values: vec![fdf::NodePropertyValue::BoolValue(true)],
        }];

        let bind_properties_3 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::StringValue("anhinga".to_string())),
            value: Some(fdf::NodePropertyValue::BoolValue(false)),
            ..fdf::NodeProperty::EMPTY
        }];

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Err(Status::NOT_FOUND.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    name: Some("test_group".to_string()),
                    nodes: Some(vec![
                        fdf::DeviceGroupNode {
                            bind_rules: bind_rules_1,
                            bind_properties: bind_properties_1,
                        },
                        fdf::DeviceGroupNode {
                            bind_rules: bind_rules_2,
                            bind_properties: bind_properties_2.clone(),
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
                    name: Some("test_group2".to_string()),
                    nodes: Some(vec![
                        fdf::DeviceGroupNode {
                            bind_rules: bind_rules_2_rearranged,
                            bind_properties: bind_properties_2,
                        },
                        fdf::DeviceGroupNode {
                            bind_rules: bind_rules_3,
                            bind_properties: bind_properties_3,
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
                name: Some("test_group".to_string()),
                node_index: Some(1),
                num_nodes: Some(2),
                ..fdi::MatchedDeviceGroupInfo::EMPTY
            }));

            assert!(matched_device_groups.contains(&fdi::MatchedDeviceGroupInfo {
                name: Some("test_group2".to_string()),
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
        let bind_rules_1 = vec![
            fdf::BindRule {
                key: fdf::NodePropertyKey::IntValue(1),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::IntValue(200)],
            },
            fdf::BindRule {
                key: fdf::NodePropertyKey::StringValue("killdeer".to_string()),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::StringValue("plover".to_string())],
            },
        ];

        let bind_properties_1 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(2)),
            value: Some(fdf::NodePropertyValue::BoolValue(false)),
            ..fdf::NodeProperty::EMPTY
        }];

        let bind_rules_2 = vec![
            fdf::BindRule {
                key: fdf::NodePropertyKey::StringValue("killdeer".to_string()),
                condition: fdf::Condition::Reject,
                values: vec![fdf::NodePropertyValue::StringValue("plover".to_string())],
            },
            fdf::BindRule {
                key: fdf::NodePropertyKey::StringValue("flycatcher".to_string()),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::EnumValue("flycatcher.phoebe".to_string())],
            },
            fdf::BindRule {
                key: fdf::NodePropertyKey::StringValue("yellowlegs".to_string()),
                condition: fdf::Condition::Reject,
                values: vec![fdf::NodePropertyValue::BoolValue(true)],
            },
        ];

        let bind_properties_2 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(3)),
            value: Some(fdf::NodePropertyValue::BoolValue(true)),
            ..fdf::NodeProperty::EMPTY
        }];

        let bind_rules_1_rearranged = vec![
            fdf::BindRule {
                key: fdf::NodePropertyKey::StringValue("killdeer".to_string()),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::StringValue("plover".to_string())],
            },
            fdf::BindRule {
                key: fdf::NodePropertyKey::IntValue(1),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::IntValue(200)],
            },
        ];

        let bind_rules_3 = vec![fdf::BindRule {
            key: fdf::NodePropertyKey::StringValue("cormorant".to_string()),
            condition: fdf::Condition::Accept,
            values: vec![fdf::NodePropertyValue::BoolValue(true)],
        }];

        let bind_properties_3 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(3)),
            value: Some(fdf::NodePropertyValue::BoolValue(false)),
            ..fdf::NodeProperty::EMPTY
        }];

        let bind_rules_4 = vec![fdf::BindRule {
            key: fdf::NodePropertyKey::IntValue(1),
            condition: fdf::Condition::Accept,
            values: vec![
                fdf::NodePropertyValue::IntValue(10),
                fdf::NodePropertyValue::IntValue(200),
            ],
        }];

        let bind_properties_4 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(2)),
            value: Some(fdf::NodePropertyValue::BoolValue(true)),
            ..fdf::NodeProperty::EMPTY
        }];

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Err(Status::NOT_FOUND.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    name: Some("test_group".to_string()),
                    nodes: Some(vec![
                        fdf::DeviceGroupNode {
                            bind_rules: bind_rules_1,
                            bind_properties: bind_properties_1.clone(),
                        },
                        fdf::DeviceGroupNode {
                            bind_rules: bind_rules_2,
                            bind_properties: bind_properties_2,
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
                    name: Some("test_group2".to_string()),
                    nodes: Some(vec![
                        fdf::DeviceGroupNode {
                            bind_rules: bind_rules_3,
                            bind_properties: bind_properties_3,
                        },
                        fdf::DeviceGroupNode {
                            bind_rules: bind_rules_1_rearranged,
                            bind_properties: bind_properties_1,
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
                    name: Some("test_group3".to_string()),
                    nodes: Some(vec![fdf::DeviceGroupNode {
                        bind_rules: bind_rules_4,
                        bind_properties: bind_properties_4,
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
                name: Some("test_group".to_string()),
                node_index: Some(0),
                num_nodes: Some(2),
                ..fdi::MatchedDeviceGroupInfo::EMPTY
            }));

            assert!(matched_device_groups.contains(&fdi::MatchedDeviceGroupInfo {
                name: Some("test_group2".to_string()),
                node_index: Some(1),
                num_nodes: Some(2),
                ..fdi::MatchedDeviceGroupInfo::EMPTY
            }));

            assert!(matched_device_groups.contains(&fdi::MatchedDeviceGroupInfo {
                name: Some("test_group3".to_string()),
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
        let bind_rules_1 = vec![
            fdf::BindRule {
                key: fdf::NodePropertyKey::IntValue(1),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::IntValue(200)],
            },
            fdf::BindRule {
                key: fdf::NodePropertyKey::IntValue(3),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::BoolValue(true)],
            },
            fdf::BindRule {
                key: fdf::NodePropertyKey::StringValue("killdeer".to_string()),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::StringValue("plover".to_string())],
            },
        ];

        let bind_properties_1 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(2)),
            value: Some(fdf::NodePropertyValue::BoolValue(false)),
            ..fdf::NodeProperty::EMPTY
        }];

        let bind_rules_2 = vec![
            fdf::BindRule {
                key: fdf::NodePropertyKey::StringValue("killdeer".to_string()),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::StringValue("plover".to_string())],
            },
            fdf::BindRule {
                key: fdf::NodePropertyKey::StringValue("yellowlegs".to_string()),
                condition: fdf::Condition::Reject,
                values: vec![fdf::NodePropertyValue::BoolValue(false)],
            },
        ];

        let bind_properties_2 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(3)),
            value: Some(fdf::NodePropertyValue::BoolValue(true)),
            ..fdf::NodeProperty::EMPTY
        }];

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Err(Status::NOT_FOUND.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    name: Some("test_group".to_string()),
                    nodes: Some(vec![
                        fdf::DeviceGroupNode {
                            bind_rules: bind_rules_1,
                            bind_properties: bind_properties_1,
                        },
                        fdf::DeviceGroupNode {
                            bind_rules: bind_rules_2,
                            bind_properties: bind_properties_2,
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
        let bind_rules_1 = vec![
            fdf::BindRule {
                key: fdf::NodePropertyKey::IntValue(10),
                condition: fdf::Condition::Reject,
                values: vec![
                    fdf::NodePropertyValue::IntValue(200),
                    fdf::NodePropertyValue::IntValue(150),
                ],
            },
            fdf::BindRule {
                key: fdf::NodePropertyKey::StringValue("plover".to_string()),
                condition: fdf::Condition::Accept,
                values: vec![
                    fdf::NodePropertyValue::StringValue("killdeer".to_string()),
                    fdf::NodePropertyValue::StringValue("lapwing".to_string()),
                ],
            },
        ];

        let bind_properties_1 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(1)),
            value: Some(fdf::NodePropertyValue::IntValue(100)),
            ..fdf::NodeProperty::EMPTY
        }];

        let bind_rules_2 = vec![
            fdf::BindRule {
                key: fdf::NodePropertyKey::IntValue(11),
                condition: fdf::Condition::Reject,
                values: vec![
                    fdf::NodePropertyValue::IntValue(20),
                    fdf::NodePropertyValue::IntValue(10),
                ],
            },
            fdf::BindRule {
                key: fdf::NodePropertyKey::StringValue("dunlin".to_string()),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::BoolValue(true)],
            },
        ];

        let bind_properties_2 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(3)),
            value: Some(fdf::NodePropertyValue::BoolValue(true)),
            ..fdf::NodeProperty::EMPTY
        }];

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Err(Status::NOT_FOUND.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    name: Some("test_group".to_string()),
                    nodes: Some(vec![
                        fdf::DeviceGroupNode {
                            bind_rules: bind_rules_1,
                            bind_properties: bind_properties_1,
                        },
                        fdf::DeviceGroupNode {
                            bind_rules: bind_rules_2,
                            bind_properties: bind_properties_2,
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
            name: Some("test_group".to_string()),
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
            name: Some("test_group".to_string()),
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
        let bind_rules_1 = vec![
            fdf::BindRule {
                key: fdf::NodePropertyKey::IntValue(10),
                condition: fdf::Condition::Reject,
                values: vec![
                    fdf::NodePropertyValue::IntValue(200),
                    fdf::NodePropertyValue::IntValue(150),
                ],
            },
            fdf::BindRule {
                key: fdf::NodePropertyKey::StringValue("plover".to_string()),
                condition: fdf::Condition::Accept,
                values: vec![
                    fdf::NodePropertyValue::StringValue("killdeer".to_string()),
                    fdf::NodePropertyValue::StringValue("lapwing".to_string()),
                ],
            },
        ];

        let bind_properties_1 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(1)),
            value: Some(fdf::NodePropertyValue::IntValue(100)),
            ..fdf::NodeProperty::EMPTY
        }];

        let bind_rules_2 = vec![
            fdf::BindRule {
                key: fdf::NodePropertyKey::IntValue(11),
                condition: fdf::Condition::Reject,
                values: vec![
                    fdf::NodePropertyValue::IntValue(20),
                    fdf::NodePropertyValue::IntValue(10),
                ],
            },
            fdf::BindRule {
                key: fdf::NodePropertyKey::IntValue(2),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::BoolValue(true)],
            },
        ];

        let bind_properties_2 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(3)),
            value: Some(fdf::NodePropertyValue::BoolValue(true)),
            ..fdf::NodeProperty::EMPTY
        }];

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Err(Status::NOT_FOUND.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    name: Some("test_group".to_string()),
                    nodes: Some(vec![
                        fdf::DeviceGroupNode {
                            bind_rules: bind_rules_1,
                            bind_properties: bind_properties_1,
                        },
                        fdf::DeviceGroupNode {
                            bind_rules: bind_rules_2,
                            bind_properties: bind_properties_2,
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
        let bind_rules = vec![fdf::BindRule {
            key: fdf::NodePropertyKey::IntValue(10),
            condition: fdf::Condition::Reject,
            values: vec![
                fdf::NodePropertyValue::IntValue(200),
                fdf::NodePropertyValue::BoolValue(false),
            ],
        }];

        let bind_properties = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(1)),
            value: Some(fdf::NodePropertyValue::IntValue(100)),
            ..fdf::NodeProperty::EMPTY
        }];

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Err(Status::INVALID_ARGS.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    name: Some("test_group".to_string()),
                    nodes: Some(vec![fdf::DeviceGroupNode {
                        bind_rules: bind_rules,
                        bind_properties: bind_properties,
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
        let bind_rules = vec![
            fdf::BindRule {
                key: fdf::NodePropertyKey::IntValue(10),
                condition: fdf::Condition::Reject,
                values: vec![fdf::NodePropertyValue::IntValue(200)],
            },
            fdf::BindRule {
                key: fdf::NodePropertyKey::IntValue(10),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::IntValue(10)],
            },
        ];

        let bind_properties = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(3)),
            value: Some(fdf::NodePropertyValue::BoolValue(true)),
            ..fdf::NodeProperty::EMPTY
        }];

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Err(Status::INVALID_ARGS.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    name: Some("test_group".to_string()),
                    nodes: Some(vec![fdf::DeviceGroupNode {
                        bind_rules: bind_rules,
                        bind_properties: bind_properties,
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
    async fn test_missing_bind_rules() {
        let bind_rules = vec![
            fdf::BindRule {
                key: fdf::NodePropertyKey::IntValue(10),
                condition: fdf::Condition::Reject,
                values: vec![fdf::NodePropertyValue::IntValue(200)],
            },
            fdf::BindRule {
                key: fdf::NodePropertyKey::IntValue(10),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::IntValue(10)],
            },
        ];

        let bind_properties_1 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(3)),
            value: Some(fdf::NodePropertyValue::BoolValue(true)),
            ..fdf::NodeProperty::EMPTY
        }];

        let bind_properties_2 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(10)),
            value: Some(fdf::NodePropertyValue::BoolValue(false)),
            ..fdf::NodeProperty::EMPTY
        }];

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Err(Status::INVALID_ARGS.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    name: Some("test_group".to_string()),
                    nodes: Some(vec![
                        fdf::DeviceGroupNode {
                            bind_rules: bind_rules,
                            bind_properties: bind_properties_1,
                        },
                        fdf::DeviceGroupNode {
                            bind_rules: vec![],
                            bind_properties: bind_properties_2
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
        let bind_rules = vec![
            fdf::BindRule {
                key: fdf::NodePropertyKey::IntValue(10),
                condition: fdf::Condition::Reject,
                values: vec![fdf::NodePropertyValue::IntValue(200)],
            },
            fdf::BindRule {
                key: fdf::NodePropertyKey::IntValue(10),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::IntValue(10)],
            },
        ];

        let bind_properties_1 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(3)),
            value: Some(fdf::NodePropertyValue::BoolValue(true)),
            ..fdf::NodeProperty::EMPTY
        }];

        let bind_properties_2 = vec![fdf::NodeProperty {
            key: Some(fdf::NodePropertyKey::IntValue(1)),
            value: Some(fdf::NodePropertyValue::BoolValue(false)),
            ..fdf::NodeProperty::EMPTY
        }];

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Err(Status::INVALID_ARGS.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    name: None,
                    nodes: Some(vec![
                        fdf::DeviceGroupNode {
                            bind_rules: bind_rules,
                            bind_properties: bind_properties_1,
                        },
                        fdf::DeviceGroupNode {
                            bind_rules: vec![],
                            bind_properties: bind_properties_2,
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
                    name: Some("test_group".to_string()),
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
        let primary_bind_rules = vec![fdf::BindRule {
            key: fdf::NodePropertyKey::IntValue(1),
            condition: fdf::Condition::Accept,
            values: vec![fdf::NodePropertyValue::IntValue(200)],
        }];

        let additional_bind_rules_1 = vec![fdf::BindRule {
            key: fdf::NodePropertyKey::IntValue(1),
            condition: fdf::Condition::Accept,
            values: vec![fdf::NodePropertyValue::IntValue(10)],
        }];

        let additional_bind_rules_2 = vec![fdf::BindRule {
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
            bind_rules: primary_bind_rules,
            bind_properties: vec![fdf::NodeProperty {
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
            bind_rules: additional_bind_rules_1,
            bind_properties: vec![fdf::NodeProperty {
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
            bind_rules: additional_bind_rules_2,
            bind_properties: vec![fdf::NodeProperty {
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
            vec![],
        );

        let nodes = Some(vec![
            primary_device_group_node,
            additional_device_group_node_b,
            additional_device_group_node_a,
        ]);

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Ok((
                fdi::MatchedCompositeInfo {
                    node_index: None,
                    num_nodes: None,
                    composite_name: Some(device_name.to_string()),
                    node_names: None,
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
                    name: Some("test_group".to_string()),
                    nodes: nodes.clone(),
                    ..fdf::DeviceGroup::EMPTY
                },
                vec![&composite_driver]
            )
        );

        assert_eq!(1, device_group_manager.get_node_groups(None).len());
        assert_eq!(0, device_group_manager.get_node_groups(Some("not_there".to_string())).len());
        let node_groups = device_group_manager.get_node_groups(Some("test_group".to_string()));
        assert_eq!(1, node_groups.len());
        let node_group = &node_groups[0];
        let expected_node_group = fdd::NodeGroupInfo {
            name: Some("test_group".to_string()),
            driver: Some("fuchsia-pkg://fuchsia.com/package#driver/my-driver.cm".to_string()),
            primary_index: Some(0),
            node_names: Some(vec![
                primary_name.to_string(),
                additional_b_name.to_string(),
                additional_a_name.to_string(),
            ]),
            nodes,
            ..fdd::NodeGroupInfo::EMPTY
        };

        assert_eq!(&expected_node_group, node_group);

        // Match additional node A, the last node in the device group at index 2.
        let mut device_properties_1: DeviceProperties = HashMap::new();
        device_properties_1.insert(PropertyKey::NumberKey(1), Symbol::NumberValue(10));

        let expected_device_group = fdi::MatchedDeviceGroupInfo {
            name: Some("test_group".to_string()),
            node_index: Some(2),
            num_nodes: Some(3),
            primary_index: Some(0),
            node_names: Some(vec![
                primary_name.to_string(),
                additional_b_name.to_string(),
                additional_a_name.to_string(),
            ]),
            composite: Some(fdi::MatchedCompositeInfo {
                node_index: None,
                num_nodes: None,
                composite_name: Some(device_name.to_string()),
                node_names: None,
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
    async fn test_composite_with_rearranged_primary_node() {
        let primary_bind_rules = vec![fdf::BindRule {
            key: fdf::NodePropertyKey::IntValue(1),
            condition: fdf::Condition::Accept,
            values: vec![fdf::NodePropertyValue::IntValue(200)],
        }];

        let additional_bind_rules_1 = vec![fdf::BindRule {
            key: fdf::NodePropertyKey::IntValue(1),
            condition: fdf::Condition::Accept,
            values: vec![fdf::NodePropertyValue::IntValue(10)],
        }];

        let additional_bind_rules_2 = vec![fdf::BindRule {
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
        let primary_name = "primary_node";
        let additional_a_name = "additional_1";
        let additional_b_name = "additional_2";

        let primary_device_group_node = fdf::DeviceGroupNode {
            bind_rules: primary_bind_rules,
            bind_properties: vec![fdf::NodeProperty {
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
            bind_rules: additional_bind_rules_1,
            bind_properties: vec![fdf::NodeProperty {
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
            bind_rules: additional_bind_rules_2,
            bind_properties: vec![fdf::NodeProperty {
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
            vec![],
        );

        let nodes = Some(vec![
            additional_device_group_node_b,
            additional_device_group_node_a,
            primary_device_group_node,
        ]);

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Ok((
                fdi::MatchedCompositeInfo {
                    node_index: None,
                    num_nodes: None,
                    composite_name: Some(device_name.to_string()),
                    node_names: None,
                    driver_info: Some(composite_driver.clone().create_matched_driver_info()),
                    ..fdi::MatchedCompositeInfo::EMPTY
                },
                vec![
                    additional_b_name.to_string(),
                    additional_a_name.to_string(),
                    primary_name.to_string(),
                ]
            )),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    name: Some("test_group".to_string()),
                    nodes: nodes.clone(),
                    ..fdf::DeviceGroup::EMPTY
                },
                vec![&composite_driver]
            )
        );

        assert_eq!(1, device_group_manager.get_node_groups(None).len());
        assert_eq!(0, device_group_manager.get_node_groups(Some("not_there".to_string())).len());
        let node_groups = device_group_manager.get_node_groups(Some("test_group".to_string()));
        assert_eq!(1, node_groups.len());
        let node_group = &node_groups[0];
        let expected_node_group = fdd::NodeGroupInfo {
            name: Some("test_group".to_string()),
            driver: Some("fuchsia-pkg://fuchsia.com/package#driver/my-driver.cm".to_string()),
            primary_index: Some(2),
            node_names: Some(vec![
                additional_b_name.to_string(),
                additional_a_name.to_string(),
                primary_name.to_string(),
            ]),
            nodes,
            ..fdd::NodeGroupInfo::EMPTY
        };

        assert_eq!(&expected_node_group, node_group);

        // Match additional node A, the last node in the device group at index 2.
        let mut device_properties_1: DeviceProperties = HashMap::new();
        device_properties_1.insert(PropertyKey::NumberKey(1), Symbol::NumberValue(10));

        let expected_device_group = fdi::MatchedDeviceGroupInfo {
            name: Some("test_group".to_string()),
            node_index: Some(1),
            num_nodes: Some(3),
            primary_index: Some(2),
            node_names: Some(vec![
                additional_b_name.to_string(),
                additional_a_name.to_string(),
                primary_name.to_string(),
            ]),
            composite: Some(fdi::MatchedCompositeInfo {
                node_index: None,
                num_nodes: None,
                composite_name: Some(device_name.to_string()),
                node_names: None,
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
    async fn test_composite_with_optional_match_without_optional() {
        let primary_bind_rules = vec![fdf::BindRule {
            key: fdf::NodePropertyKey::IntValue(1),
            condition: fdf::Condition::Accept,
            values: vec![fdf::NodePropertyValue::IntValue(200)],
        }];

        let additional_bind_rules_1 = vec![fdf::BindRule {
            key: fdf::NodePropertyKey::IntValue(1),
            condition: fdf::Condition::Accept,
            values: vec![fdf::NodePropertyValue::IntValue(10)],
        }];

        let additional_bind_rules_2 = vec![fdf::BindRule {
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

        let optional_a_key_1 = 200;
        let optional_a_val_1: u32 = 10;

        let device_name = "mimid";
        let primary_name = "catbird";
        let additional_a_name = "mockingbird";
        let additional_b_name = "lapwing";
        let optional_a_name = "trembler";

        let primary_device_group_node = fdf::DeviceGroupNode {
            bind_rules: primary_bind_rules,
            bind_properties: vec![fdf::NodeProperty {
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
            bind_rules: additional_bind_rules_1,
            bind_properties: vec![fdf::NodeProperty {
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
            bind_rules: additional_bind_rules_2,
            bind_properties: vec![fdf::NodeProperty {
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

        let optional_node_a_inst = vec![
            SymbolicInstructionInfo {
                location: None,
                instruction: SymbolicInstruction::AbortIfNotEqual {
                    lhs: Symbol::DeprecatedKey(optional_a_key_1),
                    rhs: Symbol::NumberValue(optional_a_val_1.clone().into()),
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

        let composite_driver = create_driver_with_rules(
            device_name,
            (primary_name, primary_node_inst),
            vec![
                (additional_a_name, additional_node_a_inst),
                (additional_b_name, additional_node_b_inst),
            ],
            vec![(optional_a_name, optional_node_a_inst)],
        );

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Ok((
                fdi::MatchedCompositeInfo {
                    node_index: None,
                    num_nodes: None,
                    composite_name: Some(device_name.to_string()),
                    node_names: None,
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
                    name: Some("test_group".to_string()),
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
            name: Some("test_group".to_string()),
            node_index: Some(2),
            num_nodes: Some(3),
            primary_index: Some(0),
            node_names: Some(vec![
                primary_name.to_string(),
                additional_b_name.to_string(),
                additional_a_name.to_string(),
            ]),
            composite: Some(fdi::MatchedCompositeInfo {
                node_index: None,
                num_nodes: None,
                composite_name: Some(device_name.to_string()),
                node_names: None,
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
    async fn test_composite_with_optional_match_with_optional() {
        let primary_bind_rules = vec![fdf::BindRule {
            key: fdf::NodePropertyKey::IntValue(1),
            condition: fdf::Condition::Accept,
            values: vec![fdf::NodePropertyValue::IntValue(200)],
        }];

        let additional_bind_rules_1 = vec![fdf::BindRule {
            key: fdf::NodePropertyKey::IntValue(1),
            condition: fdf::Condition::Accept,
            values: vec![fdf::NodePropertyValue::IntValue(10)],
        }];

        let additional_bind_rules_2 = vec![fdf::BindRule {
            key: fdf::NodePropertyKey::IntValue(10),
            condition: fdf::Condition::Accept,
            values: vec![fdf::NodePropertyValue::BoolValue(true)],
        }];

        let optional_bind_rules_1 = vec![fdf::BindRule {
            key: fdf::NodePropertyKey::IntValue(1000),
            condition: fdf::Condition::Accept,
            values: vec![fdf::NodePropertyValue::IntValue(1000)],
        }];

        let primary_key_1 = "whimbrel";
        let primary_val_1 = "sanderling";

        let additional_a_key_1 = 100;
        let additional_a_val_1 = 50;

        let additional_b_key_1 = "curlew";
        let additional_b_val_1 = 500;

        let optional_a_key_1 = 200;
        let optional_a_val_1 = 10;

        let device_name = "mimid";
        let primary_name = "catbird";
        let additional_a_name = "mockingbird";
        let additional_b_name = "lapwing";
        let optional_a_name = "trembler";

        let primary_device_group_node = fdf::DeviceGroupNode {
            bind_rules: primary_bind_rules,
            bind_properties: vec![fdf::NodeProperty {
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
            bind_rules: additional_bind_rules_1,
            bind_properties: vec![fdf::NodeProperty {
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
            bind_rules: additional_bind_rules_2,
            bind_properties: vec![fdf::NodeProperty {
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

        let optional_device_group_node_a = fdf::DeviceGroupNode {
            bind_rules: optional_bind_rules_1,
            bind_properties: vec![fdf::NodeProperty {
                key: Some(fdf::NodePropertyKey::IntValue(optional_a_key_1)),
                value: Some(fdf::NodePropertyValue::IntValue(optional_a_val_1)),
                ..fdf::NodeProperty::EMPTY
            }],
        };

        let optional_node_a_inst = vec![
            SymbolicInstructionInfo {
                location: None,
                instruction: SymbolicInstruction::AbortIfNotEqual {
                    lhs: Symbol::DeprecatedKey(optional_a_key_1),
                    rhs: Symbol::NumberValue(optional_a_val_1.clone().into()),
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

        let composite_driver = create_driver_with_rules(
            device_name,
            (primary_name, primary_node_inst),
            vec![
                (additional_a_name, additional_node_a_inst),
                (additional_b_name, additional_node_b_inst),
            ],
            vec![(optional_a_name, optional_node_a_inst)],
        );

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Ok((
                fdi::MatchedCompositeInfo {
                    node_index: None,
                    num_nodes: None,
                    composite_name: Some(device_name.to_string()),
                    node_names: None,
                    driver_info: Some(composite_driver.clone().create_matched_driver_info()),
                    ..fdi::MatchedCompositeInfo::EMPTY
                },
                vec![
                    primary_name.to_string(),
                    additional_b_name.to_string(),
                    optional_a_name.to_string(),
                    additional_a_name.to_string()
                ]
            )),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    name: Some("test_group".to_string()),
                    nodes: Some(vec![
                        primary_device_group_node,
                        additional_device_group_node_b,
                        optional_device_group_node_a,
                        additional_device_group_node_a,
                    ]),
                    ..fdf::DeviceGroup::EMPTY
                },
                vec![&composite_driver]
            )
        );

        // Match additional node A, the last node in the device group at index 3.
        let mut device_properties_1: DeviceProperties = HashMap::new();
        device_properties_1.insert(PropertyKey::NumberKey(1), Symbol::NumberValue(10));

        let expected_device_group = fdi::MatchedDeviceGroupInfo {
            name: Some("test_group".to_string()),
            node_index: Some(3),
            num_nodes: Some(4),
            primary_index: Some(0),
            node_names: Some(vec![
                primary_name.to_string(),
                additional_b_name.to_string(),
                optional_a_name.to_string(),
                additional_a_name.to_string(),
            ]),
            composite: Some(fdi::MatchedCompositeInfo {
                node_index: None,
                num_nodes: None,
                composite_name: Some(device_name.to_string()),
                node_names: None,
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

        // Match optional node A, the second to last node in the device group at index 2.
        let mut device_properties_1: DeviceProperties = HashMap::new();
        device_properties_1.insert(PropertyKey::NumberKey(1000), Symbol::NumberValue(1000));

        let expected_device_group = fdi::MatchedDeviceGroupInfo {
            name: Some("test_group".to_string()),
            node_index: Some(2),
            num_nodes: Some(4),
            primary_index: Some(0),
            node_names: Some(vec![
                primary_name.to_string(),
                additional_b_name.to_string(),
                optional_a_name.to_string(),
                additional_a_name.to_string(),
            ]),
            composite: Some(fdi::MatchedCompositeInfo {
                node_index: None,
                num_nodes: None,
                composite_name: Some(device_name.to_string()),
                node_names: None,
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
        let primary_bind_rules = vec![fdf::BindRule {
            key: fdf::NodePropertyKey::IntValue(1),
            condition: fdf::Condition::Accept,
            values: vec![fdf::NodePropertyValue::IntValue(200)],
        }];

        let additional_bind_rules_1 = vec![fdf::BindRule {
            key: fdf::NodePropertyKey::IntValue(1),
            condition: fdf::Condition::Accept,
            values: vec![fdf::NodePropertyValue::IntValue(10)],
        }];

        let additional_bind_rules_2 = vec![fdf::BindRule {
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
            bind_rules: primary_bind_rules,
            bind_properties: vec![fdf::NodeProperty {
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
            bind_rules: additional_bind_rules_1,
            bind_properties: vec![fdf::NodeProperty {
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
            bind_rules: additional_bind_rules_2,
            bind_properties: vec![fdf::NodeProperty {
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
            vec![],
        );

        let mut device_group_manager = DeviceGroupManager::new();
        assert_eq!(
            Err(Status::NOT_FOUND.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    name: Some("test_group".to_string()),
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

    #[fasync::run_singlethreaded(test)]
    async fn test_valid_name() {
        let mut device_group_manager = DeviceGroupManager::new();

        let node = fdf::DeviceGroupNode {
            bind_rules: vec![fdf::BindRule {
                key: fdf::NodePropertyKey::StringValue("wrybill".to_string()),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::IntValue(200)],
            }],
            bind_properties: vec![fdf::NodeProperty {
                key: Some(fdf::NodePropertyKey::StringValue("dotteral".to_string())),
                value: Some(fdf::NodePropertyValue::StringValue("wrybill".to_string())),
                ..fdf::NodeProperty::EMPTY
            }],
        };
        assert_eq!(
            Err(Status::NOT_FOUND.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    name: Some("test-group".to_string()),
                    nodes: Some(vec![node.clone(),]),
                    ..fdf::DeviceGroup::EMPTY
                },
                vec![]
            )
        );

        assert_eq!(
            Err(Status::NOT_FOUND.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    name: Some("TEST_group".to_string()),
                    nodes: Some(vec![node]),
                    ..fdf::DeviceGroup::EMPTY
                },
                vec![]
            )
        );
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_invalid_name() {
        let mut device_group_manager = DeviceGroupManager::new();
        let node = fdf::DeviceGroupNode {
            bind_rules: vec![fdf::BindRule {
                key: fdf::NodePropertyKey::StringValue("wrybill".to_string()),
                condition: fdf::Condition::Accept,
                values: vec![fdf::NodePropertyValue::IntValue(200)],
            }],
            bind_properties: vec![fdf::NodeProperty {
                key: Some(fdf::NodePropertyKey::StringValue("dotteral".to_string())),
                value: Some(fdf::NodePropertyValue::IntValue(100)),
                ..fdf::NodeProperty::EMPTY
            }],
        };
        assert_eq!(
            Err(Status::INVALID_ARGS.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    name: Some("test/group".to_string()),
                    nodes: Some(vec![node.clone(),]),
                    ..fdf::DeviceGroup::EMPTY
                },
                vec![]
            )
        );

        assert_eq!(
            Err(Status::INVALID_ARGS.into_raw()),
            device_group_manager.add_device_group(
                fdf::DeviceGroup {
                    name: Some("test:group".to_string()),
                    nodes: Some(vec![node]),
                    ..fdf::DeviceGroup::EMPTY
                },
                vec![]
            )
        );
    }
}
