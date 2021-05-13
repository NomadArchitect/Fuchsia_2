// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    diagnostics_data::{Data, Logs, Severity},
    diagnostics_reader::{ArchiveReader, SubscriptionResultsStream},
    fidl::endpoints::create_endpoints,
    fidl_fuchsia_io::DirectoryMarker,
    fidl_fuchsia_sys2 as fsys,
    fuchsia_async::{OnSignals, Task},
    fuchsia_zircon as zx,
    futures::StreamExt,
};

const BASE_PACKAGE_URL: &str = "fuchsia-pkg://fuchsia.com/elf_runner_stdout_test";
const COLLECTION_NAME: &str = "puppets";
const EXPECTED_STDOUT_PAYLOAD: &str = "Hello Stdout!";
const EXPECTED_STDERR_PAYLOAD: &str = "Hello Stderr!";
const TESTED_LANGUAGUES: [&str; 3] = ["cpp", "rust", "go"];

#[derive(Debug)]
struct Component {
    url: String,
    moniker: String,
}

#[derive(Clone, Debug)]
struct MessageAssertion {
    payload: &'static str,
    severity: Severity,
}

// TODO(fxbug.dev/69684): Refactor this to receive puppet components
// through argv once ArchiveAccesor is exposed from Test Runner.
#[fuchsia::test]
async fn test_components_logs_to_stdout() {
    let realm = fuchsia_component::client::realm().unwrap();

    let mut subscription = launch_embedded_archivist().await;

    // Doing this in loop as opposed to separate test cases ensures linear
    // execution for Archivist's log streams.
    for (component, message_assertions) in get_all_test_components().iter() {
        start_child_component_and_wait_for_exit(&realm, &component).await;

        // Golang prints messages to stdout and stderr when it finds it's missing any of the stdio
        // handles. Ignore messages that come from the runtime so we can match on our expectations.
        // TODO(fxbug.dev/69588): Remove this workaround.
        let is_go = component.moniker.ends_with("go");

        let messages = subscription
            .by_ref()
            .filter(|log| {
                futures::future::ready(
                    !(is_go
                        && log
                            .msg()
                            .map(|s| s.to_string())
                            .map(|s| s.starts_with("runtime") || s.starts_with("syscall"))
                            .unwrap_or(false)),
                )
            })
            .take(message_assertions.len())
            .collect::<Vec<_>>()
            .await;
        assert_all_have_attribution(&messages, &component);

        for message_assertion in message_assertions.iter() {
            assert_any_has_content(
                &messages,
                message_assertion.payload,
                message_assertion.severity,
            );
        }
    }
}

fn get_all_test_components() -> Vec<(Component, Vec<MessageAssertion>)> {
    let stdout_assertion =
        MessageAssertion { payload: EXPECTED_STDOUT_PAYLOAD, severity: Severity::Info };

    let stderr_assertion =
        MessageAssertion { payload: EXPECTED_STDERR_PAYLOAD, severity: Severity::Warn };

    TESTED_LANGUAGUES
        .iter()
        .flat_map(|language| {
            vec![
                (
                    Component {
                        url: format!(
                            "{}#meta/logs-stdout-and-stderr-{}.cm",
                            BASE_PACKAGE_URL, language
                        ),
                        moniker: format!("logs_stdout_and_stderr_{}", language),
                    },
                    vec![stdout_assertion.clone(), stderr_assertion.clone()],
                ),
                (
                    Component {
                        url: format!("{}#meta/logs-stdout-{}.cm", BASE_PACKAGE_URL, language),
                        moniker: format!("logs_stdout_{}", language),
                    },
                    vec![stdout_assertion.clone()],
                ),
                (
                    Component {
                        url: format!("{}#meta/logs-stderr-{}.cm", BASE_PACKAGE_URL, language),
                        moniker: format!("logs_stderr_{}", language),
                    },
                    vec![stderr_assertion.clone()],
                ),
            ]
        })
        .collect()
}

async fn launch_embedded_archivist() -> SubscriptionResultsStream<Logs> {
    let (subscription, mut errors) = ArchiveReader::new()
        .with_minimum_schema_count(0) // we want this to return even when no log messages
        .retry_if_empty(false)
        .snapshot_then_subscribe::<Logs>()
        .unwrap()
        .split_streams();

    let _ = Task::spawn(async move {
        if let Some(error) = errors.next().await {
            panic!("{:#?}", error);
        }
    });

    subscription
}

async fn start_child_component_and_wait_for_exit(realm: &fsys::RealmProxy, component: &Component) {
    let mut collection_ref = fsys::CollectionRef { name: COLLECTION_NAME.to_owned() };
    let child_decl = fsys::ChildDecl {
        name: Some(component.moniker.to_owned()),
        url: Some(component.url.to_owned()),
        startup: Some(fsys::StartupMode::Lazy),
        environment: None,
        ..fsys::ChildDecl::EMPTY
    };

    realm
        .create_child(&mut collection_ref, child_decl)
        .await
        .expect("Failed to make FIDL call")
        .expect("Failed to create child");

    let mut child_ref = fsys::ChildRef {
        name: component.moniker.to_owned(),
        collection: Some(COLLECTION_NAME.to_owned()),
    };

    let (client_end, server_end) = create_endpoints::<DirectoryMarker>().unwrap();
    realm
        .bind_child(&mut child_ref, server_end)
        .await
        .expect("failed to make FIDL call")
        .expect("failed to bind child");
    OnSignals::new(&client_end, zx::Signals::CHANNEL_PEER_CLOSED).await.unwrap();
}

#[track_caller]
fn assert_all_have_attribution(messages: &[Data<Logs>], component: &Component) {
    let check_attribution = |msg: &Data<Logs>| {
        msg.moniker == format!("{}:{}", COLLECTION_NAME, component.moniker)
            && msg.metadata.component_url == component.url
    };

    assert!(
        messages.iter().all(check_attribution),
        "Messages found without attribution of moniker='{}' and url='{}' in log: {:?}",
        component.moniker,
        component.url,
        messages
    );
}

#[track_caller]
fn assert_any_has_content(messages: &[Data<Logs>], payload: &str, severity: Severity) {
    let check_content = |msg: &Data<Logs>| {
        msg.metadata.severity == severity
            && msg.payload_message().unwrap().get_property("value").unwrap().string().unwrap()
                == payload
    };

    assert!(
        messages.iter().any(check_content),
        "Message with payload='{}' and severity={} not found in logs: {:?}",
        payload,
        severity,
        messages
    );
}
