// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use ::update_package::{ImageMetadata, ImagePackagesManifest};
use anyhow::{anyhow, Context, Result};
use assembly_blobfs::BlobFSBuilder;
use assembly_images_manifest::{Image, ImagesManifest};
use assembly_partitions_config::PartitionsConfig;
use assembly_tool::ToolProvider;
use assembly_update_packages_manifest::UpdatePackagesManifest;
use assembly_util::PathToStringExt;
use epoch::EpochFile;
use fuchsia_pkg::PackageBuilder;
use fuchsia_url::pkg_url::PinnedPkgUrl;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A builder that constructs update packages.
pub struct UpdatePackageBuilder {
    /// The tool provider, used to invoke the blobfs tool from the SDK.
    tool_provider: Box<dyn ToolProvider>,

    /// Root name of the UpdatePackage and its associated images packages.
    /// This is typically only modified for OTA tests so that multiple UpdatePackages can be
    /// published to the same repository.
    name: String,

    /// Mapping of physical partitions to images.
    partitions: PartitionsConfig,

    /// Name of the board.
    /// Fuchsia confirms the board name matches before applying an update.
    board_name: String,

    /// Version of the update.
    version_file: PathBuf,

    /// The epoch of the system.
    /// Fuchsia confirms that the epoch changes in increasing order before applying an update.
    epoch: EpochFile,

    /// Images to update for a particular slot, such as the ZBI or VBMeta for SlotA.
    /// Currently, the UpdatePackage does not support both A and B slots.
    slot_primary: Option<Slot>,
    slot_recovery: Option<Slot>,

    /// Manifest of packages to include in the update.
    packages: UpdatePackagesManifest,

    /// Directory to write outputs.
    outdir: PathBuf,

    /// Directory to write intermediate files.
    gendir: PathBuf,
}

/// A set of images to be updated in a particular slot.
pub enum Slot {
    /// A or B slots.
    Primary(ImagesManifest),

    /// R slot.
    Recovery(ImagesManifest),
}

impl Slot {
    /// Get the image manifest.
    fn manifest(&self) -> &ImagesManifest {
        match self {
            Slot::Primary(m) => m,
            Slot::Recovery(m) => m,
        }
    }

    /// Get the (preferably signed) zbi and optional vbmeta, or None if no zbi image is present in
    /// this manifest.
    fn zbi_and_vbmeta(&self) -> Option<(ImageMapping, Option<ImageMapping>)> {
        let mut zbi = None;
        let mut vbmeta = None;

        for image in &self.manifest().images {
            match image {
                Image::ZBI { path: _, signed } => {
                    if *signed || zbi.is_none() {
                        zbi = Some(ImageMapping::new(image.source(), "zbi"));
                    }
                }
                Image::VBMeta(_) => {
                    vbmeta = Some(ImageMapping::new(image.source(), "vbmeta"));
                }
                _ => {}
            }
        }

        match zbi {
            Some(zbi) => Some((zbi, vbmeta)),
            None => None,
        }
    }
}

/// A mapping between an image source path on host to the destination in an UpdatePackage.
struct ImageMapping {
    source: PathBuf,
    destination: String,
}

impl ImageMapping {
    /// Create a new Image Mapping from |source | to |destination|.
    fn new(source: impl AsRef<Path>, destination: impl AsRef<str>) -> Self {
        Self {
            source: source.as_ref().to_path_buf(),
            destination: destination.as_ref().to_string(),
        }
    }

    /// Create an ImageMapping from the |image| and |slot|.
    fn try_from(image: &Image, slot: &Slot) -> Result<Self> {
        match slot {
            Slot::Primary(_) => match image {
                Image::ZBI { path: _, signed: true } => {
                    Ok(ImageMapping::new(image.source(), "zbi.signed"))
                }
                Image::ZBI { path: _, signed: false } => {
                    Ok(ImageMapping::new(image.source(), "zbi"))
                }
                Image::VBMeta(_) => Ok(ImageMapping::new(image.source(), "fuchsia.vbmeta")),
                _ => Err(anyhow!("Invalid primary image mapping")),
            },
            Slot::Recovery(_) => match image {
                Image::ZBI { path: _, signed: _ } => {
                    Ok(ImageMapping::new(image.source(), "recovery"))
                }
                Image::VBMeta(_) => Ok(ImageMapping::new(image.source(), "recovery.vbmeta")),
                _ => Err(anyhow!("Invalid recovery image mapping")),
            },
        }
    }

    fn metadata(&self) -> Result<ImageMetadata> {
        ImageMetadata::for_path(&self.source)
            .with_context(|| format!("Failed to read/hash {:?}", self.source))
    }
}

/// A PackageBuilder configured to build the update package or one of its subpackages.
struct SubpackageBuilder {
    package: PackageBuilder,
    package_name: String,
    manifest_path: PathBuf,
    far_path: PathBuf,
    gendir: PathBuf,
}

impl SubpackageBuilder {
    /// Build and publish an update package or one of its subpackages. Returns a merkle-pinned
    /// fuchsia-pkg:// URL for the package with the hostname set to "fuchsia.com".
    fn build(self, blobfs_builder: &mut BlobFSBuilder) -> Result<PinnedPkgUrl> {
        let SubpackageBuilder { package: builder, package_name, manifest_path, far_path, gendir } =
            self;

        let manifest = builder
            .build(&gendir, &far_path)
            .with_context(|| format!("Failed to build the {package_name} package"))?;

        blobfs_builder.add_package(&manifest_path).with_context(|| {
            format!("Adding the {package_name} package to update packages blobfs")
        })?;

        // Okay to unwrap because:
        // * "fuchsia.com" is a valid hostname,
        // * the formatted package path is valid if the package name and variant are valid, which
        //   both are statically known to be here, and
        // * all Hash instances are valid.
        let url = PinnedPkgUrl::new_package(
            "fuchsia.com".into(),
            format!(
                "/{}/{}",
                manifest.package_path().name().to_string(),
                manifest.package_path().variant().to_string()
            ),
            manifest.hash(),
        )
        .unwrap();

        Ok(url)
    }
}

impl UpdatePackageBuilder {
    /// Construct a new UpdatePackageBuilder with the minimal requirements for an UpdatePackage.
    pub fn new(
        tool_provider: Box<dyn ToolProvider>,
        partitions: PartitionsConfig,
        board_name: impl AsRef<str>,
        version_file: impl AsRef<Path>,
        epoch: EpochFile,
        outdir: impl AsRef<Path>,
    ) -> Self {
        Self {
            tool_provider,
            name: "update".into(),
            partitions,
            board_name: board_name.as_ref().into(),
            version_file: version_file.as_ref().to_path_buf(),
            epoch,
            slot_primary: None,
            slot_recovery: None,
            packages: UpdatePackagesManifest::default(),
            outdir: outdir.as_ref().to_path_buf(),
            gendir: outdir.as_ref().to_path_buf(),
        }
    }

    /// Set the name of the UpdatePackage.
    pub fn set_name(&mut self, name: impl AsRef<str>) {
        self.name = name.as_ref().to_string();
    }

    /// Set the directory for writing intermediate files.
    pub fn set_gendir(&mut self, gendir: impl AsRef<Path>) {
        self.gendir = gendir.as_ref().to_path_buf();
    }

    /// Update the images in |slot|.
    pub fn add_slot_images(&mut self, slot: Slot) {
        match slot {
            Slot::Primary(_) => self.slot_primary = Some(slot),
            Slot::Recovery(_) => self.slot_recovery = Some(slot),
        }
    }

    /// Add |packages| to the update.
    pub fn add_packages(&mut self, packages: UpdatePackagesManifest) {
        self.packages.append(packages);
    }

    /// Add the ZBI and VBMeta from the |slot| to the |map|.
    fn add_images_to_builder(slot: &Slot, builder: &mut PackageBuilder) -> Result<()> {
        let mappings: Vec<ImageMapping> = slot
            .manifest()
            .images
            .iter()
            .filter_map(|i| ImageMapping::try_from(i, slot).ok())
            .collect();
        for ImageMapping { source, destination } in mappings {
            builder.add_file_as_blob(destination, source.path_to_string()?)?;
        }
        Ok(())
    }

    /// Start building an update package or one of its subpackages, performing the steps that
    /// are common to all update packages.
    fn make_subpackage_builder(&self, subname: &str) -> Result<SubpackageBuilder> {
        let suffix = match subname {
            "" => subname.to_owned(),
            _ => format!("_{subname}"),
        };

        // The update package needs to be named 'update' to be accepted by the
        // `system-updater`.  Follow that convention for images packages as well.
        let package_name = format!("update{suffix}");
        let mut builder = PackageBuilder::new(&package_name);

        // However, they can have different published names.  And the name here
        // is the name to publish it under (and to include in the generated
        // package manifest).
        let base_publish_name = &self.name;
        let publish_name = format!("{base_publish_name}{suffix}");
        builder.published_name(publish_name);

        // Export the package's package manifest to paths that don't change
        // based on the configured publishing name.
        let manifest_path = self.outdir.join(format!("update{suffix}_package_manifest.json"));
        builder.manifest_path(&manifest_path);

        let far_path = self.outdir.join(format!("{package_name}.far"));
        let gendir = self.gendir.join(&package_name);

        Ok(SubpackageBuilder { package: builder, package_name, manifest_path, far_path, gendir })
    }

    /// Build the update package and associated update images packages.
    pub fn build(self) -> Result<()> {
        use serde_json::to_string;

        let blobfs_tool = self.tool_provider.get_tool("blobfs")?;
        let mut blobfs_builder = BlobFSBuilder::new(blobfs_tool, "compact");
        let mut images_manifest = ImagePackagesManifest::builder();

        // Generate the update_images_fuchsia package.
        let mut builder = self.make_subpackage_builder("images_fuchsia")?;
        if let Some(slot) = &self.slot_primary {
            let (zbi, vbmeta) =
                slot.zbi_and_vbmeta().ok_or(anyhow!("primary slot missing a zbi image"))?;

            builder.package.add_file_as_blob(&zbi.destination, zbi.source.path_to_string()?)?;

            if let Some(vbmeta) = &vbmeta {
                builder
                    .package
                    .add_file_as_blob(&vbmeta.destination, vbmeta.source.path_to_string()?)?;
            }

            let url = builder.build(&mut blobfs_builder)?;
            images_manifest.fuchsia_package(
                url,
                zbi.metadata()?,
                vbmeta.map(|vbmeta| vbmeta.metadata()).transpose()?,
            );
        } else {
            builder.build(&mut blobfs_builder)?;
        }

        // Generate the update_images_recovery package.
        let mut builder = self.make_subpackage_builder("images_recovery")?;
        if let Some(slot) = &self.slot_recovery {
            let (zbi, vbmeta) =
                slot.zbi_and_vbmeta().ok_or(anyhow!("recovery slot missing a zbi image"))?;

            builder.package.add_file_as_blob(&zbi.destination, zbi.source.path_to_string()?)?;

            if let Some(vbmeta) = &vbmeta {
                builder
                    .package
                    .add_file_as_blob(&vbmeta.destination, vbmeta.source.path_to_string()?)?;
            }

            let url = builder.build(&mut blobfs_builder)?;
            images_manifest.recovery_package(
                url,
                zbi.metadata()?,
                vbmeta.map(|vbmeta| vbmeta.metadata()).transpose()?,
            );
        } else {
            builder.build(&mut blobfs_builder)?;
        }

        // Generate the update_images_firmware package.
        let mut builder = self.make_subpackage_builder("images_firmware")?;
        if !self.partitions.bootloader_partitions.is_empty() {
            let mut firmware = BTreeMap::new();

            for bootloader in &self.partitions.bootloader_partitions {
                let destination = match bootloader.partition_type.as_str() {
                    "" => "firmware".to_string(),
                    t => format!("firmware_{}", t),
                };
                builder
                    .package
                    .add_file_as_blob(destination, bootloader.image.path_to_string()?)?;
                firmware.insert(
                    bootloader.partition_type.clone(),
                    ImageMetadata::for_path(&bootloader.image)
                        .with_context(|| format!("Failed to read/hash {:?}", &bootloader.image))?,
                );
            }

            let url = builder.build(&mut blobfs_builder)?;
            images_manifest.firmware_package(url, firmware);
        } else {
            builder.build(&mut blobfs_builder)?;
        }

        let images_manifest = images_manifest.build();

        // Generate the update package itself.
        let mut builder = self.make_subpackage_builder("")?;
        builder.package.add_contents_as_blob(
            "packages.json",
            to_string(&self.packages)?,
            &self.gendir,
        )?;
        builder.package.add_contents_as_blob(
            // Emit images.json as images.json.orig so the system-updater can differentiate
            // between an images.json that hasn't been modified by downstream tooling and one
            // that has. Once that tooling is modified to also modify/rename this manifest,
            // this can be updated to write to images.json directly.
            "images.json.orig",
            to_string(&images_manifest)?,
            &self.gendir,
        )?;
        builder.package.add_contents_as_blob(
            "epoch.json",
            to_string(&self.epoch)?,
            &self.gendir,
        )?;
        builder.package.add_contents_as_blob("board", &self.board_name, &self.gendir)?;

        builder.package.add_file_as_blob("version", self.version_file.path_to_string()?)?;

        // Add the images.
        let slots = vec![&self.slot_primary, &self.slot_recovery];
        for slot in slots.iter().filter_map(|s| s.as_ref()) {
            Self::add_images_to_builder(slot, &mut builder.package)?;
        }

        // Add the bootloaders.
        for bootloader in &self.partitions.bootloader_partitions {
            let destination = match bootloader.partition_type.as_str() {
                "" => "firmware".to_string(),
                t => format!("firmware_{}", t),
            };
            builder.package.add_file_as_blob(destination, bootloader.image.path_to_string()?)?;
        }
        builder.build(&mut blobfs_builder)?;

        // Generate a blobfs with the generated update package and update images packages inside
        // it. This is useful for packaging up the blobs to use in adversarial tests. We do not
        // care which blobfs layout we use, or whether it is compressed, because blobfs is mostly
        // just being used as a content-addressed container.
        let update_blob_path = self.gendir.join("update.blob.blk");
        blobfs_builder
            .build(self.gendir, update_blob_path)
            .context("Building blobfs for update package")?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assembly_images_manifest::Image;
    use assembly_partitions_config::Slot as PartitionSlot;
    use assembly_partitions_config::{BootloaderPartition, Partition, PartitionsConfig};
    use assembly_tool::testing::FakeToolProvider;
    use assembly_tool::{ToolCommandLog, ToolProvider};
    use assembly_update_packages_manifest::UpdatePackagesManifest;
    use fuchsia_archive::Reader;
    use fuchsia_hash::{Hash, HASH_SIZE};
    use fuchsia_pkg::{PackageManifest, PackagePath};
    use fuchsia_url::pkg_url::PkgUrl;
    use serde_json::json;
    use std::fs::File;
    use std::io::{BufReader, Write};
    use std::str::FromStr;
    use tempfile::{tempdir, NamedTempFile};

    #[test]
    fn build() {
        let outdir = tempdir().unwrap();
        let tools = FakeToolProvider::default();

        let fake_bootloader = NamedTempFile::new().unwrap();
        let partitions_config = PartitionsConfig {
            bootstrap_partitions: vec![],
            unlock_credentials: vec![],
            bootloader_partitions: vec![BootloaderPartition {
                partition_type: "tpl".into(),
                name: Some("firmware_tpl".into()),
                image: fake_bootloader.path().to_path_buf(),
            }],
            partitions: vec![Partition::ZBI { name: "zircon_a".into(), slot: PartitionSlot::A }],
            hardware_revision: "hw".into(),
        };
        let epoch = EpochFile::Version1 { epoch: 0 };
        let mut fake_version = NamedTempFile::new().unwrap();
        writeln!(fake_version, "1.2.3.4").unwrap();
        let mut builder = UpdatePackageBuilder::new(
            Box::new(tools.clone()),
            partitions_config,
            "board",
            fake_version.path().to_path_buf(),
            epoch.clone(),
            &outdir.path(),
        );

        // Add a ZBI to the update.
        let fake_zbi = NamedTempFile::new().unwrap();
        builder.add_slot_images(Slot::Primary(ImagesManifest {
            images: vec![Image::ZBI { path: fake_zbi.path().to_path_buf(), signed: true }],
        }));

        builder.build().unwrap();

        // Ensure the blobfs tool was invoked correctly.
        let blob_blk_path = outdir.path().join("update.blob.blk").path_to_string().unwrap();
        let blobs_json_path = outdir.path().join("blobs.json").path_to_string().unwrap();
        let blob_manifest_path = outdir.path().join("blob.manifest").path_to_string().unwrap();
        let expected_commands: ToolCommandLog = serde_json::from_value(json!({
            "commands": [
                {
                    "tool": "./host_x64/blobfs",
                    "args": [
                        "--json-output",
                        blobs_json_path,
                        blob_blk_path,
                        "create",
                        "--manifest",
                        blob_manifest_path,
                    ]
                }
            ]
        }))
        .unwrap();
        assert_eq!(&expected_commands, tools.log());

        let file = File::open(outdir.path().join("images.json.orig")).unwrap();
        let reader = BufReader::new(file);
        let i: serde_json::Value = serde_json::from_reader(reader).unwrap();
        assert_eq!(
            serde_json::json!({
                "version": "1",
                "contents": {
                    "fuchsia": {
                        "url": "fuchsia-pkg://fuchsia.com/update_images_fuchsia/0?hash=f9e2033364bdbb0e28d07882c8811a6219d266b7f5e1a7810b424f878ea19b30",
                        "images": {
                            "zbi": {
                                "hash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                                "size": 0,
                            },
                        },
                    },
                    "firmware": {
                        "url": "fuchsia-pkg://fuchsia.com/update_images_firmware/0?hash=795b05ab882f4f5959b50fcddeea18dba67b5ec6309c761460f0777e73c3f12d",
                        "images": {
                            "tpl": {
                                "hash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                                "size": 0,
                            },
                        },
                    },
                },
            }),
            i
        );

        let file = File::open(outdir.path().join("packages.json")).unwrap();
        let reader = BufReader::new(file);
        let p: UpdatePackagesManifest = serde_json::from_reader(reader).unwrap();
        assert_eq!(UpdatePackagesManifest::default(), p);

        let file = File::open(outdir.path().join("epoch.json")).unwrap();
        let reader = BufReader::new(file);
        let e: EpochFile = serde_json::from_reader(reader).unwrap();
        assert_eq!(epoch, e);

        let b = std::fs::read_to_string(outdir.path().join("board")).unwrap();
        assert_eq!("board", b);

        // Read the output and ensure it contains the right files (and their hashes).
        let far_path = outdir.path().join("update.far");
        let mut far_reader = Reader::new(File::open(&far_path).unwrap()).unwrap();
        let package = far_reader.read_file("meta/package").unwrap();
        assert_eq!(package, br#"{"name":"update","version":"0"}"#);
        let contents = far_reader.read_file("meta/contents").unwrap();
        let contents = std::str::from_utf8(&contents).unwrap();
        let expected_contents = "\
            board=9c579992f6e9f8cbd4ba81af6e23b1d5741e280af60f795e9c2bbcc76c4b7065\n\
            epoch.json=0362de83c084397826800778a1cf927280a5d5388cb1f828d77f74108726ad69\n\
            firmware_tpl=15ec7bf0b50732b49f8228e07d24365338f9e3ab994b00af08e5a3bffe55fd8b\n\
            images.json.orig=73894a190e5658901049dc9f83087309669b687ee5d6f40fd63de20e0196e76a\n\
            packages.json=85a3911ff39c118ee1a4be5f7a117f58a5928a559f456b6874440a7fb8c47a9a\n\
            version=d2ff44655653e2cbbecaf89dbf33a8daa8867e41dade2c6b4f127c3f0450c96b\n\
            zbi.signed=15ec7bf0b50732b49f8228e07d24365338f9e3ab994b00af08e5a3bffe55fd8b\n\
        "
        .to_string();
        assert_eq!(expected_contents, contents);

        let far_path = outdir.path().join("update_images_fuchsia.far");
        let mut far_reader = Reader::new(File::open(&far_path).unwrap()).unwrap();
        let package = far_reader.read_file("meta/package").unwrap();
        assert_eq!(package, br#"{"name":"update_images_fuchsia","version":"0"}"#);
        let contents = far_reader.read_file("meta/contents").unwrap();
        let contents = std::str::from_utf8(&contents).unwrap();
        let expected_contents = "\
            zbi=15ec7bf0b50732b49f8228e07d24365338f9e3ab994b00af08e5a3bffe55fd8b\n\
        "
        .to_string();
        assert_eq!(expected_contents, contents);

        let far_path = outdir.path().join("update_images_recovery.far");
        let mut far_reader = Reader::new(File::open(&far_path).unwrap()).unwrap();
        let package = far_reader.read_file("meta/package").unwrap();
        assert_eq!(package, br#"{"name":"update_images_recovery","version":"0"}"#);
        let contents = far_reader.read_file("meta/contents").unwrap();
        let contents = std::str::from_utf8(&contents).unwrap();
        let expected_contents = "\
        "
        .to_string();
        assert_eq!(expected_contents, contents);

        let far_path = outdir.path().join("update_images_firmware.far");
        let mut far_reader = Reader::new(File::open(&far_path).unwrap()).unwrap();
        let package = far_reader.read_file("meta/package").unwrap();
        assert_eq!(package, br#"{"name":"update_images_firmware","version":"0"}"#);
        let contents = far_reader.read_file("meta/contents").unwrap();
        let contents = std::str::from_utf8(&contents).unwrap();
        let expected_contents = "\
            firmware_tpl=15ec7bf0b50732b49f8228e07d24365338f9e3ab994b00af08e5a3bffe55fd8b\n\
        "
        .to_string();
        assert_eq!(expected_contents, contents);

        // Ensure the expected package fars/manifests were generated.
        assert!(outdir.path().join("update.far").exists());
        assert!(outdir.path().join("update_package_manifest.json").exists());
        assert!(outdir.path().join("update_images_fuchsia.far").exists());
        assert!(outdir.path().join("update_images_recovery.far").exists());
        assert!(outdir.path().join("update_images_firmware.far").exists());
        assert!(outdir.path().join("update_images_fuchsia_package_manifest.json").exists());
        assert!(outdir.path().join("update_images_recovery_package_manifest.json").exists());
        assert!(outdir.path().join("update_images_firmware_package_manifest.json").exists());
    }

    #[test]
    fn build_full() {
        let outdir = tempdir().unwrap();
        let tools = FakeToolProvider::default();

        let fake_bootloader = NamedTempFile::new().unwrap();
        let partitions_config = PartitionsConfig {
            bootstrap_partitions: vec![],
            unlock_credentials: vec![],
            bootloader_partitions: vec![BootloaderPartition {
                partition_type: "tpl".into(),
                name: Some("firmware_tpl".into()),
                image: fake_bootloader.path().to_path_buf(),
            }],
            partitions: vec![Partition::ZBI { name: "zircon_a".into(), slot: PartitionSlot::A }],
            hardware_revision: "hw".into(),
        };
        let epoch = EpochFile::Version1 { epoch: 0 };
        let mut fake_version = NamedTempFile::new().unwrap();
        writeln!(fake_version, "1.2.3.4").unwrap();
        let mut builder = UpdatePackageBuilder::new(
            Box::new(tools.clone()),
            partitions_config,
            "board",
            fake_version.path().to_path_buf(),
            epoch.clone(),
            &outdir.path(),
        );

        // Add a ZBI to the update.
        let fake_zbi = NamedTempFile::new().unwrap();
        builder.add_slot_images(Slot::Primary(ImagesManifest {
            images: vec![Image::ZBI { path: fake_zbi.path().to_path_buf(), signed: true }],
        }));

        // Add a Recovery ZBI/VBMeta to the update.
        let fake_recovery_zbi = NamedTempFile::new().unwrap();
        let fake_recovery_vbmeta = NamedTempFile::new().unwrap();
        builder.add_slot_images(Slot::Recovery(ImagesManifest {
            images: vec![
                Image::ZBI { path: fake_recovery_zbi.path().to_path_buf(), signed: true },
                Image::VBMeta(fake_recovery_vbmeta.path().to_path_buf()),
            ],
        }));

        builder.build().unwrap();

        // Ensure the blobfs tool was invoked correctly.
        let blob_blk_path = outdir.path().join("update.blob.blk").path_to_string().unwrap();
        let blobs_json_path = outdir.path().join("blobs.json").path_to_string().unwrap();
        let blob_manifest_path = outdir.path().join("blob.manifest").path_to_string().unwrap();
        let expected_commands: ToolCommandLog = serde_json::from_value(json!({
            "commands": [
                {
                    "tool": "./host_x64/blobfs",
                    "args": [
                        "--json-output",
                        blobs_json_path,
                        blob_blk_path,
                        "create",
                        "--manifest",
                        blob_manifest_path,
                    ]
                }
            ]
        }))
        .unwrap();
        assert_eq!(&expected_commands, tools.log());

        let file = File::open(outdir.path().join("images.json.orig")).unwrap();
        let reader = BufReader::new(file);
        let i: serde_json::Value = serde_json::from_reader(reader).unwrap();
        assert_eq!(
            serde_json::json!({
                "version": "1",
                "contents": {
                    "fuchsia": {
                        "url": "fuchsia-pkg://fuchsia.com/update_images_fuchsia/0?hash=f9e2033364bdbb0e28d07882c8811a6219d266b7f5e1a7810b424f878ea19b30",
                        "images": {
                            "zbi": {
                                "hash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                                "size": 0,
                            },
                        },
                    },
                    "recovery": {
                        "url": "fuchsia-pkg://fuchsia.com/update_images_recovery/0?hash=c0be02242469c633ddcaeaef1493b67158688f10e41131c7e19fbd2b5bc86acd",
                        "images": {
                            "vbmeta": {
                                "hash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                                "size": 0,
                            },
                            "zbi": {
                                "hash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                                "size": 0,
                            },
                        },
                    },
                    "firmware": {
                        "url": "fuchsia-pkg://fuchsia.com/update_images_firmware/0?hash=795b05ab882f4f5959b50fcddeea18dba67b5ec6309c761460f0777e73c3f12d",
                        "images": {
                            "tpl": {
                                "hash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                                "size": 0,
                            },
                        },
                    },
                },
            }),
            i
        );

        let file = File::open(outdir.path().join("packages.json")).unwrap();
        let reader = BufReader::new(file);
        let p: UpdatePackagesManifest = serde_json::from_reader(reader).unwrap();
        assert_eq!(UpdatePackagesManifest::default(), p);

        let file = File::open(outdir.path().join("epoch.json")).unwrap();
        let reader = BufReader::new(file);
        let e: EpochFile = serde_json::from_reader(reader).unwrap();
        assert_eq!(epoch, e);

        let b = std::fs::read_to_string(outdir.path().join("board")).unwrap();
        assert_eq!("board", b);

        // Read the output and ensure it contains the right files (and their hashes).
        let far_path = outdir.path().join("update.far");
        let mut far_reader = Reader::new(File::open(&far_path).unwrap()).unwrap();
        let package = far_reader.read_file("meta/package").unwrap();
        assert_eq!(package, br#"{"name":"update","version":"0"}"#);
        let contents = far_reader.read_file("meta/contents").unwrap();
        let contents = std::str::from_utf8(&contents).unwrap();
        let expected_contents = "\
            board=9c579992f6e9f8cbd4ba81af6e23b1d5741e280af60f795e9c2bbcc76c4b7065\n\
            epoch.json=0362de83c084397826800778a1cf927280a5d5388cb1f828d77f74108726ad69\n\
            firmware_tpl=15ec7bf0b50732b49f8228e07d24365338f9e3ab994b00af08e5a3bffe55fd8b\n\
            images.json.orig=9ad58f6f9d561c019c85fa616be925828a0d02b5c1805d02cebdbe47a4d5dbad\n\
            packages.json=85a3911ff39c118ee1a4be5f7a117f58a5928a559f456b6874440a7fb8c47a9a\n\
            recovery=15ec7bf0b50732b49f8228e07d24365338f9e3ab994b00af08e5a3bffe55fd8b\n\
            recovery.vbmeta=15ec7bf0b50732b49f8228e07d24365338f9e3ab994b00af08e5a3bffe55fd8b\n\
            version=d2ff44655653e2cbbecaf89dbf33a8daa8867e41dade2c6b4f127c3f0450c96b\n\
            zbi.signed=15ec7bf0b50732b49f8228e07d24365338f9e3ab994b00af08e5a3bffe55fd8b\n\
        "
        .to_string();
        assert_eq!(expected_contents, contents);

        let far_path = outdir.path().join("update_images_fuchsia.far");
        let mut far_reader = Reader::new(File::open(&far_path).unwrap()).unwrap();
        let package = far_reader.read_file("meta/package").unwrap();
        assert_eq!(package, br#"{"name":"update_images_fuchsia","version":"0"}"#);
        let contents = far_reader.read_file("meta/contents").unwrap();
        let contents = std::str::from_utf8(&contents).unwrap();
        let expected_contents = "\
            zbi=15ec7bf0b50732b49f8228e07d24365338f9e3ab994b00af08e5a3bffe55fd8b\n\
        "
        .to_string();
        assert_eq!(expected_contents, contents);

        let far_path = outdir.path().join("update_images_recovery.far");
        let mut far_reader = Reader::new(File::open(&far_path).unwrap()).unwrap();
        let package = far_reader.read_file("meta/package").unwrap();
        assert_eq!(package, br#"{"name":"update_images_recovery","version":"0"}"#);
        let contents = far_reader.read_file("meta/contents").unwrap();
        let contents = std::str::from_utf8(&contents).unwrap();
        let expected_contents = "\
            vbmeta=15ec7bf0b50732b49f8228e07d24365338f9e3ab994b00af08e5a3bffe55fd8b\n\
            zbi=15ec7bf0b50732b49f8228e07d24365338f9e3ab994b00af08e5a3bffe55fd8b\n\
        "
        .to_string();
        assert_eq!(expected_contents, contents);

        let far_path = outdir.path().join("update_images_firmware.far");
        let mut far_reader = Reader::new(File::open(&far_path).unwrap()).unwrap();
        let package = far_reader.read_file("meta/package").unwrap();
        assert_eq!(package, br#"{"name":"update_images_firmware","version":"0"}"#);
        let contents = far_reader.read_file("meta/contents").unwrap();
        let contents = std::str::from_utf8(&contents).unwrap();
        let expected_contents = "\
            firmware_tpl=15ec7bf0b50732b49f8228e07d24365338f9e3ab994b00af08e5a3bffe55fd8b\n\
        "
        .to_string();
        assert_eq!(expected_contents, contents);

        // Ensure the expected package fars/manifests were generated.
        assert!(outdir.path().join("update.far").exists());
        assert!(outdir.path().join("update_package_manifest.json").exists());
        assert!(outdir.path().join("update_images_fuchsia.far").exists());
        assert!(outdir.path().join("update_images_recovery.far").exists());
        assert!(outdir.path().join("update_images_firmware.far").exists());
        assert!(outdir.path().join("update_images_fuchsia_package_manifest.json").exists());
        assert!(outdir.path().join("update_images_recovery_package_manifest.json").exists());
        assert!(outdir.path().join("update_images_firmware_package_manifest.json").exists());
    }

    #[test]
    fn build_emits_empty_image_packages() {
        let outdir = tempdir().unwrap();
        let tools = FakeToolProvider::default();

        let partitions_config = PartitionsConfig::default();
        let epoch = EpochFile::Version1 { epoch: 0 };
        let mut fake_version = NamedTempFile::new().unwrap();
        writeln!(fake_version, "1.2.3.4").unwrap();
        let builder = UpdatePackageBuilder::new(
            Box::new(tools.clone()),
            partitions_config,
            "board",
            fake_version.path().to_path_buf(),
            epoch.clone(),
            &outdir.path(),
        );

        builder.build().unwrap();

        // Ensure the generated images.json manifest is empty.
        let file = File::open(outdir.path().join("images.json.orig")).unwrap();
        let reader = BufReader::new(file);
        let i: ::update_package::VersionedImagePackagesManifest =
            serde_json::from_reader(reader).unwrap();
        assert_eq!(ImagePackagesManifest::builder().build(), i);

        // Ensure the expected package fars/manifests were generated.
        assert!(outdir.path().join("update.far").exists());
        assert!(outdir.path().join("update_package_manifest.json").exists());
        assert!(outdir.path().join("update_images_fuchsia.far").exists());
        assert!(outdir.path().join("update_images_recovery.far").exists());
        assert!(outdir.path().join("update_images_firmware.far").exists());
        assert!(outdir.path().join("update_images_fuchsia_package_manifest.json").exists());
        assert!(outdir.path().join("update_images_recovery_package_manifest.json").exists());
        assert!(outdir.path().join("update_images_firmware_package_manifest.json").exists());
    }

    #[test]
    fn name() {
        let outdir = tempdir().unwrap();
        let tools = FakeToolProvider::default();

        let mut fake_version = NamedTempFile::new().unwrap();
        writeln!(fake_version, "1.2.3.4").unwrap();
        let mut builder = UpdatePackageBuilder::new(
            Box::new(tools.clone()),
            PartitionsConfig::default(),
            "board",
            fake_version.path().to_path_buf(),
            EpochFile::Version1 { epoch: 0 },
            &outdir.path(),
        );
        builder.set_name("update_2");
        assert!(builder.build().is_ok());

        // Read the package manifest and ensure it contains the updated name.
        let manifest_path = outdir.path().join("update_package_manifest.json");
        let manifest = PackageManifest::try_load_from(manifest_path).unwrap();
        assert_eq!("update_2", manifest.name().as_ref());
    }

    #[test]
    fn packages() {
        let outdir = tempdir().unwrap();
        let tools = FakeToolProvider::default();

        let mut fake_version = NamedTempFile::new().unwrap();
        writeln!(fake_version, "1.2.3.4").unwrap();
        let mut builder = UpdatePackageBuilder::new(
            Box::new(tools.clone()),
            PartitionsConfig::default(),
            "board",
            fake_version.path().to_path_buf(),
            EpochFile::Version1 { epoch: 0 },
            &outdir.path(),
        );

        let hash = Hash::from([0u8; HASH_SIZE]);
        let mut list1 = UpdatePackagesManifest::default();
        list1.add(PackagePath::from_str("one/0").unwrap(), hash.clone()).unwrap();
        list1.add(PackagePath::from_str("two/0").unwrap(), hash.clone()).unwrap();
        builder.add_packages(list1);

        let mut list2 = UpdatePackagesManifest::default();
        list2.add(PackagePath::from_str("three/0").unwrap(), hash.clone()).unwrap();
        list2.add(PackagePath::from_str("four/0").unwrap(), hash.clone()).unwrap();
        builder.add_packages(list2);

        assert!(builder.build().is_ok());

        // Read the package list and ensure it contains the correct contents.
        let package_list_path = outdir.path().join("packages.json");
        let package_list_file = File::open(package_list_path).unwrap();
        let list3: UpdatePackagesManifest = serde_json::from_reader(package_list_file).unwrap();
        let UpdatePackagesManifest::V1(pkg_urls) = list3;
        let pkg1 =
            PkgUrl::new_package("fuchsia.com".into(), "/one/0".into(), Some(hash.clone())).unwrap();
        println!("pkg_urls={:?}", &pkg_urls);
        println!("pkg={:?}", pkg1);
        assert!(pkg_urls.contains(&pkg1));
    }
}
