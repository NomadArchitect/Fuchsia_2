// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::reverser::ReverserServerFactory,
    anyhow::{Context, Error},
    fidl_fuchsia_examples_inspect::FizzBuzzMarker,
    fuchsia_async as fasync,
    fuchsia_component::{client, server::ServiceFs},
    fuchsia_inspect::{component, health::Reporter},
    fuchsia_syslog,
    futures::{future::try_join, FutureExt, StreamExt},
    tracing::info,
};

mod reverser;

#[fasync::run_singlethreaded]
async fn main() -> Result<(), Error> {
    fuchsia_syslog::init_with_tags(&["inspect_rust_codelab", "part5"])?;
    let mut fs = ServiceFs::new();

    info!("starting up...");

    inspect_runtime::serve(component::inspector(), &mut fs)?;

    // ComponentInspector has built-in health checking. Set it to "starting up" so snapshots show
    // we may still be initializing.
    component::health().set_starting_up();

    // Create a version string. We use record_ rather than create_ to tie the lifecyle of the
    // inspector root with the string property.
    // It is an error to not retain the created property.
    component::inspector().root().record_string("version", "part5");

    // Create a new Reverser Server factory. The factory holds global stats for the reverser
    // server.
    let reverser_factory =
        ReverserServerFactory::new(component::inspector().root().create_child("reverser_service"));

    // Serve the reverser service
    fs.dir("svc").add_fidl_service(move |stream| reverser_factory.spawn_new(stream));
    fs.take_and_serve_directory_handle()?;

    // Send a request to the FizzBuzz service and print the response when it arrives.
    let fizzbuzz_fut = async move {
        let fizzbuzz = client::connect_to_service::<FizzBuzzMarker>()
            .context("failed to connect to fizzbuzz")?;
        match fizzbuzz.execute(30u32).await {
            Ok(result) => {
                component::health().set_ok();
                info!(%result, "Got FizzBuzz");
            }
            Err(_) => {
                component::health().set_unhealthy("FizzBuzz connection closed");
            }
        };
        Ok(())
    };

    let running_service_fs = fs.collect::<()>().map(Ok);
    try_join(running_service_fs, fizzbuzz_fut).await.map(|((), ())| ())
}
