// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{
        capability::{CapabilityProvider, CapabilitySource, InternalCapability, OptionalTask},
        channel,
        model::{
            error::ModelError,
            hooks::{Event, EventPayload, EventType, Hook, HooksRegistration},
            rights,
            testing::routing_test_helpers::*,
        },
    },
    async_trait::async_trait,
    cm_rust::*,
    cm_rust_testing::*,
    fidl::endpoints::ServerEnd,
    fidl_fuchsia_io::{self as fio, DirectoryProxy},
    fuchsia_zircon as zx,
    std::{
        convert::TryFrom,
        path::PathBuf,
        sync::{Arc, Weak},
    },
};

#[fuchsia::test]
async fn offer_increasing_rights() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .offer(OfferDecl::Directory(OfferDirectoryDecl {
                    source: OfferSource::Child("b".to_string()),
                    source_name: "bar_data".into(),
                    target_name: "baz_data".into(),
                    target: OfferTarget::Child("c".to_string()),
                    rights: Some(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS),
                    subdir: None,
                    dependency_type: DependencyType::Strong,
                }))
                .add_lazy_child("b")
                .add_lazy_child("c")
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .directory(
                    DirectoryDeclBuilder::new("foo_data")
                        .rights(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS)
                        .build(),
                )
                .expose(ExposeDecl::Directory(ExposeDirectoryDecl {
                    source: ExposeSource::Self_,
                    source_name: "foo_data".into(),
                    target_name: "bar_data".into(),
                    target: ExposeTarget::Parent,
                    rights: Some(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS),
                    subdir: None,
                }))
                .build(),
        ),
        (
            "c",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Directory(UseDirectoryDecl {
                    source: UseSource::Parent,
                    source_name: "baz_data".into(),
                    target_path: CapabilityPath::try_from("/data/hippo").unwrap(),
                    rights: *rights::READ_RIGHTS,
                    subdir: None,
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;
    test.check_use(vec!["c:0"].into(), CheckUse::default_directory(ExpectedResult::Ok)).await;
}

#[fuchsia::test]
async fn offer_incompatible_rights() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .offer(OfferDecl::Directory(OfferDirectoryDecl {
                    source: OfferSource::Child("b".to_string()),
                    source_name: "bar_data".into(),
                    target_name: "baz_data".into(),
                    target: OfferTarget::Child("c".to_string()),
                    rights: Some(*rights::WRITE_RIGHTS),
                    subdir: None,
                    dependency_type: DependencyType::Strong,
                }))
                .add_lazy_child("b")
                .add_lazy_child("c")
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .directory(
                    DirectoryDeclBuilder::new("foo_data")
                        .rights(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS)
                        .build(),
                )
                .expose(ExposeDecl::Directory(ExposeDirectoryDecl {
                    source: ExposeSource::Self_,
                    source_name: "foo_data".into(),
                    target_name: "bar_data".into(),
                    target: ExposeTarget::Parent,
                    rights: Some(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS),
                    subdir: None,
                }))
                .build(),
        ),
        (
            "c",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Directory(UseDirectoryDecl {
                    source: UseSource::Parent,
                    source_name: "baz_data".into(),
                    target_path: CapabilityPath::try_from("/data/hippo").unwrap(),
                    rights: *rights::READ_RIGHTS,
                    subdir: None,
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;
    test.check_use(
        vec!["c:0"].into(),
        CheckUse::default_directory(ExpectedResult::Err(zx::Status::UNAVAILABLE)),
    )
    .await;
}

#[fuchsia::test]
async fn expose_increasing_rights() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .offer(OfferDecl::Directory(OfferDirectoryDecl {
                    source: OfferSource::Child("b".to_string()),
                    source_name: "bar_data".into(),
                    target_name: "baz_data".into(),
                    target: OfferTarget::Child("c".to_string()),
                    rights: Some(*rights::READ_RIGHTS),
                    subdir: None,
                    dependency_type: DependencyType::Strong,
                }))
                .add_lazy_child("b")
                .add_lazy_child("c")
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .directory(
                    DirectoryDeclBuilder::new("foo_data")
                        .rights(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS)
                        .build(),
                )
                .expose(ExposeDecl::Directory(ExposeDirectoryDecl {
                    source: ExposeSource::Self_,
                    source_name: "foo_data".into(),
                    target_name: "bar_data".into(),
                    target: ExposeTarget::Parent,
                    rights: Some(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS),
                    subdir: None,
                }))
                .build(),
        ),
        (
            "c",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Directory(UseDirectoryDecl {
                    source: UseSource::Parent,
                    source_name: "baz_data".into(),
                    target_path: CapabilityPath::try_from("/data/hippo").unwrap(),
                    rights: *rights::READ_RIGHTS,
                    subdir: None,
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;
    test.check_use(vec!["c:0"].into(), CheckUse::default_directory(ExpectedResult::Ok)).await;
}

#[fuchsia::test]
async fn expose_incompatible_rights() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .offer(OfferDecl::Directory(OfferDirectoryDecl {
                    source: OfferSource::Child("b".to_string()),
                    source_name: "bar_data".into(),
                    target_name: "baz_data".into(),
                    target: OfferTarget::Child("c".to_string()),
                    rights: Some(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS),
                    subdir: None,
                    dependency_type: DependencyType::Strong,
                }))
                .add_lazy_child("b")
                .add_lazy_child("c")
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .directory(
                    DirectoryDeclBuilder::new("foo_data")
                        .rights(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS)
                        .build(),
                )
                .expose(ExposeDecl::Directory(ExposeDirectoryDecl {
                    source: ExposeSource::Self_,
                    source_name: "foo_data".into(),
                    target_name: "bar_data".into(),
                    target: ExposeTarget::Parent,
                    rights: Some(*rights::WRITE_RIGHTS),
                    subdir: None,
                }))
                .build(),
        ),
        (
            "c",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Directory(UseDirectoryDecl {
                    source: UseSource::Parent,
                    source_name: "baz_data".into(),
                    target_path: CapabilityPath::try_from("/data/hippo").unwrap(),
                    rights: *rights::READ_RIGHTS,
                    subdir: None,
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;
    test.check_use(
        vec!["c:0"].into(),
        CheckUse::default_directory(ExpectedResult::Err(zx::Status::UNAVAILABLE)),
    )
    .await;
}

#[fuchsia::test]
async fn capability_increasing_rights() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .offer(OfferDecl::Directory(OfferDirectoryDecl {
                    source: OfferSource::Child("b".to_string()),
                    source_name: "bar_data".into(),
                    target_name: "baz_data".into(),
                    target: OfferTarget::Child("c".to_string()),
                    rights: Some(*rights::READ_RIGHTS),
                    subdir: None,
                    dependency_type: DependencyType::Strong,
                }))
                .add_lazy_child("b")
                .add_lazy_child("c")
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .directory(
                    DirectoryDeclBuilder::new("foo_data")
                        .rights(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS)
                        .build(),
                )
                .expose(ExposeDecl::Directory(ExposeDirectoryDecl {
                    source: ExposeSource::Self_,
                    source_name: "foo_data".into(),
                    target_name: "bar_data".into(),
                    target: ExposeTarget::Parent,
                    rights: Some(*rights::READ_RIGHTS),
                    subdir: None,
                }))
                .build(),
        ),
        (
            "c",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Directory(UseDirectoryDecl {
                    source: UseSource::Parent,
                    source_name: "baz_data".into(),
                    target_path: CapabilityPath::try_from("/data/hippo").unwrap(),
                    rights: *rights::READ_RIGHTS,
                    subdir: None,
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;
    test.check_use(vec!["c:0"].into(), CheckUse::default_directory(ExpectedResult::Ok)).await;
}

#[fuchsia::test]
async fn capability_incompatible_rights() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .offer(OfferDecl::Directory(OfferDirectoryDecl {
                    source: OfferSource::Child("b".to_string()),
                    source_name: "bar_data".into(),
                    target_name: "baz_data".into(),
                    target: OfferTarget::Child("c".to_string()),
                    rights: Some(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS),
                    subdir: None,
                    dependency_type: DependencyType::Strong,
                }))
                .add_lazy_child("b")
                .add_lazy_child("c")
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .directory(
                    DirectoryDeclBuilder::new("foo_data").rights(*rights::WRITE_RIGHTS).build(),
                )
                .expose(ExposeDecl::Directory(ExposeDirectoryDecl {
                    source: ExposeSource::Self_,
                    source_name: "foo_data".into(),
                    target_name: "bar_data".into(),
                    target: ExposeTarget::Parent,
                    rights: Some(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS),
                    subdir: None,
                }))
                .build(),
        ),
        (
            "c",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Directory(UseDirectoryDecl {
                    source: UseSource::Parent,
                    source_name: "baz_data".into(),
                    target_path: CapabilityPath::try_from("/data/hippo").unwrap(),
                    rights: *rights::READ_RIGHTS,
                    subdir: None,
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;
    test.check_use(
        vec!["c:0"].into(),
        CheckUse::default_directory(ExpectedResult::Err(zx::Status::UNAVAILABLE)),
    )
    .await;
}

struct MockFrameworkDirectoryProvider {
    test_dir_proxy: DirectoryProxy,
}
struct MockFrameworkDirectoryHost {
    test_dir_proxy: DirectoryProxy,
}

#[async_trait]
impl CapabilityProvider for MockFrameworkDirectoryProvider {
    async fn open(
        self: Box<Self>,
        flags: u32,
        open_mode: u32,
        relative_path: PathBuf,
        server_end: &mut zx::Channel,
    ) -> Result<OptionalTask, ModelError> {
        let relative_path = relative_path.to_str().unwrap();
        let server_end = channel::take_channel(server_end);
        let server_end = ServerEnd::<fio::NodeMarker>::new(server_end);
        self.test_dir_proxy
            .open(flags, open_mode, relative_path, server_end)
            .expect("failed to open test dir");
        Ok(None.into())
    }
}

#[async_trait]
impl Hook for MockFrameworkDirectoryHost {
    async fn on(self: Arc<Self>, event: &Event) -> Result<(), ModelError> {
        if let Ok(EventPayload::CapabilityRouted {
            source:
                CapabilitySource::Framework {
                    capability: InternalCapability::Directory(source_name),
                    ..
                },
            capability_provider,
        }) = &event.result
        {
            let mut capability_provider = capability_provider.lock().await;
            if source_name.str() == "foo_data" {
                let test_dir_proxy =
                    io_util::clone_directory(&self.test_dir_proxy, fio::CLONE_FLAG_SAME_RIGHTS)
                        .expect("failed to clone test dir");
                *capability_provider =
                    Some(Box::new(MockFrameworkDirectoryProvider { test_dir_proxy }));
            }
        }
        Ok(())
    }
}

#[fuchsia::test]
async fn framework_directory_rights() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .offer(OfferDecl::Directory(OfferDirectoryDecl {
                    source: OfferSource::Framework,
                    source_name: "foo_data".into(),
                    target_name: "foo_data".into(),
                    target: OfferTarget::Child("b".to_string()),
                    rights: None,
                    subdir: Some("foo".into()),
                    dependency_type: DependencyType::Strong,
                }))
                .add_lazy_child("b")
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Directory(UseDirectoryDecl {
                    source: UseSource::Parent,
                    source_name: "foo_data".into(),
                    target_path: CapabilityPath::try_from("/data/hippo").unwrap(),
                    rights: *rights::READ_RIGHTS,
                    subdir: None,
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;
    let test_dir_proxy =
        io_util::clone_directory(&test.test_dir_proxy, fio::CLONE_FLAG_SAME_RIGHTS)
            .expect("failed to clone test dir");
    let directory_host = Arc::new(MockFrameworkDirectoryHost { test_dir_proxy });
    test.model
        .root
        .hooks
        .install(vec![HooksRegistration::new(
            "MockFrameworkDirectoryHost",
            vec![EventType::CapabilityRouted],
            Arc::downgrade(&directory_host) as Weak<dyn Hook>,
        )])
        .await;
    test.check_use(vec!["b:0"].into(), CheckUse::default_directory(ExpectedResult::Ok)).await;
}

#[fuchsia::test]
async fn framework_directory_incompatible_rights() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .offer(OfferDecl::Directory(OfferDirectoryDecl {
                    source: OfferSource::Framework,
                    source_name: "foo_data".into(),
                    target_name: "foo_data".into(),
                    target: OfferTarget::Child("b".to_string()),
                    rights: None,
                    subdir: Some("foo".into()),
                    dependency_type: DependencyType::Strong,
                }))
                .add_lazy_child("b")
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Directory(UseDirectoryDecl {
                    source: UseSource::Parent,
                    source_name: "foo_data".into(),
                    target_path: CapabilityPath::try_from("/data/hippo").unwrap(),
                    rights: *rights::READ_RIGHTS | *rights::WRITE_RIGHTS,
                    subdir: None,
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;
    let test_dir_proxy =
        io_util::clone_directory(&test.test_dir_proxy, fio::CLONE_FLAG_SAME_RIGHTS)
            .expect("failed to clone test dir");
    let directory_host = Arc::new(MockFrameworkDirectoryHost { test_dir_proxy });
    test.model
        .root
        .hooks
        .install(vec![HooksRegistration::new(
            "MockFrameworkDirectoryHost",
            vec![EventType::CapabilityRouted],
            Arc::downgrade(&directory_host) as Weak<dyn Hook>,
        )])
        .await;
    test.check_use(
        vec!["b:0"].into(),
        CheckUse::default_directory(ExpectedResult::Err(zx::Status::UNAVAILABLE)),
    )
    .await;
}

///  component manager's namespace
///   |
///   a
///    \
///     b
///
/// a: offers directory /offer_from_cm_namespace/data/foo from realm as bar_data
/// b: uses directory bar_data as /data/hippo, but the rights don't match
#[fuchsia::test]
async fn offer_from_component_manager_namespace_directory_incompatible_rights() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .offer(OfferDecl::Directory(OfferDirectoryDecl {
                    source: OfferSource::Parent,
                    source_name: "foo_data".into(),
                    target_name: "bar_data".into(),
                    target: OfferTarget::Child("b".to_string()),
                    rights: None,
                    subdir: None,
                    dependency_type: DependencyType::Strong,
                }))
                .add_lazy_child("b")
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Directory(UseDirectoryDecl {
                    source: UseSource::Parent,
                    source_name: "bar_data".into(),
                    target_path: CapabilityPath::try_from("/data/hippo").unwrap(),
                    rights: *rights::READ_RIGHTS,
                    subdir: None,
                }))
                .build(),
        ),
    ];
    let namespace_capabilities = vec![CapabilityDecl::Directory(
        DirectoryDeclBuilder::new("foo_data")
            .path("/offer_from_cm_namespace/data/foo")
            .rights(*rights::WRITE_RIGHTS)
            .build(),
    )];
    let test = RoutingTestBuilder::new("a", components)
        .set_namespace_capabilities(namespace_capabilities)
        .build()
        .await;
    let _ns_dir = ScopedNamespaceDir::new(&test, "/offer_from_cm_namespace");
    test.check_use(
        vec!["b:0"].into(),
        CheckUse::default_directory(ExpectedResult::Err(zx::Status::UNAVAILABLE)),
    )
    .await;
}
