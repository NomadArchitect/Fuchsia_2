// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Result};
use assembly_update_packages_manifest::UpdatePackagesManifest;
use assembly_util::create_meta_package_file;
use fuchsia_hash::Hash;
use fuchsia_pkg::{CreationManifest, PackageManifest, PackagePath};
use serde_json::ser;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::Path;

/// A builder that constructs update packages.
pub struct UpdatePackageBuilder {
    // Maps the blob destination -> source.
    contents: BTreeMap<String, String>,
    packages: UpdatePackagesManifest,
}

impl UpdatePackageBuilder {
    /// Construct a new UpdatePackageBuilder.
    pub fn new() -> Self {
        UpdatePackageBuilder {
            contents: BTreeMap::new(),
            packages: UpdatePackagesManifest::V1(BTreeSet::new()),
        }
    }

    /// Add a file to be updated.
    pub fn add_file(&mut self, file: impl AsRef<Path>, destination: impl AsRef<str>) -> Result<()> {
        let file = file
            .as_ref()
            .to_str()
            .context(format!("File path is not valid UTF-8: {}", file.as_ref().display()))?;
        self.contents.insert(destination.as_ref().to_string(), file.to_string());
        Ok(())
    }

    /// Add a package to be updated by its PackageManifest.
    pub fn add_package_by_manifest(&mut self, package: PackageManifest) -> Result<()> {
        self.packages.add_by_manifest(package)
    }

    /// Add a package to be updated by its path and meta far merkle.
    pub fn add_package(&mut self, path: PackagePath, merkle: Hash) -> Result<()> {
        self.packages.add(path, merkle)
    }

    /// Build the update package.
    pub fn build(
        mut self,
        outdir: impl AsRef<Path>,
        gendir: impl AsRef<Path>,
        name: impl AsRef<str>,
        out: impl AsRef<Path>,
    ) -> Result<BTreeMap<String, String>> {
        // Add the package list.
        let packages_path = gendir.as_ref().join("packages.json");
        let packages = File::create(&packages_path).context("Failed to create packages.json")?;
        ser::to_writer(packages, &self.packages)?;
        self.add_file(&packages_path, "packages.json")?;

        let mut far_contents = BTreeMap::new();

        far_contents
            .insert("meta/package".to_string(), create_meta_package_file(&gendir, "update", "0")?);

        // Build the update package.
        let update_contents = self.contents.clone();
        let creation_manifest =
            CreationManifest::from_external_and_far_contents(self.contents, far_contents)?;
        let package_manifest = fuchsia_pkg::build(&creation_manifest, out, name.as_ref())?;

        // Write the package manifest to a file.
        let package_manifest_path = outdir.as_ref().join("update_package_manifest.json");
        let package_manifest_file = File::create(&package_manifest_path)
            .context("Failed to create update_package_manifest.json")?;
        ser::to_writer(package_manifest_file, &package_manifest)?;

        Ok(update_contents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuchsia_archive::Reader;
    use fuchsia_hash::Hash;
    use fuchsia_pkg::PackagePath;
    use serde_json::json;
    use std::str::FromStr;
    use tempfile::{tempdir, NamedTempFile};

    #[test]
    fn add_package() {
        let outdir = tempdir().unwrap();
        let far_path = outdir.path().join("update.far");

        let test_file = NamedTempFile::new().unwrap();
        let gendir = tempdir().unwrap();

        // Build the update package.
        let mut builder = UpdatePackageBuilder::new();
        builder.add_file(test_file.path(), "data/file.txt").unwrap();
        builder
            .add_package(
                PackagePath::from_name_and_variant(
                    "package".parse().unwrap(),
                    "0".parse().unwrap(),
                ),
                Hash::from_str("0000000000000000000000000000000000000000000000000000000000000000")
                    .unwrap(),
            )
            .unwrap();
        builder.build(&outdir.path(), &gendir.path(), "myupdate", &far_path).unwrap();

        // Read the output and ensure it contains the right files (and their hashes).
        let mut far_reader = Reader::new(File::open(&far_path).unwrap()).unwrap();
        let package = far_reader.read_file("meta/package").unwrap();
        assert_eq!(package, br#"{"name":"update","version":"0"}"#);
        let contents = far_reader.read_file("meta/contents").unwrap();
        let contents = std::str::from_utf8(&contents).unwrap();
        let expected_contents = "\
            data/file.txt=15ec7bf0b50732b49f8228e07d24365338f9e3ab994b00af08e5a3bffe55fd8b\n\
            packages.json=aa03f851446e18e59f3431ff3bedb98ecbba923de67885b0a0fc034b11cfe29a\n\
        "
        .to_string();
        assert_eq!(contents, expected_contents);
    }

    #[test]
    fn add_package_by_manifest() {
        let outdir = tempdir().unwrap();
        let far_path = outdir.path().join("update.far");

        let test_file = NamedTempFile::new().unwrap();
        let gendir = tempdir().unwrap();

        // Build the update package.
        let mut builder = UpdatePackageBuilder::new();
        builder.add_file(test_file.path(), "data/file.txt").unwrap();
        builder
            .add_package_by_manifest(generate_test_manifest("package", "0", Some(test_file.path())))
            .unwrap();
        builder.build(&outdir.path(), &gendir.path(), "myupdate", &far_path).unwrap();

        // Read the output and ensure it contains the right files (and their hashes).
        let mut far_reader = Reader::new(File::open(&far_path).unwrap()).unwrap();
        let package = far_reader.read_file("meta/package").unwrap();
        assert_eq!(package, br#"{"name":"update","version":"0"}"#);
        let contents = far_reader.read_file("meta/contents").unwrap();
        let contents = std::str::from_utf8(&contents).unwrap();
        let expected_contents = "\
            data/file.txt=15ec7bf0b50732b49f8228e07d24365338f9e3ab994b00af08e5a3bffe55fd8b\n\
            packages.json=aa03f851446e18e59f3431ff3bedb98ecbba923de67885b0a0fc034b11cfe29a\n\
        "
        .to_string();
        assert_eq!(contents, expected_contents);
    }

    // Generates a package manifest to be used for testing. The `name` is used in the blob file
    // names to make each manifest somewhat unique. If supplied, `file_path` will be used as the
    // non-meta-far blob source path, which allows the tests to use a real file.
    // TODO(fxbug.dev/76993): See if we can share this with BasePackage.
    fn generate_test_manifest(
        name: &str,
        version: &str,
        file_path: Option<&Path>,
    ) -> PackageManifest {
        let meta_source = format!("path/to/{}/meta.far", name);
        let file_source = match file_path {
            Some(path) => path.to_string_lossy().into_owned(),
            _ => format!("path/to/{}/file.txt", name),
        };
        serde_json::from_value::<PackageManifest>(json!(
            {
                "version": "1",
                "package": {
                    "name": name,
                    "version": version,
                },
                "blobs": [
                    {
                        "source_path": meta_source,
                        "path": "meta/",
                        "merkle":
                            "0000000000000000000000000000000000000000000000000000000000000000",
                        "size": 1
                    },

                    {
                        "source_path": file_source,
                        "path": "data/file.txt",
                        "merkle":
                            "1111111111111111111111111111111111111111111111111111111111111111",
                        "size": 1
                    },
                ]
            }
        ))
        .expect("valid json")
    }
}
