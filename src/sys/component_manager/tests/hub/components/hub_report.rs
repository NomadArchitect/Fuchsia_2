// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    fidl_fidl_examples_routing_echo as fecho, fidl_fuchsia_sys2 as fsys,
    files_async::readdir,
    fuchsia_component::client::connect_to_service_at_path,
    io_util::{open_directory_in_namespace, open_file_in_namespace},
    log::info,
};

pub async fn expect_dir_listing(path: &str, mut expected_listing: Vec<&str>) {
    info!("{} should contain {:?}", path, expected_listing);
    let dir_proxy = open_directory_in_namespace(path, io_util::OPEN_RIGHT_READABLE).unwrap();
    let actual_listing = readdir(&dir_proxy).await.unwrap();

    for actual_entry in &actual_listing {
        let index = expected_listing
            .iter()
            .position(|expected_entry| *expected_entry == actual_entry.name)
            .unwrap();
        expected_listing.remove(index);
    }

    assert_eq!(expected_listing.len(), 0);
}

pub async fn expect_file_content(path: &str, expected_file_content: &str) {
    info!("{} should contain \"{}\"", path, expected_file_content);
    let file_proxy = open_file_in_namespace(path, io_util::OPEN_RIGHT_READABLE).unwrap();
    let actual_file_content = io_util::read_file(&file_proxy).await.unwrap();
    assert_eq!(expected_file_content, actual_file_content);
}

pub async fn expect_echo_service(path: &str) {
    info!("{} should be an Echo service", path);
    let echo_proxy = connect_to_service_at_path::<fecho::EchoMarker>(path).unwrap();
    let result = echo_proxy.echo_string(Some("hippos")).await.unwrap().unwrap();
    assert_eq!(&result, "hippos");
}

pub async fn resolve_component(path: &str, relative_moniker: &str, expect_success: bool) {
    info!("Attempting to resolve {} from {}", relative_moniker, path);
    let resolve_component_proxy =
        connect_to_service_at_path::<fsys::ResolveComponentMarker>(path).unwrap();
    let result = resolve_component_proxy.resolve(relative_moniker).await.unwrap();
    if expect_success {
        result.unwrap();
    } else {
        result.unwrap_err();
    }
}
