// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{
        capability::CapabilitySource,
        config::{CapabilityAllowlistKey, CapabilityAllowlistSource, RuntimeConfig},
        model::error::ModelError,
    },
    fuchsia_zircon as zx,
    log::{error, warn},
    moniker::{AbsoluteMoniker, ChildMoniker, ExtendedMoniker},
    std::sync::{Arc, Weak},
    thiserror::Error,
};
/// Errors returned by the PolicyChecker and the ScopedPolicyChecker.
#[derive(Debug, Clone, Error)]
pub enum PolicyError {
    #[error("security policy was unavailable to check")]
    PolicyUnavailable,

    #[error("security policy disallows \"{policy}\" job policy for \"{moniker}\"")]
    JobPolicyDisallowed { policy: String, moniker: AbsoluteMoniker },

    #[error("security policy was unable to extract the source from the routed capability")]
    InvalidCapabilitySource,

    #[error("security policy disallows \"{cap}\" from \"{source_moniker}\" being used at \"{target_moniker}\"")]
    CapabilityUseDisallowed {
        cap: String,
        source_moniker: ExtendedMoniker,
        target_moniker: AbsoluteMoniker,
    },

    #[error("debug security policy disallows \"{cap}\" from \"{source_moniker}\" being routed from environment \"{env_moniker}:{env_name}\" to \"{target_moniker}\"")]
    DebugCapabilityUseDisallowed {
        cap: String,
        source_moniker: ExtendedMoniker,
        env_moniker: AbsoluteMoniker,
        env_name: String,
        target_moniker: AbsoluteMoniker,
    },
}

impl PolicyError {
    fn job_policy_disallowed(policy: impl Into<String>, moniker: &AbsoluteMoniker) -> Self {
        PolicyError::JobPolicyDisallowed { policy: policy.into(), moniker: moniker.clone() }
    }

    fn capability_use_disallowed(
        cap: impl Into<String>,
        source_moniker: &ExtendedMoniker,
        target_moniker: &AbsoluteMoniker,
    ) -> Self {
        PolicyError::CapabilityUseDisallowed {
            cap: cap.into(),
            source_moniker: source_moniker.clone(),
            target_moniker: target_moniker.clone(),
        }
    }

    fn debug_capability_use_disallowed(
        cap: impl Into<String>,
        source_moniker: &ExtendedMoniker,
        env_moniker: &AbsoluteMoniker,
        env_name: impl Into<String>,
        target_moniker: &AbsoluteMoniker,
    ) -> Self {
        PolicyError::DebugCapabilityUseDisallowed {
            cap: cap.into(),
            source_moniker: source_moniker.clone(),
            env_moniker: env_moniker.clone(),
            env_name: env_name.into(),
            target_moniker: target_moniker.clone(),
        }
    }

    /// Convert this error into its approximate `zx::Status` equivalent.
    pub fn as_zx_status(&self) -> zx::Status {
        zx::Status::ACCESS_DENIED
    }
}

/// Evaluates security policy globally across the entire Model and all components.
/// This is used to enforce runtime capability routing restrictions across all
/// components to prevent high privilleged capabilities from being routed to
/// components outside of the list defined in the runtime configs security
/// policy.
pub struct GlobalPolicyChecker {
    /// The runtime configuration containing the security policy to apply.
    config: Arc<RuntimeConfig>,
}

impl GlobalPolicyChecker {
    /// Constructs a new PolicyChecker object configured by the
    /// RuntimeConfig::SecurityPolicy.
    pub fn new(config: Arc<RuntimeConfig>) -> Self {
        Self { config: config }
    }

    /// Absolute monikers contain instance_id. This change normalizes all
    /// incoming instance identifiers to 0 so for example
    /// /foo:1/bar:0 -> /foo:0/bar:0.
    fn strip_moniker_instance_id(moniker: &AbsoluteMoniker) -> AbsoluteMoniker {
        let mut normalized_children = Vec::with_capacity(moniker.path().len());
        for child in moniker.path().iter() {
            normalized_children.push(ChildMoniker::new(
                child.name().to_string(),
                child.collection().map(String::from),
                0,
            ));
        }
        AbsoluteMoniker::new(normalized_children)
    }

    fn get_policy_key<'a>(
        capability_source: &'a CapabilitySource,
    ) -> Result<CapabilityAllowlistKey, PolicyError> {
        Ok(match &capability_source {
            CapabilitySource::Namespace { capability, .. } => CapabilityAllowlistKey {
                source_moniker: ExtendedMoniker::ComponentManager,
                source_name: capability
                    .source_name()
                    .ok_or(PolicyError::InvalidCapabilitySource)?
                    .clone(),
                source: CapabilityAllowlistSource::Self_,
                capability: capability.type_name(),
            },
            CapabilitySource::Component { capability, component } => CapabilityAllowlistKey {
                source_moniker: ExtendedMoniker::ComponentInstance(component.moniker.clone()),
                source_name: capability
                    .source_name()
                    .ok_or(PolicyError::InvalidCapabilitySource)?
                    .clone(),
                source: CapabilityAllowlistSource::Self_,
                capability: capability.type_name(),
            },
            CapabilitySource::Builtin { capability, .. } => CapabilityAllowlistKey {
                source_moniker: ExtendedMoniker::ComponentManager,
                source_name: capability.source_name().clone(),
                source: CapabilityAllowlistSource::Self_,
                capability: capability.type_name(),
            },
            CapabilitySource::Framework { capability, component } => CapabilityAllowlistKey {
                source_moniker: ExtendedMoniker::ComponentInstance(component.moniker.clone()),
                source_name: capability.source_name().clone(),
                source: CapabilityAllowlistSource::Framework,
                capability: capability.type_name(),
            },
            CapabilitySource::Capability { source_capability, component } => {
                CapabilityAllowlistKey {
                    source_moniker: ExtendedMoniker::ComponentInstance(component.moniker.clone()),
                    source_name: source_capability
                        .source_name()
                        .ok_or(PolicyError::InvalidCapabilitySource)?
                        .clone(),
                    source: CapabilityAllowlistSource::Capability,
                    capability: source_capability.type_name(),
                }
            }
            CapabilitySource::Collection { source_name, component, .. } => CapabilityAllowlistKey {
                source_moniker: ExtendedMoniker::ComponentInstance(component.moniker.clone()),
                source_name: source_name.clone(),
                source: CapabilityAllowlistSource::Self_,
                capability: cm_rust::CapabilityTypeName::Service,
            },
        })
    }

    /// Returns Ok(()) if the provided capability source can be routed to the
    /// given target_moniker, else a descriptive PolicyError.
    pub fn can_route_capability<'a>(
        &self,
        capability_source: &'a CapabilitySource,
        target_moniker: &'a AbsoluteMoniker,
    ) -> Result<(), ModelError> {
        let target_moniker = Self::strip_moniker_instance_id(&target_moniker);
        let policy_key = Self::get_policy_key(capability_source).map_err(|e| {
            error!("Security policy could not generate a policy key for `{}`", capability_source);
            e
        })?;

        match self.config.security_policy.capability_policy.get(&policy_key) {
            Some(allowed_monikers) => match allowed_monikers.get(&target_moniker) {
                Some(_) => Ok(()),
                None => {
                    warn!(
                        "Security policy prevented `{}` from `{}` being routed to `{}`.",
                        policy_key.source_name, policy_key.source_moniker, target_moniker
                    );
                    Err(ModelError::PolicyError {
                        err: PolicyError::capability_use_disallowed(
                            policy_key.source_name.str(),
                            &policy_key.source_moniker,
                            &target_moniker,
                        ),
                    })
                }
            },
            None => Ok(()),
        }
    }

    /// Returns Ok(()) if the provided debug capability source is allowed to be routed from given
    /// environment.
    pub fn can_route_debug_capability<'a>(
        &self,
        capability_source: &'a CapabilitySource,
        env_moniker: &'a AbsoluteMoniker,
        env_name: &'a str,
        target_moniker: &'a AbsoluteMoniker,
    ) -> Result<(), ModelError> {
        let policy_key = Self::get_policy_key(capability_source).map_err(|e| {
            error!("Security policy could not generate a policy key for `{}`", capability_source);
            e
        })?;
        if let Some(allowed_envs) =
            self.config.security_policy.debug_capability_policy.get(&policy_key)
        {
            if let Some(_) = allowed_envs.get(&(env_moniker.clone(), env_name.to_string())) {
                return Ok(());
            }
        }
        warn!(
            "Debug security policy prevented `{}` from `{}` being routed to `{}`.",
            policy_key.source_name, policy_key.source_moniker, target_moniker
        );
        Err(ModelError::PolicyError {
            err: PolicyError::debug_capability_use_disallowed(
                policy_key.source_name.str(),
                &policy_key.source_moniker,
                &env_moniker,
                env_name,
                target_moniker,
            ),
        })
    }
}

/// Evaluates security policy relative to a specific Component (based on that Component's
/// AbsoluteMoniker).
pub struct ScopedPolicyChecker {
    /// The runtime configuration containing the security policy to apply.
    config: Weak<RuntimeConfig>,

    /// The absolute moniker of the component that policy will be evaluated for.
    moniker: AbsoluteMoniker,
}

impl ScopedPolicyChecker {
    pub fn new(config: Weak<RuntimeConfig>, moniker: AbsoluteMoniker) -> Self {
        ScopedPolicyChecker { config, moniker }
    }

    // This interface is super simple for now since there's only three allowlists. In the future
    // we'll probably want a different interface than an individual function per policy item.

    pub fn ambient_mark_vmo_exec_allowed(&self) -> Result<(), PolicyError> {
        let config = self.config.upgrade().ok_or(PolicyError::PolicyUnavailable)?;
        if config.security_policy.job_policy.ambient_mark_vmo_exec.contains(&self.moniker) {
            Ok(())
        } else {
            Err(PolicyError::job_policy_disallowed("ambient_mark_vmo_exec", &self.moniker))
        }
    }

    pub fn main_process_critical_allowed(&self) -> Result<(), PolicyError> {
        let config = self.config.upgrade().ok_or(PolicyError::PolicyUnavailable)?;
        if config.security_policy.job_policy.main_process_critical.contains(&self.moniker) {
            Ok(())
        } else {
            Err(PolicyError::job_policy_disallowed("main_process_critical", &self.moniker))
        }
    }

    pub fn create_raw_processes_allowed(&self) -> Result<(), PolicyError> {
        let config = self.config.upgrade().ok_or(PolicyError::PolicyUnavailable)?;
        if config.security_policy.job_policy.create_raw_processes.contains(&self.moniker) {
            Ok(())
        } else {
            Err(PolicyError::job_policy_disallowed("create_raw_processes", &self.moniker))
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            capability::{ComponentCapability, InternalCapability},
            config::{JobPolicyAllowlists, SecurityPolicy},
            model::{
                component::{
                    ComponentInstance, ComponentManagerInstance, WeakComponentInstance,
                    WeakExtendedInstance,
                },
                context::WeakModelContext,
                environment::{DebugRegistry, Environment, RunnerRegistry},
                hooks::Hooks,
                resolver::ResolverRegistry,
            },
        },
        ::routing::component_instance::ComponentInstanceInterface,
        cm_rust::*,
        fidl_fuchsia_sys2 as fsys,
        matches::assert_matches,
        moniker::ChildMoniker,
        std::{
            collections::HashMap,
            collections::HashSet,
            iter::FromIterator,
            sync::{Arc, Weak},
        },
    };

    /// Creates a RuntimeConfig based on the capability allowlist entries provided during
    /// construction.
    struct CapabilityAllowlistConfigBuilder {
        capability_policy: HashMap<CapabilityAllowlistKey, HashSet<AbsoluteMoniker>>,
        debug_capability_policy:
            HashMap<CapabilityAllowlistKey, HashSet<(AbsoluteMoniker, String)>>,
    }

    impl CapabilityAllowlistConfigBuilder {
        pub fn new() -> Self {
            Self { capability_policy: HashMap::new(), debug_capability_policy: HashMap::new() }
        }

        /// Add a new entry to the configuration.
        pub fn add_capability_policy<'a>(
            &'a mut self,
            key: CapabilityAllowlistKey,
            value: Vec<AbsoluteMoniker>,
        ) -> &'a mut Self {
            let value_set = HashSet::from_iter(value.iter().cloned());
            self.capability_policy.insert(key, value_set);
            self
        }

        /// Add a new entry to the configuration.
        pub fn add_debug_capability_policy<'a>(
            &'a mut self,
            key: CapabilityAllowlistKey,
            value: Vec<(AbsoluteMoniker, String)>,
        ) -> &'a mut Self {
            let value_set = HashSet::from_iter(value.iter().cloned());
            self.debug_capability_policy.insert(key, value_set);
            self
        }

        /// Creates a configuration from the provided policies.
        pub fn build(&self) -> Arc<RuntimeConfig> {
            let config = Arc::new(RuntimeConfig {
                security_policy: SecurityPolicy {
                    job_policy: JobPolicyAllowlists {
                        ambient_mark_vmo_exec: vec![],
                        main_process_critical: vec![],
                        create_raw_processes: vec![],
                    },
                    capability_policy: self.capability_policy.clone(),
                    debug_capability_policy: self.debug_capability_policy.clone(),
                },
                ..Default::default()
            });
            config
        }
    }

    #[test]
    fn scoped_policy_checker_vmex() {
        macro_rules! assert_vmex_allowed_matches {
            ($config:expr, $moniker:expr, $expected:pat) => {
                let result = ScopedPolicyChecker::new($config.clone(), $moniker.clone())
                    .ambient_mark_vmo_exec_allowed();
                assert_matches!(result, $expected);
            };
        }
        macro_rules! assert_vmex_disallowed {
            ($config:expr, $moniker:expr) => {
                assert_vmex_allowed_matches!(
                    $config,
                    $moniker,
                    Err(PolicyError::JobPolicyDisallowed { .. })
                );
            };
        }
        let strong_config = Arc::new(RuntimeConfig::default());
        let config = Arc::downgrade(&strong_config);
        assert_vmex_disallowed!(config, AbsoluteMoniker::root());
        assert_vmex_disallowed!(config, AbsoluteMoniker::from(vec!["foo:0"]));

        let allowed1 = AbsoluteMoniker::from(vec!["foo:0", "bar:0"]);
        let allowed2 = AbsoluteMoniker::from(vec!["baz:0", "fiz:0"]);
        let strong_config = Arc::new(RuntimeConfig {
            security_policy: SecurityPolicy {
                job_policy: JobPolicyAllowlists {
                    ambient_mark_vmo_exec: vec![allowed1.clone(), allowed2.clone()],
                    main_process_critical: vec![allowed1.clone(), allowed2.clone()],
                    create_raw_processes: vec![allowed1.clone(), allowed2.clone()],
                },
                capability_policy: HashMap::new(),
                debug_capability_policy: HashMap::new(),
            },
            ..Default::default()
        });
        let config = Arc::downgrade(&strong_config);
        assert_vmex_allowed_matches!(config, allowed1, Ok(()));
        assert_vmex_allowed_matches!(config, allowed2, Ok(()));
        assert_vmex_disallowed!(config, AbsoluteMoniker::root());
        assert_vmex_disallowed!(config, allowed1.parent().unwrap());
        assert_vmex_disallowed!(config, allowed1.child(ChildMoniker::from("baz:0")));

        drop(strong_config);
        assert_vmex_allowed_matches!(config, allowed1, Err(PolicyError::PolicyUnavailable));
        assert_vmex_allowed_matches!(config, allowed2, Err(PolicyError::PolicyUnavailable));
    }

    #[test]
    fn scoped_policy_checker_create_raw_processes() {
        macro_rules! assert_create_raw_processes_allowed_matches {
            ($config:expr, $moniker:expr, $expected:pat) => {
                let result = ScopedPolicyChecker::new($config.clone(), $moniker.clone())
                    .create_raw_processes_allowed();
                assert_matches!(result, $expected);
            };
        }
        macro_rules! assert_create_raw_processes_disallowed {
            ($config:expr, $moniker:expr) => {
                assert_create_raw_processes_allowed_matches!(
                    $config,
                    $moniker,
                    Err(PolicyError::JobPolicyDisallowed { .. })
                );
            };
        }
        let strong_config = Arc::new(RuntimeConfig::default());
        let config = Arc::downgrade(&strong_config);
        assert_create_raw_processes_disallowed!(config, AbsoluteMoniker::root());
        assert_create_raw_processes_disallowed!(config, AbsoluteMoniker::from(vec!["foo:0"]));

        let allowed1 = AbsoluteMoniker::from(vec!["foo:0", "bar:0"]);
        let allowed2 = AbsoluteMoniker::from(vec!["baz:0", "fiz:0"]);
        let strong_config = Arc::new(RuntimeConfig {
            security_policy: SecurityPolicy {
                job_policy: JobPolicyAllowlists {
                    ambient_mark_vmo_exec: vec![],
                    main_process_critical: vec![],
                    create_raw_processes: vec![allowed1.clone(), allowed2.clone()],
                },
                capability_policy: HashMap::new(),
                debug_capability_policy: HashMap::new(),
            },
            ..Default::default()
        });
        let config = Arc::downgrade(&strong_config);
        assert_create_raw_processes_allowed_matches!(config, allowed1, Ok(()));
        assert_create_raw_processes_allowed_matches!(config, allowed2, Ok(()));
        assert_create_raw_processes_disallowed!(config, AbsoluteMoniker::root());
        assert_create_raw_processes_disallowed!(config, allowed1.parent().unwrap());
        assert_create_raw_processes_disallowed!(
            config,
            allowed1.child(ChildMoniker::from("baz:0"))
        );

        drop(strong_config);
        assert_create_raw_processes_allowed_matches!(
            config,
            allowed1,
            Err(PolicyError::PolicyUnavailable)
        );
        assert_create_raw_processes_allowed_matches!(
            config,
            allowed2,
            Err(PolicyError::PolicyUnavailable)
        );
    }

    #[test]
    fn scoped_policy_checker_critical_allowed() {
        macro_rules! assert_critical_allowed_matches {
            ($config:expr, $moniker:expr, $expected:pat) => {
                let result = ScopedPolicyChecker::new($config.clone(), $moniker.clone())
                    .main_process_critical_allowed();
                assert_matches!(result, $expected);
            };
        }
        macro_rules! assert_critical_disallowed {
            ($config:expr, $moniker:expr) => {
                assert_critical_allowed_matches!(
                    $config,
                    $moniker,
                    Err(PolicyError::JobPolicyDisallowed { .. })
                );
            };
        }
        let strong_config = Arc::new(RuntimeConfig::default());
        let config = Arc::downgrade(&strong_config);
        assert_critical_disallowed!(config, AbsoluteMoniker::root());
        assert_critical_disallowed!(config, AbsoluteMoniker::from(vec!["foo:0"]));

        let allowed1 = AbsoluteMoniker::from(vec!["foo:0", "bar:0"]);
        let allowed2 = AbsoluteMoniker::from(vec!["baz:0", "fiz:0"]);
        let strong_config = Arc::new(RuntimeConfig {
            security_policy: SecurityPolicy {
                job_policy: JobPolicyAllowlists {
                    ambient_mark_vmo_exec: vec![allowed1.clone(), allowed2.clone()],
                    main_process_critical: vec![allowed1.clone(), allowed2.clone()],
                    create_raw_processes: vec![allowed1.clone(), allowed2.clone()],
                },
                capability_policy: HashMap::new(),
                debug_capability_policy: HashMap::new(),
            },
            ..Default::default()
        });
        let config = Arc::downgrade(&strong_config);
        assert_critical_allowed_matches!(config, allowed1, Ok(()));
        assert_critical_allowed_matches!(config, allowed2, Ok(()));
        assert_critical_disallowed!(config, AbsoluteMoniker::root());
        assert_critical_disallowed!(config, allowed1.parent().unwrap());
        assert_critical_disallowed!(config, allowed1.child(ChildMoniker::from("baz:0")));

        drop(strong_config);
        assert_critical_allowed_matches!(config, allowed1, Err(PolicyError::PolicyUnavailable));
        assert_critical_allowed_matches!(config, allowed2, Err(PolicyError::PolicyUnavailable));
    }

    #[test]
    fn global_policy_checker_can_route_capability_framework_cap() {
        let mut config_builder = CapabilityAllowlistConfigBuilder::new();
        config_builder.add_capability_policy(
            CapabilityAllowlistKey {
                source_moniker: ExtendedMoniker::ComponentInstance(AbsoluteMoniker::from(vec![
                    "foo:0", "bar:0",
                ])),
                source_name: CapabilityName::from("running"),
                source: CapabilityAllowlistSource::Framework,
                capability: CapabilityTypeName::Event,
            },
            vec![
                AbsoluteMoniker::from(vec!["foo:0", "bar:0"]),
                AbsoluteMoniker::from(vec!["foo:0", "bar:0", "baz:0"]),
            ],
        );
        let global_policy_checker = GlobalPolicyChecker::new(config_builder.build());

        let top_instance = Arc::new(ComponentManagerInstance::new(vec![]));
        let component = ComponentInstance::new(
            Arc::new(Environment::new_root(
                &top_instance,
                RunnerRegistry::default(),
                ResolverRegistry::new(),
                DebugRegistry::default(),
            )),
            vec!["foo:0", "bar:0"].into(),
            "test:///bar".into(),
            fsys::StartupMode::Lazy,
            WeakModelContext::default(),
            WeakExtendedInstance::Component(WeakComponentInstance::default()),
            Arc::new(Hooks::new(None)),
        );
        let weak_component = component.as_weak();

        let event_capability = CapabilitySource::Framework {
            capability: InternalCapability::Event(CapabilityName::from("running")),
            component: weak_component,
        };
        let valid_path_0 = AbsoluteMoniker::from(vec!["foo:0", "bar:0"]);
        let valid_path_1 = AbsoluteMoniker::from(vec!["foo:0", "bar:0", "baz:0"]);
        let invalid_path_0 = AbsoluteMoniker::from(vec!["foobar:0"]);
        let invalid_path_1 = AbsoluteMoniker::from(vec!["foo:0", "bar:0", "foobar:0"]);

        assert_matches!(
            global_policy_checker.can_route_capability(&event_capability, &valid_path_0),
            Ok(())
        );
        assert_matches!(
            global_policy_checker.can_route_capability(&event_capability, &valid_path_1),
            Ok(())
        );
        assert_matches!(
            global_policy_checker.can_route_capability(&event_capability, &invalid_path_0),
            Err(_)
        );
        assert_matches!(
            global_policy_checker.can_route_capability(&event_capability, &invalid_path_1),
            Err(_)
        );
    }

    #[test]
    fn global_policy_checker_can_route_capability_namespace_cap() {
        let mut config_builder = CapabilityAllowlistConfigBuilder::new();
        config_builder.add_capability_policy(
            CapabilityAllowlistKey {
                source_moniker: ExtendedMoniker::ComponentManager,
                source_name: CapabilityName::from("fuchsia.kernel.RootResource"),
                source: CapabilityAllowlistSource::Self_,
                capability: CapabilityTypeName::Protocol,
            },
            vec![
                AbsoluteMoniker::from(vec!["root:0"]),
                AbsoluteMoniker::from(vec!["root:0", "bootstrap:0"]),
                AbsoluteMoniker::from(vec!["root:0", "core:0"]),
            ],
        );
        let global_policy_checker = GlobalPolicyChecker::new(config_builder.build());

        let protocol_capability = CapabilitySource::Namespace {
            capability: ComponentCapability::Protocol(ProtocolDecl {
                name: "fuchsia.kernel.RootResource".into(),
                source_path: "/svc/fuchsia.kernel.RootResource".parse().unwrap(),
            }),
            top_instance: Weak::new(),
        };
        let valid_path_0 = AbsoluteMoniker::from(vec!["root:0"]);
        let valid_path_1 = AbsoluteMoniker::from(vec!["root:0", "bootstrap:0"]);
        let valid_path_2 = AbsoluteMoniker::from(vec!["root:0", "core:0"]);
        let invalid_path_0 = AbsoluteMoniker::from(vec!["foobar:0"]);
        let invalid_path_1 = AbsoluteMoniker::from(vec!["foo:0", "bar:0", "foobar:0"]);

        assert_matches!(
            global_policy_checker.can_route_capability(&protocol_capability, &valid_path_0),
            Ok(())
        );
        assert_matches!(
            global_policy_checker.can_route_capability(&protocol_capability, &valid_path_1),
            Ok(())
        );
        assert_matches!(
            global_policy_checker.can_route_capability(&protocol_capability, &valid_path_2),
            Ok(())
        );
        assert_matches!(
            global_policy_checker.can_route_capability(&protocol_capability, &invalid_path_0),
            Err(_)
        );
        assert_matches!(
            global_policy_checker.can_route_capability(&protocol_capability, &invalid_path_1),
            Err(_)
        );
    }

    #[test]
    fn global_policy_checker_can_route_capability_component_cap() {
        let mut config_builder = CapabilityAllowlistConfigBuilder::new();
        config_builder.add_capability_policy(
            CapabilityAllowlistKey {
                source_moniker: ExtendedMoniker::ComponentInstance(AbsoluteMoniker::from(vec![
                    "foo:0",
                ])),
                source_name: CapabilityName::from("fuchsia.foo.FooBar"),
                source: CapabilityAllowlistSource::Self_,
                capability: CapabilityTypeName::Protocol,
            },
            vec![
                AbsoluteMoniker::from(vec!["foo:0"]),
                AbsoluteMoniker::from(vec!["root:0", "bootstrap:0"]),
                AbsoluteMoniker::from(vec!["root:0", "core:0"]),
            ],
        );
        let global_policy_checker = GlobalPolicyChecker::new(config_builder.build());

        // Create a fake component instance.
        let resolver = ResolverRegistry::new();
        let top_instance = Arc::new(ComponentManagerInstance::new(vec![]));
        let component = ComponentInstance::new(
            Arc::new(Environment::new_root(
                &top_instance,
                RunnerRegistry::default(),
                resolver,
                DebugRegistry::default(),
            )),
            vec!["foo:0"].into(),
            "test:///foo".into(),
            fsys::StartupMode::Lazy,
            WeakModelContext::default(),
            WeakExtendedInstance::Component(WeakComponentInstance::default()),
            Arc::new(Hooks::new(None)),
        );
        let weak_component = component.as_weak();

        let protocol_capability = CapabilitySource::Component {
            capability: ComponentCapability::Protocol(ProtocolDecl {
                name: "fuchsia.foo.FooBar".into(),
                source_path: "/svc/fuchsia.foo.FooBar".parse().unwrap(),
            }),
            component: weak_component,
        };
        let valid_path_0 = AbsoluteMoniker::from(vec!["root:0", "bootstrap:0"]);
        let valid_path_1 = AbsoluteMoniker::from(vec!["root:0", "core:0"]);
        let invalid_path_0 = AbsoluteMoniker::from(vec!["foobar:0"]);
        let invalid_path_1 = AbsoluteMoniker::from(vec!["foo:0", "bar:0", "foobar:0"]);

        assert_matches!(
            global_policy_checker.can_route_capability(&protocol_capability, &valid_path_0),
            Ok(())
        );
        assert_matches!(
            global_policy_checker.can_route_capability(&protocol_capability, &valid_path_1),
            Ok(())
        );
        assert_matches!(
            global_policy_checker.can_route_capability(&protocol_capability, &invalid_path_0),
            Err(_)
        );
        assert_matches!(
            global_policy_checker.can_route_capability(&protocol_capability, &invalid_path_1),
            Err(_)
        );
    }

    #[test]
    fn global_policy_checker_can_route_capability_capability_cap() {
        let mut config_builder = CapabilityAllowlistConfigBuilder::new();
        config_builder.add_capability_policy(
            CapabilityAllowlistKey {
                source_moniker: ExtendedMoniker::ComponentInstance(AbsoluteMoniker::from(vec![
                    "foo:0",
                ])),
                source_name: CapabilityName::from("cache"),
                source: CapabilityAllowlistSource::Capability,
                capability: CapabilityTypeName::Storage,
            },
            vec![
                AbsoluteMoniker::from(vec!["foo:0"]),
                AbsoluteMoniker::from(vec!["root:0", "bootstrap:0"]),
                AbsoluteMoniker::from(vec!["root:0", "core:0"]),
            ],
        );
        let global_policy_checker = GlobalPolicyChecker::new(config_builder.build());

        // Create a fake component instance.
        let resolver = ResolverRegistry::new();
        let top_instance = Arc::new(ComponentManagerInstance::new(vec![]));
        let component = ComponentInstance::new(
            Arc::new(Environment::new_root(
                &top_instance,
                RunnerRegistry::default(),
                resolver,
                DebugRegistry::default(),
            )),
            vec!["foo:0"].into(),
            "test:///foo".into(),
            fsys::StartupMode::Lazy,
            WeakModelContext::default(),
            WeakExtendedInstance::Component(WeakComponentInstance::default()),
            Arc::new(Hooks::new(None)),
        );
        let weak_component = component.as_weak();

        let protocol_capability = CapabilitySource::Capability {
            source_capability: ComponentCapability::Storage(StorageDecl {
                backing_dir: "/cache".into(),
                name: "cache".into(),
                source: StorageDirectorySource::Parent,
                subdir: None,
            }),
            component: weak_component,
        };
        let valid_path_0 = AbsoluteMoniker::from(vec!["root:0", "bootstrap:0"]);
        let valid_path_1 = AbsoluteMoniker::from(vec!["root:0", "core:0"]);
        let invalid_path_0 = AbsoluteMoniker::from(vec!["foobar:0"]);
        let invalid_path_1 = AbsoluteMoniker::from(vec!["foo:0", "bar:0", "foobar:0"]);

        assert_matches!(
            global_policy_checker.can_route_capability(&protocol_capability, &valid_path_0),
            Ok(())
        );
        assert_matches!(
            global_policy_checker.can_route_capability(&protocol_capability, &valid_path_1),
            Ok(())
        );
        assert_matches!(
            global_policy_checker.can_route_capability(&protocol_capability, &invalid_path_0),
            Err(_)
        );
        assert_matches!(
            global_policy_checker.can_route_capability(&protocol_capability, &invalid_path_1),
            Err(_)
        );
    }

    #[test]
    fn global_policy_checker_can_route_debug_capability_capability_cap() {
        let mut config_builder = CapabilityAllowlistConfigBuilder::new();
        config_builder.add_debug_capability_policy(
            CapabilityAllowlistKey {
                source_moniker: ExtendedMoniker::ComponentInstance(AbsoluteMoniker::from(vec![
                    "foo:0",
                ])),
                source_name: CapabilityName::from("debug_service1"),
                source: CapabilityAllowlistSource::Self_,
                capability: CapabilityTypeName::Protocol,
            },
            vec![
                (AbsoluteMoniker::from(vec!["foo:0"]), "foo_env".to_string()),
                (AbsoluteMoniker::from(vec!["root:0", "bootstrap:0"]), "bootstrap_env".to_string()),
            ],
        );
        let global_policy_checker = GlobalPolicyChecker::new(config_builder.build());

        // Create a fake component instance.
        let resolver = ResolverRegistry::new();
        let top_instance = Arc::new(ComponentManagerInstance::new(vec![]));
        let component = ComponentInstance::new(
            Arc::new(Environment::new_root(
                &top_instance,
                RunnerRegistry::default(),
                resolver,
                DebugRegistry::default(),
            )),
            vec!["foo:0"].into(),
            "test:///foo".into(),
            fsys::StartupMode::Lazy,
            WeakModelContext::default(),
            WeakExtendedInstance::Component(WeakComponentInstance::default()),
            Arc::new(Hooks::new(None)),
        );
        let weak_component = component.as_weak();

        let protocol_capability = CapabilitySource::Component {
            capability: ComponentCapability::Protocol(ProtocolDecl {
                name: "debug_service1".into(),
                source_path: "/svc/debug_service1".parse().unwrap(),
            }),
            component: weak_component,
        };
        let valid_0 =
            (AbsoluteMoniker::from(vec!["root:0", "bootstrap:0"]), "bootstrap_env".to_string());
        let valid_1 = (AbsoluteMoniker::from(vec!["foo:0"]), "foo_env".to_string());
        let invalid_0 = (AbsoluteMoniker::from(vec!["foobar:0"]), "foobar_env".to_string());
        let invalid_1 =
            (AbsoluteMoniker::from(vec!["foo:0", "bar:0", "foobar:0"]), "foobar_env".to_string());
        let target_moniker = AbsoluteMoniker::from(vec!["target:0"]);

        assert_matches!(
            global_policy_checker.can_route_debug_capability(
                &protocol_capability,
                &valid_0.0,
                &valid_0.1,
                &target_moniker
            ),
            Ok(())
        );
        assert_matches!(
            global_policy_checker.can_route_debug_capability(
                &protocol_capability,
                &valid_1.0,
                &valid_1.1,
                &target_moniker
            ),
            Ok(())
        );
        assert_matches!(
            global_policy_checker.can_route_debug_capability(
                &protocol_capability,
                &invalid_0.0,
                &invalid_0.1,
                &target_moniker
            ),
            Err(_)
        );
        assert_matches!(
            global_policy_checker.can_route_debug_capability(
                &protocol_capability,
                &invalid_1.0,
                &invalid_1.1,
                &target_moniker
            ),
            Err(_)
        );
    }

    #[test]
    fn global_policy_checker_can_route_capability_builtin_cap() {
        let mut config_builder = CapabilityAllowlistConfigBuilder::new();
        config_builder.add_capability_policy(
            CapabilityAllowlistKey {
                source_moniker: ExtendedMoniker::ComponentManager,
                source_name: CapabilityName::from("hub"),
                source: CapabilityAllowlistSource::Self_,
                capability: CapabilityTypeName::Directory,
            },
            vec![
                AbsoluteMoniker::from(vec!["root:0"]),
                AbsoluteMoniker::from(vec!["root:0", "core:0"]),
            ],
        );
        let global_policy_checker = GlobalPolicyChecker::new(config_builder.build());

        let dir_capability = CapabilitySource::Builtin {
            capability: InternalCapability::Directory(CapabilityName::from("hub")),
            top_instance: Weak::new(),
        };
        let valid_path_0 = AbsoluteMoniker::from(vec!["root:0"]);
        let valid_path_1 = AbsoluteMoniker::from(vec!["root:0", "core:0"]);
        let invalid_path_0 = AbsoluteMoniker::from(vec!["foobar:0"]);
        let invalid_path_1 = AbsoluteMoniker::from(vec!["foo:0", "bar:0", "foobar:0"]);

        assert_matches!(
            global_policy_checker.can_route_capability(&dir_capability, &valid_path_0),
            Ok(())
        );
        assert_matches!(
            global_policy_checker.can_route_capability(&dir_capability, &valid_path_1),
            Ok(())
        );
        assert_matches!(
            global_policy_checker.can_route_capability(&dir_capability, &invalid_path_0),
            Err(_)
        );
        assert_matches!(
            global_policy_checker.can_route_capability(&dir_capability, &invalid_path_1),
            Err(_)
        );
    }

    #[test]
    fn global_policy_checker_can_route_capability_with_instance_ids_cap() {
        let mut config_builder = CapabilityAllowlistConfigBuilder::new();
        config_builder.add_capability_policy(
            CapabilityAllowlistKey {
                source_moniker: ExtendedMoniker::ComponentManager,
                source_name: CapabilityName::from("hub"),
                source: CapabilityAllowlistSource::Self_,
                capability: CapabilityTypeName::Directory,
            },
            vec![
                AbsoluteMoniker::from(vec!["root:0"]),
                AbsoluteMoniker::from(vec!["root:0", "core:0"]),
            ],
        );
        let global_policy_checker = GlobalPolicyChecker::new(config_builder.build());
        let dir_capability = CapabilitySource::Builtin {
            capability: InternalCapability::Directory(CapabilityName::from("hub")),
            top_instance: Weak::new(),
        };
        let valid_path_0 = AbsoluteMoniker::from(vec!["root:1"]);
        let valid_path_1 = AbsoluteMoniker::from(vec!["root:5", "core:3"]);
        let invalid_path_0 = AbsoluteMoniker::from(vec!["foobar:0"]);
        let invalid_path_1 = AbsoluteMoniker::from(vec!["foo:0", "bar:2", "foobar:0"]);

        assert_matches!(
            global_policy_checker.can_route_capability(&dir_capability, &valid_path_0),
            Ok(())
        );
        assert_matches!(
            global_policy_checker.can_route_capability(&dir_capability, &valid_path_1),
            Ok(())
        );
        assert_matches!(
            global_policy_checker.can_route_capability(&dir_capability, &invalid_path_0),
            Err(_)
        );
        assert_matches!(
            global_policy_checker.can_route_capability(&dir_capability, &invalid_path_1),
            Err(_)
        );
    }
}
