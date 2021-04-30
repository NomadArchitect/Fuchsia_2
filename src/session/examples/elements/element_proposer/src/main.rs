// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::{format_err, Context as _, Error},
    fidl::encoding::Decodable,
    fidl_fuchsia_session::{ElementManagerMarker, ElementSpec},
    fuchsia_async as fasync,
    fuchsia_component::client::connect_to_protocol,
};

/// Connect to the `ElementManager` service and propose two elements; the `simple_element` from
/// these examples and `spinning_cube` from //src/experiences/examples.
#[fasync::run_singlethreaded]
async fn main() -> Result<(), Error> {
    let element_manager = connect_to_protocol::<ElementManagerMarker>()
        .context("Could not connect to element manager service.")?;

    element_manager
        .propose_element(
            ElementSpec {
                component_url: Some(
                    "fuchsia-pkg://fuchsia.com/simple_element#meta/simple_element.cm".to_string(),
                ),
                ..ElementSpec::new_empty()
            },
            None,
        )
        .await?
        .map_err(|_| format_err!("Error sending ProposeElement message"))?;

    element_manager
        .propose_element(
            ElementSpec {
                component_url: Some(
                    "fuchsia-pkg://fuchsia.com/spinning_cube#meta/spinning_cube.cmx".to_string(),
                ),
                ..ElementSpec::new_empty()
            },
            None,
        )
        .await?
        .map_err(|_| format_err!("Error launching spinning_cube.cmx"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn noop_test() {
        println!("Don't panic!(), you've got this!");
    }
}
