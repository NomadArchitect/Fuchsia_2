// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_ui_input as uii;
use fidl_fuchsia_ui_text as txt;
use fidl_fuchsia_ui_text_testing as txt_testing;
use fuchsia_component::client::{launch, launcher};
use futures::prelude::*;

/// Runs the `TextFieldTestSuite` integration tests.
#[fuchsia_async::run_singlethreaded(test)]
async fn test_external_text_field_implementation() {
    fuchsia_syslog::init_with_tags(&["ime_service"]).expect("ime syslog init should not fail");
    let launcher = launcher().expect("Failed to open launcher service");
    let app = launch(
        &launcher,
        "fuchsia-pkg://fuchsia.com/text_test_suite#meta/text_test_suite.cmx".to_string(),
        None,
    )
    .expect("Failed to launch testing service");
    let tester = app
        .connect_to_service::<txt_testing::TextFieldTestSuiteMarker>()
        .expect("Failed to connect to testing service");
    let mut passed = true;
    let test_list = tester.list_tests().await.expect("Failed to get list of tests");
    for test in test_list {
        if let Err(e) = run_test(&tester, test.id).await {
            passed = false;
            eprintln!("[ FAIL ] {}\n{}", test.name, e);
        } else {
            eprintln!("[  ok  ] {}", test.name);
        }
    }
    if !passed {
        panic!("Text integration tests failed");
    }
}

async fn run_test(
    tester: &txt_testing::TextFieldTestSuiteProxy,
    test_id: u64,
) -> Result<(), String> {
    let mut ime_service = crate::ime_service::ImeService::new();
    let (text_proxy, text_stream) =
        fidl::endpoints::create_proxy_and_stream::<txt::TextInputContextLegacyMarker>()
            .expect("Failed to create proxy");
    let (imec_client, _imec_server) =
        fidl::endpoints::create_endpoints::<uii::InputMethodEditorClientMarker>()
            .expect("Failed to create endpoints");
    let (_ime_client, ime_server) =
        fidl::endpoints::create_endpoints::<uii::InputMethodEditorMarker>()
            .expect("Failed to create endpoints");
    ime_service.bind_text_input_context(text_stream);
    ime_service
        .get_input_method_editor(
            uii::KeyboardType::Text,
            uii::InputMethodAction::Done,
            crate::fidl_helpers::default_state(),
            imec_client,
            ime_server,
        )
        .await;
    let mut stream = text_proxy.take_event_stream();
    let msg = stream
        .try_next()
        .await
        .expect("Failed to get event.")
        .expect("TextInputContext event stream unexpectedly closed.");
    let text_field = match msg {
        txt::TextInputContextLegacyEvent::OnFocus { text_field, .. } => text_field,
        _ => panic!("Expected text_field to pass OnFocus event type"),
    };
    let (passed, msg) =
        tester.run_test(text_field, test_id).await.expect("Call to text testing service failed");
    if passed {
        Ok(())
    } else {
        Err(msg)
    }
}
