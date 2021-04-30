// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;

use {
    anyhow::Context as _, fidl_fuchsia_session_examples::ElementPingMarker,
    fuchsia_async as fasync, fuchsia_component::client::connect_to_protocol,
};

/// An `Element` that connects to the `ElementPing` Service and calls the `ping` method.
#[fasync::run_singlethreaded]
async fn main() -> Result<(), Error> {
    let element_ping = connect_to_protocol::<ElementPingMarker>()
        .context("Could not connect to ElementPing service.")?;

    element_ping.ping()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn noop_test() {
        println!("Don't panic!(), you've got this!");
    }
}
