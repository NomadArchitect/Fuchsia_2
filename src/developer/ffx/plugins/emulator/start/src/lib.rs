// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::pbm::make_configs;
use anyhow::Result;
use errors::ffx_bail;
use ffx_core::ffx_plugin;
use ffx_emulator_common::config::FfxConfigWrapper;
use ffx_emulator_engines::EngineBuilder;
use ffx_emulator_start_args::StartCommand;
use fidl_fuchsia_developer_ffx::TargetCollectionProxy;

mod pbm;

const OOT_BUNDLE_ERROR: &'static str =
    "Encountered a problem reading the emulator configuration. This may mean you\n\
don't have an appropriate Product Bundle available. Try `ffx product-bundle`\n\
to list and download available bundles.";

const IT_CONFIG_ERROR: &'static str =
    "Encountered a problem reading the emulator configuration. This may mean the\n\
currently selected board configuration isn't supported for emulation. Try\n\
using 'qemu-x64' or 'qemu-arm64' in your `fx set <PRODUCT>.<BOARD>` command\n\
then rebuild to enable emulation.";

#[ffx_plugin(TargetCollectionProxy = "daemon::protocol")]
pub async fn start(cmd: StartCommand, proxy: TargetCollectionProxy) -> Result<()> {
    let config = FfxConfigWrapper::new();
    let in_tree = config
        .get("sdk.type")
        .await
        // This is a legitimate "expect": if we can't access the ffx config values, we should panic
        .expect("Couldn't get sdk.type from ffx config.")
        .contains("in-tree");

    let emulator_configuration = match make_configs(&cmd, &config).await {
        Ok(config) => config,
        Err(e) => {
            ffx_bail!("{:?}", e.context(if in_tree { IT_CONFIG_ERROR } else { OOT_BUNDLE_ERROR }));
        }
    };

    // Initialize an engine of the requested type with the configuration defined in the manifest.
    let mut engine = match EngineBuilder::new()
        .config(emulator_configuration)
        .engine_type(cmd.engine)
        .build()
        .await
    {
        Ok(engine) => engine,
        Err(e) => ffx_bail!("{:?}", e.context("The emulator could not be configured.")),
    };

    match engine.start(&proxy).await {
        Ok(result) => std::process::exit(result),
        Err(e) => ffx_bail!("{:?}", e.context("The emulator failed to start.")),
    }
}
