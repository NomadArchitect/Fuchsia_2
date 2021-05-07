// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    async_trait::async_trait,
    cm_fidl_analyzer::{
        component_model::{ComponentModelForAnalyzer, ModelBuilderForAnalyzer},
        component_tree::{ComponentTreeBuilder, NodePath},
    },
    cm_rust::{ComponentDecl, ExposeDecl, ExposeDeclCommon, UseDecl},
    fuchsia_zircon_status as zx_status,
    moniker::AbsoluteMoniker,
    routing::{component_instance::ComponentInstanceInterface, config::RuntimeConfig},
    routing_test_helpers::{
        CheckUse, CommonRoutingTest, ExpectedResult, RoutingTestModel, RoutingTestModelBuilder,
    },
    std::{collections::HashMap, iter::FromIterator, sync::Arc},
    thiserror::Error,
};

const TEST_URL_PREFIX: &str = "test:///";

struct RoutingTestForAnalyzer {
    model: Arc<ComponentModelForAnalyzer>,
}

struct RoutingTestBuilderForAnalyzer {
    root_url: String,
    decls_by_url: HashMap<String, ComponentDecl>,
}

impl RoutingTestBuilderForAnalyzer {
    fn build_runtime_config(&self) -> Arc<RuntimeConfig> {
        Arc::new(RuntimeConfig::default())
    }
}

#[async_trait]
impl RoutingTestModelBuilder for RoutingTestBuilderForAnalyzer {
    type Model = RoutingTestForAnalyzer;

    fn new(root_component: &str, components: Vec<(&'static str, ComponentDecl)>) -> Self {
        let root_url = format!("{}{}", TEST_URL_PREFIX, root_component);
        let decls_by_url = HashMap::from_iter(
            components
                .into_iter()
                .map(|(name, decl)| (format!("{}{}", TEST_URL_PREFIX, name), decl)),
        );
        Self { root_url, decls_by_url }
    }

    async fn build(self) -> RoutingTestForAnalyzer {
        let config = self.build_runtime_config();
        let tree = ComponentTreeBuilder::new(self.decls_by_url)
            .build(self.root_url)
            .tree
            .expect("failed to build ComponentTree");

        let model = ModelBuilderForAnalyzer::new()
            .build(tree, config)
            .await
            .expect("failed to build ComponentModelForAnalyzer");
        RoutingTestForAnalyzer { model }
    }
}

#[derive(Debug, Error)]
pub enum TestModelError {
    #[error("matching use decl not found")]
    UseDeclNotFound,
    #[error("matching expose decl not found")]
    ExposeDeclNotFound,
}

impl TestModelError {
    pub fn as_zx_status(&self) -> zx_status::Status {
        match self {
            Self::UseDeclNotFound | Self::ExposeDeclNotFound => zx_status::Status::NOT_FOUND,
        }
    }
}

impl RoutingTestForAnalyzer {
    fn find_matching_use(
        &self,
        check: CheckUse,
        decl: &ComponentDecl,
    ) -> (Result<UseDecl, TestModelError>, ExpectedResult) {
        match check {
            CheckUse::Directory { path, expected_res, .. } => (
                decl.uses
                    .iter()
                    .find_map(|u| match u {
                        UseDecl::Directory(d) if d.target_path == path => Some(u.clone()),
                        _ => None,
                    })
                    .ok_or(TestModelError::UseDeclNotFound),
                expected_res,
            ),
            CheckUse::Event { .. } => unimplemented![],
            CheckUse::Protocol { path, expected_res, .. } => (
                decl.uses
                    .iter()
                    .find_map(|u| match u {
                        UseDecl::Protocol(d) if d.target_path == path => Some(u.clone()),
                        _ => None,
                    })
                    .ok_or(TestModelError::UseDeclNotFound),
                expected_res,
            ),
            CheckUse::Service { .. } => unimplemented![],
            CheckUse::Storage { .. } => unimplemented![],
            CheckUse::StorageAdmin { .. } => unimplemented![],
        }
    }

    fn find_matching_expose(
        &self,
        check: CheckUse,
        decl: &ComponentDecl,
    ) -> (Result<ExposeDecl, TestModelError>, ExpectedResult) {
        match check {
            CheckUse::Directory { path, expected_res, .. }
            | CheckUse::Protocol { path, expected_res, .. } => (
                decl.exposes
                    .iter()
                    .find(|&e| e.target_name().to_string() == path.basename)
                    .cloned()
                    .ok_or(TestModelError::ExposeDeclNotFound),
                expected_res,
            ),
            CheckUse::Service { .. } => unimplemented![],
            CheckUse::Event { .. } | CheckUse::Storage { .. } | CheckUse::StorageAdmin { .. } => {
                panic!("attempted to use from expose for unsupported capability type")
            }
        }
    }
}

#[async_trait]
impl RoutingTestModel for RoutingTestForAnalyzer {
    async fn check_use(&self, moniker: AbsoluteMoniker, check: CheckUse) {
        let target_id = NodePath::new(
            moniker.path().into_iter().map(|child_moniker| child_moniker.to_partial()).collect(),
        );
        let target = self.model.get_instance(&target_id).expect("target instance not found");
        let target_decl = target.decl().await.expect("target ComponentDecl not found");

        let (find_decl, expected) = self.find_matching_use(check, &target_decl);

        // If `find_decl` is not OK, check that `expected` has a matching error.
        // Otherwise, route the capability and compare the result to `expected`.
        match &find_decl {
            Err(err) => {
                match expected {
                    ExpectedResult::Ok => panic!("expected ExposeDecl was not found"),
                    ExpectedResult::Err(status) => {
                        assert_eq!(err.as_zx_status(), status);
                    }
                    _ => unimplemented![],
                };
                return;
            }
            Ok(use_decl) => match self.model.check_use_capability(use_decl, &target).await {
                Err(ref err) => match expected {
                    ExpectedResult::Ok => panic!("routing failed, expected success"),
                    ExpectedResult::Err(status) => {
                        assert_eq!(err.as_zx_status(), status);
                    }
                    _ => unimplemented![],
                },
                Ok(()) => match expected {
                    ExpectedResult::Ok => {}
                    _ => panic!("capability use succeeded, expected failure"),
                },
            },
        }
    }

    async fn check_use_exposed_dir(&self, moniker: AbsoluteMoniker, check: CheckUse) {
        let target =
            self.model.get_instance(&NodePath::from(moniker)).expect("target instance not found");
        let target_decl = target.decl().await.expect("target ComponentDecl not found");

        let (find_decl, expected) = self.find_matching_expose(check, &target_decl);

        // If `find_decl` is not OK, check that `expected` has a matching error.
        // Otherwise, route the capability and compare the result to `expected`.
        match &find_decl {
            Err(err) => {
                match expected {
                    ExpectedResult::Ok => panic!("expected ExposeDecl was not found"),
                    ExpectedResult::Err(status) => {
                        assert_eq!(err.as_zx_status(), status);
                    }
                    _ => unimplemented![],
                };
                return;
            }
            Ok(expose_decl) => {
                match self.model.check_use_exposed_capability(expose_decl, &target).await {
                    Err(err) => match expected {
                        ExpectedResult::Ok => panic!("routing failed, expected success"),
                        ExpectedResult::Err(status) => {
                            assert_eq!(err.as_zx_status(), status);
                        }
                        _ => unimplemented![],
                    },
                    Ok(()) => match expected {
                        ExpectedResult::Ok => {}
                        _ => panic!("capability use succeeded, expected failure"),
                    },
                }
            }
        }
    }
}

mod tests {
    use {super::*, futures::executor::block_on};

    #[test]
    fn use_from_child() {
        block_on(async {
            CommonRoutingTest::<RoutingTestBuilderForAnalyzer>::new().test_use_from_child().await
        });
    }

    #[test]
    fn use_from_grandchild() {
        block_on(async {
            CommonRoutingTest::<RoutingTestBuilderForAnalyzer>::new()
                .test_use_from_grandchild()
                .await
        });
    }

    #[test]
    fn use_from_grandparent() {
        block_on(async {
            CommonRoutingTest::<RoutingTestBuilderForAnalyzer>::new()
                .test_use_from_grandparent()
                .await
        });
    }

    #[test]
    fn use_from_sibling_no_root() {
        block_on(async {
            CommonRoutingTest::<RoutingTestBuilderForAnalyzer>::new()
                .test_use_from_sibling_no_root()
                .await
        });
    }

    #[test]
    fn use_from_sibling_root() {
        block_on(async {
            CommonRoutingTest::<RoutingTestBuilderForAnalyzer>::new()
                .test_use_from_sibling_root()
                .await
        });
    }

    #[test]
    fn use_from_niece() {
        block_on(async {
            CommonRoutingTest::<RoutingTestBuilderForAnalyzer>::new().test_use_from_niece().await
        });
    }

    #[test]
    fn use_kitchen_sink() {
        block_on(async {
            CommonRoutingTest::<RoutingTestBuilderForAnalyzer>::new().test_use_kitchen_sink().await
        });
    }

    #[test]
    fn use_not_offered() {
        block_on(async {
            CommonRoutingTest::<RoutingTestBuilderForAnalyzer>::new().test_use_not_offered().await
        });
    }

    #[test]
    fn use_offer_source_not_offered() {
        block_on(async {
            CommonRoutingTest::<RoutingTestBuilderForAnalyzer>::new()
                .test_use_offer_source_not_offered()
                .await
        });
    }

    #[test]
    fn use_offer_source_not_exposed() {
        block_on(async {
            CommonRoutingTest::<RoutingTestBuilderForAnalyzer>::new()
                .test_use_offer_source_not_exposed()
                .await
        });
    }

    #[test]
    fn use_from_expose() {
        block_on(async {
            CommonRoutingTest::<RoutingTestBuilderForAnalyzer>::new().test_use_from_expose().await
        });
    }

    #[test]
    fn use_from_expose_to_framework() {
        block_on(async {
            CommonRoutingTest::<RoutingTestBuilderForAnalyzer>::new()
                .test_use_from_expose_to_framework()
                .await
        });
    }

    #[test]
    fn offer_from_non_executable() {
        block_on(async {
            CommonRoutingTest::<RoutingTestBuilderForAnalyzer>::new()
                .test_offer_from_non_executable()
                .await
        });
    }

    #[test]
    fn expose_from_self_and_child() {
        block_on(async {
            CommonRoutingTest::<RoutingTestBuilderForAnalyzer>::new()
                .test_expose_from_self_and_child()
                .await
        });
    }

    #[test]
    fn use_not_exposed() {
        block_on(async {
            CommonRoutingTest::<RoutingTestBuilderForAnalyzer>::new().test_use_not_exposed().await
        });
    }
}
