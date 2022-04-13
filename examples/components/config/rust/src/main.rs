// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// [START imports]
use example_config::Config;
// [END imports]
use fuchsia_component::server::ServiceFs;
use futures::StreamExt;
use tracing::info;

#[fuchsia::main]
async fn main() {
    // [START get_config]
    // Retrieve configuration
    let config = Config::from_args();
    // [END get_config]

    // Print greeting to the log
    info!("Hello, {}!", config.greeting);

    // [START inspect]
    // Record configuration to inspect
    let inspector = fuchsia_inspect::component::inspector();
    config.record_to_inspect(inspector.root());
    // [END inspect]

    let mut fs = ServiceFs::new_local();
    inspect_runtime::serve(inspector, &mut fs).unwrap();
    fs.take_and_serve_directory_handle().unwrap();
    while let Some(()) = fs.next().await {}
}
// [END code]
