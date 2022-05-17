// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::Result,
    blackout_host::{BlackoutError, TestEnv},
    ffx_core::ffx_plugin,
    ffx_storage_blackout_integration_args::BlackoutIntegrationCommand,
    std::time::Duration,
};

async fn failure() -> Result<()> {
    let opts = blackout_host::CommonOpts {
        block_device: String::from("fail"),
        seed: None,
        relay: None,
        iterations: None,
        run_until_failure: false,
    };
    let mut test =
        TestEnv::new("blackout-integration-target", "blackout-integration-target-component", opts)
            .await;

    test.setup_step()
        .load_step(Duration::from_secs(1))
        .reboot_step()
        .verify_step(20, Duration::from_secs(15));

    match test.run().await {
        Err(BlackoutError::Verification(_)) => Ok(()),
        Ok(()) => Err(anyhow::anyhow!("test succeeded when it should've failed")),
        Err(e) => Err(anyhow::anyhow!("test failed, but not in the expected way: {:?}", e)),
    }
}

async fn success(iterations: Option<u64>) -> Result<()> {
    let opts = blackout_host::CommonOpts {
        block_device: String::from("/nothing"),
        seed: None,
        relay: None,
        iterations: iterations,
        run_until_failure: false,
    };
    let mut test =
        TestEnv::new("blackout-integration-target", "blackout-integration-target-component", opts)
            .await;

    test.setup_step()
        .load_step(Duration::from_secs(1))
        .reboot_step()
        .verify_step(20, Duration::from_secs(15));
    test.run().await?;

    Ok(())
}

#[ffx_plugin("storage_dev")]
async fn integration(cmd: BlackoutIntegrationCommand) -> Result<()> {
    // make sure verification failure detection works
    println!("testing a verification failure...");
    failure().await?;

    // make sure a successful test run works
    println!("testing a successful run...");
    success(cmd.iterations).await?;

    Ok(())
}
