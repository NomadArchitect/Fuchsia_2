// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::config::{from_reader, BoardConfig, ProductConfig};
use anyhow::{Context, Result};
use assembly_base_package::BasePackageBuilder;
use ffx_assembly_args::ImageArgs;
use ffx_core::ffx_bail;
use fuchsia_hash::Hash;
use fuchsia_merkle::MerkleTree;
use fuchsia_pkg::PackageManifest;
use std::fs::{File, OpenOptions};
use std::io::BufReader;
use std::path::Path;
use zbi::ZbiBuilder;

pub fn assemble(args: ImageArgs) -> Result<()> {
    let ImageArgs { product, board, outdir, gendir } = args;
    let (product, _board) = read_configs(product, board)?;
    let gendir = gendir.unwrap_or(outdir.clone());
    let base_package = construct_base_package(&outdir, &gendir, &product)?;
    let base_merkle = MerkleTree::from_reader(&base_package)
        .context("Failed to calculate the base merkle")?
        .root();
    println!("Base merkle: {}", base_merkle);
    let _zbi = construct_zbi(&outdir, &gendir, &product, Some(base_merkle))?;

    Ok(())
}

fn read_configs(
    product: impl AsRef<Path>,
    board: impl AsRef<Path>,
) -> Result<(ProductConfig, BoardConfig)> {
    let mut product = File::open(product)?;
    let mut board = File::open(board)?;
    let product: ProductConfig =
        from_reader(&mut product).context("Failed to read the product config")?;
    let board: BoardConfig = from_reader(&mut board).context("Failed to read the board config")?;
    Ok((product, board))
}

fn construct_base_package(
    outdir: impl AsRef<Path>,
    gendir: impl AsRef<Path>,
    product: &ProductConfig,
) -> Result<File> {
    let mut base_pkg_builder = BasePackageBuilder::default();
    for pkg_manifest_path in &product.extra_packages_for_base_package {
        let pkg_manifest = pkg_manifest_from_path(pkg_manifest_path)?;
        base_pkg_builder.add_files_from_package(pkg_manifest);
    }
    for pkg_manifest_path in &product.base_packages {
        let pkg_manifest = pkg_manifest_from_path(pkg_manifest_path)?;
        base_pkg_builder.add_base_package(pkg_manifest).context(format!(
            "Failed to add package to base package list with manifest: {}",
            pkg_manifest_path.display()
        ))?;
    }
    for pkg_manifest_path in &product.cache_packages {
        let pkg_manifest = pkg_manifest_from_path(pkg_manifest_path)?;
        base_pkg_builder.add_cache_package(pkg_manifest).context(format!(
            "Failed to add package to cache package list with manifest: {}",
            pkg_manifest_path.display()
        ))?;
    }

    let base_package_path = outdir.as_ref().join("base.far");
    let mut base_package = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(base_package_path)
        .context("Failed to create the base package file")?;
    let _ = base_pkg_builder
        .build(gendir, &mut base_package)
        .context("Failed to build the base package")?;
    Ok(base_package)
}

fn pkg_manifest_from_path(path: impl AsRef<Path>) -> Result<PackageManifest> {
    let manifest_file = File::open(path)?;
    let pkg_manifest_reader = BufReader::new(manifest_file);
    serde_json::from_reader(pkg_manifest_reader).map_err(Into::into)
}

fn construct_zbi(
    outdir: impl AsRef<Path>,
    gendir: impl AsRef<Path>,
    product: &ProductConfig,
    base_merkle: Option<Hash>,
) -> Result<File> {
    let mut zbi_builder = ZbiBuilder::default();

    // Add the kernel image.
    zbi_builder.set_kernel(&product.kernel_image);

    // Instruct devmgr that a /system volume is required.
    zbi_builder.add_boot_arg("devmgr.require-system=true");

    // If a base merkle is supplied, then add the boot arguments for startup up pkgfs with the
    // merkle of the Base Package.
    if let Some(base_merkle) = base_merkle {
        // Specify how to launch pkgfs: bin/pkgsvr <base-merkle>
        zbi_builder.add_boot_arg(&format!("zircon.system.pkgfs.cmd=bin/pkgsvr+{}", base_merkle));

        // Add the pkgfs blobs to the boot arguments, so that pkgfs can be bootstrapped out of blobfs,
        // before the blobfs service is available.
        let pkgfs_manifest: PackageManifest = product
            .base_packages
            .iter()
            .find_map(|p| {
                if let Ok(m) = pkg_manifest_from_path(p) {
                    if m.name() == "pkgfs" {
                        return Some(m);
                    }
                }
                return None;
            })
            .context("Failed to find pkgfs in the base packages")?;

        pkgfs_manifest.into_blobs().into_iter().filter(|b| b.path != "meta/").for_each(|b| {
            zbi_builder.add_boot_arg(&format!("zircon.system.pkgfs.file.{}={}", b.path, b.merkle));
        });
    }

    // Add the command line.
    for cmd in &product.kernel_cmdline {
        zbi_builder.add_cmdline_arg(cmd);
    }

    // Add the BootFS files.
    for bootfs_entry in &product.bootfs_files {
        zbi_builder.add_bootfs_file(&bootfs_entry.source, &bootfs_entry.destination);
    }

    // Build and return the ZBI.
    let zbi_path = outdir.as_ref().join("fuchsia.zbi");
    zbi_builder.build(gendir, zbi_path.as_path())?;
    let zbi = OpenOptions::new()
        .read(true)
        .open(zbi_path)
        .or_else(|e| ffx_bail!("Failed to open the zbi: {}", e))?;
    Ok(zbi)
}
