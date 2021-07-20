// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::base_package::BasePackage;
use crate::config::{BlobFSConfig, ProductConfig};

use anyhow::{Context, Result};
use assembly_blobfs::BlobFSBuilder;
use std::path::{Path, PathBuf};

pub fn construct_blobfs(
    outdir: impl AsRef<Path>,
    gendir: impl AsRef<Path>,
    product: &ProductConfig,
    blobfs_config: &BlobFSConfig,
    base_package: &BasePackage,
) -> Result<PathBuf> {
    let mut blobfs_builder = BlobFSBuilder::new(&blobfs_config.layout);
    blobfs_builder.set_compressed(blobfs_config.compress);

    // Add the base and cache packages.
    for package_manifest_path in &product.base_packages {
        blobfs_builder.add_package(&package_manifest_path)?;
    }
    for package_manifest_path in &product.cache_packages {
        blobfs_builder.add_package(&package_manifest_path)?;
    }

    // Add the base package and its contents.
    blobfs_builder.add_file(&base_package.path)?;
    for (_, source) in &base_package.contents {
        blobfs_builder.add_file(source)?;
    }

    // Build the blobfs and return its path.
    let blobfs_path = outdir.as_ref().join("blob.blk");
    blobfs_builder.build(gendir, &blobfs_path).context("Failed to build the blobfs")?;
    Ok(blobfs_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::{BlobFSConfig, ProductConfig};
    use fuchsia_hash::Hash;
    use std::collections::BTreeMap;
    use std::str::FromStr;
    use tempfile::tempdir;

    #[test]
    fn construct() {
        let dir = tempdir().unwrap();
        let product_config = ProductConfig::default();
        let blobfs_config = BlobFSConfig { layout: "padded".to_string(), compress: true };

        // Create a fake base package.
        let base_path = dir.path().join("base.far");
        std::fs::write(&base_path, "fake base").unwrap();
        let base = BasePackage {
            merkle: Hash::from_str(
                "0000000000000000000000000000000000000000000000000000000000000000",
            )
            .unwrap(),
            contents: BTreeMap::default(),
            path: base_path,
        };
        let blobfs_path =
            construct_blobfs(dir.path(), dir.path(), &product_config, &blobfs_config, &base)
                .unwrap();
        assert_eq!(blobfs_path, dir.path().join("blob.blk"));
    }
}
