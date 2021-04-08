// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{
        capability_source::CapabilitySourceInterface,
        component_instance::ComponentInstanceInterface,
        config::{CapabilityAllowlistKey, CapabilityAllowlistSource, RuntimeConfig},
    },
    fuchsia_zircon_status as zx,
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

    fn get_policy_key<'a, C>(
        capability_source: &'a CapabilitySourceInterface<C>,
    ) -> Result<CapabilityAllowlistKey, PolicyError>
    where
        C: ComponentInstanceInterface,
    {
        Ok(match &capability_source {
            CapabilitySourceInterface::Namespace { capability, .. } => CapabilityAllowlistKey {
                source_moniker: ExtendedMoniker::ComponentManager,
                source_name: capability
                    .source_name()
                    .ok_or(PolicyError::InvalidCapabilitySource)?
                    .clone(),
                source: CapabilityAllowlistSource::Self_,
                capability: capability.type_name(),
            },
            CapabilitySourceInterface::Component { capability, component } => {
                CapabilityAllowlistKey {
                    source_moniker: ExtendedMoniker::ComponentInstance(component.moniker.clone()),
                    source_name: capability
                        .source_name()
                        .ok_or(PolicyError::InvalidCapabilitySource)?
                        .clone(),
                    source: CapabilityAllowlistSource::Self_,
                    capability: capability.type_name(),
                }
            }
            CapabilitySourceInterface::Builtin { capability, .. } => CapabilityAllowlistKey {
                source_moniker: ExtendedMoniker::ComponentManager,
                source_name: capability.source_name().clone(),
                source: CapabilityAllowlistSource::Self_,
                capability: capability.type_name(),
            },
            CapabilitySourceInterface::Framework { capability, component } => {
                CapabilityAllowlistKey {
                    source_moniker: ExtendedMoniker::ComponentInstance(component.moniker.clone()),
                    source_name: capability.source_name().clone(),
                    source: CapabilityAllowlistSource::Framework,
                    capability: capability.type_name(),
                }
            }
            CapabilitySourceInterface::Capability { source_capability, component } => {
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
            CapabilitySourceInterface::Collection { source_name, component, .. } => {
                CapabilityAllowlistKey {
                    source_moniker: ExtendedMoniker::ComponentInstance(component.moniker.clone()),
                    source_name: source_name.clone(),
                    source: CapabilityAllowlistSource::Self_,
                    capability: cm_rust::CapabilityTypeName::Service,
                }
            }
        })
    }

    /// Returns Ok(()) if the provided capability source can be routed to the
    /// given target_moniker, else a descriptive PolicyError.
    pub fn can_route_capability<'a, C>(
        &self,
        capability_source: &'a CapabilitySourceInterface<C>,
        target_moniker: &'a AbsoluteMoniker,
    ) -> Result<(), PolicyError>
    where
        C: ComponentInstanceInterface,
    {
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
                    Err(PolicyError::capability_use_disallowed(
                        policy_key.source_name.str(),
                        &policy_key.source_moniker,
                        &target_moniker,
                    ))
                }
            },
            None => Ok(()),
        }
    }

    /// Returns Ok(()) if the provided debug capability source is allowed to be routed from given
    /// environment.
    pub fn can_route_debug_capability<'a, C>(
        &self,
        capability_source: &'a CapabilitySourceInterface<C>,
        env_moniker: &'a AbsoluteMoniker,
        env_name: &'a str,
        target_moniker: &'a AbsoluteMoniker,
    ) -> Result<(), PolicyError>
    where
        C: ComponentInstanceInterface,
    {
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
        Err(PolicyError::debug_capability_use_disallowed(
            policy_key.source_name.str(),
            &policy_key.source_moniker,
            &env_moniker,
            env_name,
            target_moniker,
        ))
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
        crate::config::{JobPolicyAllowlists, RuntimeConfig, SecurityPolicy},
        matches::assert_matches,
        moniker::{AbsoluteMoniker, ChildMoniker},
        std::collections::HashMap,
    };

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
}
