// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context as _, Error};
use fidl_fuchsia_examples::EchoServiceMarker;
use fuchsia_async as fasync;
use fuchsia_component::client::{launch, launcher};

static SERVER_URL: &str = "fuchsia-pkg://fuchsia.com/echo-rust-service-server#meta/echo-server.cmx";

#[fasync::run_singlethreaded]
async fn main() -> Result<(), Error> {
    let launcher = launcher().context("Failed to open launcher service")?;
    let app =
        launch(&launcher, SERVER_URL.to_string(), None).context("Failed to launch echo service")?;

    let echo = app
        .connect_to_service::<EchoServiceMarker>()
        .context("Failed to connect to echo service")?;

    let regular = echo.regular_echo().context("failed to connect to regular_echo member")?;
    let regular_response = regular.echo_string("hello world!").await?;
    println!("regular response: {:?}", regular_response);

    let reversed = echo.reversed_echo().context("failed to connect to reversed_echo member")?;
    let reversed_response = reversed.echo_string("hello world!").await?;
    println!("reversed response: {:?}", reversed_response);

    Ok(())
}
