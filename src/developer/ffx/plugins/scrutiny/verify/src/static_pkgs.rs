// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::{anyhow, Context, Result},
    ffx_scrutiny_verify_args::static_pkgs::Command,
    scrutiny_config::{Config, LoggingConfig, ModelConfig, PluginConfig, RuntimeConfig},
    scrutiny_frontend::{command_builder::CommandBuilder, launcher},
    scrutiny_plugins::static_pkgs::StaticPkgsCollection,
    scrutiny_utils::{
        artifact::{ArtifactReader, FileArtifactReader},
        golden::{CompareResult, GoldenFile},
    },
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

// Package URLs often include "variant #0" path suffix.
const EXPECTED_PACKAGE_VARIANT_SUFFIX: &str = "/0";

struct Query {
    build_path: PathBuf,
    zbi_path: PathBuf,
    blobfs_manifest_path: PathBuf,
}

fn verify_static_pkgs(query: &Query, golden_file_path: PathBuf) -> Result<HashSet<String>> {
    let config = Config::run_command_with_runtime(
        CommandBuilder::new("static.pkgs").build(),
        RuntimeConfig {
            model: ModelConfig {
                build_path: query.build_path.clone(),
                zbi_path: query.zbi_path.clone(),
                blob_manifest_path: query.blobfs_manifest_path.clone(),
                ..ModelConfig::minimal()
            },
            logging: LoggingConfig { silent_mode: true, ..LoggingConfig::minimal() },
            plugin: PluginConfig {
                plugins: vec!["DevmgrConfigPlugin".to_string(), "StaticPkgsPlugin".to_string()],
            },
            ..RuntimeConfig::minimal()
        },
    );

    let scrutiny_output =
        launcher::launch_from_config(config).context("Failed to run static.pkgs")?;
    let static_pkgs_result: StaticPkgsCollection =
        serde_json::from_str(&scrutiny_output).context(format!(
            "Failed to parse static.pkgs JSON output as structured static packages list: {}",
            scrutiny_output
        ))?;
    if static_pkgs_result.errors.len() > 0 {
        return Err(anyhow!("static.pkgs reported errors: {:#?}", static_pkgs_result.errors));
    }
    if static_pkgs_result.static_pkgs.is_none() {
        return Err(anyhow!("static.pkgs returned empty result"));
    }
    let static_pkgs = static_pkgs_result.static_pkgs.unwrap();

    // Drop trailing "/0" from package URLs; skip any that do not follow this convention.
    let static_package_urls: Vec<String> = static_pkgs
        .into_iter()
        .filter_map(|(mut url, _)| {
            if url.ends_with(EXPECTED_PACKAGE_VARIANT_SUFFIX) {
                url.truncate(url.len() - EXPECTED_PACKAGE_VARIANT_SUFFIX.chars().count());
                Some(url)
            } else {
                None
            }
        })
        .collect();

    let mut golden_reader = FileArtifactReader::new(&query.build_path, &query.build_path);
    let golden_contents =
        golden_reader.read_raw(golden_file_path.as_path()).context("Failed to read golden file")?;
    let golden_file = GoldenFile::from_contents(golden_file_path.as_path(), golden_contents)
        .context("Failed to parse golden file")?;
    match golden_file.compare(static_package_urls) {
        CompareResult::Matches => {
            Ok(static_pkgs_result.deps.union(&golden_reader.get_deps()).map(String::from).collect())
        }
        CompareResult::Mismatch { errors } => {
            println!("Static package file mismatch");
            println!("");
            for error in errors.iter() {
                println!("{}", error);
            }
            println!("");
            println!("If you intended to change the static package contents, please acknowledge it by updating {:?} with the added or removed lines.", golden_file_path);
            println!("{}", SOFT_TRANSITION_MSG);
            Err(anyhow!("static file mismatch"))
        }
    }
}

pub async fn verify(cmd: Command) -> Result<HashSet<String>> {
    let query = Query {
        build_path: cmd.build_path,
        zbi_path: cmd.zbi,
        blobfs_manifest_path: cmd.blobfs_manifest,
    };
    let mut deps = HashSet::new();

    for golden_file_path in cmd.golden.into_iter() {
        deps.extend(verify_static_pkgs(&query, golden_file_path)?);
    }

    Ok(deps)
}
