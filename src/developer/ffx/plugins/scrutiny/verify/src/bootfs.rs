// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::{anyhow, bail, Context, Result},
    ffx_scrutiny_verify_args::bootfs::Command,
    scrutiny_config::{Config, LoggingConfig, PluginConfig, RuntimeConfig},
    scrutiny_frontend::{command_builder::CommandBuilder, launcher},
    scrutiny_utils::golden::{CompareResult, GoldenFile},
    serde_json,
    std::{collections::HashSet, path::PathBuf},
};

const SOFT_TRANSITION_MSG : &str = "
If you are making a change in fuchsia.git that causes this, you need to perform a soft transition:
1: Instead of adding lines as written above, add each line prefixed with a question mark to mark it as transitional.
2: Instead of removing lines as written above, prefix the line with a question mark to mark it as transitional.
3: Check in your fuchsia.git change.
4: For each new line you added in 1, remove the question mark.
5: For each existing line you modified in 2, remove the line.
";

pub async fn verify(cmd: &Command) -> Result<HashSet<PathBuf>> {
    if cmd.golden.len() == 0 {
        bail!("Must specify at least one --golden");
    }
    let mut deps = HashSet::new();
    let zbi_path = &cmd.zbi;
    let zbi = zbi_path
        .to_str()
        .ok_or_else(|| anyhow!("Failed to convert ZBI path to string: {:?}", cmd.zbi))?;
    deps.insert(zbi_path.clone());

    let config = Config::run_command_with_runtime(
        CommandBuilder::new("tool.zbi.list.bootfs").param("input", zbi).build(),
        RuntimeConfig {
            logging: LoggingConfig { silent_mode: true, ..LoggingConfig::minimal() },
            plugin: PluginConfig { plugins: vec!["ToolkitPlugin".to_string()] },
            ..RuntimeConfig::minimal()
        },
    );
    let scrutiny_output =
        launcher::launch_from_config(config).context("Failed to launch scrutiny")?;
    let bootfs_files: Vec<String> = serde_json::from_str(&scrutiny_output)
        .context(format!("Failed to deserialize scrutiny output: {}", scrutiny_output))?;
    for golden_file_path in cmd.golden.iter() {
        let golden_file =
            GoldenFile::open(golden_file_path).context("Failed to open the golden file")?;
        match golden_file.compare(bootfs_files.clone()) {
            CompareResult::Matches => Ok(()),
            CompareResult::Mismatch { errors } => {
                println!("Bootfs file mismatch");
                println!("");
                for error in errors.iter() {
                    println!("{}", error);
                }
                println!("");
                println!(
                    "If you intended to change the bootfs contents, please acknowledge it by updating {:?} with the added or removed lines.",
                    golden_file_path,
                );
                println!("{}", SOFT_TRANSITION_MSG);
                Err(anyhow!("bootfs file mismatch"))
            }
        }?;

        deps.insert(golden_file_path.clone());
    }

    Ok(deps)
}
