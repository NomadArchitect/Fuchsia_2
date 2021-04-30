// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{
        config::{CapabilityAllowlistKey, CapabilityAllowlistSource},
        model::{
            component::BindReason,
            error::ModelError,
            rights,
            routing::{route_and_open_capability, OpenOptions, OpenStorageOptions},
            testing::{routing_test_helpers::*, test_helpers::*},
        },
    },
    cm_rust::*,
    cm_rust_testing::*,
    component_id_index::gen_instance_id,
    fidl_fuchsia_io as fio, fidl_fuchsia_sys2 as fsys, fuchsia_zircon as zx,
    moniker::{AbsoluteMoniker, ExtendedMoniker, RelativeMoniker},
    routing::{error::RoutingError, RouteRequest},
    routing_test_helpers::component_id_index::make_index_file,
    std::{collections::HashSet, convert::TryInto, fs, path::PathBuf},
};

///   component manager's namespace
///    |
///    a
///    |
///    b
///
/// a: has storage decl with name "mystorage" with a source of realm at path /data
/// a: offers cache storage to b from "mystorage"
/// b: uses cache storage as /storage.
#[fuchsia::test]
async fn storage_dir_from_cm_namespace() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .offer(OfferDecl::Storage(OfferStorageDecl {
                    source_name: "cache".into(),
                    target_name: "cache".into(),
                    source: OfferSource::Self_,
                    target: OfferTarget::Child("b".to_string()),
                }))
                .add_lazy_child("b")
                .storage(StorageDecl {
                    name: "cache".into(),
                    backing_dir: "tmp".try_into().unwrap(),
                    source: StorageDirectorySource::Parent,
                    subdir: Some(PathBuf::from("cache")),
                    storage_id: fsys::StorageId::StaticInstanceIdOrMoniker,
                })
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Storage(UseStorageDecl {
                    source_name: "cache".into(),
                    target_path: "/storage".try_into().unwrap(),
                }))
                .build(),
        ),
    ];
    let namespace_capabilities = vec![CapabilityDecl::Directory(
        DirectoryDeclBuilder::new("tmp")
            .path("/tmp")
            .rights(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS)
            .build(),
    )];
    let test = RoutingTestBuilder::new("a", components)
        .set_namespace_capabilities(namespace_capabilities)
        .build()
        .await;
    test.check_use(
        vec!["b:0"].into(),
        CheckUse::Storage {
            path: "/storage".try_into().unwrap(),
            storage_relation: Some(RelativeMoniker::new(vec![], vec!["b:0".into()])),
            from_cm_namespace: true,
            storage_subdir: Some("cache".to_string()),
            expected_res: ExpectedResult::Ok,
        },
    )
    .await;
    let tmp_cache_entries =
        fs::read_dir("/tmp/cache").unwrap().map(|e| e.unwrap().path()).collect::<Vec<PathBuf>>();
    assert_eq!(tmp_cache_entries, vec![PathBuf::from("/tmp/cache/b:0")]);
}

///   a
///    \
///     b
///
/// a: has storage decl with name "mystorage" with a source of self at path /data
/// a: offers cache storage to b from "mystorage"
/// b: uses cache storage as /storage
#[fuchsia::test]
async fn storage_and_dir_from_parent() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .directory(
                    DirectoryDeclBuilder::new("data")
                        .path("/data")
                        .rights(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS)
                        .build(),
                )
                .offer(OfferDecl::Storage(OfferStorageDecl {
                    source: OfferSource::Self_,
                    target: OfferTarget::Child("b".to_string()),
                    source_name: "cache".into(),
                    target_name: "cache".into(),
                }))
                .add_lazy_child("b")
                .storage(StorageDecl {
                    name: "cache".into(),
                    backing_dir: "data".try_into().unwrap(),
                    source: StorageDirectorySource::Self_,
                    subdir: None,
                    storage_id: fsys::StorageId::StaticInstanceIdOrMoniker,
                })
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Storage(UseStorageDecl {
                    source_name: "cache".into(),
                    target_path: "/storage".try_into().unwrap(),
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;
    test.check_use(
        vec!["b:0"].into(),
        CheckUse::Storage {
            path: "/storage".try_into().unwrap(),
            storage_relation: Some(RelativeMoniker::new(vec![], vec!["b:0".into()])),
            from_cm_namespace: false,
            storage_subdir: None,
            expected_res: ExpectedResult::Ok,
        },
    )
    .await;
    assert_eq!(test.list_directory(".").await, vec!["b:0".to_string(), "foo".to_string()],);
}

///   a
///    \
///     b
///
/// a: has storage decl with name "mystorage" with a source of self at path /data, with subdir
///    "cache"
/// a: offers cache storage to b from "mystorage"
/// b: uses cache storage as /storage
#[fuchsia::test]
async fn storage_and_dir_from_parent_with_subdir() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .directory(
                    DirectoryDeclBuilder::new("data")
                        .path("/data")
                        .rights(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS)
                        .build(),
                )
                .offer(OfferDecl::Storage(OfferStorageDecl {
                    source: OfferSource::Self_,
                    target: OfferTarget::Child("b".to_string()),
                    source_name: "cache".into(),
                    target_name: "cache".into(),
                }))
                .add_lazy_child("b")
                .storage(StorageDecl {
                    name: "cache".into(),
                    backing_dir: "data".try_into().unwrap(),
                    source: StorageDirectorySource::Self_,
                    subdir: Some(PathBuf::from("cache")),
                    storage_id: fsys::StorageId::StaticInstanceIdOrMoniker,
                })
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Storage(UseStorageDecl {
                    source_name: "cache".into(),
                    target_path: "/storage".try_into().unwrap(),
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;
    test.check_use(
        vec!["b:0"].into(),
        CheckUse::Storage {
            path: "/storage".try_into().unwrap(),
            storage_relation: Some(RelativeMoniker::new(vec![], vec!["b:0".into()])),
            from_cm_namespace: false,
            storage_subdir: Some("cache".to_string()),
            expected_res: ExpectedResult::Ok,
        },
    )
    .await;
    assert_eq!(test.list_directory(".").await, vec!["cache".to_string(), "foo".to_string()],);
}

///   a
///    \
///     b
///
/// a: has storage decl with name "mystorage" with a source of self at path /data, but /data
///    has only read rights
/// a: offers cache storage to b from "mystorage"
/// b: uses cache storage as /storage
#[fuchsia::test]
async fn storage_and_dir_from_parent_rights_invalid() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .directory(
                    DirectoryDeclBuilder::new("data")
                        .path("/data")
                        .rights(*rights::READ_RIGHTS)
                        .build(),
                )
                .offer(OfferDecl::Storage(OfferStorageDecl {
                    source: OfferSource::Self_,
                    target: OfferTarget::Child("b".to_string()),
                    source_name: "cache".into(),
                    target_name: "cache".into(),
                }))
                .add_lazy_child("b")
                .storage(StorageDecl {
                    name: "cache".into(),
                    backing_dir: "data".try_into().unwrap(),
                    source: StorageDirectorySource::Self_,
                    subdir: None,
                    storage_id: fsys::StorageId::StaticInstanceIdOrMoniker,
                })
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Storage(UseStorageDecl {
                    source_name: "cache".into(),
                    target_path: "/storage".try_into().unwrap(),
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;
    test.check_use(
        vec!["b:0"].into(),
        CheckUse::Storage {
            path: "/storage".try_into().unwrap(),
            storage_relation: None,
            from_cm_namespace: false,
            storage_subdir: None,
            expected_res: ExpectedResult::Err(zx::Status::UNAVAILABLE),
        },
    )
    .await;
}

///   a
///    \
///     b
///      \
///       c
///
/// a: offers directory /data to b as /minfs
/// b: has storage decl with name "mystorage" with a source of realm at path /minfs
/// b: offers data storage to c from "mystorage"
/// c: uses data storage as /storage
#[fuchsia::test]
async fn storage_from_parent_dir_from_grandparent() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .directory(
                    DirectoryDeclBuilder::new("data")
                        .path("/data")
                        .rights(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS)
                        .build(),
                )
                .offer(OfferDecl::Directory(OfferDirectoryDecl {
                    source: OfferSource::Self_,
                    source_name: "data".try_into().unwrap(),
                    target_name: "minfs".try_into().unwrap(),
                    target: OfferTarget::Child("b".to_string()),
                    rights: Some(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS),
                    subdir: None,
                    dependency_type: DependencyType::Strong,
                }))
                .add_lazy_child("b")
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .offer(OfferDecl::Storage(OfferStorageDecl {
                    source: OfferSource::Self_,
                    target: OfferTarget::Child("c".to_string()),
                    source_name: "data".into(),
                    target_name: "data".into(),
                }))
                .add_lazy_child("c")
                .storage(StorageDecl {
                    name: "data".into(),
                    backing_dir: "minfs".try_into().unwrap(),
                    source: StorageDirectorySource::Parent,
                    subdir: None,
                    storage_id: fsys::StorageId::StaticInstanceIdOrMoniker,
                })
                .build(),
        ),
        (
            "c",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Storage(UseStorageDecl {
                    source_name: "data".into(),
                    target_path: "/storage".try_into().unwrap(),
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;
    test.check_use(
        vec!["b:0", "c:0"].into(),
        CheckUse::Storage {
            path: "/storage".try_into().unwrap(),
            storage_relation: Some(RelativeMoniker::new(vec![], vec!["c:0".into()])),
            from_cm_namespace: false,
            storage_subdir: None,
            expected_res: ExpectedResult::Ok,
        },
    )
    .await;
}

///   a
///    \
///     b
///      \
///       c
///
/// a: offers directory /data to b as /minfs with subdir "subdir_1"
/// b: has storage decl with name "mystorage" with a source of realm at path /minfs with subdir
///    "subdir_2"
/// b: offers data storage to c from "mystorage"
/// c: uses data storage as /storage
#[fuchsia::test]
async fn storage_from_parent_dir_from_grandparent_with_subdirs() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .directory(
                    DirectoryDeclBuilder::new("data")
                        .path("/data")
                        .rights(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS)
                        .build(),
                )
                .offer(OfferDecl::Directory(OfferDirectoryDecl {
                    source: OfferSource::Self_,
                    source_name: "data".try_into().unwrap(),
                    target_name: "minfs".try_into().unwrap(),
                    target: OfferTarget::Child("b".to_string()),
                    rights: Some(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS),
                    subdir: Some("subdir_1".into()),
                    dependency_type: DependencyType::Strong,
                }))
                .add_lazy_child("b")
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .offer(OfferDecl::Storage(OfferStorageDecl {
                    source: OfferSource::Self_,
                    target: OfferTarget::Child("c".to_string()),
                    source_name: "data".into(),
                    target_name: "data".into(),
                }))
                .add_lazy_child("c")
                .storage(StorageDecl {
                    name: "data".into(),
                    backing_dir: "minfs".try_into().unwrap(),
                    source: StorageDirectorySource::Parent,
                    subdir: Some("subdir_2".into()),
                    storage_id: fsys::StorageId::StaticInstanceIdOrMoniker,
                })
                .build(),
        ),
        (
            "c",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Storage(UseStorageDecl {
                    source_name: "data".into(),
                    target_path: "/storage".try_into().unwrap(),
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;
    test.add_subdir_to_data_directory("subdir_1");
    test.check_use(
        vec!["b:0", "c:0"].into(),
        CheckUse::Storage {
            path: "/storage".try_into().unwrap(),
            storage_relation: Some(RelativeMoniker::new(vec![], vec!["c:0".into()])),
            from_cm_namespace: false,
            storage_subdir: Some("subdir_1/subdir_2".to_string()),
            expected_res: ExpectedResult::Ok,
        },
    )
    .await;
    assert_eq!(test.list_directory(".").await, vec!["foo".to_string(), "subdir_1".to_string()]);
    assert_eq!(test.list_directory("subdir_1").await, vec!["subdir_2".to_string()]);
    assert_eq!(test.list_directory("subdir_1/subdir_2").await, vec!["c:0".to_string()]);
}

///   a
///    \
///     b
///      \
///       c
///
/// a: offers directory /data to b as /minfs
/// b: has storage decl with name "mystorage" with a source of realm at path /minfs, subdir "bar"
/// b: offers data storage to c from "mystorage"
/// c: uses data storage as /storage
#[fuchsia::test]
async fn storage_from_parent_dir_from_grandparent_with_subdir() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .directory(
                    DirectoryDeclBuilder::new("data")
                        .path("/data")
                        .rights(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS)
                        .build(),
                )
                .offer(OfferDecl::Directory(OfferDirectoryDecl {
                    source: OfferSource::Self_,
                    source_name: "data".try_into().unwrap(),
                    target_name: "minfs".try_into().unwrap(),
                    target: OfferTarget::Child("b".to_string()),
                    rights: Some(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS),
                    subdir: None,
                    dependency_type: DependencyType::Strong,
                }))
                .add_lazy_child("b")
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .offer(OfferDecl::Storage(OfferStorageDecl {
                    source: OfferSource::Self_,
                    target: OfferTarget::Child("c".to_string()),
                    source_name: "data".into(),
                    target_name: "data".into(),
                }))
                .add_lazy_child("c")
                .storage(StorageDecl {
                    name: "data".into(),
                    backing_dir: "minfs".try_into().unwrap(),
                    source: StorageDirectorySource::Parent,
                    subdir: Some("bar".try_into().unwrap()),
                    storage_id: fsys::StorageId::StaticInstanceIdOrMoniker,
                })
                .build(),
        ),
        (
            "c",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Storage(UseStorageDecl {
                    source_name: "data".into(),
                    target_path: "/storage".try_into().unwrap(),
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;
    test.check_use(
        vec!["b:0", "c:0"].into(),
        CheckUse::Storage {
            path: "/storage".try_into().unwrap(),
            storage_relation: Some(RelativeMoniker::new(vec![], vec!["c:0".into()])),
            from_cm_namespace: false,
            storage_subdir: Some("bar".to_string()),
            expected_res: ExpectedResult::Ok,
        },
    )
    .await;
    assert_eq!(test.list_directory(".").await, vec!["bar".to_string(), "foo".to_string()],);
}

///   a
///    \
///     b
///      \
///       c
///
/// a: has storage decl with name "mystorage" with a source of self at path /data
/// a: offers data storage to b from "mystorage"
/// b: offers data storage to c from realm
/// c: uses data storage as /storage
#[fuchsia::test]
async fn storage_and_dir_from_grandparent() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .directory(
                    DirectoryDeclBuilder::new("data-root")
                        .path("/data")
                        .rights(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS)
                        .build(),
                )
                .offer(OfferDecl::Storage(OfferStorageDecl {
                    source: OfferSource::Self_,
                    target: OfferTarget::Child("b".to_string()),
                    source_name: "data".into(),
                    target_name: "data".into(),
                }))
                .add_lazy_child("b")
                .storage(StorageDecl {
                    name: "data".into(),
                    backing_dir: "data-root".try_into().unwrap(),
                    source: StorageDirectorySource::Self_,
                    subdir: None,
                    storage_id: fsys::StorageId::StaticInstanceIdOrMoniker,
                })
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .offer(OfferDecl::Storage(OfferStorageDecl {
                    source: OfferSource::Parent,
                    target: OfferTarget::Child("c".to_string()),
                    source_name: "data".into(),
                    target_name: "data".into(),
                }))
                .add_lazy_child("c")
                .build(),
        ),
        (
            "c",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Storage(UseStorageDecl {
                    source_name: "data".into(),
                    target_path: "/storage".try_into().unwrap(),
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;
    test.check_use(
        vec!["b:0", "c:0"].into(),
        CheckUse::Storage {
            path: "/storage".try_into().unwrap(),
            storage_relation: Some(RelativeMoniker::new(vec![], vec!["b:0".into(), "c:0".into()])),
            from_cm_namespace: false,
            storage_subdir: None,
            expected_res: ExpectedResult::Ok,
        },
    )
    .await;
}

///   a
///  / \
/// b   c
///
/// b: exposes directory /data as /minfs
/// a: has storage decl with name "mystorage" with a source of child b at path /minfs
/// a: offers cache storage to c from "mystorage"
/// c: uses cache storage as /storage
#[fuchsia::test]
async fn storage_from_parent_dir_from_sibling() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .storage(StorageDecl {
                    name: "cache".into(),
                    backing_dir: "minfs".try_into().unwrap(),
                    source: StorageDirectorySource::Child("b".to_string()),
                    subdir: None,
                    storage_id: fsys::StorageId::StaticInstanceIdOrMoniker,
                })
                .offer(OfferDecl::Storage(OfferStorageDecl {
                    source: OfferSource::Self_,
                    target: OfferTarget::Child("c".to_string()),
                    source_name: "cache".into(),
                    target_name: "cache".into(),
                }))
                .add_lazy_child("b")
                .add_lazy_child("c")
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .directory(
                    DirectoryDeclBuilder::new("data")
                        .path("/data")
                        .rights(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS)
                        .build(),
                )
                .expose(ExposeDecl::Directory(ExposeDirectoryDecl {
                    source_name: "data".try_into().unwrap(),
                    source: ExposeSource::Self_,
                    target_name: "minfs".try_into().unwrap(),
                    target: ExposeTarget::Parent,
                    rights: Some(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS),
                    subdir: None,
                }))
                .build(),
        ),
        (
            "c",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Storage(UseStorageDecl {
                    source_name: "cache".into(),
                    target_path: "/storage".try_into().unwrap(),
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;
    test.check_use(
        vec!["c:0"].into(),
        CheckUse::Storage {
            path: "/storage".try_into().unwrap(),
            storage_relation: Some(RelativeMoniker::new(vec![], vec!["c:0".into()])),
            from_cm_namespace: false,
            storage_subdir: None,
            expected_res: ExpectedResult::Ok,
        },
    )
    .await;
}

///   a
///  / \
/// b   c
///
/// b: exposes directory /data as /minfs with subdir "subdir_1"
/// a: has storage decl with name "mystorage" with a source of child b at path /minfs and subdir
///    "subdir_2"
/// a: offers cache storage to c from "mystorage"
/// c: uses cache storage as /storage
#[fuchsia::test]
async fn storage_from_parent_dir_from_sibling_with_subdir() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .storage(StorageDecl {
                    name: "cache".into(),
                    backing_dir: "minfs".try_into().unwrap(),
                    source: StorageDirectorySource::Child("b".to_string()),
                    subdir: Some("subdir_2".into()),
                    storage_id: fsys::StorageId::StaticInstanceIdOrMoniker,
                })
                .offer(OfferDecl::Storage(OfferStorageDecl {
                    source: OfferSource::Self_,
                    target: OfferTarget::Child("c".to_string()),
                    source_name: "cache".into(),
                    target_name: "cache".into(),
                }))
                .add_lazy_child("b")
                .add_lazy_child("c")
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .directory(
                    DirectoryDeclBuilder::new("data")
                        .path("/data")
                        .rights(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS)
                        .build(),
                )
                .expose(ExposeDecl::Directory(ExposeDirectoryDecl {
                    source_name: "data".try_into().unwrap(),
                    source: ExposeSource::Self_,
                    target_name: "minfs".try_into().unwrap(),
                    target: ExposeTarget::Parent,
                    rights: Some(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS),
                    subdir: Some("subdir_1".into()),
                }))
                .build(),
        ),
        (
            "c",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Storage(UseStorageDecl {
                    source_name: "cache".into(),
                    target_path: "/storage".try_into().unwrap(),
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;
    test.add_subdir_to_data_directory("subdir_1");
    test.check_use(
        vec!["c:0"].into(),
        CheckUse::Storage {
            path: "/storage".try_into().unwrap(),
            storage_relation: Some(RelativeMoniker::new(vec![], vec!["c:0".into()])),
            from_cm_namespace: false,
            storage_subdir: Some("subdir_1/subdir_2".to_string()),
            expected_res: ExpectedResult::Ok,
        },
    )
    .await;
    assert_eq!(test.list_directory(".").await, vec!["foo".to_string(), "subdir_1".to_string()]);
    assert_eq!(test.list_directory("subdir_1").await, vec!["subdir_2".to_string()]);
    assert_eq!(test.list_directory("subdir_1/subdir_2").await, vec!["c:0".to_string()]);
}

///   a
///    \
///     b
///      \
///      [c]
///
/// a: offers directory to b at path /minfs
/// b: has storage decl with name "mystorage" with a source of realm at path /data
/// b: offers storage to collection from "mystorage"
/// [c]: uses storage as /storage
/// [c]: destroyed and storage goes away
#[fuchsia::test]
async fn use_in_collection_from_parent() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .directory(
                    DirectoryDeclBuilder::new("data")
                        .path("/data")
                        .rights(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS)
                        .build(),
                )
                .offer(OfferDecl::Directory(OfferDirectoryDecl {
                    source: OfferSource::Self_,
                    source_name: "data".try_into().unwrap(),
                    target_name: "minfs".try_into().unwrap(),
                    target: OfferTarget::Child("b".to_string()),
                    rights: Some(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS),
                    subdir: None,
                    dependency_type: DependencyType::Strong,
                }))
                .add_lazy_child("b")
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Protocol(UseProtocolDecl {
                    source: UseSource::Framework,
                    source_name: "fuchsia.sys2.Realm".try_into().unwrap(),
                    target_path: "/svc/fuchsia.sys2.Realm".try_into().unwrap(),
                }))
                .offer(OfferDecl::Storage(OfferStorageDecl {
                    source: OfferSource::Self_,
                    target: OfferTarget::Collection("coll".to_string()),
                    source_name: "data".into(),
                    target_name: "data".into(),
                }))
                .offer(OfferDecl::Storage(OfferStorageDecl {
                    source: OfferSource::Self_,
                    target: OfferTarget::Collection("coll".to_string()),
                    source_name: "cache".into(),
                    target_name: "cache".into(),
                }))
                .storage(StorageDecl {
                    name: "data".into(),
                    backing_dir: "minfs".try_into().unwrap(),
                    source: StorageDirectorySource::Parent,
                    subdir: Some(PathBuf::from("data")),
                    storage_id: fsys::StorageId::StaticInstanceIdOrMoniker,
                })
                .storage(StorageDecl {
                    name: "cache".into(),
                    backing_dir: "minfs".try_into().unwrap(),
                    source: StorageDirectorySource::Parent,
                    subdir: Some(PathBuf::from("cache")),
                    storage_id: fsys::StorageId::StaticInstanceIdOrMoniker,
                })
                .add_transient_collection("coll")
                .build(),
        ),
        (
            "c",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Storage(UseStorageDecl {
                    source_name: "data".into(),
                    target_path: "/data".try_into().unwrap(),
                }))
                .use_(UseDecl::Storage(UseStorageDecl {
                    source_name: "cache".into(),
                    target_path: "/cache".try_into().unwrap(),
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;
    test.create_dynamic_child(
        vec!["b:0"].into(),
        "coll",
        ChildDecl {
            name: "c".into(),
            url: "test:///c".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        },
    )
    .await;

    // Use storage and confirm its existence.
    test.check_use(
        vec!["b:0", "coll:c:1"].into(),
        CheckUse::Storage {
            path: "/data".try_into().unwrap(),
            storage_relation: Some(RelativeMoniker::new(vec![], vec!["coll:c:1".into()])),
            from_cm_namespace: false,
            storage_subdir: Some("data".to_string()),
            expected_res: ExpectedResult::Ok,
        },
    )
    .await;
    test.check_use(
        vec!["b:0", "coll:c:1"].into(),
        CheckUse::Storage {
            path: "/cache".try_into().unwrap(),
            storage_relation: Some(RelativeMoniker::new(vec![], vec!["coll:c:1".into()])),
            from_cm_namespace: false,
            storage_subdir: Some("cache".to_string()),
            expected_res: ExpectedResult::Ok,
        },
    )
    .await;
    // Confirm storage directory exists for component in collection
    assert_eq!(
        test.list_directory_in_storage(
            Some("data"),
            RelativeMoniker::new(vec![], vec![]),
            None,
            ""
        )
        .await,
        vec!["coll:c:1".to_string()],
    );
    assert_eq!(
        test.list_directory_in_storage(
            Some("cache"),
            RelativeMoniker::new(vec![], vec![]),
            None,
            ""
        )
        .await,
        vec!["coll:c:1".to_string()],
    );
    test.destroy_dynamic_child(vec!["b:0"].into(), "coll", "c").await;

    // Confirm storage no longer exists.
    assert_eq!(
        test.list_directory_in_storage(
            Some("data"),
            RelativeMoniker::new(vec![], vec![]),
            None,
            ""
        )
        .await,
        Vec::<String>::new(),
    );
    assert_eq!(
        test.list_directory_in_storage(
            Some("cache"),
            RelativeMoniker::new(vec![], vec![]),
            None,
            ""
        )
        .await,
        Vec::<String>::new(),
    );
}

///   a
///    \
///     b
///      \
///      [c]
///
/// a: has storage decl with name "mystorage" with a source of self at path /data
/// a: offers storage to b from "mystorage"
/// b: offers storage to collection from "mystorage"
/// [c]: uses storage as /storage
/// [c]: destroyed and storage goes away
#[fuchsia::test]
async fn use_in_collection_from_grandparent() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .directory(
                    DirectoryDeclBuilder::new("minfs")
                        .path("/data")
                        .rights(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS)
                        .build(),
                )
                .offer(OfferDecl::Storage(OfferStorageDecl {
                    source: OfferSource::Self_,
                    target: OfferTarget::Child("b".to_string()),
                    source_name: "data".into(),
                    target_name: "data".into(),
                }))
                .offer(OfferDecl::Storage(OfferStorageDecl {
                    source: OfferSource::Self_,
                    target: OfferTarget::Child("b".to_string()),
                    source_name: "cache".into(),
                    target_name: "cache".into(),
                }))
                .add_lazy_child("b")
                .storage(StorageDecl {
                    name: "data".into(),
                    backing_dir: "minfs".try_into().unwrap(),
                    source: StorageDirectorySource::Self_,
                    subdir: Some(PathBuf::from("data")),
                    storage_id: fsys::StorageId::StaticInstanceIdOrMoniker,
                })
                .storage(StorageDecl {
                    name: "cache".into(),
                    backing_dir: "minfs".try_into().unwrap(),
                    source: StorageDirectorySource::Self_,
                    subdir: Some(PathBuf::from("cache")),
                    storage_id: fsys::StorageId::StaticInstanceIdOrMoniker,
                })
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Protocol(UseProtocolDecl {
                    source: UseSource::Framework,
                    source_name: "fuchsia.sys2.Realm".try_into().unwrap(),
                    target_path: "/svc/fuchsia.sys2.Realm".try_into().unwrap(),
                }))
                .offer(OfferDecl::Storage(OfferStorageDecl {
                    source: OfferSource::Parent,
                    target: OfferTarget::Collection("coll".to_string()),
                    source_name: "data".into(),
                    target_name: "data".into(),
                }))
                .offer(OfferDecl::Storage(OfferStorageDecl {
                    source: OfferSource::Parent,
                    target: OfferTarget::Collection("coll".to_string()),
                    source_name: "cache".into(),
                    target_name: "cache".into(),
                }))
                .add_transient_collection("coll")
                .build(),
        ),
        (
            "c",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Storage(UseStorageDecl {
                    source_name: "data".into(),
                    target_path: "/data".try_into().unwrap(),
                }))
                .use_(UseDecl::Storage(UseStorageDecl {
                    source_name: "cache".into(),
                    target_path: "/cache".try_into().unwrap(),
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;
    test.create_dynamic_child(
        vec!["b:0"].into(),
        "coll",
        ChildDecl {
            name: "c".into(),
            url: "test:///c".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        },
    )
    .await;

    // Use storage and confirm its existence.
    test.check_use(
        vec!["b:0", "coll:c:1"].into(),
        CheckUse::Storage {
            path: "/data".try_into().unwrap(),
            storage_relation: Some(RelativeMoniker::new(
                vec![],
                vec!["b:0".into(), "coll:c:1".into()],
            )),
            from_cm_namespace: false,
            storage_subdir: Some("data".to_string()),
            expected_res: ExpectedResult::Ok,
        },
    )
    .await;
    test.check_use(
        vec!["b:0", "coll:c:1"].into(),
        CheckUse::Storage {
            path: "/cache".try_into().unwrap(),
            storage_relation: Some(RelativeMoniker::new(
                vec![],
                vec!["b:0".into(), "coll:c:1".into()],
            )),
            from_cm_namespace: false,
            storage_subdir: Some("cache".to_string()),
            expected_res: ExpectedResult::Ok,
        },
    )
    .await;
    assert_eq!(
        test.list_directory_in_storage(
            Some("data"),
            RelativeMoniker::new(vec![], vec!["b:0".into()]),
            None,
            "children",
        )
        .await,
        vec!["coll:c:1".to_string()]
    );
    assert_eq!(
        test.list_directory_in_storage(
            Some("cache"),
            RelativeMoniker::new(vec![], vec!["b:0".into()]),
            None,
            "children",
        )
        .await,
        vec!["coll:c:1".to_string()]
    );
    test.destroy_dynamic_child(vec!["b:0"].into(), "coll", "c").await;

    // Confirm storage no longer exists.
    assert_eq!(
        test.list_directory_in_storage(
            Some("data"),
            RelativeMoniker::new(vec![], vec!["b:0".into()]),
            None,
            "children"
        )
        .await,
        Vec::<String>::new(),
    );
    assert_eq!(
        test.list_directory_in_storage(
            Some("cache"),
            RelativeMoniker::new(vec![], vec!["b:0".into()]),
            None,
            "children"
        )
        .await,
        Vec::<String>::new(),
    );
}

///   a
///  / \
/// b   c
///      \
///       d
///
/// b: exposes directory /data as /minfs
/// a: has storage decl with name "mystorage" with a source of child b at path /minfs
/// a: offers data, cache, and meta storage to c from "mystorage"
/// c: uses cache and meta storage as /storage
/// c: offers data and meta storage to d
/// d: uses data and meta storage
#[fuchsia::test]
async fn storage_multiple_types() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .storage(StorageDecl {
                    name: "data".into(),
                    backing_dir: "minfs".try_into().unwrap(),
                    source: StorageDirectorySource::Child("b".to_string()),
                    subdir: Some(PathBuf::from("data")),
                    storage_id: fsys::StorageId::StaticInstanceIdOrMoniker,
                })
                .storage(StorageDecl {
                    name: "cache".into(),
                    backing_dir: "minfs".try_into().unwrap(),
                    source: StorageDirectorySource::Child("b".to_string()),
                    subdir: Some(PathBuf::from("cache")),
                    storage_id: fsys::StorageId::StaticInstanceIdOrMoniker,
                })
                .offer(OfferDecl::Storage(OfferStorageDecl {
                    source: OfferSource::Self_,
                    target: OfferTarget::Child("c".to_string()),
                    source_name: "cache".into(),
                    target_name: "cache".into(),
                }))
                .offer(OfferDecl::Storage(OfferStorageDecl {
                    source: OfferSource::Self_,
                    target: OfferTarget::Child("c".to_string()),
                    source_name: "data".into(),
                    target_name: "data".into(),
                }))
                .add_lazy_child("b")
                .add_lazy_child("c")
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .directory(
                    DirectoryDeclBuilder::new("data")
                        .path("/data")
                        .rights(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS)
                        .build(),
                )
                .expose(ExposeDecl::Directory(ExposeDirectoryDecl {
                    source_name: "data".try_into().unwrap(),
                    source: ExposeSource::Self_,
                    target_name: "minfs".try_into().unwrap(),
                    target: ExposeTarget::Parent,
                    rights: Some(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS),
                    subdir: None,
                }))
                .build(),
        ),
        (
            "c",
            ComponentDeclBuilder::new()
                .offer(OfferDecl::Storage(OfferStorageDecl {
                    source: OfferSource::Parent,
                    target: OfferTarget::Child("d".to_string()),
                    source_name: "data".into(),
                    target_name: "data".into(),
                }))
                .offer(OfferDecl::Storage(OfferStorageDecl {
                    source: OfferSource::Parent,
                    target: OfferTarget::Child("d".to_string()),
                    source_name: "cache".into(),
                    target_name: "cache".into(),
                }))
                .use_(UseDecl::Storage(UseStorageDecl {
                    source_name: "data".into(),
                    target_path: "/storage".try_into().unwrap(),
                }))
                .use_(UseDecl::Storage(UseStorageDecl {
                    source_name: "cache".into(),
                    target_path: "/cache".try_into().unwrap(),
                }))
                .add_lazy_child("d")
                .build(),
        ),
        (
            "d",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Storage(UseStorageDecl {
                    source_name: "data".into(),
                    target_path: "/storage".try_into().unwrap(),
                }))
                .use_(UseDecl::Storage(UseStorageDecl {
                    source_name: "cache".into(),
                    target_path: "/cache".try_into().unwrap(),
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;
    test.check_use(
        vec!["c:0"].into(),
        CheckUse::Storage {
            path: "/storage".try_into().unwrap(),
            storage_relation: Some(RelativeMoniker::new(vec![], vec!["c:0".into()])),
            from_cm_namespace: false,
            storage_subdir: Some("data".to_string()),
            expected_res: ExpectedResult::Ok,
        },
    )
    .await;
    test.check_use(
        vec!["c:0"].into(),
        CheckUse::Storage {
            path: "/cache".try_into().unwrap(),
            storage_relation: Some(RelativeMoniker::new(vec![], vec!["c:0".into()])),
            from_cm_namespace: false,
            storage_subdir: Some("cache".to_string()),
            expected_res: ExpectedResult::Ok,
        },
    )
    .await;
    test.check_use(
        vec!["c:0", "d:0"].into(),
        CheckUse::Storage {
            path: "/storage".try_into().unwrap(),
            storage_relation: Some(RelativeMoniker::new(vec![], vec!["c:0".into(), "d:0".into()])),
            from_cm_namespace: false,
            storage_subdir: Some("data".to_string()),
            expected_res: ExpectedResult::Ok,
        },
    )
    .await;
    test.check_use(
        vec!["c:0", "d:0"].into(),
        CheckUse::Storage {
            path: "/cache".try_into().unwrap(),
            storage_relation: Some(RelativeMoniker::new(vec![], vec!["c:0".into(), "d:0".into()])),
            from_cm_namespace: false,
            storage_subdir: Some("cache".to_string()),
            expected_res: ExpectedResult::Ok,
        },
    )
    .await;
}

///   a
///    \
///     b
///
/// a: has storage decl with name "mystorage" with a source of self at path /storage
/// a: offers cache storage to b from "mystorage"
/// b: uses data storage as /storage, fails to since data != cache
/// b: uses meta storage, fails to since meta != cache
#[fuchsia::test]
async fn use_the_wrong_type_of_storage() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .directory(
                    DirectoryDeclBuilder::new("data")
                        .path("/data")
                        .rights(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS)
                        .build(),
                )
                .offer(OfferDecl::Storage(OfferStorageDecl {
                    source: OfferSource::Self_,
                    target: OfferTarget::Child("b".to_string()),
                    source_name: "cache".into(),
                    target_name: "cache".into(),
                }))
                .add_lazy_child("b")
                .storage(StorageDecl {
                    name: "cache".into(),
                    backing_dir: "data".try_into().unwrap(),
                    source: StorageDirectorySource::Self_,
                    subdir: None,
                    storage_id: fsys::StorageId::StaticInstanceIdOrMoniker,
                })
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Storage(UseStorageDecl {
                    source_name: "data".into(),
                    target_path: "/storage".try_into().unwrap(),
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;
    test.check_use(
        vec!["b:0"].into(),
        CheckUse::Storage {
            path: "/storage".try_into().unwrap(),
            storage_relation: None,
            from_cm_namespace: false,
            storage_subdir: None,
            expected_res: ExpectedResult::Err(zx::Status::UNAVAILABLE),
        },
    )
    .await;
}

///   a
///    \
///     b
///
/// a: offers directory from self at path "/data"
/// b: uses data storage as /storage, fails to since data storage != "/data" directories
#[fuchsia::test]
async fn directories_are_not_storage() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .directory(
                    DirectoryDeclBuilder::new("data")
                        .path("/data")
                        .rights(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS)
                        .build(),
                )
                .offer(OfferDecl::Directory(OfferDirectoryDecl {
                    source: OfferSource::Self_,
                    source_name: "data".try_into().unwrap(),
                    target_name: "data".try_into().unwrap(),
                    target: OfferTarget::Child("b".to_string()),
                    rights: Some(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS),
                    subdir: None,
                    dependency_type: DependencyType::Strong,
                }))
                .add_lazy_child("b")
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Storage(UseStorageDecl {
                    source_name: "data".into(),
                    target_path: "/storage".try_into().unwrap(),
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;
    test.check_use(
        vec!["b:0"].into(),
        CheckUse::Storage {
            path: "/storage".try_into().unwrap(),
            storage_relation: None,
            from_cm_namespace: false,
            storage_subdir: None,
            expected_res: ExpectedResult::Err(zx::Status::UNAVAILABLE),
        },
    )
    .await;
}

///   a
///    \
///     b
///
/// a: has storage decl with name "mystorage" with a source of self at path /data
/// a: does not offer any storage to b
/// b: uses meta storage and data storage as /storage, fails to since it was not offered either
#[fuchsia::test]
async fn use_storage_when_not_offered() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .add_lazy_child("b")
                .directory(
                    DirectoryDeclBuilder::new("minfs")
                        .path("/data")
                        .rights(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS)
                        .build(),
                )
                .storage(StorageDecl {
                    name: "data".into(),
                    backing_dir: "minfs".try_into().unwrap(),
                    source: StorageDirectorySource::Self_,
                    subdir: None,
                    storage_id: fsys::StorageId::StaticInstanceIdOrMoniker,
                })
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Storage(UseStorageDecl {
                    source_name: "data".into(),
                    target_path: "/storage".try_into().unwrap(),
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;
    test.check_use(
        vec!["b:0"].into(),
        CheckUse::Storage {
            path: "/storage".try_into().unwrap(),
            storage_relation: None,
            from_cm_namespace: false,
            storage_subdir: None,
            expected_res: ExpectedResult::Err(zx::Status::UNAVAILABLE),
        },
    )
    .await;
}

///   a
///    \
///     b
///      \
///       c
///
/// a: offers directory /data to b as /minfs, but a is non-executable
/// b: has storage decl with name "mystorage" with a source of realm at path /minfs
/// b: offers data and meta storage to b from "mystorage"
/// c: uses meta and data storage as /storage, fails to since a is non-executable
#[fuchsia::test]
async fn dir_offered_from_nonexecutable() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new_empty_component()
                .directory(
                    DirectoryDeclBuilder::new("data")
                        .path("/data")
                        .rights(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS)
                        .build(),
                )
                .offer(OfferDecl::Directory(OfferDirectoryDecl {
                    source: OfferSource::Self_,
                    source_name: "data".try_into().unwrap(),
                    target_name: "minfs".try_into().unwrap(),
                    target: OfferTarget::Child("b".to_string()),
                    rights: Some(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS),
                    subdir: None,
                    dependency_type: DependencyType::Strong,
                }))
                .add_lazy_child("b")
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .offer(OfferDecl::Storage(OfferStorageDecl {
                    source: OfferSource::Self_,
                    target: OfferTarget::Child("c".to_string()),
                    source_name: "data".into(),
                    target_name: "data".into(),
                }))
                .add_lazy_child("c")
                .storage(StorageDecl {
                    name: "data".into(),
                    backing_dir: "minfs".try_into().unwrap(),
                    source: StorageDirectorySource::Parent,
                    subdir: None,
                    storage_id: fsys::StorageId::StaticInstanceIdOrMoniker,
                })
                .build(),
        ),
        (
            "c",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Storage(UseStorageDecl {
                    source_name: "data".into(),
                    target_path: "/storage".try_into().unwrap(),
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;
    test.check_use(
        vec!["b:0", "c:0"].into(),
        CheckUse::Storage {
            path: "/storage".try_into().unwrap(),
            storage_relation: None,
            from_cm_namespace: false,
            storage_subdir: None,
            expected_res: ExpectedResult::Err(zx::Status::UNAVAILABLE),
        },
    )
    .await;
}

///   component manager's namespace
///    |
///    a
///    |
///    b
///
/// a: has storage decl with name "mystorage" with a source of parent at path /data
/// a: offers cache storage to b from "mystorage"
/// b: uses cache storage as /storage.
/// Policy prevents b from using storage.
#[fuchsia::test]
async fn storage_dir_from_cm_namespace_prevented_by_policy() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .offer(OfferDecl::Storage(OfferStorageDecl {
                    source_name: "cache".into(),
                    target_name: "cache".into(),
                    source: OfferSource::Self_,
                    target: OfferTarget::Child("b".to_string()),
                }))
                .add_lazy_child("b")
                .storage(StorageDecl {
                    name: "cache".into(),
                    backing_dir: "tmp".try_into().unwrap(),
                    source: StorageDirectorySource::Parent,
                    subdir: Some(PathBuf::from("cache")),
                    storage_id: fsys::StorageId::StaticInstanceIdOrMoniker,
                })
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Storage(UseStorageDecl {
                    source_name: "cache".into(),
                    target_path: "/storage".try_into().unwrap(),
                }))
                .build(),
        ),
    ];
    let namespace_capabilities = vec![CapabilityDecl::Directory(
        DirectoryDeclBuilder::new("tmp")
            .path("/tmp")
            .rights(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS)
            .build(),
    )];
    let test = RoutingTestBuilder::new("a", components)
        .set_namespace_capabilities(namespace_capabilities)
        .add_capability_policy(
            CapabilityAllowlistKey {
                source_moniker: ExtendedMoniker::ComponentInstance(AbsoluteMoniker::root()),
                source_name: CapabilityName::from("cache"),
                source: CapabilityAllowlistSource::Self_,
                capability: CapabilityTypeName::Storage,
            },
            HashSet::new(),
        )
        .build()
        .await;

    test.check_use(
        vec!["b:0"].into(),
        CheckUse::Storage {
            path: "/storage".try_into().unwrap(),
            storage_relation: Some(RelativeMoniker::new(vec![], vec!["b:0".into()])),
            from_cm_namespace: true,
            storage_subdir: Some("cache".to_string()),
            expected_res: ExpectedResult::Err(zx::Status::ACCESS_DENIED),
        },
    )
    .await;
}

///   component manager's namespace
///    |
///    a
///    |
///    b
///    |
///    c
///
/// Instance IDs defined only for `b` in the component ID index.
/// Check that the correct storge layout is used when a component has an instance ID.
#[fuchsia::test]
async fn instance_id_from_index() {
    let b_instance_id = Some(gen_instance_id(&mut rand::thread_rng()));
    let component_id_index_path = make_index_file(component_id_index::Index {
        instances: vec![component_id_index::InstanceIdEntry {
            instance_id: b_instance_id.clone(),
            appmgr_moniker: None,
            moniker: Some(AbsoluteMoniker::parse_string_without_instances("/b").unwrap()),
        }],
        ..component_id_index::Index::default()
    })
    .unwrap();
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .directory(
                    DirectoryDeclBuilder::new("data")
                        .path("/data")
                        .rights(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS)
                        .build(),
                )
                .offer(OfferDecl::Storage(OfferStorageDecl {
                    source: OfferSource::Self_,
                    target: OfferTarget::Child("b".to_string()),
                    source_name: "cache".into(),
                    target_name: "cache".into(),
                }))
                .add_lazy_child("b")
                .storage(StorageDecl {
                    name: "cache".into(),
                    backing_dir: "data".try_into().unwrap(),
                    source: StorageDirectorySource::Self_,
                    subdir: None,
                    storage_id: fsys::StorageId::StaticInstanceIdOrMoniker,
                })
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Storage(UseStorageDecl {
                    source_name: "cache".into(),
                    target_path: "/storage".try_into().unwrap(),
                }))
                .offer(OfferDecl::Storage(OfferStorageDecl {
                    source: OfferSource::Parent,
                    target: OfferTarget::Child("c".to_string()),
                    source_name: "cache".into(),
                    target_name: "cache".into(),
                }))
                .add_lazy_child("c")
                .build(),
        ),
        (
            "c",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Storage(UseStorageDecl {
                    source_name: "cache".into(),
                    target_path: "/storage".try_into().unwrap(),
                }))
                .build(),
        ),
    ];
    let test = RoutingTestBuilder::new("a", components)
        .set_component_id_index_path(component_id_index_path.path().to_str().unwrap().to_string())
        .build()
        .await;

    // instance `b` uses instance-id based paths.
    test.check_use(
        AbsoluteMoniker::parse_string_without_instances("/b").unwrap(),
        CheckUse::Storage {
            path: "/storage".try_into().unwrap(),
            storage_relation: Some(RelativeMoniker::new(vec![], vec!["b:0".into()])),
            from_cm_namespace: false,
            storage_subdir: None,
            expected_res: ExpectedResult::Ok,
        },
    )
    .await;
    assert!(test.list_directory(".").await.contains(b_instance_id.as_ref().unwrap()));

    // instance `c` uses moniker-based paths.
    let storage_relation = RelativeMoniker::new(vec![], vec!["b:0".into(), "c:0".into()]);
    test.check_use(
        AbsoluteMoniker::parse_string_without_instances("/b/c").unwrap(),
        CheckUse::Storage {
            path: "/storage".try_into().unwrap(),
            storage_relation: Some(storage_relation.clone()),
            from_cm_namespace: false,
            storage_subdir: None,
            expected_res: ExpectedResult::Ok,
        },
    )
    .await;

    let expected_storage_path =
        capability_util::generate_storage_path(None, &storage_relation, None)
            .to_str()
            .unwrap()
            .to_string();
    assert!(list_directory_recursive(&test.test_dir_proxy)
        .await
        .iter()
        .find(|&name| name.starts_with(&expected_storage_path))
        .is_some());
}

///   component manager's namespace
///    |
///   provider (provides storage capability, restricted to component ID index)
///    |
///   parent_consumer (in component ID index)
///    |
///   child_consumer (not in component ID index)
///
/// Test that a component cannot start if it uses restricted storage but isn't in the component ID
/// index.
#[fuchsia::test]
async fn use_restricted_storage_start_failure() {
    let parent_consumer_instance_id = Some(gen_instance_id(&mut rand::thread_rng()));
    let component_id_index_path = make_index_file(component_id_index::Index {
        instances: vec![component_id_index::InstanceIdEntry {
            instance_id: parent_consumer_instance_id.clone(),
            appmgr_moniker: None,
            moniker: Some(
                AbsoluteMoniker::parse_string_without_instances("/parent_consumer").unwrap(),
            ),
        }],
        ..component_id_index::Index::default()
    })
    .unwrap();
    let components = vec![
        (
            "provider",
            ComponentDeclBuilder::new()
                .directory(
                    DirectoryDeclBuilder::new("data")
                        .path("/data")
                        .rights(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS)
                        .build(),
                )
                .storage(StorageDecl {
                    name: "cache".into(),
                    backing_dir: "data".try_into().unwrap(),
                    source: StorageDirectorySource::Self_,
                    subdir: None,
                    storage_id: fsys::StorageId::StaticInstanceId,
                })
                .offer(OfferDecl::Storage(OfferStorageDecl {
                    source: OfferSource::Self_,
                    target: OfferTarget::Child("parent_consumer".to_string()),
                    source_name: "cache".into(),
                    target_name: "cache".into(),
                }))
                .add_lazy_child("parent_consumer")
                .build(),
        ),
        (
            "parent_consumer",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Storage(UseStorageDecl {
                    source_name: "cache".into(),
                    target_path: "/storage".try_into().unwrap(),
                }))
                .offer(OfferDecl::Storage(OfferStorageDecl {
                    source: OfferSource::Parent,
                    target: OfferTarget::Child("child_consumer".to_string()),
                    source_name: "cache".into(),
                    target_name: "cache".into(),
                }))
                .add_lazy_child("child_consumer")
                .build(),
        ),
        (
            "child_consumer",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Storage(UseStorageDecl {
                    source_name: "cache".into(),
                    target_path: "/storage".try_into().unwrap(),
                }))
                .build(),
        ),
    ];
    let test = RoutingTestBuilder::new("provider", components)
        .set_component_id_index_path(component_id_index_path.path().to_str().unwrap().to_string())
        .build()
        .await;

    test.bind_instance(
        &AbsoluteMoniker::parse_string_without_instances("/parent_consumer").unwrap(),
    )
    .await
    .expect("bind to /parent_consumer failed");

    let child_consumer_moniker =
        AbsoluteMoniker::parse_string_without_instances("/parent_consumer/child_consumer").unwrap();
    let child_bind_result = test.bind_instance(&child_consumer_moniker).await;
    assert!(matches!(
        child_bind_result,
        Err(ModelError::RoutingError { err: RoutingError::ComponentNotInIdIndex { moniker: _ } })
    ));
}

///   component manager's namespace
///    |
///   provider (provides storage capability, restricted to component ID index)
///    |
///   parent_consumer (in component ID index)
///    |
///   child_consumer (not in component ID index)
///
/// Test that a component cannot open a restricted storage capability if the component isn't in
/// the component iindex.
#[fuchsia::test]
async fn use_restricted_storage_open_failure() {
    let parent_consumer_instance_id = Some(gen_instance_id(&mut rand::thread_rng()));
    let component_id_index_path = make_index_file(component_id_index::Index {
        instances: vec![component_id_index::InstanceIdEntry {
            instance_id: parent_consumer_instance_id.clone(),
            appmgr_moniker: None,
            moniker: Some(
                AbsoluteMoniker::parse_string_without_instances("/parent_consumer/child_consumer")
                    .unwrap(),
            ),
        }],
        ..component_id_index::Index::default()
    })
    .unwrap();
    let components = vec![
        (
            "provider",
            ComponentDeclBuilder::new()
                .directory(
                    DirectoryDeclBuilder::new("data")
                        .path("/data")
                        .rights(*rights::READ_RIGHTS | *rights::WRITE_RIGHTS)
                        .build(),
                )
                .storage(StorageDecl {
                    name: "cache".into(),
                    backing_dir: "data".try_into().unwrap(),
                    source: StorageDirectorySource::Self_,
                    subdir: None,
                    storage_id: fsys::StorageId::StaticInstanceIdOrMoniker,
                })
                .offer(OfferDecl::Storage(OfferStorageDecl {
                    source: OfferSource::Self_,
                    target: OfferTarget::Child("parent_consumer".to_string()),
                    source_name: "cache".into(),
                    target_name: "cache".into(),
                }))
                .add_lazy_child("parent_consumer")
                .build(),
        ),
        (
            "parent_consumer",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Storage(UseStorageDecl {
                    source_name: "cache".into(),
                    target_path: "/storage".try_into().unwrap(),
                }))
                .build(),
        ),
    ];
    let test = RoutingTestBuilder::new("provider", components)
        .set_component_id_index_path(component_id_index_path.path().to_str().unwrap().to_string())
        .build()
        .await;

    let parent_consumer_moniker =
        AbsoluteMoniker::parse_string_without_instances("/parent_consumer").unwrap();
    let parent_consumer_instance = test
        .bind_and_get_instance(&parent_consumer_moniker, BindReason::Eager, false)
        .await
        .expect("could not resolve state");

    // `parent_consumer` should be able to open its storage because its not restricted
    let (_client_end, mut server_end) =
        zx::Channel::create().expect("could not create storage dir endpoints");
    route_and_open_capability(
        RouteRequest::UseStorage(UseStorageDecl {
            source_name: "cache".into(),
            target_path: "/storage".try_into().unwrap(),
        }),
        &parent_consumer_instance,
        OpenOptions::Storage(OpenStorageOptions {
            open_mode: fio::MODE_TYPE_DIRECTORY,
            bind_reason: BindReason::Eager,
            server_chan: &mut server_end,
        }),
    )
    .await
    .expect("Unable to route.  oh no!!");

    // now modify StorageDecl so that it restricts storage
    let provider_instance = test
        .bind_and_get_instance(
            &AbsoluteMoniker::parse_string_without_instances("/").unwrap(),
            BindReason::Eager,
            false,
        )
        .await
        .expect("could not resolve state");
    {
        let mut resolved_state = provider_instance.lock_resolved_state().await.unwrap();
        for cap in resolved_state.decl_as_mut().capabilities.iter_mut() {
            match cap {
                CapabilityDecl::Storage(storage_decl) => {
                    storage_decl.storage_id = fsys::StorageId::StaticInstanceId;
                }
                _ => {}
            }
        }
    }

    // `parent_consumer` should NOT be able to open its storage because its IS restricted
    let (_client_end, mut server_end) =
        zx::Channel::create().expect("could not create storage dir endpoints");
    let result = route_and_open_capability(
        RouteRequest::UseStorage(UseStorageDecl {
            source_name: "cache".into(),
            target_path: "/storage".try_into().unwrap(),
        }),
        &parent_consumer_instance,
        OpenOptions::Storage(OpenStorageOptions {
            open_mode: fio::MODE_TYPE_DIRECTORY,
            bind_reason: BindReason::Eager,
            server_chan: &mut server_end,
        }),
    )
    .await;
    assert!(matches!(
        result,
        Err(ModelError::RoutingError { err: RoutingError::ComponentNotInIdIndex { moniker: _ } })
    ));
}
