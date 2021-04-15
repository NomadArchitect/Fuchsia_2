// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg(test)]

use {
    crate::{assert_command, commands::expand_paths, tests::utils, types::Error},
    diagnostics_testing::{EnvWithDiagnostics, Launched},
    matches::assert_matches,
    std::path::Path,
    tempfile::tempdir,
};

// List command

#[fuchsia::test]
async fn test_list() {
    let (_env, app) = utils::start_basic_component("list-test").await.expect("create comp 1");
    let (_env2, app2) = utils::start_basic_component("list-test2").await.expect("create comp 2");
    assert_command!(
        command: "list",
        golden_basename: list_test,
        args: [],
        test_opts: [ "with_retries" ]
    );
    utils::wait_for_terminated(app).await;
    utils::wait_for_terminated(app2).await;
}

#[fuchsia::test]
async fn test_list_no_duplicates() {
    let (_env, app) = utils::start_test_component("list-dup-test").await.expect("create comp 1");
    assert_command!(
        command: "list",
        golden_basename: list_no_dups,
        args: [],
        test_opts: [ "with_retries" ]
    );
    utils::wait_for_terminated(app).await;
}

#[fuchsia::test]
async fn test_list_filter_manifest() {
    let (_env, app) =
        utils::start_basic_component("list-filter-test").await.expect("create comp 1");
    let (_env, app2) =
        utils::start_test_component("list-filter-test2").await.expect("create comp 2");
    assert_command!(
        command: "list",
        golden_basename: list_filter_manifest,
        args: [ "--manifest", "basic_component.cmx" ],
        test_opts: [ "with_retries" ]
    );
    utils::wait_for_terminated(app).await;
    utils::wait_for_terminated(app2).await;
}

#[fuchsia::test]
async fn test_list_with_urls() {
    let (_env, app) = utils::start_basic_component("list-url-test").await.expect("create comp 1");
    let (_env, app2) = utils::start_test_component("list-url-test2").await.expect("create comp 2");
    assert_command!(
        command: "list",
        golden_basename: list_with_url,
        args: [ "--with-url" ],
        test_opts: [ "with_retries" ]
    );
    utils::wait_for_terminated(app).await;
    utils::wait_for_terminated(app2).await;
}

#[fuchsia::test]
async fn list_archive() {
    let test_env = EnvWithDiagnostics::new().await;
    let Launched { app, .. } = test_env.launch(utils::BASIC_COMPONENT_URL, None);
    assert_command!(
        command: "list",
        golden_basename: list_archive,
        args: [
            "--accessor-path",
            "/hub/r/diagnostics_*/*/c/archivist-for-embedding.cmx/*/out/svc/fuchsia.diagnostics.ArchiveAccessor"
        ],
        test_opts: [ "with_retries" ]
    );
    utils::wait_for_terminated(app).await;
}

// List files command

#[fuchsia::test]
async fn list_files_empty_path_uses_cwd() {
    std::env::set_current_dir(Path::new("/hub")).expect("change dir");
    let (_env, app) =
        utils::start_basic_component("list-file-test-1").await.expect("create comp 1");
    assert_command!(
        command: "list-files",
        golden_basename: list_files_cwd,
        args: []
    );
    utils::wait_for_terminated(app).await;
}

#[fuchsia::test]
async fn list_files() {
    let (_env, app) =
        utils::start_basic_component("list-file-test-2").await.expect("create comp 1");
    let (_env2, app2) =
        utils::start_test_component("list-file-test-3").await.expect("create comp 2");
    assert_command!(
        command: "list-files",
        golden_basename: list_files_test,
        args: [
            "/hub/c/archivist-for-embedding.cmx/",
            "/hub/r/list-file-test-*/*/c/*/*/out/diagnostics/"
        ]
    );
    utils::wait_for_terminated(app).await;
    utils::wait_for_terminated(app2).await;
}

#[fuchsia::test]
async fn log() {
    let (_env, app) =
        utils::start_basic_component_with_logs("log-test").await.expect("create comp 1");

    assert_command!(
        command: "logs",
        golden_basename: log,
        args: [],
        test_opts: [ "with_retries" ]
    );
    utils::wait_for_terminated(app).await;
}

// Selectors command

#[fuchsia::test]
async fn test_selectors_empty() {
    let result = utils::execute_command(&["selectors"]).await;
    assert_matches!(result, Err(Error::InvalidArguments(_)));
}

#[fuchsia::test]
async fn test_selectors() {
    let (_env, app) = utils::start_basic_component("selectors-test").await.expect("create comp 1");
    let (_env2, app2) =
        utils::start_basic_component("selectors-test2").await.expect("create comp 2");
    let (_env3, app3) =
        utils::start_test_component("selectors-test3").await.expect("create comp 3");
    assert_command!(
        command: "selectors",
        golden_basename: selectors_test,
        args: [
            "selectors-test/basic_component.cmx:root/fuchsia.inspect.Health",
            "selectors-test2/basic_component.cmx:root",
            "selectors-test3/test_component.cmx"
        ],
        test_opts: [ "with_retries" ]
    );
    utils::wait_for_terminated(app).await;
    utils::wait_for_terminated(app2).await;
    utils::wait_for_terminated(app3).await;
}

#[fuchsia::test]
async fn test_selectors_filter() {
    let (_env, app) =
        utils::start_basic_component("selectors-filter").await.expect("create comp 1");
    let (_env, app2) =
        utils::start_test_component("selectors-filter2").await.expect("create comp 2");
    assert_command!(
        command: "selectors",
        golden_basename: selectors_filter_test,
        args: [
            "--manifest",
            "basic_component.cmx",
            "root/fuchsia.inspect.Health"
        ],
        test_opts: [ "with_retries" ]
    );
    utils::wait_for_terminated(app).await;
    utils::wait_for_terminated(app2).await;
}

#[fuchsia::test]
async fn selectors_archive() {
    let test_env = EnvWithDiagnostics::new().await;
    let Launched { app, .. } = test_env.launch(utils::BASIC_COMPONENT_URL, None);
    assert_command!(
        command: "selectors",
        golden_basename: selectors_archive,
        args: [
            "basic_component.cmx:root",
            "--accessor-path",
            "/hub/r/diagnostics_*/*/c/archivist-for-embedding.cmx/*/out/svc/fuchsia.diagnostics.ArchiveAccessor"
        ],
        test_opts: [ "with_retries" ]
    );
    utils::wait_for_terminated(app).await;
}

// Show file

#[fuchsia::test]
async fn test_no_paths() {
    let result = utils::execute_command(&["show-file"]).await;
    assert_matches!(result, Err(Error::InvalidArguments(_)));
}

#[fuchsia::test]
async fn test_invalid_location() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("root.inspect").to_string_lossy().to_string();
    let result = utils::execute_command(&["show-file", &file_path]).await;
    assert_matches!(result, Err(Error::ReadLocation(path, _)) if path == file_path);
}

#[fuchsia::test]
async fn show_file_test() {
    let (_env, app) =
        utils::start_basic_component("show-file-test-1").await.expect("create comp 1");
    let (_env2, app2) =
        utils::start_test_component("show-file-test-2").await.expect("create comp 2");
    assert_command!(
        command: "show-file",
        golden_basename: show_file_test,
        args: [
            "/hub/r/show-file-test-1/*/c/basic_component.cmx/*/out/diagnostics/fuchsia.inspect.Tree",
            "/hub/r/show-file-test-2/*/c/test_component.cmx/*/out/diagnostics/*"
        ]
    );
    utils::wait_for_terminated(app).await;
    utils::wait_for_terminated(app2).await;
}

#[fuchsia::test]
async fn inspect_vmo_file_directly() {
    let (_env, app) = utils::start_test_component("show-file-vmo-2").await.expect("create comp 2");
    let paths = expand_paths(&[
        "/hub/r/show-file-vmo-2/*/c/test_component.cmx/*/out/diagnostics/*".to_string(),
    ])
    .expect("got paths");

    // Pass only the path to the vmo file. Without the workaround in `get_paths` comments this
    // wouldn't work and the `result` would be an emtpy list.
    let path =
        paths.into_iter().find(|p| p.ends_with("root.inspect")).expect("found root.inspect path");
    assert_command!(
        command: "show-file",
        golden_basename: show_file_vmo,
        args: [ &path ]
    );
    utils::wait_for_terminated(app).await;
}

// Show

#[fuchsia::test]
async fn test_no_selectors() {
    let (_env, app) = utils::start_basic_component("show-all-test").await.expect("create comp 1");
    let (_env2, app2) =
        utils::start_basic_component("show-all-test2").await.expect("create comp 2");
    assert_command!(
        command: "show",
        golden_basename: show_all_test,
        args: [],
        test_opts: [ "with_retries", "remove_observer" ]
    );
    utils::wait_for_terminated(app).await;
    utils::wait_for_terminated(app2).await;
}

#[fuchsia::test]
async fn show_test() {
    let (_env, app) = utils::start_basic_component("show-test").await.expect("create comp 1");
    let (_env2, app2) = utils::start_basic_component("show-test2").await.expect("create comp 2");
    let (_env3, app3) = utils::start_basic_component("show-test3").await.expect("create comp 3");
    let (_env4, app4) =
        utils::start_basic_component("show-test4:with_colon").await.expect("create comp 4");
    assert_command!(
        command: "show",
        golden_basename: show_test,
        args: [
            "show-test/basic_component.cmx:root/fuchsia.inspect.Health",
            "show-test2/basic_component.cmx:root:iquery",
            "show-test3/basic_component.cmx",
            r#"show-test4\:with_colon/basic_component.cmx"#
        ],
        test_opts: [ "with_retries" ]
    );
    utils::wait_for_terminated(app).await;
    utils::wait_for_terminated(app2).await;
    utils::wait_for_terminated(app3).await;
    utils::wait_for_terminated(app4).await;
}

#[fuchsia::test]
async fn empty_result_on_null_payload() {
    let (_env, app) = utils::start_basic_component("show-test-empty").await.expect("create comp 1");
    let result =
        utils::execute_command(&["show", "show-test-empty/basic_component.cmx:root/nothing:here"])
            .await;
    assert_matches!(result, Ok(res) if res == "" || res.contains("payload: null"));
    utils::wait_for_terminated(app).await;
}

#[fuchsia::test]
async fn show_filter_manifest() {
    let (_env, app) = utils::start_basic_component("show-filter").await.expect("create comp 1");
    let (_env, app2) = utils::start_test_component("show-filter2").await.expect("create comp 2");
    assert_command!(
        command: "show",
        golden_basename: show_filter_test,
        args: [
            "--manifest",
            "basic_component.cmx",
            "root/fuchsia.inspect.Health"
        ],
        test_opts: [ "with_retries" ]
    );
    utils::wait_for_terminated(app).await;
    utils::wait_for_terminated(app2).await;
}

#[fuchsia::test]
async fn show_filter_manifest_no_selectors() {
    let (_env, app) =
        utils::start_basic_component("show-filter-no-selectors").await.expect("create comp 1");
    let (_env, app2) =
        utils::start_test_component("show-filter-no-selectors2").await.expect("create comp 2");
    assert_command!(
        command: "show",
        golden_basename: show_filter_no_selectors_test,
        args: [
            "--manifest",
            "basic_component.cmx"
        ],
        test_opts: [ "with_retries" ]
    );
    utils::wait_for_terminated(app).await;
    utils::wait_for_terminated(app2).await;
}

#[fuchsia::test]
async fn show_archive() {
    let test_env = EnvWithDiagnostics::new().await;
    let Launched { app, .. } = test_env.launch(utils::BASIC_COMPONENT_URL, None);
    assert_command!(
        command: "show",
        golden_basename: show_archive,
        args: [
            "basic_component.cmx:root",
            "--accessor-path",
            "/hub/r/diagnostics_*/*/c/archivist-for-embedding.cmx/*/out/svc/fuchsia.diagnostics.ArchiveAccessor"
        ],
        test_opts: [ "with_retries" ]
    );
    utils::wait_for_terminated(app).await;
}

#[fuchsia::test]
async fn list_accessors() {
    std::env::set_current_dir(Path::new("/hub/c")).expect("change dir");
    assert_command!(
        command: "list-accessors",
        golden_basename: list_accessors,
        args: []
    );
}
