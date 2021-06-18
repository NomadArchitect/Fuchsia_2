// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::{format_err, Error},
    fidl_fidl_examples_routing_echo::{self as fecho, EchoMarker as EchoClientStatsMarker},
    fidl_fuchsia_data as fdata, fuchsia_async as fasync,
    fuchsia_component::server as fserver,
    fuchsia_component_test::{builder::*, mock, Moniker},
    futures::{channel::oneshot, lock::Mutex, StreamExt, TryStreamExt},
    std::sync::Arc,
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

const DEFAULT_ECHO_STR: &'static str = "Hippos rule!";

#[fasync::run_singlethreaded(test)]
async fn protocol_with_uncle_test() -> Result<(), Error> {
    let (send_echo_server_called, receive_echo_server_called) = oneshot::channel();
    let sender = Arc::new(Mutex::new(Some(send_echo_server_called)));

    let mut builder = RealmBuilder::new().await?;
    builder
        .add_component(
            "echo-server",
            ComponentSource::mock(move |mock_handles: mock::MockHandles| {
                Box::pin(echo_server_mock(DEFAULT_ECHO_STR, sender.clone(), mock_handles))
            }),
        )
        .await?
        .add_eager_component(
            "parent/echo-client",
            ComponentSource::url(V2_ECHO_CLIENT_ABSOLUTE_URL),
        )
        .await?
        .add_route(CapabilityRoute {
            capability: Capability::protocol("fidl.examples.routing.echo.Echo"),
            source: RouteEndpoint::component("echo-server"),
            targets: vec![RouteEndpoint::component("parent/echo-client")],
        })?
        .add_route(CapabilityRoute {
            capability: Capability::protocol("fuchsia.logger.LogSink"),
            source: RouteEndpoint::above_root(),
            targets: vec![
                RouteEndpoint::component("echo-server"),
                RouteEndpoint::component("parent/echo-client"),
            ],
        })?;
    let _child_instance = builder.build().create().await?;

    receive_echo_server_called.await?;
    Ok(())
}

#[fasync::run_singlethreaded(test)]
async fn protocol_with_siblings_test() -> Result<(), Error> {
    // [START mock_component_example]
    // Create a new oneshot for passing a message from the echo server function
    let (send_echo_server_called, receive_echo_server_called) = oneshot::channel();

    // Wrap the sender in an Arc, Mutex, and Option so that it can safely be sent
    // across threads, only interacted with by one thread at a time, and removed
    // from the mutex to be consumed.
    let send_echo_server_called = Arc::new(Mutex::new(Some(send_echo_server_called)));

    // Build a new realm
    let mut builder = RealmBuilder::new().await?;
    builder
        // Add the echo server, which is implemented by the echo_server_mock
        // function (defined below). Give this function access to the oneshot
        // created above, along with the mock component's handles
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
        .add_route(CapabilityRoute {
            capability: Capability::protocol("fidl.examples.routing.echo.Echo"),
            source: RouteEndpoint::component("a"),
            targets: vec![RouteEndpoint::component("b")],
        })?
        // Route the logsink to `b`, so it can inform us of any issues
        .add_route(CapabilityRoute {
            capability: Capability::protocol("fuchsia.logger.LogSink"),
            source: RouteEndpoint::above_root(),
            targets: vec![RouteEndpoint::component("b")],
        })?;

    // Create the realm
    let _child_instance = builder.build().create().await?;

    // Wait for the oneshot we created above to receive a message
    receive_echo_server_called.await?;
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
        builder.add_route(CapabilityRoute {
            capability: Capability::protocol("fidl.examples.routing.echo.Echo"),
            source: RouteEndpoint::component("a"),
            targets: vec![RouteEndpoint::component("b")],
        })?;
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
        builder.add_route(CapabilityRoute {
            capability: Capability::protocol("fuchsia.logger.LogSink"),
            source: RouteEndpoint::above_root(),
            targets: vec![RouteEndpoint::component("a"), RouteEndpoint::component("b")],
        })?;
        // [END route_logsink_example]
    }
    {
        let mut builder = RealmBuilder::new().await?;
        builder.add_component("b", ComponentSource::url(V2_ECHO_CLIENT_ABSOLUTE_URL)).await?;
        // [START route_to_above_root_example]
        // Adds a route for the protocol capability
        // `fidl.examples.routing.echo.EchoClientStats` from `b` to the realm's parent
        builder.add_route(CapabilityRoute {
            capability: Capability::protocol("fidl.examples.routing.echo.EchoClientStats"),
            source: RouteEndpoint::component("b"),
            targets: vec![RouteEndpoint::above_root()],
        })?;

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
    let (send_echo_server_called, receive_echo_server_called) = oneshot::channel();
    let sender = Arc::new(Mutex::new(Some(send_echo_server_called)));

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
                Box::pin(echo_server_mock(DEFAULT_ECHO_STR, sender.clone(), mock_handles))
            }),
        )
        .await?
        .add_route(CapabilityRoute {
            capability: Capability::protocol("fidl.examples.routing.echo.Echo"),
            source: RouteEndpoint::component("parent-2/echo-server"),
            targets: vec![RouteEndpoint::component("parent-1/echo-client")],
        })?
        .add_route(CapabilityRoute {
            capability: Capability::protocol("fuchsia.logger.LogSink"),
            source: RouteEndpoint::above_root(),
            targets: vec![
                RouteEndpoint::component("parent-1/echo-client"),
                RouteEndpoint::component("parent-2/echo-server"),
            ],
        })?;
    let _child_instance = builder.build().create().await?;

    receive_echo_server_called.await?;
    Ok(())
}

#[fasync::run_singlethreaded(test)]
async fn mock_component_with_a_child() -> Result<(), Error> {
    let (send_echo_server_called, receive_echo_server_called) = oneshot::channel();
    let sender = Arc::new(Mutex::new(Some(send_echo_server_called)));

    let mut builder = RealmBuilder::new().await?;
    builder
        .add_component(
            "echo-server",
            ComponentSource::mock(move |mock_handles: mock::MockHandles| {
                Box::pin(echo_server_mock(DEFAULT_ECHO_STR, sender.clone(), mock_handles))
            }),
        )
        .await?
        .add_eager_component(
            "echo-server/echo-client",
            ComponentSource::url(V2_ECHO_CLIENT_ABSOLUTE_URL),
        )
        .await?
        .add_route(CapabilityRoute {
            capability: Capability::protocol("fidl.examples.routing.echo.Echo"),
            source: RouteEndpoint::component("echo-server"),
            targets: vec![RouteEndpoint::component("echo-server/echo-client")],
        })?
        .add_route(CapabilityRoute {
            capability: Capability::protocol("fuchsia.logger.LogSink"),
            source: RouteEndpoint::above_root(),
            targets: vec![
                RouteEndpoint::component("echo-server"),
                RouteEndpoint::component("echo-server/echo-client"),
            ],
        })?;
    let _child_instance = builder.build().create().await?;

    receive_echo_server_called.await?;
    Ok(())
}

#[fasync::run_singlethreaded(test)]
async fn relative_echo_realm() -> Result<(), Error> {
    let mut builder = RealmBuilder::new().await?;
    builder
        .add_component(Moniker::root(), ComponentSource::url("#meta/echo_realm.cm"))
        .await?
        // This route will result in the imported echo_realm exposing this protocol, whereas before
        // it only offered it to echo_client
        .add_route(CapabilityRoute {
            capability: Capability::protocol("fidl.examples.routing.echo.Echo"),
            source: RouteEndpoint::component("echo_server"),
            targets: vec![RouteEndpoint::above_root()],
        })?;
    let realm_instance = builder.build().create().await?;

    let echo_proxy =
        realm_instance.root.connect_to_protocol_at_exposed_dir::<fecho::EchoMarker>()?;
    assert_eq!(Some("hello".to_string()), echo_proxy.echo_string(Some("hello")).await?);

    Ok(())
}

#[fasync::run_singlethreaded(test)]
async fn altered_echo_client_args() -> Result<(), Error> {
    let (send_echo_server_called, receive_echo_server_called) = oneshot::channel();
    let sender = Arc::new(Mutex::new(Some(send_echo_server_called)));

    let mut builder = RealmBuilder::new().await?;
    builder
        .add_component(Moniker::root(), ComponentSource::url("#meta/echo_realm.cm"))
        .await?
        .override_component(
            "echo_server",
            ComponentSource::mock(move |mock_handles: mock::MockHandles| {
                Box::pin(echo_server_mock("Whales rule!", sender.clone(), mock_handles))
            }),
        )
        .await?
        // echo_realm already has the offer we need, but we still need to add this route so that
        // the proper exposes are added to our mock component
        .add_route(CapabilityRoute {
            capability: Capability::protocol("fidl.examples.routing.echo.Echo"),
            source: RouteEndpoint::component("echo_server"),
            targets: vec![RouteEndpoint::component("echo_client")],
        })?;

    // Change the program.args section of the manifest, to alter the string it will try to echo
    let mut realm = builder.build();
    let mut echo_client_decl = realm.get_decl(&"echo_client".into()).await?;
    for entry in echo_client_decl.program.as_mut().unwrap().info.entries.as_mut().unwrap() {
        if entry.key.as_str() == "args" {
            entry.value = Some(Box::new(fdata::DictionaryValue::StrVec(vec![
                "Whales".to_string(),
                "rule!".to_string(),
            ])));
        }
    }
    realm.set_component(&"echo_client".into(), echo_client_decl).await?;
    let _realm_instance = realm.create().await?;

    receive_echo_server_called.await?;

    Ok(())
}

#[fasync::run_singlethreaded(test)]
async fn echo_clients() -> Result<(), Error> {
    // This test runs a series of echo clients from different sources against a mock echo server,
    // confirming that each client successfully connects to the server.

    let (send_echo_client_results, receive_echo_client_results) = oneshot::channel();
    let sender = Arc::new(Mutex::new(Some(send_echo_client_results)));
    let client_sources = vec![
        ComponentSource::legacy_url(V1_ECHO_CLIENT_URL),
        ComponentSource::url(V2_ECHO_CLIENT_ABSOLUTE_URL),
        ComponentSource::url(V2_ECHO_CLIENT_RELATIVE_URL),
        ComponentSource::mock(move |h| Box::pin(echo_client_mock(sender.clone(), h))),
    ];

    for client_source in client_sources {
        let (send_echo_server_called, receive_echo_server_called) = oneshot::channel();
        let sender = Arc::new(Mutex::new(Some(send_echo_server_called)));

        let mut builder = RealmBuilder::new().await?;
        builder
            .add_component(
                "echo-server",
                ComponentSource::mock(move |h| {
                    Box::pin(echo_server_mock(DEFAULT_ECHO_STR, sender.clone(), h))
                }),
            )
            .await?
            .add_eager_component("echo-client", client_source)
            .await?
            .add_route(CapabilityRoute {
                capability: Capability::protocol("fidl.examples.routing.echo.Echo"),
                source: RouteEndpoint::component("echo-server"),
                targets: vec![RouteEndpoint::component("echo-client")],
            })?
            .add_route(CapabilityRoute {
                capability: Capability::protocol("fuchsia.logger.LogSink"),
                source: RouteEndpoint::above_root(),
                targets: vec![
                    RouteEndpoint::component("echo-server"),
                    RouteEndpoint::component("echo-client"),
                ],
            })?;

        let _child_instance = builder.build().create().await?;

        receive_echo_server_called.await?;
    }

    receive_echo_client_results.await?;
    Ok(())
}

#[fasync::run_singlethreaded(test)]
async fn echo_servers() -> Result<(), Error> {
    // This test runs a series of echo servers from different sources against a mock echo client,
    // confirming that the client can successfully connect to and use each server.

    let (send_echo_server_called, receive_echo_server_called) = oneshot::channel();
    let sender = Arc::new(Mutex::new(Some(send_echo_server_called)));

    let server_sources = vec![
        ComponentSource::legacy_url(V1_ECHO_SERVER_URL),
        ComponentSource::url(V2_ECHO_SERVER_ABSOLUTE_URL),
        ComponentSource::url(V2_ECHO_SERVER_RELATIVE_URL),
        ComponentSource::mock(move |h| {
            Box::pin(echo_server_mock(DEFAULT_ECHO_STR, sender.clone(), h))
        }),
    ];

    for server_source in server_sources {
        let (send_echo_client_results, receive_echo_client_results) = oneshot::channel();
        let sender = Arc::new(Mutex::new(Some(send_echo_client_results)));

        let mut builder = RealmBuilder::new().await?;
        builder
            .add_component("echo-server", server_source)
            .await?
            .add_eager_component(
                "echo-client",
                ComponentSource::mock(move |h| Box::pin(echo_client_mock(sender.clone(), h))),
            )
            .await?
            .add_route(CapabilityRoute {
                capability: Capability::protocol("fidl.examples.routing.echo.Echo"),
                source: RouteEndpoint::component("echo-server"),
                targets: vec![RouteEndpoint::component("echo-client")],
            })?
            .add_route(CapabilityRoute {
                capability: Capability::protocol("fuchsia.logger.LogSink"),
                source: RouteEndpoint::above_root(),
                targets: vec![
                    RouteEndpoint::component("echo-server"),
                    RouteEndpoint::component("echo-client"),
                ],
            })?;

        let _child_instance = builder.build().create().await?;

        receive_echo_client_results.await?;
    }

    receive_echo_server_called.await?;
    Ok(())
}

// [START echo_server_mock]
// A mock echo server implementation, that will crash if it doesn't receive anything other than the
// contents of `expected_echo_str`. It takes and sends a message over `send_echo_server_called`
// once it receives one echo request.
async fn echo_server_mock(
    expected_echo_string: &'static str,
    send_echo_server_called: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    mock_handles: mock::MockHandles,
) -> Result<(), Error> {
    // Create a new ServiceFs to host FIDL protocols from
    let mut fs = fserver::ServiceFs::new();
    let mut tasks = vec![];

    // Add the echo protocol to the ServiceFs
    fs.dir("svc").add_fidl_service(move |mut stream: fecho::EchoRequestStream| {
        let send_echo_server_called = send_echo_server_called.clone();
        tasks.push(fasync::Task::local(async move {
            while let Some(fecho::EchoRequest::EchoString { value, responder }) =
                stream.try_next().await.expect("failed to serve echo service")
            {
                assert_eq!(Some(expected_echo_string.to_string()), value);
                // Send the received string back to the client
                responder.send(value.as_ref().map(|s| &**s)).expect("failed to send echo response");

                // Take the sender from send_echo_server_called and pass a
                // message through it
                send_echo_server_called
                    .lock()
                    .await
                    .take()
                    .unwrap()
                    .send(())
                    .expect("failed to send results");
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
    send_echo_client_results: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    mock_handles: mock::MockHandles,
) -> Result<(), Error> {
    let echo = mock_handles.connect_to_service::<fecho::EchoMarker>()?;
    let out = echo.echo_string(Some(DEFAULT_ECHO_STR)).await?;
    send_echo_client_results.lock().await.take().unwrap().send(()).expect("failed to send results");
    if Some(DEFAULT_ECHO_STR.to_string()) != out {
        return Err(format_err!("unexpected echo result: {:?}", out));
    }
    Ok(())
}
