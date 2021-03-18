// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::{anyhow, format_err, Context, Error},
    ffx_core::ffx_plugin,
    ffx_test_args::TestCommand,
    fidl::endpoints::create_proxy,
    fidl_fuchsia_test::CaseIteratorMarker,
    fidl_fuchsia_test_manager as ftest_manager,
    run_test_suite_lib::diagnostics,
    std::io::{stdout, Write},
};

#[ffx_plugin(ftest_manager::HarnessProxy = "core/appmgr:out:fuchsia.test.manager.Harness")]
pub async fn test(
    harness_proxy: ftest_manager::HarnessProxy,
    cmd: TestCommand,
) -> Result<(), Error> {
    let writer = Box::new(stdout());
    let count = cmd.count.unwrap_or(1);
    let count = std::num::NonZeroU16::new(count)
        .ok_or_else(|| anyhow!("--count should be greater than zero."))?;

    if cmd.list {
        get_tests(harness_proxy, writer, &cmd.test_url).await
    } else {
        match run_test_suite_lib::run_tests_and_get_outcome(
            run_test_suite_lib::TestParams {
                test_url: cmd.test_url,
                timeout: cmd.timeout.and_then(std::num::NonZeroU32::new),
                test_filter: cmd.test_filter,
                also_run_disabled_tests: cmd.run_disabled,
                parallel: cmd.parallel,
                test_args: vec![],
                harness: harness_proxy,
            },
            diagnostics::LogCollectionOptions {
                min_severity: cmd.min_severity_logs,
                max_severity: cmd.max_severity_logs,
            },
            count,
            cmd.filter_ansi,
        )
        .await
        {
            run_test_suite_lib::Outcome::Passed => Ok(()),
            run_test_suite_lib::Outcome::Timedout => Err(anyhow!("Tests timed out")),
            run_test_suite_lib::Outcome::Failed
            | run_test_suite_lib::Outcome::Inconclusive
            | run_test_suite_lib::Outcome::Error => {
                Err(anyhow!("There was an error running tests"))
            }
        }
    }
}

async fn get_tests<W: Write>(
    harness_proxy: ftest_manager::HarnessProxy,
    mut write: W,
    suite_url: &String,
) -> Result<(), Error> {
    let writer = &mut write;
    let (suite_proxy, suite_server_end) = create_proxy().unwrap();
    let (_controller_proxy, controller_server_end) = create_proxy().unwrap();

    log::info!("launching test suite {}", suite_url);

    let _result = harness_proxy
        .launch_suite(
            &suite_url,
            ftest_manager::LaunchOptions::EMPTY,
            suite_server_end,
            controller_server_end,
        )
        .await
        .context("launch_suite call failed")?
        .map_err(|e| format_err!("error launching test: {:?}", e))?;

    let (case_iterator, test_server_end) = create_proxy::<CaseIteratorMarker>()?;
    suite_proxy
        .get_tests(test_server_end)
        .map_err(|e| format_err!("Error getting test steps: {}", e))?;

    loop {
        let cases = case_iterator.get_next().await?;
        if cases.is_empty() {
            return Ok(());
        }
        writeln!(writer, "Tests in suite {}:\n", suite_url)?;
        for case in cases {
            match case.name {
                Some(n) => writeln!(writer, "{}", n)?,
                None => writeln!(writer, "<No name>")?,
            };
        }
    }
}
