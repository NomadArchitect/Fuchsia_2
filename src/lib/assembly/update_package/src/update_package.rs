// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{bail, Context, Result};
use fuchsia_hash::Hash;
use fuchsia_pkg::{CreationManifest, MetaPackage, PackageManifest, PackagePath};
use fuchsia_url::pkg_url::PkgUrl;
use serde::{Deserialize, Serialize};
use serde_json::ser;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Write;
use std::path::Path;

/// A builder that constructs update packages.
pub struct UpdatePackageBuilder {
    // Maps the blob destination -> source.
    contents: BTreeMap<String, String>,
    packages: PackageList,
}

impl UpdatePackageBuilder {
    /// Construct a new UpdatePackageBuilder.
    pub fn new() -> Self {
        UpdatePackageBuilder {
            contents: BTreeMap::new(),
            packages: PackageList::V1(BTreeSet::new()),
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
        let path = package.package_path()?;
        let meta_blob = package.into_blobs().into_iter().find(|blob| blob.path == "meta/");
        match meta_blob {
            Some(meta_blob) => self.add_package(path, meta_blob.merkle),
            _ => bail!(format!("Failed to find the meta far in package {}", path)),
        }
    }

    /// Add a package to be updated by its path and meta far merkle.
    pub fn add_package(&mut self, path: PackagePath, merkle: Hash) -> Result<()> {
        let path = format!("/{}", path.to_string());
        self.packages.insert(path, merkle)
    }

    /// Build the update package.
    pub fn build(mut self, gendir: impl AsRef<Path>, out: &mut impl Write) -> Result<()> {
        // Add the package list.
        let packages_path = gendir.as_ref().join("packages.json");
        let packages = File::create(&packages_path).context("Failed to create packages.json")?;
        ser::to_writer(packages, &self.packages)?;
        self.add_file(&packages_path, "packages.json")?;

        // The update package does not have any files inside the meta.far.
        let far_contents = BTreeMap::new();

        // Build the update package.
        let creation_manifest =
            CreationManifest::from_external_and_far_contents(self.contents, far_contents)?;
        let meta_package = MetaPackage::from_name_and_variant("update", "0")?;
        fuchsia_pkg::build(&creation_manifest, &meta_package, out)?;

        Ok(())
    }
}

/// A list of all packages included in an update, which can be written as a JSON
/// package list.
/// TODO(fxbug.dev/76488): Share this enum with the rest of the SWD code.
///
/// ```
/// {
///   "version": "1",
///   "content": [
///       "fuchsia-pkg://fuchsia.com/build-info/0?hash=30a83..",
///       "fuchsia-pkg://fuchsia.com/log_listener/0?hash=816c8..",
///     ...
///   ]
/// }
/// ```
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "version", content = "content", deny_unknown_fields)]
enum PackageList {
    #[serde(rename = "1")]
    V1(BTreeSet<PkgUrl>),
}

impl PackageList {
    /// Add a new package with `name` and `merkle`.
    fn insert(&mut self, path: impl AsRef<str>, merkle: Hash) -> Result<()> {
        let url =
            PkgUrl::new_package("fuchsia.com".to_string(), path.as_ref().to_string(), Some(merkle))
                .context(format!("Failed to create package url for {}", path.as_ref()))?;
        match self {
            PackageList::V1(contents) => contents.insert(url),
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuchsia_archive::Reader;
    use fuchsia_hash::Hash;
    use fuchsia_pkg::PackagePath;
    use serde_json::json;
    use std::io::Cursor;
    use std::str::FromStr;
    use tempfile::{tempdir, NamedTempFile};

    #[test]
    fn package_list() {
        let mut list = PackageList::V1(BTreeSet::new());
        list.insert("/one/0", [0u8; 32].into()).unwrap();
        let out = serde_json::to_value(&list).unwrap();
        assert_eq!(
            out,
            json!({
                "version": "1",
                "content": [
                    "fuchsia-pkg://fuchsia.com/one/0?hash=0000000000000000000000000000000000000000000000000000000000000000"
                ],
            })
        );
    }

    #[test]
    fn add_package() {
        let mut update_bytes: Vec<u8> = Vec::new();
        let test_file = NamedTempFile::new().unwrap();
        let gendir = tempdir().unwrap();

        // Build the update package.
        let mut builder = UpdatePackageBuilder::new();
        builder.add_file(test_file.path(), "data/file.txt").unwrap();
        builder
            .add_package(
                PackagePath::from_name_and_variant("package", "0").unwrap(),
                Hash::from_str("0000000000000000000000000000000000000000000000000000000000000000")
                    .unwrap(),
            )
            .unwrap();
        builder.build(&gendir.path(), &mut update_bytes).unwrap();

        // Read the output and ensure it contains the right files (and their hashes).
        let mut far_reader = Reader::new(Cursor::new(update_bytes)).unwrap();
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
        let mut update_bytes: Vec<u8> = Vec::new();
        let test_file = NamedTempFile::new().unwrap();
        let gendir = tempdir().unwrap();

        // Build the update package.
        let mut builder = UpdatePackageBuilder::new();
        builder.add_file(test_file.path(), "data/file.txt").unwrap();
        builder
            .add_package_by_manifest(generate_test_manifest("package", "0", Some(test_file.path())))
            .unwrap();
        builder.build(&gendir.path(), &mut update_bytes).unwrap();

        // Read the output and ensure it contains the right files (and their hashes).
        let mut far_reader = Reader::new(Cursor::new(update_bytes)).unwrap();
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
