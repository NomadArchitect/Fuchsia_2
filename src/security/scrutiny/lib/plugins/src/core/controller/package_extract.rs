// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::core::collection::Packages,
    anyhow::anyhow,
    anyhow::Result,
    fuchsia_archive::Reader as FarReader,
    scrutiny::{
        model::controller::{ConnectionMode, DataController, HintDataType},
        model::model::*,
    },
    scrutiny_utils::usage::*,
    serde::{Deserialize, Serialize},
    serde_json::{json, value::Value},
    std::fs::{self, File},
    std::io::prelude::*,
    std::io::Cursor,
    std::path::PathBuf,
    std::sync::Arc,
};

#[derive(Deserialize, Serialize)]
pub struct PackageExtractRequest {
    // The input path for the ZBI.
    pub url: String,
    // The output directory for the extracted ZBI.
    pub output: String,
}

#[derive(Default)]
pub struct PackageExtractController {}

impl DataController for PackageExtractController {
    fn query(&self, model: Arc<DataModel>, query: Value) -> Result<Value> {
        let blob_dir = model.config().repository_blobs_path();
        let request: PackageExtractRequest = serde_json::from_value(query)?;
        let packages = &model.get::<Packages>()?.entries;
        for package in packages.iter() {
            if package.url == request.url {
                let output_path = PathBuf::from(request.output);
                fs::create_dir_all(&output_path)?;

                let pkg_path = blob_dir.clone().join(package.merkle.clone());
                let mut pkg_file = File::open(pkg_path)?;
                let mut pkg_buffer = Vec::new();
                pkg_file.read_to_end(&mut pkg_buffer)?;

                let mut cursor = Cursor::new(pkg_buffer);
                let mut far = FarReader::new(&mut cursor)?;

                let pkg_files: Vec<String> = far.list().map(|e| e.path().to_string()).collect();
                // Extract all the far meta files.
                for file_name in pkg_files.iter() {
                    let data = far.read_file(file_name)?;
                    let file_path = output_path.clone().join(file_name);
                    if let Some(parent_dir) = file_path.as_path().parent() {
                        fs::create_dir_all(parent_dir)?;
                    }
                    let mut package_file = File::create(file_path)?;
                    package_file.write_all(&data)?;
                }

                // Extract all the contents of the package.
                for (file_name, blob) in package.contents.iter() {
                    let file_path = output_path.clone().join(file_name);

                    let blob_path = blob_dir.clone().join(blob);
                    let mut blob_file = File::open(blob_path)?;
                    let mut blob_buffer = Vec::new();
                    blob_file.read_to_end(&mut blob_buffer)?;

                    if let Some(parent_dir) = file_path.as_path().parent() {
                        fs::create_dir_all(parent_dir)?;
                    }
                    let mut package_file = File::create(file_path)?;
                    package_file.write_all(&blob_buffer)?;
                }
                return Ok(json!({"status": "ok"}));
            }
        }
        Err(anyhow!("Unable to find package url"))
    }

    fn description(&self) -> String {
        "Extracts a package from a url to a directory.".to_string()
    }

    fn usage(&self) -> String {
        UsageBuilder::new()
            .name("package.extract - Extracts Fuchsia package to a directory.")
            .summary("package.extract --url fuchsia-pkg://fuchsia.com/foo --output /tmp/foo")
            .description(
                "Extracts package from a given url to some provided file path. \
                Internally this is resolving the URL and extracting the internal
                Fuchsia Archive and resolving all the merkle paths.
                ",
            )
            .arg("--url", "Package url that you wish to extract.")
            .arg("--output", "Path to the output directory")
            .build()
    }

    fn hints(&self) -> Vec<(String, HintDataType)> {
        vec![
            ("--url".to_string(), HintDataType::NoType),
            ("--output".to_string(), HintDataType::NoType),
        ]
    }

    /// PackageExtract is only available to the local shell as it directly
    /// modifies files on disk.
    fn connection_mode(&self) -> ConnectionMode {
        ConnectionMode::Local
    }
}
