// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::{format_err, Error},
    cm_rust, cm_types,
    fidl::endpoints::ProtocolMarker,
    fidl_fidl_examples_routing_echo::{self as fecho, EchoMarker as EchoClientStatsMarker},
    fidl_fuchsia_component as fcomponent, fidl_fuchsia_component_decl as fcdecl,
    fidl_fuchsia_data as fdata, fidl_fuchsia_sys2 as fsys, fuchsia_async as fasync,
    fuchsia_component::server as fserver,
    fuchsia_component_test::{builder::*, mock, Moniker, RouteBuilder},
    futures::{channel::mpsc, SinkExt, StreamExt, TryStreamExt},
    std::convert::TryInto,
};

const V1_ECHO_CLIENT_URL: &'static str =
    "fuchsia-pkg://fuchsia.com/fuchsia-component-test-tests#meta/echo_client.cmx";
const V2_ECHO_CLIENT_ABSOLUTE_URL: &'static str =
    "fuchsia-pkg://fuchsia.com/fuchsia-component-test-tests#meta/echo_client.cm";
const V2_ECHO_CLIENT_RELATIVE_URL: &'static str = "#meta/echo_client.cm";

const V1_ECHO_SERVER_URL: &'static str =
    "fuchsia-pkg://fuchsia.com/fuchsia-component-test-tests#meta/echo_server.cmx";
const V2_ECHO_SERVER_ABSOLUTE_URL: &'static str =
    "fuchsia-pkg://fuchsia.com/fuchsia-component-test-tests#meta/echo_server.cm";
const V2_ECHO_SERVER_RELATIVE_URL: &'static str = "#meta/echo_server.cm";

const DEFAULT_ECHO_STR: &'static str = "Hello Fuchsia!";

#[fasync::run_singlethreaded(test)]
async fn protocol_with_uncle_test() -> Result<(), Error> {
    let (send_echo_server_called, mut receive_echo_server_called) = mpsc::channel(1);

    let mut builder = RealmBuilder::new().await?;
    builder
        .add_component(
            "echo-server",
            ComponentSource::mock(move |mock_handles: mock::MockHandles| {
                Box::pin(echo_server_mock(
                    DEFAULT_ECHO_STR,
                    send_echo_server_called.clone(),
                    mock_handles,
                ))
            }),
        )
        .await?
        .add_eager_component(
            "parent/echo-client",
            ComponentSource::url(V2_ECHO_CLIENT_ABSOLUTE_URL),
        )
        .await?
        .add_route(
            RouteBuilder::protocol("fidl.examples.routing.echo.Echo")
                .source(RouteEndpoint::component("echo-server"))
                .targets(vec![RouteEndpoint::component("parent/echo-client")]),
        )?
        .add_route(
            RouteBuilder::protocol("fuchsia.logger.LogSink")
                .source(RouteEndpoint::above_root())
                .targets(vec![
                    RouteEndpoint::component("echo-server"),
                    RouteEndpoint::component("parent/echo-client"),
                ]),
        )?;
    let _instance = builder.build().create().await?;

    assert!(receive_echo_server_called.next().await.is_some());
    Ok(())
}

#[fasync::run_singlethreaded(test)]
async fn protocol_with_siblings_test() -> Result<(), Error> {
    // [START mock_component_example]
    // Create a new mpsc channel for passing a message from the echo server function
    let (send_echo_server_called, mut receive_echo_server_called) = mpsc::channel(1);

    // Build a new realm
    let mut builder = RealmBuilder::new().await?;
    builder
        // Add the echo server, which is implemented by the echo_server_mock function (defined
        // below). Give this function access to the channel created above, along with the mock
        // component's handles
        .add_component(
            "a",
            ComponentSource::mock(move |mock_handles: mock::MockHandles| {
                Box::pin(echo_server_mock(
                    DEFAULT_ECHO_STR,
                    send_echo_server_called.clone(),
                    mock_handles,
                ))
            }),
        )
        .await?
        // Add the echo client with a URL source
        .add_eager_component(
            "b",
            ComponentSource::url(
                "fuchsia-pkg://fuchsia.com/fuchsia-component-test-tests#meta/echo_client.cm",
            ),
        )
        .await?
        // Route the fidl.examples.routing.echo.Echo protocol from a to b
        .add_route(
            RouteBuilder::protocol("fidl.examples.routing.echo.Echo")
                .source(RouteEndpoint::component("a"))
                .targets(vec![RouteEndpoint::component("b")]),
        )?
        // Route the logsink to `b`, so it can inform us of any issues
        .add_route(
            RouteBuilder::protocol("fuchsia.logger.LogSink")
                .source(RouteEndpoint::above_root())
                .targets(vec![RouteEndpoint::component("b")]),
        )?;

    // Create and start the realm
    let _instance = builder.build().create().await?;

    // Wait for the channel we created above to receive a message
    assert!(receive_echo_server_called.next().await.is_some());
    // [END mock_component_example]
    Ok(())
}

#[fasync::run_singlethreaded(test)]
async fn examples() -> Result<(), Error> {
    // This test exists purely to provide us with live snippets for the realm builder
    // documentation
    {
        // [START add_a_and_b_example]
        // Create a new RealmBuilder instance, which we will use to define a new realm
        let mut builder = RealmBuilder::new().await?;
        builder
            // Add component `a` to the realm, which will be fetched with a URL
            .add_component("a", ComponentSource::url("fuchsia-pkg://fuchsia.com/foo#meta/foo.cm"))
            .await?
            // Add component `b` to the realm, which will be fetched with a URL
            .add_component("b", ComponentSource::url("fuchsia-pkg://fuchsia.com/bar#meta/bar.cm"))
            .await?;
        // [END add_a_and_b_example]

        // [START route_from_a_to_b_example]
        // Add a new route for the protocol capability `fidl.examples.routing.echo.Echo`
        // from `a` to `b`
        builder.add_route(
            RouteBuilder::protocol("fidl.examples.routing.echo.Echo")
                .source(RouteEndpoint::component("a"))
                .targets(vec![RouteEndpoint::component("b")]),
        )?;
        // [END route_from_a_to_b_example]
    }
    {
        let mut builder = RealmBuilder::new().await?;
        builder
            .add_component("a", ComponentSource::url(V2_ECHO_CLIENT_ABSOLUTE_URL))
            .await?
            .add_component("b", ComponentSource::url(V2_ECHO_CLIENT_ABSOLUTE_URL))
            .await?;
        // [START route_logsink_example]
        // Routes `fuchsia.logger.LogSink` from above root to `a` and `b`
        builder.add_route(
            RouteBuilder::protocol("fuchsia.logger.LogSink")
                .source(RouteEndpoint::above_root())
                .targets(vec![RouteEndpoint::component("a"), RouteEndpoint::component("b")]),
        )?;
        // [END route_logsink_example]
    }
    {
        let mut builder = RealmBuilder::new().await?;
        builder.add_component("b", ComponentSource::url(V2_ECHO_CLIENT_ABSOLUTE_URL)).await?;
        // [START route_to_above_root_example]
        // Adds a route for the protocol capability
        // `fidl.examples.routing.echo.EchoClientStats` from `b` to the realm's parent
        builder.add_route(
            RouteBuilder::protocol("fidl.examples.routing.echo.EchoClientStats")
                .source(RouteEndpoint::component("b"))
                .targets(vec![RouteEndpoint::above_root()]),
        )?;

        let realm = builder.build();
        // [START create_realm]
        // Creates the realm, and add it to the collection to start its execution
        let realm_instance = realm.create().await?;
        // [END create_realm]

        // [START connect_to_protocol]
        // Connects to `fidl.examples.routing.echo.EchoClientStats`, which is provided
        // by `b` in the created realm
        let echo_client_stats_proxy =
            realm_instance.root.connect_to_protocol_at_exposed_dir::<EchoClientStatsMarker>()?;
        // [END connect_to_protocol]
        // [END route_to_above_root_example]
        drop(echo_client_stats_proxy);
    }
    #[allow(unused_mut)]
    {
        let mut builder = RealmBuilder::new().await?;
        builder.add_component("a/b", ComponentSource::url(V2_ECHO_CLIENT_ABSOLUTE_URL)).await?;

        // [START mutate_generated_manifest_example]
        let mut realm = builder.build();
        let mut root_manifest = realm.get_decl(&Moniker::root()).await?;
        // root_manifest is mutated in whatever way is needed
        realm.set_component(&Moniker::root(), root_manifest).await?;

        let mut a_manifest = realm.get_decl(&"a".into()).await?;
        // a_manifest is mutated in whatever way is needed
        realm.set_component(&"a".into(), a_manifest).await?;
        // [END mutate_generated_manifest_example]
    }
    Ok(())
}

#[fasync::run_singlethreaded(test)]
async fn protocol_with_cousins_test() -> Result<(), Error> {
    let (send_echo_server_called, mut receive_echo_server_called) = mpsc::channel(1);

    let mut builder = RealmBuilder::new().await?;
    builder
        .add_eager_component(
            "parent-1/echo-client",
            ComponentSource::url(V2_ECHO_CLIENT_ABSOLUTE_URL),
        )
        .await?
        .add_component(
            "parent-2/echo-server",
            ComponentSource::mock(move |mock_handles: mock::MockHandles| {
                Box::pin(echo_server_mock(
                    DEFAULT_ECHO_STR,
                    send_echo_server_called.clone(),
                    mock_handles,
                ))
            }),
        )
        .await?
        .add_route(
            RouteBuilder::protocol("fidl.examples.routing.echo.Echo")
                .source(RouteEndpoint::component("parent-2/echo-server"))
                .targets(vec![RouteEndpoint::component("parent-1/echo-client")]),
        )?
        .add_route(
            RouteBuilder::protocol("fuchsia.logger.LogSink")
                .source(RouteEndpoint::above_root())
                .targets(vec![
                    RouteEndpoint::component("parent-1/echo-client"),
                    RouteEndpoint::component("parent-2/echo-server"),
                ]),
        )?;
    let _instance = builder.build().create().await?;

    assert!(receive_echo_server_called.next().await.is_some());
    Ok(())
}

#[fasync::run_singlethreaded(test)]
async fn mock_component_with_a_child() -> Result<(), Error> {
    let (send_echo_server_called, mut receive_echo_server_called) = mpsc::channel(1);

    let mut builder = RealmBuilder::new().await?;
    builder
        .add_component(
            "echo-server",
            ComponentSource::mock(move |mock_handles: mock::MockHandles| {
                Box::pin(echo_server_mock(
                    DEFAULT_ECHO_STR,
                    send_echo_server_called.clone(),
                    mock_handles,
                ))
            }),
        )
        .await?
        .add_eager_component(
            "echo-server/echo-client",
            ComponentSource::url(V2_ECHO_CLIENT_ABSOLUTE_URL),
        )
        .await?
        .add_route(
            RouteBuilder::protocol("fidl.examples.routing.echo.Echo")
                .source(RouteEndpoint::component("echo-server"))
                .targets(vec![RouteEndpoint::component("echo-server/echo-client")]),
        )?
        .add_route(
            RouteBuilder::protocol("fuchsia.logger.LogSink")
                .source(RouteEndpoint::above_root())
                .targets(vec![
                    RouteEndpoint::component("echo-server"),
                    RouteEndpoint::component("echo-server/echo-client"),
                ]),
        )?;
    let _instance = builder.build().create().await?;

    assert!(receive_echo_server_called.next().await.is_some());
    Ok(())
}

// This test confirms that dynamic components in the built realm can use URLs that are relative to
// the test package (this is a special case the realm builder resolver needs to handle).
#[fasync::run_singlethreaded(test)]
async fn mock_component_with_a_relative_dynamic_child() -> Result<(), Error> {
    let (send_echo_client_results, mut receive_echo_client_results) = mpsc::channel(1);

    let collection_name = "dynamic-children".to_string();
    let collection_name_for_mock = collection_name.clone();

    let mut builder = RealmBuilder::new().await?;
    builder
        .add_eager_component(
            "echo-client",
            ComponentSource::mock(move |mock_handles: mock::MockHandles| {
                let collection_name_for_mock = collection_name_for_mock.clone();
                let mut send_echo_client_results = send_echo_client_results.clone();
                Box::pin(async move {
                    let realm_proxy =
                        mock_handles.connect_to_service::<fcomponent::RealmMarker>()?;
                    realm_proxy
                        .create_child(
                            &mut fcdecl::CollectionRef { name: collection_name_for_mock.clone() },
                            fcdecl::Child {
                                name: Some("echo-server".to_string()),
                                url: Some(V2_ECHO_SERVER_RELATIVE_URL.to_string()),
                                startup: Some(fcdecl::StartupMode::Lazy),
                                environment: None,
                                on_terminate: None,
                                ..fcdecl::Child::EMPTY
                            },
                            fcomponent::CreateChildArgs::EMPTY,
                        )
                        .await?
                        .expect("failed to create child");
                    let (exposed_dir_proxy, exposed_dir_server_end) =
                        fidl::endpoints::create_proxy()?;
                    realm_proxy
                        .open_exposed_dir(
                            &mut fcdecl::ChildRef {
                                name: "echo-server".to_string(),
                                collection: Some(collection_name_for_mock.clone()),
                            },
                            exposed_dir_server_end,
                        )
                        .await?
                        .expect("failed to open exposed dir");
                    let echo_proxy = fuchsia_component::client::connect_to_protocol_at_dir_root::<
                        fecho::EchoMarker,
                    >(&exposed_dir_proxy)?;
                    let out = echo_proxy.echo_string(Some(DEFAULT_ECHO_STR)).await?;
                    if Some(DEFAULT_ECHO_STR.to_string()) != out {
                        return Err(format_err!("unexpected echo result: {:?}", out));
                    }
                    send_echo_client_results.send(()).await.expect("failed to send results");
                    Ok(())
                })
            }),
        )
        .await?;
    let mut realm = builder.build();
    let mut echo_client_decl = realm.get_decl(&"echo-client".into()).await?;
    echo_client_decl.collections.push(cm_rust::CollectionDecl {
        name: collection_name.clone(),
        durability: fsys::Durability::Transient,
        allowed_offers: cm_types::AllowedOffers::StaticOnly,
        environment: None,
    });
    echo_client_decl.capabilities.push(cm_rust::CapabilityDecl::Protocol(cm_rust::ProtocolDecl {
        name: "fidl.examples.routing.echo.Echo".into(),
        source_path: Some("/svc/fidl.examples.routing.echo.Echo".try_into().unwrap()),
    }));
    echo_client_decl.offers.push(cm_rust::OfferDecl::Protocol(cm_rust::OfferProtocolDecl {
        source: cm_rust::OfferSource::Self_,
        source_name: "fidl.examples.routing.echo.Echo".into(),
        target: cm_rust::OfferTarget::Collection(collection_name.clone()),
        target_name: "fidl.examples.routing.echo.Echo".into(),
        dependency_type: cm_rust::DependencyType::Strong,
    }));
    echo_client_decl.uses.push(cm_rust::UseDecl::Protocol(cm_rust::UseProtocolDecl {
        source: cm_rust::UseSource::Framework,
        source_name: "fuchsia.component.Realm".into(),
        target_path: "/svc/fuchsia.component.Realm".try_into().unwrap(),
        dependency_type: cm_rust::DependencyType::Strong,
    }));
    realm.set_component(&"echo-client".into(), echo_client_decl).await?;

    let _instance = realm.create().await?;

    assert!(receive_echo_client_results.next().await.is_some());
    Ok(())
}

#[fasync::run_singlethreaded(test)]
async fn relative_echo_realm() -> Result<(), Error> {
    let mut builder = RealmBuilder::new().await?;
    builder.add_component(Moniker::root(), ComponentSource::url("#meta/echo_realm.cm")).await?;
    let mut realm = builder.build();
    // This route will result in the imported echo_realm exposing this protocol, whereas before
    // it only offered it to echo_client
    realm
        .add_route(
            RouteBuilder::protocol("fidl.examples.routing.echo.Echo")
                .source(RouteEndpoint::component("echo_server"))
                .targets(vec![RouteEndpoint::above_root()])
                .force(),
        )
        .await?;
    let realm_instance = realm.create().await?;

    let echo_proxy =
        realm_instance.root.connect_to_protocol_at_exposed_dir::<fecho::EchoMarker>()?;
    assert_eq!(Some("hello".to_string()), echo_proxy.echo_string(Some("hello")).await?);

    Ok(())
}

#[fasync::run_singlethreaded(test)]
async fn altered_echo_client_args() -> Result<(), Error> {
    let (send_echo_server_called, mut receive_echo_server_called) = mpsc::channel(1);

    let mut builder = RealmBuilder::new().await?;
    builder
        .add_component(Moniker::root(), ComponentSource::url("#meta/echo_realm.cm"))
        .await?
        .override_component(
            "echo_server",
            ComponentSource::mock(move |mock_handles: mock::MockHandles| {
                Box::pin(echo_server_mock(
                    "Whales rule!",
                    send_echo_server_called.clone(),
                    mock_handles,
                ))
            }),
        )
        .await?;
    let mut realm = builder.build();
    // echo_realm already has the offer we need, but we still need to add this route so that
    // the proper exposes are added to our mock component
    realm
        .add_route(
            RouteBuilder::protocol("fidl.examples.routing.echo.Echo")
                .source(RouteEndpoint::component("echo_server"))
                .targets(vec![RouteEndpoint::component("echo_client")])
                .force(),
        )
        .await?;
    // Mark echo_client as eager so it starts automatically when we bind to
    // the realm component.
    realm.mark_as_eager(&"echo_client".into()).await?;

    // Change the program.args section of the manifest, to alter the string it will try to echo
    let mut echo_client_decl = realm.get_decl(&"echo_client".into()).await?;
    for entry in echo_client_decl.program.as_mut().unwrap().info.entries.as_mut().unwrap() {
        if entry.key.as_str() == "args" {
            entry.value =
                Some(Box::new(fdata::DictionaryValue::StrVec(vec!["Whales rule!".to_string()])));
        }
    }
    realm.set_component(&"echo_client".into(), echo_client_decl).await?;
    let _instance = realm.create().await?;

    assert!(receive_echo_server_called.next().await.is_some());

    Ok(())
}

#[fasync::run_singlethreaded(test)]
async fn echo_clients() -> Result<(), Error> {
    // This test runs a series of echo clients from different sources against a mock echo server,
    // confirming that each client successfully connects to the server.

    let (send_echo_client_results, mut receive_echo_client_results) = mpsc::channel(1);
    let client_sources = vec![
        ComponentSource::legacy_url(V1_ECHO_CLIENT_URL),
        ComponentSource::url(V2_ECHO_CLIENT_ABSOLUTE_URL),
        ComponentSource::url(V2_ECHO_CLIENT_RELATIVE_URL),
        ComponentSource::mock(move |h| {
            Box::pin(echo_client_mock(send_echo_client_results.clone(), h))
        }),
    ];

    let mut client_counter = 0;
    for client_source in client_sources {
        let (send_echo_server_called, mut receive_echo_server_called) = mpsc::channel(1);

        let mut builder = RealmBuilder::new().await?;
        builder
            .add_component(
                "echo-server",
                ComponentSource::mock(move |h| {
                    Box::pin(echo_server_mock(DEFAULT_ECHO_STR, send_echo_server_called.clone(), h))
                }),
            )
            .await?
            .add_eager_component("echo-client", client_source)
            .await?
            .add_route(
                RouteBuilder::protocol("fidl.examples.routing.echo.Echo")
                    .source(RouteEndpoint::component("echo-server"))
                    .targets(vec![RouteEndpoint::component("echo-client")]),
            )?
            .add_route(
                RouteBuilder::protocol("fuchsia.logger.LogSink")
                    .source(RouteEndpoint::above_root())
                    .targets(vec![
                        RouteEndpoint::component("echo-server"),
                        RouteEndpoint::component("echo-client"),
                    ]),
            )?;

        let realm_instance = builder.build().create().await?;

        assert!(
            receive_echo_server_called.next().await.is_some(),
            "failed to observe the mock server report a successful connection from client #{}",
            client_counter
        );
        client_counter += 1;

        realm_instance.destroy().await?;
    }

    assert!(
        receive_echo_client_results.next().await.is_some(),
        "failed to observe the mock client report success"
    );
    Ok(())
}

#[fasync::run_singlethreaded(test)]
async fn echo_clients_in_nested_component_manager() -> Result<(), Error> {
    // This test is identical to the one preceding it, but component are launched in a nested
    // component manager.

    let (send_echo_client_results, mut receive_echo_client_results) = mpsc::channel(1);
    let client_sources = vec![
        ComponentSource::url(V2_ECHO_CLIENT_ABSOLUTE_URL),
        ComponentSource::url(V2_ECHO_CLIENT_RELATIVE_URL),
        ComponentSource::legacy_url(V1_ECHO_CLIENT_URL),
        ComponentSource::mock(move |h| {
            Box::pin(echo_client_mock(send_echo_client_results.clone(), h))
        }),
    ];

    let mut client_counter = 0;
    for client_source in client_sources {
        let (send_echo_server_called, mut receive_echo_server_called) = mpsc::channel(1);

        let mut builder = RealmBuilder::new().await?;
        builder
            .add_eager_component(
                "echo-server",
                ComponentSource::mock(move |h| {
                    Box::pin(echo_server_mock(DEFAULT_ECHO_STR, send_echo_server_called.clone(), h))
                }),
            )
            .await?
            .add_eager_component("echo-client", client_source)
            .await?
            .add_route(
                RouteBuilder::protocol("fidl.examples.routing.echo.Echo")
                    .source(RouteEndpoint::component("echo-server"))
                    .targets(vec![RouteEndpoint::component("echo-client")]),
            )?
            .add_route(
                RouteBuilder::protocol("fuchsia.logger.LogSink")
                    .source(RouteEndpoint::above_root())
                    .targets(vec![
                        RouteEndpoint::component("echo-server"),
                        RouteEndpoint::component("echo-client"),
                    ]),
            )?;

        let realm_instance = builder
            .build()
            .create_in_nested_component_manager("#meta/component_manager.cm")
            .await?;

        assert!(
            receive_echo_server_called.next().await.is_some(),
            "failed to observe the mock server report a successful connection from client #{}",
            client_counter
        );
        client_counter += 1;

        realm_instance.destroy().await?;
    }
    assert!(
        receive_echo_client_results.next().await.is_some(),
        "failed to observe the mock client report success"
    );
    Ok(())
}

#[fasync::run_singlethreaded(test)]
async fn echo_servers() -> Result<(), Error> {
    // This test runs a series of echo servers from different sources against a mock echo client,
    // confirming that the client can successfully connect to and use each server.

    let (send_echo_server_called, mut receive_echo_server_called) = mpsc::channel(1);

    let server_sources = vec![
        ComponentSource::legacy_url(V1_ECHO_SERVER_URL),
        ComponentSource::url(V2_ECHO_SERVER_ABSOLUTE_URL),
        ComponentSource::url(V2_ECHO_SERVER_RELATIVE_URL),
        ComponentSource::mock(move |h| {
            Box::pin(echo_server_mock(DEFAULT_ECHO_STR, send_echo_server_called.clone(), h))
        }),
    ];

    let mut server_counter = 0;
    for server_source in server_sources {
        let (send_echo_client_results, mut receive_echo_client_results) = mpsc::channel(1);

        let mut builder = RealmBuilder::new().await?;
        builder
            .add_component("echo-server", server_source)
            .await?
            .add_eager_component(
                "echo-client",
                ComponentSource::mock(move |h| {
                    Box::pin(echo_client_mock(send_echo_client_results.clone(), h))
                }),
            )
            .await?
            .add_route(
                RouteBuilder::protocol("fidl.examples.routing.echo.Echo")
                    .source(RouteEndpoint::component("echo-server"))
                    .targets(vec![RouteEndpoint::component("echo-client")]),
            )?
            .add_route(
                RouteBuilder::protocol("fuchsia.logger.LogSink")
                    .source(RouteEndpoint::above_root())
                    .targets(vec![
                        RouteEndpoint::component("echo-server"),
                        RouteEndpoint::component("echo-client"),
                    ]),
            )?;

        let realm_instance = builder.build().create().await?;

        assert!(
            receive_echo_client_results.next().await.is_some(),
            "failed to observe the mock client report success for server #{}",
            server_counter
        );
        server_counter += 1;

        realm_instance.destroy().await?;
    }

    assert!(
        receive_echo_server_called.next().await.is_some(),
        "failed to observe the mock server report a successful connection from a client"
    );
    Ok(())
}

#[fasync::run_singlethreaded(test)]
async fn protocol_with_use_from_url_child_test() -> Result<(), Error> {
    let (send_echo_server_called, mut receive_echo_server_called) = mpsc::channel(1);

    let mut builder = RealmBuilder::new().await?;
    builder
        .add_eager_component(
            "echo-client",
            ComponentSource::mock(move |mock_handles: mock::MockHandles| {
                let mut send_echo_server_called = send_echo_server_called.clone();
                Box::pin(async move {
                    let echo_proxy = mock_handles.connect_to_service::<fecho::EchoMarker>()?;
                    assert_eq!(
                        Some("hello".to_string()),
                        echo_proxy.echo_string(Some("hello")).await?
                    );
                    send_echo_server_called.send(()).await.expect("failed to send results");
                    Ok(())
                })
            }),
        )
        .await?
        .add_component("echo-client/echo-server", ComponentSource::url(V2_ECHO_SERVER_ABSOLUTE_URL))
        .await?
        .add_route(
            RouteBuilder::protocol("fidl.examples.routing.echo.Echo")
                .source(RouteEndpoint::component("echo-client/echo-server"))
                .targets(vec![RouteEndpoint::component("echo-client")]),
        )?
        .add_route(
            RouteBuilder::protocol("fuchsia.logger.LogSink")
                .source(RouteEndpoint::AboveRoot)
                .targets(vec![
                    RouteEndpoint::component("echo-client"),
                    RouteEndpoint::component("echo-client/echo-server"),
                ]),
        )?;
    let _instance = builder.build().create().await?;

    assert!(receive_echo_server_called.next().await.is_some());
    Ok(())
}

#[fasync::run_singlethreaded(test)]
async fn protocol_with_use_from_mock_child_test() -> Result<(), Error> {
    let (send_echo_server_called, mut receive_echo_server_called) = mpsc::channel(1);

    let mut builder = RealmBuilder::new().await?;
    builder
        .add_eager_component(
            "echo-client",
            ComponentSource::mock(move |mock_handles: mock::MockHandles| {
                Box::pin(async move {
                    let echo_proxy = mock_handles.connect_to_service::<fecho::EchoMarker>()?;
                    let _ = echo_proxy.echo_string(Some(DEFAULT_ECHO_STR)).await?;
                    Ok(())
                })
            }),
        )
        .await?
        .add_component(
            "echo-client/echo-server",
            ComponentSource::mock(move |mock_handles: mock::MockHandles| {
                Box::pin(echo_server_mock(
                    DEFAULT_ECHO_STR,
                    send_echo_server_called.clone(),
                    mock_handles,
                ))
            }),
        )
        .await?
        .add_route(
            RouteBuilder::protocol("fidl.examples.routing.echo.Echo")
                .source(RouteEndpoint::component("echo-client/echo-server"))
                .targets(vec![RouteEndpoint::component("echo-client")]),
        )?
        .add_route(
            RouteBuilder::protocol("fuchsia.logger.LogSink")
                .source(RouteEndpoint::AboveRoot)
                .targets(vec![
                    RouteEndpoint::component("echo-client"),
                    RouteEndpoint::component("echo-client/echo-server"),
                ]),
        )?;
    let _instance = builder.build().create().await?;

    assert!(receive_echo_server_called.next().await.is_some());
    Ok(())
}

#[fasync::run_singlethreaded(test)]
async fn force_route_echo_server() -> Result<(), Error> {
    // This test connects a package-local echo server to a mock, but the echo server's manifest is
    // missing the manifest parts to declare and expose the echo protocol. They're added by setting
    // the `force_route` flag.

    let (send_echo_client_results, mut receive_echo_client_results) = mpsc::channel(1);

    let mut builder = RealmBuilder::new().await?;
    builder
        .add_component("echo-server", ComponentSource::url("#meta/echo_server_empty.cm"))
        .await?
        .add_eager_component(
            "echo-client",
            ComponentSource::mock(move |h| {
                Box::pin(echo_client_mock(send_echo_client_results.clone(), h))
            }),
        )
        .await?;
    let mut realm = builder.build();
    realm
        .add_route(
            RouteBuilder::protocol("fidl.examples.routing.echo.Echo")
                .source(RouteEndpoint::component("echo-server"))
                .targets(vec![RouteEndpoint::component("echo-client")])
                .force(),
        )
        .await?;
    realm
        .add_route(
            RouteBuilder::protocol("fuchsia.logger.LogSink")
                .source(RouteEndpoint::above_root())
                .targets(vec![
                    RouteEndpoint::component("echo-server"),
                    RouteEndpoint::component("echo-client"),
                ])
                .force(),
        )
        .await?;

    let _instance = realm.create().await?;

    assert!(receive_echo_client_results.next().await.is_some());

    Ok(())
}

#[fasync::run_singlethreaded(test)]
async fn force_route_echo_client() -> Result<(), Error> {
    // This test connects a package-local echo client to a mock, but the echo client's manifest is
    // missing the manifest part to use the echo protocol. It's added by setting the `force_route`
    // flag.

    let (send_echo_server_called, mut receive_echo_server_called) = mpsc::channel(1);

    let mut builder = RealmBuilder::new().await?;
    builder
        .add_eager_component("echo-client", ComponentSource::url("#meta/echo_client_empty.cm"))
        .await?
        .add_component(
            "echo-server",
            ComponentSource::mock(move |mock_handles: mock::MockHandles| {
                Box::pin(echo_server_mock(
                    DEFAULT_ECHO_STR,
                    send_echo_server_called.clone(),
                    mock_handles,
                ))
            }),
        )
        .await?;
    let mut realm = builder.build();
    realm
        .add_route(
            RouteBuilder::protocol("fidl.examples.routing.echo.Echo")
                .source(RouteEndpoint::component("echo-server"))
                .targets(vec![RouteEndpoint::component("echo-client")])
                .force(),
        )
        .await?;
    realm
        .add_route(
            RouteBuilder::protocol("fuchsia.logger.LogSink")
                .source(RouteEndpoint::above_root())
                .targets(vec![
                    RouteEndpoint::component("echo-server"),
                    RouteEndpoint::component("echo-client"),
                ])
                .force(),
        )
        .await?;

    let realm_instance = realm.create().await?;

    assert!(receive_echo_server_called.next().await.is_some());

    realm_instance.destroy().await?;
    Ok(())
}

#[fasync::run_singlethreaded(test)]
async fn no_force_doesnt_modify_package_local() -> Result<(), Error> {
    // This test confirms that package-local components are not modified when the `force_route`
    // flag is set to `false`.

    let (send_echo_client_failed, mut receive_echo_client_failed) = mpsc::channel(1);

    let mut builder = RealmBuilder::new().await?;
    builder
        .add_component("echo-server", ComponentSource::url("#meta/echo_server_empty.cm"))
        .await?
        .add_eager_component(
            "echo-client",
            ComponentSource::mock(move |mock_handles| {
                let mut send_echo_client_failed = send_echo_client_failed.clone();
                Box::pin(async move {
                    let echo = mock_handles.connect_to_service::<fecho::EchoMarker>()?;
                    let res = echo.echo_string(Some("this should fail")).await;
                    match res {
                        Err(fidl::Error::ClientChannelClosed {
                            status: fidl::handle::fuchsia_handles::Status::UNAVAILABLE,
                            protocol_name: fecho::EchoMarker::NAME,
                        }) => (),
                        res => panic!("unexpected result from echo_string: {:?}", res),
                    }
                    send_echo_client_failed.send(()).await.expect("failed to send results");
                    Ok(())
                })
            }),
        )
        .await?;
    let mut realm = builder.build();
    realm
        .add_route(
            RouteBuilder::protocol("fidl.examples.routing.echo.Echo")
                .source(RouteEndpoint::component("echo-server"))
                .targets(vec![RouteEndpoint::component("echo-client")]),
        )
        .await?;
    realm
        .add_route(
            RouteBuilder::protocol("fuchsia.logger.LogSink")
                .source(RouteEndpoint::above_root())
                .targets(vec![
                    RouteEndpoint::component("echo-server"),
                    RouteEndpoint::component("echo-client"),
                ])
                .force(),
        )
        .await?;

    let _realm_instance = realm.create().await?;

    assert!(receive_echo_client_failed.next().await.is_some());

    Ok(())
}

// [START echo_server_mock]
// A mock echo server implementation, that will crash if it doesn't receive anything other than the
// contents of `expected_echo_str`. It takes and sends a message over `send_echo_server_called`
// once it receives one echo request.
async fn echo_server_mock(
    expected_echo_string: &'static str,
    send_echo_server_called: mpsc::Sender<()>,
    mock_handles: mock::MockHandles,
) -> Result<(), Error> {
    // Create a new ServiceFs to host FIDL protocols from
    let mut fs = fserver::ServiceFs::new();
    let mut tasks = vec![];

    // Add the echo protocol to the ServiceFs
    fs.dir("svc").add_fidl_service(move |mut stream: fecho::EchoRequestStream| {
        let mut send_echo_server_called = send_echo_server_called.clone();
        tasks.push(fasync::Task::local(async move {
            while let Some(fecho::EchoRequest::EchoString { value, responder }) =
                stream.try_next().await.expect("failed to serve echo service")
            {
                assert_eq!(Some(expected_echo_string.to_string()), value);
                // Send the received string back to the client
                responder.send(value.as_ref().map(|s| &**s)).expect("failed to send echo response");

                // Use send_echo_server_called to report back that we successfully received a
                // message and it aligned with our expectations
                send_echo_server_called.send(()).await.expect("failed to send results");
            }
        }));
    });

    // Run the ServiceFs on the outgoing directory handle from the mock handles
    fs.serve_connection(mock_handles.outgoing_dir.into_channel())?;
    fs.collect::<()>().await;
    Ok(())
}
// [END echo_server_mock]

async fn echo_client_mock(
    mut send_echo_client_results: mpsc::Sender<()>,
    mock_handles: mock::MockHandles,
) -> Result<(), Error> {
    let echo = mock_handles.connect_to_service::<fecho::EchoMarker>()?;
    let out = echo.echo_string(Some(DEFAULT_ECHO_STR)).await?;
    if Some(DEFAULT_ECHO_STR.to_string()) != out {
        return Err(format_err!("unexpected echo result: {:?}", out));
    }
    send_echo_client_results.send(()).await.expect("failed to send results");
    Ok(())
}
