// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/// Tests of capability routing in ComponentManager.
///
/// Most routing tests should be defined as methods on the ::routing_test_helpers::CommonRoutingTest
/// type and should be run both in this file (using a CommonRoutingTest<RoutingTestBuilder>) and in
/// the cm_fidl_analyzer_tests crate (using a specialization of CommonRoutingTest for the static
/// routing analyzer). This ensures that the static analyzer's routing verification is consistent
/// with ComponentManager's intended routing behavior.
///
/// However, tests of behavior that is out-of-scope for the static analyzer (e.g. routing to/from
/// dynamic component instances) should be defined here.
use {
    crate::{
        capability::{
            CapabilityProvider, CapabilitySource, ComponentCapability, InternalCapability,
            OptionalTask,
        },
        channel,
        config::{AllowlistEntry, CapabilityAllowlistKey, CapabilityAllowlistSource},
        framework::REALM_SERVICE,
        model::{
            actions::{
                ActionSet, DeleteChildAction, DestroyAction, MarkDeletedAction, ShutdownAction,
            },
            error::ModelError,
            events::registry::EventSubscription,
            hooks::{Event, EventPayload, EventType, Hook, HooksRegistration},
            rights,
            routing::{RouteRequest, RouteSource, RoutingError},
            testing::{routing_test_helpers::*, test_helpers::*},
        },
    },
    anyhow::Error,
    async_trait::async_trait,
    cm_rust::*,
    cm_rust_testing::*,
    fidl::endpoints::ServerEnd,
    fidl_fidl_examples_echo::{self as echo},
    fidl_fuchsia_component_runner as fcrunner, fidl_fuchsia_mem as fmem, fidl_fuchsia_sys2 as fsys,
    fuchsia_async as fasync, fuchsia_zircon as zx,
    futures::{join, lock::Mutex, StreamExt, TryStreamExt},
    log::*,
    maplit::hashmap,
    matches::assert_matches,
    moniker::{AbsoluteMoniker, ExtendedMoniker},
    routing::{error::ComponentInstanceError, route_capability},
    routing_test_helpers::{
        default_service_capability, instantiate_common_routing_tests, RoutingTestModel,
    },
    std::{
        collections::HashSet,
        convert::{TryFrom, TryInto},
        path::PathBuf,
        sync::{Arc, Weak},
    },
    vfs::pseudo_directory,
};

instantiate_common_routing_tests! { RoutingTestBuilder }

///   a
///    \
///     b
///
/// b: uses framework service /svc/fuchsia.sys2.Realm
#[fuchsia::test]
async fn use_framework_service() {
    pub struct MockRealmCapabilityProvider {
        scope_moniker: AbsoluteMoniker,
        host: MockRealmCapabilityHost,
    }

    impl MockRealmCapabilityProvider {
        pub fn new(scope_moniker: AbsoluteMoniker, host: MockRealmCapabilityHost) -> Self {
            Self { scope_moniker, host }
        }
    }

    #[async_trait]
    impl CapabilityProvider for MockRealmCapabilityProvider {
        async fn open(
            self: Box<Self>,
            _flags: u32,
            _open_mode: u32,
            _relative_path: PathBuf,
            server_end: &mut zx::Channel,
        ) -> Result<OptionalTask, ModelError> {
            let server_end = channel::take_channel(server_end);
            let stream = ServerEnd::<fsys::RealmMarker>::new(server_end)
                .into_stream()
                .expect("could not convert channel into stream");
            let scope_moniker = self.scope_moniker.clone();
            let host = self.host.clone();
            Ok(fasync::Task::spawn(async move {
                if let Err(e) = host.serve(scope_moniker, stream).await {
                    // TODO: Set an epitaph to indicate this was an unexpected error.
                    warn!("serve_realm failed: {}", e);
                }
            })
            .into())
        }
    }

    #[async_trait]
    impl Hook for MockRealmCapabilityHost {
        async fn on(self: Arc<Self>, event: &Event) -> Result<(), ModelError> {
            if let Ok(EventPayload::CapabilityRouted {
                source: CapabilitySource::Framework { capability, component },
                capability_provider,
            }) = &event.result
            {
                let mut capability_provider = capability_provider.lock().await;
                *capability_provider = self
                    .on_scoped_framework_capability_routed_async(
                        component.moniker.clone(),
                        &capability,
                        capability_provider.take(),
                    )
                    .await?;
            }
            Ok(())
        }
    }

    #[derive(Clone)]
    pub struct MockRealmCapabilityHost {
        /// List of calls to `BindChild` with component's relative moniker.
        bind_calls: Arc<Mutex<Vec<String>>>,
    }

    impl MockRealmCapabilityHost {
        pub fn new() -> Self {
            Self { bind_calls: Arc::new(Mutex::new(vec![])) }
        }

        pub fn bind_calls(&self) -> Arc<Mutex<Vec<String>>> {
            self.bind_calls.clone()
        }

        async fn serve(
            &self,
            scope_moniker: AbsoluteMoniker,
            mut stream: fsys::RealmRequestStream,
        ) -> Result<(), Error> {
            while let Some(request) = stream.try_next().await? {
                match request {
                    fsys::RealmRequest::BindChild { responder, .. } => {
                        self.bind_calls.lock().await.push(
                            scope_moniker
                                .path()
                                .last()
                                .expect("did not expect root component")
                                .name()
                                .to_string(),
                        );
                        responder.send(&mut Ok(()))?;
                    }
                    _ => {}
                }
            }
            Ok(())
        }

        pub async fn on_scoped_framework_capability_routed_async<'a>(
            &'a self,
            scope_moniker: AbsoluteMoniker,
            capability: &'a InternalCapability,
            capability_provider: Option<Box<dyn CapabilityProvider>>,
        ) -> Result<Option<Box<dyn CapabilityProvider>>, ModelError> {
            // If some other capability has already been installed, then there's nothing to
            // do here.
            if capability.matches_protocol(&REALM_SERVICE) {
                Ok(Some(Box::new(MockRealmCapabilityProvider::new(
                    scope_moniker.clone(),
                    self.clone(),
                )) as Box<dyn CapabilityProvider>))
            } else {
                Ok(capability_provider)
            }
        }
    }

    let components = vec![
        ("a", ComponentDeclBuilder::new().add_lazy_child("b").build()),
        (
            "b",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Protocol(UseProtocolDecl {
                    source: UseSource::Framework,
                    source_name: "fuchsia.sys2.Realm".into(),
                    target_path: CapabilityPath::try_from("/svc/fuchsia.sys2.Realm").unwrap(),
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;
    // RoutingTest installs the real RealmCapabilityHost. Installing the
    // MockRealmCapabilityHost here overrides the previously installed one.
    let realm_service_host = Arc::new(MockRealmCapabilityHost::new());
    test.model
        .root
        .hooks
        .install(vec![HooksRegistration::new(
            "MockRealmCapabilityHost",
            vec![EventType::CapabilityRouted],
            Arc::downgrade(&realm_service_host) as Weak<dyn Hook>,
        )])
        .await;
    test.check_use_realm(vec!["b:0"].into(), realm_service_host.bind_calls()).await;
}

///   a
///    \
///     b
///
/// a: offers service /svc/foo from self as /svc/bar
/// b: uses service /svc/bar as /svc/hippo
///
/// This test verifies that the parent, if subscribed to the CapabilityRequested event will receive
/// if when the child connects to /svc/hippo.
#[fuchsia::test]
async fn capability_requested_event_at_parent() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .protocol(ProtocolDeclBuilder::new("foo_svc").build())
                .offer(OfferDecl::Protocol(OfferProtocolDecl {
                    source: OfferSource::Self_,
                    source_name: "foo_svc".into(),
                    target_name: "bar_svc".into(),
                    target: OfferTarget::Child("b".to_string()),
                    dependency_type: DependencyType::Strong,
                }))
                .use_(UseDecl::Protocol(UseProtocolDecl {
                    source: UseSource::Parent,
                    source_name: "fuchsia.sys2.EventSource".try_into().unwrap(),
                    target_path: "/svc/fuchsia.sys2.EventSource".try_into().unwrap(),
                }))
                .use_(UseDecl::Event(UseEventDecl {
                    source: UseSource::Framework,
                    source_name: "capability_requested".into(),
                    target_name: "capability_requested".into(),
                    filter: Some(hashmap!{"name".to_string() => DictionaryValue::Str("foo_svc".to_string())}),
                    mode: cm_rust::EventMode::Async,
                }))
                .use_(UseDecl::Event(UseEventDecl {
                    source: UseSource::Framework,
                    source_name: "resolved".into(),
                    target_name: "resolved".into(),
                    filter: None,
                    mode: cm_rust::EventMode::Sync,
                }))
                .use_(UseDecl::EventStream(UseEventStreamDecl {
                    name: CapabilityName::try_from("StartComponentTree").unwrap(),
                    subscriptions: vec![cm_rust::EventSubscription {
                        event_name: "resolved".into(),
                        mode: cm_rust::EventMode::Sync,
                    }],
                }))
                .add_lazy_child("b")
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Protocol(UseProtocolDecl {
                    source: UseSource::Parent,
                    source_name: "bar_svc".into(),
                    target_path: CapabilityPath::try_from("/svc/hippo").unwrap(),
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;

    let namespace_root = test.bind_and_get_namespace(AbsoluteMoniker::root()).await;
    let mut event_stream = capability_util::subscribe_to_events(
        &namespace_root,
        &CapabilityPath::try_from("/svc/fuchsia.sys2.EventSource").unwrap(),
        vec![EventSubscription::new("capability_requested".into(), EventMode::Async)],
    )
    .await
    .unwrap();

    let namespace_b = test.bind_and_get_namespace(vec!["b:0"].into()).await;
    let _echo_proxy = capability_util::connect_to_svc_in_namespace::<echo::EchoMarker>(
        &namespace_b,
        &"/svc/hippo".try_into().unwrap(),
    )
    .await;

    let event = match event_stream.next().await {
        Some(Ok(fsys::EventStreamRequest::OnEvent { event, .. })) => event,
        _ => panic!("Event not found"),
    };

    // 'b' is the target and 'a' is receiving the event so the relative moniker
    // is './b:0'.
    assert_matches!(&event,
        fsys::Event {
            header: Some(fsys::EventHeader {
            moniker: Some(moniker), .. }), ..
        } if *moniker == "./b:0".to_string() );

    assert_matches!(&event,
        fsys::Event {
            header: Some(fsys::EventHeader {
            component_url: Some(component_url), .. }), ..
        } if *component_url == "test:///b".to_string() );

    assert_matches!(&event,
        fsys::Event {
            event_result: Some(
                fsys::EventResult::Payload(
                        fsys::EventPayload::CapabilityRequested(
                            fsys::CapabilityRequestedPayload { name: Some(name), .. }))), ..}

    if *name == "foo_svc".to_string()
    );
}

///   a
///    \
///     b
///    / \
///  [c] [d]
/// a: offers service /svc/hippo to b
/// b: offers service /svc/hippo to collection, creates [c]
/// [c]: instance in collection uses service /svc/hippo
/// [d]: ditto, but with /data/hippo
#[fuchsia::test]
async fn use_in_collection() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .directory(DirectoryDeclBuilder::new("foo_data").build())
                .protocol(ProtocolDeclBuilder::new("foo_svc").build())
                .offer(OfferDecl::Directory(OfferDirectoryDecl {
                    source_name: "foo_data".into(),
                    source: OfferSource::Self_,
                    target_name: "hippo_data".into(),
                    target: OfferTarget::Child("b".to_string()),
                    rights: Some(*rights::READ_RIGHTS),
                    subdir: None,
                    dependency_type: DependencyType::Strong,
                }))
                .offer(OfferDecl::Protocol(OfferProtocolDecl {
                    source_name: "foo_svc".into(),
                    source: OfferSource::Self_,
                    target_name: "hippo_svc".into(),
                    target: OfferTarget::Child("b".to_string()),
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
                    source_name: "fuchsia.sys2.Realm".into(),
                    target_path: CapabilityPath::try_from("/svc/fuchsia.sys2.Realm").unwrap(),
                }))
                .offer(OfferDecl::Directory(OfferDirectoryDecl {
                    source_name: "hippo_data".into(),
                    source: OfferSource::Parent,
                    target_name: "hippo_data".into(),
                    target: OfferTarget::Collection("coll".to_string()),
                    rights: Some(*rights::READ_RIGHTS),
                    subdir: None,
                    dependency_type: DependencyType::Strong,
                }))
                .offer(OfferDecl::Protocol(OfferProtocolDecl {
                    source_name: "hippo_svc".into(),
                    source: OfferSource::Parent,
                    target_name: "hippo_svc".into(),
                    target: OfferTarget::Collection("coll".to_string()),
                    dependency_type: DependencyType::Strong,
                }))
                .add_transient_collection("coll")
                .build(),
        ),
        (
            "c",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Directory(UseDirectoryDecl {
                    source: UseSource::Parent,
                    source_name: "hippo_data".into(),
                    target_path: CapabilityPath::try_from("/data/hippo").unwrap(),
                    rights: *rights::READ_RIGHTS,
                    subdir: None,
                }))
                .build(),
        ),
        (
            "d",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Protocol(UseProtocolDecl {
                    source: UseSource::Parent,
                    source_name: "hippo_svc".into(),
                    target_path: CapabilityPath::try_from("/svc/hippo").unwrap(),
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;
    test.create_dynamic_child(
        vec!["b:0"].into(),
        "coll",
        ChildDecl {
            name: "c".to_string(),
            url: "test:///c".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        },
    )
    .await;
    test.create_dynamic_child(
        vec!["b:0"].into(),
        "coll",
        ChildDecl {
            name: "d".to_string(),
            url: "test:///d".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        },
    )
    .await;
    test.check_use(vec!["b:0", "coll:c:1"].into(), CheckUse::default_directory(ExpectedResult::Ok))
        .await;
    test.check_use(
        vec!["b:0", "coll:d:2"].into(),
        CheckUse::Protocol { path: default_service_capability(), expected_res: ExpectedResult::Ok },
    )
    .await;
}

///   a
///    \
///     b
///      \
///      [c]
/// a: offers service /svc/hippo to b
/// b: creates [c]
/// [c]: tries to use /svc/hippo, but can't because service was not offered to its collection
#[fuchsia::test]
async fn use_in_collection_not_offered() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .directory(DirectoryDeclBuilder::new("foo_data").build())
                .protocol(ProtocolDeclBuilder::new("foo_svc").build())
                .offer(OfferDecl::Directory(OfferDirectoryDecl {
                    source_name: "foo_data".into(),
                    source: OfferSource::Self_,
                    target_name: "hippo_data".into(),
                    target: OfferTarget::Child("b".to_string()),
                    rights: Some(*rights::READ_RIGHTS),
                    subdir: None,
                    dependency_type: DependencyType::Strong,
                }))
                .offer(OfferDecl::Protocol(OfferProtocolDecl {
                    source_name: "foo_svc".into(),
                    source: OfferSource::Self_,
                    target_name: "hippo_svc".into(),
                    target: OfferTarget::Child("b".to_string()),
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
                    source_name: "fuchsia.sys2.Realm".into(),
                    target_path: CapabilityPath::try_from("/svc/fuchsia.sys2.Realm").unwrap(),
                }))
                .add_transient_collection("coll")
                .build(),
        ),
        (
            "c",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Directory(UseDirectoryDecl {
                    source: UseSource::Parent,
                    source_name: "hippo_data".into(),
                    target_path: CapabilityPath::try_from("/data/hippo").unwrap(),
                    rights: *rights::READ_RIGHTS,
                    subdir: None,
                }))
                .use_(UseDecl::Protocol(UseProtocolDecl {
                    source: UseSource::Parent,
                    source_name: "hippo_svc".into(),
                    target_path: CapabilityPath::try_from("/svc/hippo").unwrap(),
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;
    test.create_dynamic_child(
        vec!["b:0"].into(),
        "coll",
        ChildDecl {
            name: "c".to_string(),
            url: "test:///c".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        },
    )
    .await;
    test.check_use(
        vec!["b:0", "coll:c:1"].into(),
        CheckUse::default_directory(ExpectedResult::Err(zx::Status::UNAVAILABLE)),
    )
    .await;
    test.check_use(
        vec!["b:0", "coll:c:1"].into(),
        CheckUse::Protocol {
            path: default_service_capability(),
            expected_res: ExpectedResult::Err(zx::Status::UNAVAILABLE),
        },
    )
    .await;
}

#[fuchsia::test]
async fn destroying_instance_kills_framework_service_task() {
    let components = vec![
        ("a", ComponentDeclBuilder::new().add_lazy_child("b").build()),
        (
            "b",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Protocol(UseProtocolDecl {
                    source: UseSource::Framework,
                    source_name: "fuchsia.sys2.Realm".into(),
                    target_path: CapabilityPath::try_from("/svc/fuchsia.sys2.Realm").unwrap(),
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;

    // Connect to `Realm`, which is a framework service.
    let namespace = test.bind_and_get_namespace(vec!["b:0"].into()).await;
    let proxy = capability_util::connect_to_svc_in_namespace::<fsys::RealmMarker>(
        &namespace,
        &"/svc/fuchsia.sys2.Realm".try_into().unwrap(),
    )
    .await;

    // Destroy `b`. This should cause the task hosted for `Realm` to be cancelled.
    let root = test.model.look_up(&vec![].into()).await.unwrap();
    ActionSet::register(root.clone(), MarkDeletedAction::new("b".into()))
        .await
        .expect("mark deleted failed");
    ActionSet::register(root.clone(), DeleteChildAction::new("b:0".into()))
        .await
        .expect("destroy failed");
    let mut event_stream = proxy.take_event_stream();
    assert_matches!(event_stream.next().await, None);
}

///  a
///   \
///    b
///
/// a: declares runner "elf" with service "/svc/runner" from "self".
/// a: registers runner "elf" from self in environment as "hobbit".
/// b: uses runner "hobbit".
#[fuchsia::test]
async fn use_runner_from_parent_environment() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .add_child(ChildDeclBuilder::new_lazy_child("b").environment("env").build())
                .add_environment(
                    EnvironmentDeclBuilder::new()
                        .name("env")
                        .extends(fsys::EnvironmentExtends::Realm)
                        .add_runner(RunnerRegistration {
                            source_name: "elf".into(),
                            source: RegistrationSource::Self_,
                            target_name: "hobbit".into(),
                        })
                        .build(),
                )
                .runner(RunnerDecl {
                    name: "elf".into(),
                    source_path: CapabilityPath::try_from("/svc/runner").unwrap(),
                })
                .build(),
        ),
        ("b", ComponentDeclBuilder::new_empty_component().add_program("hobbit").build()),
    ];

    // Set up the system.
    let (runner_service, mut receiver) =
        create_service_directory_entry::<fcrunner::ComponentRunnerMarker>();
    let universe = RoutingTestBuilder::new("a", components)
        // Component "b" exposes a runner service.
        .add_outgoing_path("a", CapabilityPath::try_from("/svc/runner").unwrap(), runner_service)
        .build()
        .await;

    join!(
        // Bind "b:0". We expect to see a call to our runner service for the new component.
        async move {
            universe.bind_instance(&vec!["b:0"].into()).await.unwrap();
        },
        // Wait for a request, and ensure it has the correct URL.
        async move {
            assert_eq!(
                wait_for_runner_request(&mut receiver).await.resolved_url,
                Some("test:///b_resolved".to_string())
            );
        }
    );
}

///  a
///   \
///   [b]
///
/// a: declares runner "elf" with service "/svc/runner" from "self".
/// a: registers runner "elf" from self in environment as "hobbit".
/// b: instance in collection uses runner "hobbit".
#[fuchsia::test]
async fn use_runner_from_environment_in_collection() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .add_collection(
                    CollectionDeclBuilder::new_transient_collection("coll")
                        .environment("env")
                        .build(),
                )
                .add_environment(
                    EnvironmentDeclBuilder::new()
                        .name("env")
                        .extends(fsys::EnvironmentExtends::Realm)
                        .add_runner(RunnerRegistration {
                            source_name: "elf".into(),
                            source: RegistrationSource::Self_,
                            target_name: "hobbit".into(),
                        })
                        .build(),
                )
                .use_(UseDecl::Protocol(UseProtocolDecl {
                    source: UseSource::Framework,
                    source_name: "fuchsia.sys2.Realm".into(),
                    target_path: CapabilityPath::try_from("/svc/fuchsia.sys2.Realm").unwrap(),
                }))
                .runner(RunnerDecl {
                    name: "elf".into(),
                    source_path: CapabilityPath::try_from("/svc/runner").unwrap(),
                })
                .build(),
        ),
        ("b", ComponentDeclBuilder::new_empty_component().add_program("hobbit").build()),
    ];

    // Set up the system.
    let (runner_service, mut receiver) =
        create_service_directory_entry::<fcrunner::ComponentRunnerMarker>();
    let universe = RoutingTestBuilder::new("a", components)
        // Component "a" exposes a runner service.
        .add_outgoing_path("a", CapabilityPath::try_from("/svc/runner").unwrap(), runner_service)
        .build()
        .await;
    universe
        .create_dynamic_child(
            AbsoluteMoniker::root(),
            "coll",
            ChildDecl {
                name: "b".to_string(),
                url: "test:///b".to_string(),
                startup: fsys::StartupMode::Lazy,
                environment: None,
            },
        )
        .await;

    join!(
        // Bind "coll:b:1". We expect to see a call to our runner service for the new component.
        async move {
            universe.bind_instance(&vec!["coll:b:1"].into()).await.unwrap();
        },
        // Wait for a request, and ensure it has the correct URL.
        async move {
            assert_eq!(
                wait_for_runner_request(&mut receiver).await.resolved_url,
                Some("test:///b_resolved".to_string())
            );
        }
    );
}

///   a
///    \
///     b
///      \
///       c
///
/// a: declares runner "elf" as service "/svc/runner" from self.
/// a: offers runner "elf" from self to "b" as "dwarf".
/// b: registers runner "dwarf" from realm in environment as "hobbit".
/// c: uses runner "hobbit".
#[fuchsia::test]
async fn use_runner_from_grandparent_environment() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .add_lazy_child("b")
                .offer(OfferDecl::Runner(OfferRunnerDecl {
                    source: OfferSource::Self_,
                    source_name: CapabilityName("elf".to_string()),
                    target: OfferTarget::Child("b".to_string()),
                    target_name: CapabilityName("dwarf".to_string()),
                }))
                .runner(RunnerDecl {
                    name: "elf".into(),
                    source_path: CapabilityPath::try_from("/svc/runner").unwrap(),
                })
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .add_child(ChildDeclBuilder::new_lazy_child("c").environment("env").build())
                .add_environment(
                    EnvironmentDeclBuilder::new()
                        .name("env")
                        .extends(fsys::EnvironmentExtends::Realm)
                        .add_runner(RunnerRegistration {
                            source_name: "dwarf".into(),
                            source: RegistrationSource::Parent,
                            target_name: "hobbit".into(),
                        })
                        .build(),
                )
                .build(),
        ),
        ("c", ComponentDeclBuilder::new_empty_component().add_program("hobbit").build()),
    ];

    // Set up the system.
    let (runner_service, mut receiver) =
        create_service_directory_entry::<fcrunner::ComponentRunnerMarker>();
    let universe = RoutingTestBuilder::new("a", components)
        // Component "a" exposes a runner service.
        .add_outgoing_path("a", CapabilityPath::try_from("/svc/runner").unwrap(), runner_service)
        .build()
        .await;

    join!(
        // Bind "c:0". We expect to see a call to our runner service for the new component.
        async move {
            universe.bind_instance(&vec!["b:0", "c:0"].into()).await.unwrap();
        },
        // Wait for a request, and ensure it has the correct URL.
        async move {
            assert_eq!(
                wait_for_runner_request(&mut receiver).await.resolved_url,
                Some("test:///c_resolved".to_string())
            );
        }
    );
}

///   a
///  / \
/// b   c
///
/// a: registers runner "dwarf" from "b" in environment as "hobbit".
/// b: exposes runner "elf" as service "/svc/runner" from self as "dwarf".
/// c: uses runner "hobbit".
#[fuchsia::test]
async fn use_runner_from_sibling_environment() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .add_lazy_child("b")
                .add_child(ChildDeclBuilder::new_lazy_child("c").environment("env").build())
                .add_environment(
                    EnvironmentDeclBuilder::new()
                        .name("env")
                        .extends(fsys::EnvironmentExtends::Realm)
                        .add_runner(RunnerRegistration {
                            source_name: "dwarf".into(),
                            source: RegistrationSource::Child("b".into()),
                            target_name: "hobbit".into(),
                        })
                        .build(),
                )
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .expose(ExposeDecl::Runner(ExposeRunnerDecl {
                    source: ExposeSource::Self_,
                    source_name: CapabilityName("elf".to_string()),
                    target: ExposeTarget::Parent,
                    target_name: CapabilityName("dwarf".to_string()),
                }))
                .runner(RunnerDecl {
                    name: "elf".into(),
                    source_path: CapabilityPath::try_from("/svc/runner").unwrap(),
                })
                .build(),
        ),
        ("c", ComponentDeclBuilder::new_empty_component().add_program("hobbit").build()),
    ];

    // Set up the system.
    let (runner_service, mut receiver) =
        create_service_directory_entry::<fcrunner::ComponentRunnerMarker>();
    let universe = RoutingTestBuilder::new("a", components)
        // Component "a" exposes a runner service.
        .add_outgoing_path("b", CapabilityPath::try_from("/svc/runner").unwrap(), runner_service)
        .build()
        .await;

    join!(
        // Bind "c:0". We expect to see a call to our runner service for the new component.
        async move {
            universe.bind_instance(&vec!["c:0"].into()).await.unwrap();
        },
        // Wait for a request, and ensure it has the correct URL.
        async move {
            assert_eq!(
                wait_for_runner_request(&mut receiver).await.resolved_url,
                Some("test:///c_resolved".to_string())
            );
        }
    );
}

///   a
///    \
///     b
///      \
///       c
///
/// a: declares runner "elf" as service "/svc/runner" from self.
/// a: registers runner "elf" from realm in environment as "hobbit".
/// b: creates environment extending from realm.
/// c: uses runner "hobbit".
#[fuchsia::test]
async fn use_runner_from_inherited_environment() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .add_child(ChildDeclBuilder::new_lazy_child("b").environment("env").build())
                .add_environment(
                    EnvironmentDeclBuilder::new()
                        .name("env")
                        .extends(fsys::EnvironmentExtends::Realm)
                        .add_runner(RunnerRegistration {
                            source_name: "elf".into(),
                            source: RegistrationSource::Self_,
                            target_name: "hobbit".into(),
                        })
                        .build(),
                )
                .runner(RunnerDecl {
                    name: "elf".into(),
                    source_path: CapabilityPath::try_from("/svc/runner").unwrap(),
                })
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .add_child(ChildDeclBuilder::new_lazy_child("c").environment("env").build())
                .add_environment(
                    EnvironmentDeclBuilder::new()
                        .name("env")
                        .extends(fsys::EnvironmentExtends::Realm)
                        .build(),
                )
                .build(),
        ),
        ("c", ComponentDeclBuilder::new_empty_component().add_program("hobbit").build()),
    ];

    // Set up the system.
    let (runner_service, mut receiver) =
        create_service_directory_entry::<fcrunner::ComponentRunnerMarker>();
    let universe = RoutingTestBuilder::new("a", components)
        // Component "a" exposes a runner service.
        .add_outgoing_path("a", CapabilityPath::try_from("/svc/runner").unwrap(), runner_service)
        .build()
        .await;

    join!(
        // Bind "c:0". We expect to see a call to our runner service for the new component.
        async move {
            universe.bind_instance(&vec!["b:0", "c:0"].into()).await.unwrap();
        },
        // Wait for a request, and ensure it has the correct URL.
        async move {
            assert_eq!(
                wait_for_runner_request(&mut receiver).await.resolved_url,
                Some("test:///c_resolved".to_string())
            );
        }
    );
}

///  a
///   \
///    b
///
/// a: declares runner "elf" with service "/svc/runner" from "self".
/// a: registers runner "elf" from self in environment as "hobbit".
/// b: uses runner "hobbit". Fails because "hobbit" was not in environment.
#[fuchsia::test]
async fn use_runner_from_environment_not_found() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .add_child(ChildDeclBuilder::new_lazy_child("b").environment("env").build())
                .add_environment(
                    EnvironmentDeclBuilder::new()
                        .name("env")
                        .extends(fsys::EnvironmentExtends::Realm)
                        .add_runner(RunnerRegistration {
                            source_name: "elf".into(),
                            source: RegistrationSource::Self_,
                            target_name: "dwarf".into(),
                        })
                        .build(),
                )
                .runner(RunnerDecl {
                    name: "elf".into(),
                    source_path: CapabilityPath::try_from("/svc/runner").unwrap(),
                })
                .build(),
        ),
        ("b", ComponentDeclBuilder::new_empty_component().add_program("hobbit").build()),
    ];

    // Set up the system.
    let (runner_service, _receiver) =
        create_service_directory_entry::<fcrunner::ComponentRunnerMarker>();
    let universe = RoutingTestBuilder::new("a", components)
        // Component "a" exposes a runner service.
        .add_outgoing_path("a", CapabilityPath::try_from("/svc/runner").unwrap(), runner_service)
        .build()
        .await;

    // Bind "b:0". We expect it to fail because routing failed.
    assert_matches!(
        universe.bind_instance(&vec!["b:0"].into()).await,
        Err(ModelError::RoutingError {
            err: RoutingError::UseFromEnvironmentNotFound {
                moniker,
                capability_type,
                capability_name,
            }
        })
        if moniker == AbsoluteMoniker::from(vec!["b:0"]) &&
        capability_type == "runner" &&
        capability_name == CapabilityName("hobbit".to_string()));
}

// TODO: Write a test for environment that extends from None. Currently, this is not
// straightforward because resolver routing is not implemented yet, which makes it impossible to
// register a new resolver and have it be usable.

///   a
///    \
///    [b]
///      \
///       c
///
/// a: offers service /svc/foo from self
/// [b]: offers service /svc/foo to c
/// [b]: is destroyed
/// c: uses service /svc/foo, which should fail
#[fuchsia::test]
async fn use_with_destroyed_parent() {
    let use_protocol_decl = UseProtocolDecl {
        source: UseSource::Parent,
        source_name: "foo_svc".into(),
        target_path: CapabilityPath::try_from("/svc/hippo").unwrap(),
    };
    let use_decl = UseDecl::Protocol(use_protocol_decl.clone());
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .protocol(ProtocolDeclBuilder::new("foo_svc").build())
                .use_(UseDecl::Protocol(UseProtocolDecl {
                    source: UseSource::Framework,
                    source_name: "fuchsia.sys2.Realm".into(),
                    target_path: CapabilityPath::try_from("/svc/fuchsia.sys2.Realm").unwrap(),
                }))
                .offer(OfferDecl::Protocol(OfferProtocolDecl {
                    source: OfferSource::Self_,
                    source_name: "foo_svc".into(),
                    target_name: "foo_svc".into(),
                    target: OfferTarget::Collection("coll".to_string()),
                    dependency_type: DependencyType::Strong,
                }))
                .add_transient_collection("coll")
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .offer(OfferDecl::Protocol(OfferProtocolDecl {
                    source: OfferSource::Parent,
                    source_name: "foo_svc".into(),
                    target_name: "foo_svc".into(),
                    target: OfferTarget::Child("c".to_string()),
                    dependency_type: DependencyType::Strong,
                }))
                .add_lazy_child("c")
                .build(),
        ),
        ("c", ComponentDeclBuilder::new().use_(use_decl.clone()).build()),
    ];
    let test = RoutingTest::new("a", components).await;
    test.create_dynamic_child(
        vec![].into(),
        "coll",
        ChildDecl {
            name: "b".to_string(),
            url: "test:///b".to_string(),
            startup: fsys::StartupMode::Lazy,
            environment: None,
        },
    )
    .await;

    // Confirm we can use service from "c".
    test.check_use(
        vec!["coll:b:1", "c:0"].into(),
        CheckUse::Protocol { path: default_service_capability(), expected_res: ExpectedResult::Ok },
    )
    .await;

    // Destroy "b", but preserve a reference to "c" so we can route from it below.
    let moniker = vec!["coll:b:1", "c:0"].into();
    let realm_c = test.model.look_up(&moniker).await.expect("failed to look up realm b");
    test.destroy_dynamic_child(vec![].into(), "coll", "b").await;

    // Now attempt to route the service from "c". Should fail because "b" does not exist so we
    // cannot follow it.
    let err = route_capability(RouteRequest::UseProtocol(use_protocol_decl), &realm_c)
        .await
        .expect_err("routing unexpectedly succeeded");
    assert_matches!(
        err,
        RoutingError::ComponentInstanceError(
            ComponentInstanceError::InstanceNotFound { moniker }
        ) if moniker == vec!["coll:b:1"].into()
    );
}

///   a
///  / \
/// b   c
///
/// b: exposes directory /data/foo from self as /data/bar
/// a: offers directory /data/bar from b as /data/baz to c, which was destroyed (but not removed
///    from the tree yet)
/// c: uses /data/baz as /data/hippo
#[fuchsia::test]
async fn use_from_destroyed_but_not_removed() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .offer(OfferDecl::Protocol(OfferProtocolDecl {
                    source: OfferSource::Child("b".to_string()),
                    source_name: "bar_svc".into(),
                    target_name: "baz_svc".into(),
                    target: OfferTarget::Child("c".to_string()),
                    dependency_type: DependencyType::Strong,
                }))
                .add_lazy_child("b")
                .add_lazy_child("c")
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .directory(DirectoryDeclBuilder::new("foo_data").build())
                .protocol(ProtocolDeclBuilder::new("foo_svc").build())
                .expose(ExposeDecl::Protocol(ExposeProtocolDecl {
                    source: ExposeSource::Self_,
                    source_name: "foo_svc".into(),
                    target_name: "bar_svc".into(),
                    target: ExposeTarget::Parent,
                }))
                .build(),
        ),
        (
            "c",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Protocol(UseProtocolDecl {
                    source: UseSource::Parent,
                    source_name: "baz_svc".into(),
                    target_path: CapabilityPath::try_from("/svc/hippo").unwrap(),
                }))
                .build(),
        ),
    ];
    let test = RoutingTest::new("a", components).await;
    let component_b =
        test.model.look_up(&vec!["b:0"].into()).await.expect("failed to look up realm b");
    // Destroy `b` but keep alive its reference from the parent.
    // TODO: If we had a "pre-destroy" event we could delete the child through normal means and
    // block on the event instead of explicitly registering actions.
    ActionSet::register(component_b.clone(), ShutdownAction::new()).await.expect("shutdown failed");
    ActionSet::register(component_b, DestroyAction::new()).await.expect("destroy failed");
    test.check_use(
        vec!["c:0"].into(),
        CheckUse::Protocol {
            path: default_service_capability(),
            expected_res: ExpectedResult::Err(zx::Status::UNAVAILABLE),
        },
    )
    .await;
}

///   a
///  / \
/// b   c
///
/// a: creates environment "env" and registers resolver "base" from c.
/// b: resolved by resolver "base" through "env".
/// b: exposes resolver "base" from self.
#[fuchsia::test]
async fn use_resolver_from_parent_environment() {
    // Note that we do not define a component "b". This will be resolved by our custom resolver.
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new_empty_component()
                .add_child(ChildDeclBuilder::new().name("b").url("base://b").environment("env"))
                .add_child(ChildDeclBuilder::new_lazy_child("c"))
                .add_environment(
                    EnvironmentDeclBuilder::new()
                        .name("env")
                        .extends(fsys::EnvironmentExtends::Realm)
                        .add_resolver(ResolverRegistration {
                            resolver: "base".into(),
                            source: RegistrationSource::Child("c".into()),
                            scheme: "base".into(),
                        }),
                )
                .build(),
        ),
        (
            "c",
            ComponentDeclBuilder::new()
                .expose(ExposeDecl::Resolver(ExposeResolverDecl {
                    source: ExposeSource::Self_,
                    source_name: "base".into(),
                    target: ExposeTarget::Parent,
                    target_name: "base".into(),
                }))
                .resolver(ResolverDecl {
                    name: "base".into(),
                    source_path: "/svc/fuchsia.sys2.ComponentResolver".parse().unwrap(),
                })
                .build(),
        ),
    ];

    // Set up the system.
    let (resolver_service, mut receiver) =
        create_service_directory_entry::<fsys::ComponentResolverMarker>();
    let universe = RoutingTestBuilder::new("a", components)
        // Component "c" exposes a resolver service.
        .add_outgoing_path(
            "c",
            CapabilityPath::try_from("/svc/fuchsia.sys2.ComponentResolver").unwrap(),
            resolver_service,
        )
        .build()
        .await;

    join!(
        // Bind "b:0". We expect to see a call to our resolver service for the new component.
        async move {
            universe
                .bind_instance(&vec!["b:0"].into())
                .await
                .expect("failed to bind to instance b:0");
        },
        // Wait for a request, and resolve it.
        async {
            while let Some(fsys::ComponentResolverRequest::Resolve { component_url, responder }) =
                receiver.next().await
            {
                assert_eq!(component_url, "base://b");
                responder
                    .send(&mut Ok(fsys::Component {
                        resolved_url: Some("test://b".into()),
                        decl: Some(fmem::Data::Bytes(
                            fidl::encoding::encode_persistent(
                                &mut default_component_decl().native_into_fidl(),
                            )
                            .unwrap(),
                        )),
                        package: None,
                        ..fsys::Component::EMPTY
                    }))
                    .expect("failed to send resolve response");
            }
        }
    );
}

///   a
///    \
///     b
///      \
///       c
/// a: creates environment "env" and registers resolver "base" from self.
/// b: has environment "env".
/// c: is resolved by resolver from grandarent.
#[fuchsia::test]
async fn use_resolver_from_grandparent_environment() {
    // Note that we do not define a component "c". This will be resolved by our custom resolver.
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .add_child(ChildDeclBuilder::new_lazy_child("b").environment("env"))
                .add_environment(
                    EnvironmentDeclBuilder::new()
                        .name("env")
                        .extends(fsys::EnvironmentExtends::Realm)
                        .add_resolver(ResolverRegistration {
                            resolver: "base".into(),
                            source: RegistrationSource::Self_,
                            scheme: "base".into(),
                        }),
                )
                .resolver(ResolverDecl {
                    name: "base".into(),
                    source_path: "/svc/fuchsia.sys2.ComponentResolver".parse().unwrap(),
                })
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new_empty_component()
                .add_child(ChildDeclBuilder::new().name("c").url("base://c"))
                .build(),
        ),
    ];

    // Set up the system.
    let (resolver_service, mut receiver) =
        create_service_directory_entry::<fsys::ComponentResolverMarker>();
    let universe = RoutingTestBuilder::new("a", components)
        // Component "c" exposes a resolver service.
        .add_outgoing_path(
            "a",
            CapabilityPath::try_from("/svc/fuchsia.sys2.ComponentResolver").unwrap(),
            resolver_service,
        )
        .build()
        .await;

    join!(
        // Bind "c:0". We expect to see a call to our resolver service for the new component.
        async move {
            universe
                .bind_instance(&vec!["b:0", "c:0"].into())
                .await
                .expect("failed to bind to instance c:0");
        },
        // Wait for a request, and resolve it.
        async {
            while let Some(fsys::ComponentResolverRequest::Resolve { component_url, responder }) =
                receiver.next().await
            {
                assert_eq!(component_url, "base://c");
                responder
                    .send(&mut Ok(fsys::Component {
                        resolved_url: Some("test://c".into()),
                        decl: Some(fmem::Data::Bytes(
                            fidl::encoding::encode_persistent(
                                &mut default_component_decl().native_into_fidl(),
                            )
                            .unwrap(),
                        )),
                        package: None,
                        ..fsys::Component::EMPTY
                    }))
                    .expect("failed to send resolve response");
            }
        }
    );
}

///   a
///  / \
/// b   c
/// a: creates environment "env" and registers resolver "base" from self.
/// b: has environment "env".
/// c: does NOT have environment "env".
#[fuchsia::test]
async fn resolver_is_not_available() {
    // Note that we do not define a component "b" or "c". This will be resolved by our custom resolver.
    let components = vec![(
        "a",
        ComponentDeclBuilder::new()
            .add_child(ChildDeclBuilder::new().name("b").url("base://b").environment("env"))
            .add_child(ChildDeclBuilder::new().name("c").url("base://c"))
            .add_environment(
                EnvironmentDeclBuilder::new()
                    .name("env")
                    .extends(fsys::EnvironmentExtends::Realm)
                    .add_resolver(ResolverRegistration {
                        resolver: "base".into(),
                        source: RegistrationSource::Self_,
                        scheme: "base".into(),
                    }),
            )
            .resolver(ResolverDecl {
                name: "base".into(),
                source_path: "/svc/fuchsia.sys2.ComponentResolver".parse().unwrap(),
            })
            .build(),
    )];

    // Set up the system.
    let (resolver_service, mut receiver) =
        create_service_directory_entry::<fsys::ComponentResolverMarker>();
    let universe = RoutingTestBuilder::new("a", components)
        // Component "c" exposes a resolver service.
        .add_outgoing_path(
            "a",
            CapabilityPath::try_from("/svc/fuchsia.sys2.ComponentResolver").unwrap(),
            resolver_service,
        )
        .build()
        .await;

    join!(
        // Bind "c:0". We expect to see a failure that the scheme is not registered.
        async move {
            match universe.bind_instance(&vec!["c:0"].into()).await {
                Err(ModelError::ComponentInstanceError {
                    err: ComponentInstanceError::ResolveFailed { err: resolve_error, .. },
                }) => {
                    assert_eq!(
                        resolve_error.to_string(),
                        "failed to resolve \"base://c\": scheme not registered"
                    );
                }
                _ => {
                    panic!("expected ModelError wrapping ComponentInstanceError::ResolveFailed");
                }
            };
        },
        // Wait for a request, and resolve it.
        async {
            while let Some(fsys::ComponentResolverRequest::Resolve { component_url, responder }) =
                receiver.next().await
            {
                assert_eq!(component_url, "base://b");
                responder
                    .send(&mut Ok(fsys::Component {
                        resolved_url: Some("test://b".into()),
                        decl: Some(fmem::Data::Bytes(
                            fidl::encoding::encode_persistent(
                                &mut default_component_decl().native_into_fidl(),
                            )
                            .unwrap(),
                        )),
                        package: None,
                        ..fsys::Component::EMPTY
                    }))
                    .expect("failed to send resolve response");
            }
        }
    );
}

///   a
///  /
/// b
/// a: creates environment "env" and registers resolver "base" from self.
/// b: has environment "env".
#[fuchsia::test]
async fn resolver_component_decl_is_validated() {
    // Note that we do not define a component "b". This will be resolved by our custom resolver.
    let components = vec![(
        "a",
        ComponentDeclBuilder::new()
            .add_child(ChildDeclBuilder::new().name("b").url("base://b").environment("env"))
            .add_environment(
                EnvironmentDeclBuilder::new()
                    .name("env")
                    .extends(fsys::EnvironmentExtends::Realm)
                    .add_resolver(ResolverRegistration {
                        resolver: "base".into(),
                        source: RegistrationSource::Self_,
                        scheme: "base".into(),
                    }),
            )
            .resolver(ResolverDecl {
                name: "base".into(),
                source_path: "/svc/fuchsia.sys2.ComponentResolver".parse().unwrap(),
            })
            .build(),
    )];

    // Set up the system.
    let (resolver_service, mut receiver) =
        create_service_directory_entry::<fsys::ComponentResolverMarker>();
    let universe = RoutingTestBuilder::new("a", components)
        // Component "a" exposes a resolver service.
        .add_outgoing_path(
            "a",
            CapabilityPath::try_from("/svc/fuchsia.sys2.ComponentResolver").unwrap(),
            resolver_service,
        )
        .build()
        .await;

    join!(
        // Bind "b:0". We expect to see a ResolverError.
        async move {
            match universe.bind_instance(&vec!["b:0"].into()).await {
                Err(ModelError::ComponentInstanceError {
                    err: ComponentInstanceError::ResolveFailed { err: resolve_error, .. },
                }) => {
                    assert!(resolve_error
                        .to_string()
                        .starts_with("failed to resolve \"base://b\": component manifest invalid"));
                }
                _ => {
                    panic!("expected ModelError wrapping ComponentInstanceError::ResolveFailed");
                }
            };
        },
        // Wait for a request, and resolve it.
        async {
            while let Some(fsys::ComponentResolverRequest::Resolve { component_url, responder }) =
                receiver.next().await
            {
                assert_eq!(component_url, "base://b");
                responder
                    .send(&mut Ok(fsys::Component {
                        resolved_url: Some("test://b".into()),
                        decl: Some(fmem::Data::Bytes({
                            let mut fidl = fsys::ComponentDecl {
                                exposes: Some(vec![fsys::ExposeDecl::Protocol(
                                    fsys::ExposeProtocolDecl {
                                        source: Some(fsys::Ref::Self_(fsys::SelfRef {})),
                                        ..fsys::ExposeProtocolDecl::EMPTY
                                    },
                                )]),
                                ..fsys::ComponentDecl::EMPTY
                            };
                            fidl::encoding::encode_persistent(&mut fidl).unwrap()
                        })),
                        package: None,
                        ..fsys::Component::EMPTY
                    }))
                    .expect("failed to send resolve response");
            }
        }
    );
}

///   a
///    \
///     b
///
/// b: uses framework events "started", and "capability_requested".
/// Capability policy denies the route from being allowed for started but
/// not for capability_requested.
#[fuchsia::test]
async fn use_event_from_framework_denied_by_capabiilty_policy() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .offer(OfferDecl::Protocol(OfferProtocolDecl {
                    source: OfferSource::Parent,
                    source_name: "fuchsia.sys2.EventSource".try_into().unwrap(),
                    target_name: "fuchsia.sys2.EventSource".try_into().unwrap(),
                    target: OfferTarget::Child("b".to_string()),
                    dependency_type: DependencyType::Strong,
                }))
                .add_lazy_child("b")
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Protocol(UseProtocolDecl {
                    source: UseSource::Parent,
                    source_name: "fuchsia.sys2.EventSource".try_into().unwrap(),
                    target_path: "/svc/fuchsia.sys2.EventSource".try_into().unwrap(),
                }))
                .use_(UseDecl::Event(UseEventDecl {
                    source: UseSource::Framework,
                    source_name: "capability_requested".into(),
                    target_name: "capability_requested".into(),
                    filter: None,
                    mode: cm_rust::EventMode::Async,
                }))
                .use_(UseDecl::Event(UseEventDecl {
                    source: UseSource::Framework,
                    source_name: "started".into(),
                    target_name: "started".into(),
                    filter: None,
                    mode: cm_rust::EventMode::Async,
                }))
                .use_(UseDecl::Event(UseEventDecl {
                    source: UseSource::Framework,
                    source_name: "resolved".into(),
                    target_name: "resolved".into(),
                    filter: None,
                    mode: cm_rust::EventMode::Sync,
                }))
                .use_(UseDecl::EventStream(UseEventStreamDecl {
                    name: CapabilityName::try_from("StartComponentTree").unwrap(),
                    subscriptions: vec![cm_rust::EventSubscription {
                        event_name: "resolved".into(),
                        mode: cm_rust::EventMode::Sync,
                    }],
                }))
                .build(),
        ),
    ];

    let mut allowlist = HashSet::new();
    allowlist.insert(AllowlistEntry::Exact(AbsoluteMoniker::from(vec!["b:0"])));

    let test = RoutingTestBuilder::new("a", components)
        .add_capability_policy(
            CapabilityAllowlistKey {
                source_moniker: ExtendedMoniker::ComponentInstance(AbsoluteMoniker::from(vec![
                    "b:0",
                ])),
                source_name: CapabilityName::from("started"),
                source: CapabilityAllowlistSource::Framework,
                capability: CapabilityTypeName::Event,
            },
            HashSet::new(),
        )
        .add_capability_policy(
            CapabilityAllowlistKey {
                source_moniker: ExtendedMoniker::ComponentInstance(AbsoluteMoniker::from(vec![
                    "b:0",
                ])),
                source_name: CapabilityName::from("capability_requested"),
                source: CapabilityAllowlistSource::Framework,
                capability: CapabilityTypeName::Event,
            },
            allowlist,
        )
        .build()
        .await;

    test.check_use(
        vec!["b:0"].into(),
        CheckUse::Event {
            requests: vec![EventSubscription::new("capability_requested".into(), EventMode::Async)],
            expected_res: ExpectedResult::Ok,
        },
    )
    .await;

    test.check_use(
        vec!["b:0"].into(),
        CheckUse::Event {
            requests: vec![EventSubscription::new("started".into(), EventMode::Async)],
            expected_res: ExpectedResult::Err(zx::Status::ACCESS_DENIED),
        },
    )
    .await
}

// a
//  \
//   b
//
// a: exposes "foo" to parent from child
// b: exposes "foo" to parent from self
#[fuchsia::test]
async fn route_protocol_from_expose() {
    let expose_decl = ExposeProtocolDecl {
        source: ExposeSource::Child("b".into()),
        source_name: "foo".into(),
        target_name: "foo".into(),
        target: ExposeTarget::Parent,
    };
    let expected_protocol_decl =
        ProtocolDecl { name: "foo".into(), source_path: "/svc/foo".parse().unwrap() };

    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .expose(ExposeDecl::Protocol(expose_decl.clone()))
                .add_lazy_child("b")
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .expose(ExposeDecl::Protocol(ExposeProtocolDecl {
                    source: ExposeSource::Self_,
                    source_name: "foo".into(),
                    target_name: "foo".into(),
                    target: ExposeTarget::Parent,
                }))
                .protocol(expected_protocol_decl.clone())
                .build(),
        ),
    ];
    let test = RoutingTestBuilder::new("a", components).build().await;
    let root_instance = test.model.look_up(&AbsoluteMoniker::root()).await.expect("root instance");
    let expected_source_moniker = AbsoluteMoniker::parse_string_without_instances("/b").unwrap();
    assert_matches!(
    route_capability(RouteRequest::ExposeProtocol(expose_decl), &root_instance).await,
        Ok(RouteSource::Protocol(
            CapabilitySource::Component {
                capability: ComponentCapability::Protocol(protocol_decl),
                component,
            })
        ) if protocol_decl == expected_protocol_decl && component.moniker == expected_source_moniker
    );
}

///   a
///  /
/// b
///
/// a: offer to b from self
/// b: use from parent
#[fuchsia::test]
async fn use_service_from_parent() {
    let use_decl = UseServiceDecl {
        source: UseSource::Parent,
        source_name: "foo".into(),
        target_path: CapabilityPath::try_from("/foo").unwrap(),
    };
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .offer(OfferDecl::Service(OfferServiceDecl {
                    sources: vec![ServiceSource {
                        source: OfferSource::Self_,
                        source_name: "foo".into(),
                    }],
                    target_name: "foo".into(),
                    target: OfferTarget::Child("b".to_string()),
                }))
                .service(ServiceDecl {
                    name: "foo".into(),
                    source_path: "/svc/foo".try_into().unwrap(),
                })
                .add_lazy_child("b")
                .build(),
        ),
        ("b", ComponentDeclBuilder::new().use_(use_decl.clone().into()).build()),
    ];
    let test = RoutingTestBuilder::new("a", components).build().await;
    let b_component = test.model.look_up(&vec!["b:0"].into()).await.expect("b instance");
    let a_component = test.model.look_up(&AbsoluteMoniker::root()).await.expect("root instance");
    let source = route_capability(RouteRequest::UseService(use_decl), &b_component)
        .await
        .expect("failed to route service");
    match source {
        RouteSource::Service(CapabilitySource::Component {
            capability: ComponentCapability::Service(ServiceDecl { name, source_path }),
            component,
        }) => {
            assert_eq!(name, CapabilityName("foo".into()));
            assert_eq!(source_path, "/svc/foo".parse::<CapabilityPath>().unwrap());
            assert!(Arc::ptr_eq(&component.upgrade().unwrap(), &a_component));
        }
        _ => panic!("bad capability source"),
    };
}

///   a
///  /
/// b
///
/// a: use from #b
/// b: expose to parent from self
#[fuchsia::test]
async fn use_service_from_child() {
    let use_decl = UseServiceDecl {
        source: UseSource::Child("b".to_string()),
        source_name: "foo".into(),
        target_path: CapabilityPath::try_from("/foo").unwrap(),
    };
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new().use_(use_decl.clone().into()).add_lazy_child("b").build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .service(ServiceDecl {
                    name: "foo".into(),
                    source_path: "/svc/foo".try_into().unwrap(),
                })
                .expose(ExposeDecl::Service(ExposeServiceDecl {
                    sources: vec![ServiceSource {
                        source: ExposeSource::Self_,
                        source_name: "foo".into(),
                    }],
                    target_name: "foo".into(),
                    target: ExposeTarget::Parent,
                }))
                .build(),
        ),
    ];
    let test = RoutingTestBuilder::new("a", components).build().await;
    let a_component = test.model.look_up(&AbsoluteMoniker::root()).await.expect("root instance");
    let b_component = test.model.look_up(&vec!["b:0"].into()).await.expect("b instance");
    let source = route_capability(RouteRequest::UseService(use_decl), &a_component)
        .await
        .expect("failed to route service");
    match source {
        RouteSource::Service(CapabilitySource::Component {
            capability: ComponentCapability::Service(ServiceDecl { name, source_path }),
            component,
        }) => {
            assert_eq!(name, CapabilityName("foo".into()));
            assert_eq!(source_path, "/svc/foo".parse::<CapabilityPath>().unwrap());
            assert!(Arc::ptr_eq(&component.upgrade().unwrap(), &b_component));
        }
        _ => panic!("bad capability source"),
    };
}

///   a
///  / \
/// b   c
///
/// a: offer to b from child c
/// b: use from parent
/// c: expose from self
#[fuchsia::test]
async fn route_service_from_sibling() {
    let use_decl = UseServiceDecl {
        source: UseSource::Parent,
        source_name: "foo".into(),
        target_path: CapabilityPath::try_from("/foo").unwrap(),
    };
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .offer(OfferDecl::Service(OfferServiceDecl {
                    sources: vec![ServiceSource {
                        source: OfferSource::Child("c".into()),
                        source_name: "foo".into(),
                    }],
                    target_name: "foo".into(),
                    target: OfferTarget::Child("b".to_string()),
                }))
                .add_lazy_child("b")
                .add_lazy_child("c")
                .build(),
        ),
        ("b", ComponentDeclBuilder::new().use_(use_decl.clone().into()).build()),
        (
            "c",
            ComponentDeclBuilder::new()
                .expose(ExposeDecl::Service(ExposeServiceDecl {
                    sources: vec![ServiceSource {
                        source: ExposeSource::Self_,
                        source_name: "foo".into(),
                    }],
                    target_name: "foo".into(),
                    target: ExposeTarget::Parent,
                }))
                .service(ServiceDecl {
                    name: "foo".into(),
                    source_path: "/svc/foo".try_into().unwrap(),
                })
                .build(),
        ),
    ];
    let test = RoutingTestBuilder::new("a", components).build().await;
    let b_component = test.model.look_up(&vec!["b:0"].into()).await.expect("b instance");
    let c_component = test.model.look_up(&vec!["c:0"].into()).await.expect("c instance");
    let source = route_capability(RouteRequest::UseService(use_decl), &b_component)
        .await
        .expect("failed to route service");

    // Verify this source comes from `c`.
    match source {
        RouteSource::Service(CapabilitySource::Component {
            capability: ComponentCapability::Service(ServiceDecl { name, source_path }),
            component,
        }) => {
            assert_eq!(name, CapabilityName("foo".into()));
            assert_eq!(source_path, "/svc/foo".parse::<CapabilityPath>().unwrap());
            assert!(Arc::ptr_eq(&component.upgrade().unwrap(), &c_component));
        }
        _ => panic!("bad capability source"),
    };
}

#[fuchsia::test]
async fn route_service_from_parent_collection() {
    let use_decl = UseServiceDecl {
        source: UseSource::Parent,
        source_name: "foo".into(),
        target_path: CapabilityPath::try_from("/foo").unwrap(),
    };
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .offer(OfferDecl::Service(OfferServiceDecl {
                    sources: vec![ServiceSource {
                        source: OfferSource::Collection("coll".to_string()),
                        source_name: "foo".into(),
                    }],
                    target_name: "foo".into(),
                    target: OfferTarget::Child("b".to_string()),
                }))
                .add_collection(CollectionDeclBuilder::new_transient_collection("coll"))
                .add_lazy_child("b")
                .build(),
        ),
        ("b", ComponentDeclBuilder::new().use_(use_decl.clone().into()).build()),
    ];
    let test = RoutingTestBuilder::new("a", components).build().await;
    let b_component = test.model.look_up(&vec!["b:0"].into()).await.expect("b instance");
    let a_component = test.model.look_up(&AbsoluteMoniker::root()).await.expect("root instance");
    let source = route_capability(RouteRequest::UseService(use_decl), &b_component)
        .await
        .expect("failed to route service");
    match source {
        RouteSource::Service(CapabilitySource::Collection {
            collection_name,
            source_name,
            component,
            ..
        }) => {
            assert_eq!(collection_name, "coll");
            assert_eq!(source_name, CapabilityName("foo".into()));
            assert!(Arc::ptr_eq(&component.upgrade().unwrap(), &a_component));
        }
        _ => panic!("bad capability source"),
    };
}

#[fuchsia::test]
async fn list_service_instances_from_collection() {
    let use_decl = UseServiceDecl {
        source: UseSource::Parent,
        source_name: "foo".into(),
        target_path: CapabilityPath::try_from("/foo").unwrap(),
    };
    let components = vec![
        (
            "root",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Protocol(UseProtocolDecl {
                    source: UseSource::Framework,
                    source_name: "fuchsia.sys2.Realm".into(),
                    target_path: CapabilityPath::try_from("/svc/fuchsia.sys2.Realm").unwrap(),
                }))
                .offer(OfferDecl::Service(OfferServiceDecl {
                    sources: vec![ServiceSource {
                        source: OfferSource::Collection("coll".to_string()),
                        source_name: "foo".into(),
                    }],
                    target_name: "foo".into(),
                    target: OfferTarget::Child("client".to_string()),
                }))
                .add_collection(CollectionDeclBuilder::new_transient_collection("coll"))
                .add_lazy_child("client")
                .build(),
        ),
        ("client", ComponentDeclBuilder::new().use_(use_decl.clone().into()).build()),
        (
            "service_child_a",
            ComponentDeclBuilder::new()
                .expose(ExposeDecl::Service(ExposeServiceDecl {
                    sources: vec![ServiceSource {
                        source: ExposeSource::Self_,
                        source_name: "foo".into(),
                    }],
                    target: ExposeTarget::Parent,
                    target_name: "foo".into(),
                }))
                .service(ServiceDecl {
                    name: "foo".into(),
                    source_path: "/svc/foo".try_into().unwrap(),
                })
                .build(),
        ),
        (
            "service_child_b",
            ComponentDeclBuilder::new()
                .expose(ExposeDecl::Service(ExposeServiceDecl {
                    sources: vec![ServiceSource {
                        source: ExposeSource::Self_,
                        source_name: "foo".into(),
                    }],
                    target: ExposeTarget::Parent,
                    target_name: "foo".into(),
                }))
                .service(ServiceDecl {
                    name: "foo".into(),
                    source_path: "/svc/foo".try_into().unwrap(),
                })
                .build(),
        ),
        ("non_service_child", ComponentDeclBuilder::new().build()),
    ];
    let test = RoutingTestBuilder::new("root", components).build().await;

    // Start a few dynamic children in the collection "coll".
    test.create_dynamic_child(
        AbsoluteMoniker::root(),
        "coll",
        ChildDeclBuilder::new_lazy_child("service_child_a"),
    )
    .await;
    test.create_dynamic_child(
        AbsoluteMoniker::root(),
        "coll",
        ChildDeclBuilder::new_lazy_child("non_service_child"),
    )
    .await;
    test.create_dynamic_child(
        AbsoluteMoniker::root(),
        "coll",
        ChildDeclBuilder::new_lazy_child("service_child_b"),
    )
    .await;

    let client_component =
        test.model.look_up(&vec!["client:0"].into()).await.expect("client instance");
    let source = route_capability(RouteRequest::UseService(use_decl), &client_component)
        .await
        .expect("failed to route service");
    let capability_provider = match source {
        RouteSource::Service(CapabilitySource::Collection { capability_provider, .. }) => {
            capability_provider
        }
        _ => panic!("bad capability source"),
    };

    // Check that only the instances that expose the service are listed.
    let instances: HashSet<String> =
        capability_provider.list_instances().await.unwrap().into_iter().collect();
    assert_eq!(instances.len(), 2);
    assert!(instances.contains("service_child_a"));
    assert!(instances.contains("service_child_b"));

    // Try routing to one of the instances.
    let source = capability_provider
        .route_instance("service_child_a")
        .await
        .expect("failed to route to child");
    match source {
        CapabilitySource::Component {
            capability: ComponentCapability::Service(ServiceDecl { name, source_path }),
            component,
        } => {
            assert_eq!(name, CapabilityName("foo".into()));
            assert_eq!(source_path, "/svc/foo".parse::<CapabilityPath>().unwrap());
            assert_eq!(component.moniker, vec!["coll:service_child_a:1"].into());
        }
        _ => panic!("bad child capability source"),
    }
}

///   a
///  / \
/// b   c
///
/// a: offer service from c to b
/// b: use service
/// c: expose service from collection
#[fuchsia::test]
async fn use_service_from_sibling_collection() {
    let components = vec![
        (
            "a",
            ComponentDeclBuilder::new()
                .offer(OfferDecl::Service(OfferServiceDecl {
                    sources: vec![ServiceSource {
                        source: OfferSource::Child("c".to_string()),
                        source_name: "my.service.Service".into(),
                    }],
                    target: OfferTarget::Child("b".to_string()),
                    target_name: "my.service.Service".into(),
                }))
                .add_child(ChildDeclBuilder::new_lazy_child("b"))
                .add_child(ChildDeclBuilder::new_lazy_child("c"))
                .build(),
        ),
        (
            "b",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Service(UseServiceDecl {
                    source: UseSource::Parent,
                    source_name: "my.service.Service".into(),
                    target_path: "/svc/my.service.Service".try_into().unwrap(),
                }))
                .build(),
        ),
        (
            "c",
            ComponentDeclBuilder::new()
                .use_(UseDecl::Protocol(UseProtocolDecl {
                    source: UseSource::Framework,
                    source_name: "fuchsia.sys2.Realm".into(),
                    target_path: "/svc/fuchsia.sys2.Realm".try_into().unwrap(),
                }))
                .expose(ExposeDecl::Service(ExposeServiceDecl {
                    sources: vec![ServiceSource {
                        source: ExposeSource::Collection("coll".to_string()),
                        source_name: "my.service.Service".into(),
                    }],
                    target_name: "my.service.Service".into(),
                    target: ExposeTarget::Parent,
                }))
                .add_collection(CollectionDeclBuilder::new_transient_collection("coll"))
                .build(),
        ),
        (
            "foo",
            ComponentDeclBuilder::new()
                .expose(ExposeDecl::Service(ExposeServiceDecl {
                    sources: vec![ServiceSource {
                        source: ExposeSource::Self_,
                        source_name: "my.service.Service".into(),
                    }],
                    target_name: "my.service.Service".into(),
                    target: ExposeTarget::Parent,
                }))
                .service(ServiceDecl {
                    name: "my.service.Service".into(),
                    source_path: "/svc/my.service.Service".try_into().unwrap(),
                })
                .build(),
        ),
        (
            "bar",
            ComponentDeclBuilder::new()
                .expose(ExposeDecl::Service(ExposeServiceDecl {
                    sources: vec![ServiceSource {
                        source: ExposeSource::Self_,
                        source_name: "my.service.Service".into(),
                    }],
                    target_name: "my.service.Service".into(),
                    target: ExposeTarget::Parent,
                }))
                .service(ServiceDecl {
                    name: "my.service.Service".into(),
                    source_path: "/svc/my.service.Service".try_into().unwrap(),
                })
                .build(),
        ),
        ("baz", ComponentDeclBuilder::new().build()),
    ];

    let (directory_entry, mut receiver) = create_service_directory_entry::<echo::EchoMarker>();
    let instance_dir = pseudo_directory! {
        "echo" => directory_entry,
    };
    let test = RoutingTestBuilder::new("a", components)
        .add_outgoing_path(
            "foo",
            "/svc/my.service.Service/default".try_into().unwrap(),
            instance_dir,
        )
        .build()
        .await;

    // Populate the collection with dynamic children.
    test.create_dynamic_child(vec!["c:0"].into(), "coll", ChildDeclBuilder::new_lazy_child("foo"))
        .await;
    test.create_dynamic_child(vec!["c:0"].into(), "coll", ChildDeclBuilder::new_lazy_child("bar"))
        .await;
    test.create_dynamic_child(vec!["c:0"].into(), "coll", ChildDeclBuilder::new_lazy_child("baz"))
        .await;

    let target: AbsoluteMoniker = vec!["b:0"].into();
    let namespace = test.bind_and_get_namespace(target.clone()).await;
    let dir = capability_util::take_dir_from_namespace(&namespace, "/svc").await;
    let service_dir = io_util::directory::open_directory(
        &dir,
        "my.service.Service",
        io_util::OPEN_RIGHT_READABLE | io_util::OPEN_RIGHT_WRITABLE,
    )
    .await
    .expect("failed to open service");
    let entries: HashSet<String> = files_async::readdir(&service_dir)
        .await
        .expect("failed to read entries")
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert_eq!(entries.len(), 2);
    assert!(entries.contains("foo"));
    assert!(entries.contains("bar"));
    capability_util::add_dir_to_namespace(&namespace, "/svc", dir).await;

    join!(
        async move {
            test.check_use(
                target.clone(),
                CheckUse::Service {
                    path: "/svc/my.service.Service".try_into().unwrap(),
                    instance: "foo/default".to_string(),
                    member: "echo".to_string(),
                    expected_res: ExpectedResult::Ok,
                },
            )
            .await;
        },
        async move {
            while let Some(echo::EchoRequest::EchoString { value, responder }) =
                receiver.next().await
            {
                responder.send(value.as_ref().map(|v| v.as_str())).expect("failed to send reply")
            }
        }
    );
}
