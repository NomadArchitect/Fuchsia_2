// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::util;
use anyhow::{anyhow, ensure, Context, Result};
use assembly_config::{
    self as image_assembly_config,
    product_config::{AssemblyInputBundle, PackageConfigPatch, StructuredConfigPatches},
    FileEntry,
};
use assembly_config_data::ConfigDataBuilder;
use assembly_structured_config::Repackager;
use assembly_util::{InsertAllUniqueExt, InsertUniqueExt, MapEntry};
use fuchsia_pkg::PackageManifest;
use image_assembly_config::product_config::ProductPackagesConfig;
use std::path::Path;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

type ConfigDataMap = BTreeMap<String, FileEntryMap>;

pub struct ImageAssemblyConfigBuilder {
    /// The base packages from the AssemblyInputBundles
    base: PackageSet,

    /// The cache packages from the AssemblyInputBundles
    cache: PackageSet,

    /// The system packages from the AssemblyInputBundles
    system: PackageSet,

    /// The bootfs packages from the AssemblyInputBundles
    bootfs_packages: PackageSet,

    /// The boot_args from the AssemblyInputBundles
    boot_args: BTreeSet<String>,

    /// The bootfs_files from the AssemblyInputBundles
    bootfs_files: FileEntryMap,

    /// The config_data entries, by package and by destination path.
    config_data: ConfigDataMap,

    // Modifications that must be made to structured config within packages.
    structured_config: StructuredConfigPatches,

    kernel_path: Option<PathBuf>,
    kernel_args: BTreeSet<String>,
    kernel_clock_backstop: Option<u64>,
}

impl Default for ImageAssemblyConfigBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageAssemblyConfigBuilder {
    pub fn new() -> Self {
        Self {
            base: PackageSet::new("base packages"),
            cache: PackageSet::new("cache packages"),
            system: PackageSet::new("system packages"),
            bootfs_packages: PackageSet::new("bootfs packages"),
            boot_args: BTreeSet::default(),
            bootfs_files: FileEntryMap::new("bootfs files"),
            config_data: ConfigDataMap::default(),
            structured_config: StructuredConfigPatches::default(),
            kernel_path: None,
            kernel_args: BTreeSet::default(),
            kernel_clock_backstop: None,
        }
    }

    /// Add an Assembly Input Bundle to the builder, via the path to its
    /// manifest.
    ///
    /// If any of the items it's trying to add are duplicates (either of itself
    /// or others, this will return an error.)
    pub fn add_bundle(&mut self, bundle_path: impl AsRef<Path>) -> Result<()> {
        let bundle = util::read_config(bundle_path.as_ref())?;

        // Strip filename from bundle path.
        let bundle_path = bundle_path.as_ref().parent().map(PathBuf::from).unwrap_or("".into());

        // Now add the parsed bundle
        self.add_parsed_bundle(bundle_path, bundle)
    }

    /// Add an Assembly Input Bundle to the builder, using a parsed
    /// AssemblyInputBundle, and the path to the folder that contains it.
    ///
    /// If any of the items it's trying to add are duplicates (either of itself
    /// or others, this will return an error.)
    pub fn add_parsed_bundle(
        &mut self,
        bundle_path: impl AsRef<Path>,
        bundle: AssemblyInputBundle,
    ) -> Result<()> {
        let bundle_path = bundle_path.as_ref();
        let AssemblyInputBundle { image_assembly: bundle, config_data, blobs: _ } = bundle;

        Self::add_bundle_packages(bundle_path, &bundle.base, &mut self.base)?;
        Self::add_bundle_packages(bundle_path, &bundle.cache, &mut self.cache)?;
        Self::add_bundle_packages(bundle_path, &bundle.system, &mut self.system)?;
        Self::add_bundle_packages(bundle_path, &bundle.bootfs_packages, &mut self.bootfs_packages)?;

        self.boot_args
            .try_insert_all_unique(bundle.boot_args)
            .map_err(|arg| anyhow!("duplicate boot_arg found: {}", arg))?;

        for entry in Self::file_entry_paths_from(bundle_path, bundle.bootfs_files) {
            self.bootfs_files.add_entry(entry)?;
        }

        if let Some(kernel) = bundle.kernel {
            assembly_util::set_option_once_or(
                &mut self.kernel_path,
                kernel.path.map(|p| bundle_path.join(p)),
                anyhow!("Only one input bundle can specify a kernel path"),
            )?;

            self.kernel_args
                .try_insert_all_unique(kernel.args)
                .map_err(|arg| anyhow!("duplicate kernel arg found: {}", arg))?;

            assembly_util::set_option_once_or(
                &mut self.kernel_clock_backstop,
                kernel.clock_backstop,
                anyhow!("Only one input bundle can specify a kernel clock backstop"),
            )?;
        }

        for (package, entries) in config_data {
            for entry in Self::file_entry_paths_from(bundle_path, entries) {
                self.add_config_data_entry(&package, entry)?;
            }
        }
        Ok(())
    }

    /// Add all the product-provided packages to the assembly configuration.
    ///
    /// This should be performed after the platform's bundles have been added,
    /// so that any packages that are in conflict with the platform bundles are
    /// flagged as being the issue (and not the platform being the issue).
    pub fn add_product_packages(&mut self, packages: &ProductPackagesConfig) -> Result<()> {
        for p in &packages.base {
            self.base.add_package_from_path(p)?
        }
        for p in &packages.cache {
            self.cache.add_package_from_path(p)?
        }
        Ok(())
    }

    /// Add a set of packages from a bundle, resolving each path to a package
    /// manifest from the bundle's path to locate it.
    fn add_bundle_packages(
        bundle_path: impl AsRef<Path>,
        bundle_package_paths: &[impl AsRef<Path>],
        package_set: &mut PackageSet,
    ) -> Result<()> {
        for path in bundle_package_paths {
            let path = bundle_path.as_ref().join(path);
            package_set.add_package_from_path(path)?;
        }
        Ok(())
    }

    fn file_entry_paths_from(
        base: &Path,
        entries: impl IntoIterator<Item = FileEntry>,
    ) -> Vec<FileEntry> {
        entries
            .into_iter()
            .map(|entry| FileEntry {
                destination: entry.destination,
                source: base.join(entry.source),
            })
            .collect()
    }

    /// Add an entry to `config_data` for the given package.  If the entry
    /// duplicates an existing entry, return an error.
    fn add_config_data_entry(&mut self, package: impl AsRef<str>, entry: FileEntry) -> Result<()> {
        self.config_data.entry(package.as_ref().into()).or_default().add_entry(entry)
    }

    /// Set the structured configuration updates for a package. Can only be called once per
    /// package.
    pub fn set_structured_config(
        &mut self,
        package: impl AsRef<str>,
        config: PackageConfigPatch,
    ) -> Result<()> {
        if self.structured_config.insert(package.as_ref().to_owned(), config).is_none() {
            Ok(())
        } else {
            Err(anyhow::format_err!("duplicate config patch"))
        }
    }

    /// Construct an ImageAssembly ImageAssemblyConfig from the collected items in the
    /// builder.
    ///
    /// If there are config_data entries, the config_data package will be
    /// created in the outdir, and it will be added to the returned
    /// ImageAssemblyConfig.
    ///
    /// If this cannot create a completed ImageAssemblyConfig, it will return an error
    /// instead.
    pub fn build(
        self,
        outdir: impl AsRef<Path>,
    ) -> Result<image_assembly_config::ImageAssemblyConfig> {
        let outdir = outdir.as_ref();
        // Decompose the fields in self, so that they can be recomposed into the generated
        // image assembly configuration.
        let Self {
            structured_config,
            mut base,
            mut cache,
            mut system,
            boot_args,
            bootfs_files,
            bootfs_packages,
            config_data,
            kernel_path,
            kernel_args,
            kernel_clock_backstop,
        } = self;

        // repackage any matching packages, ignoring whether we actually succeed. if a patch has
        // been provided that doesn't match a package, we silently skip it and let product
        // validation catch any issues
        for (package, config) in structured_config {
            // get the manifest for this package name, returning the set from which it was removed
            if let Some((manifest, source_package_set)) =
                remove_package_from_sets(&package, [&mut base, &mut cache, &mut system])
                    .with_context(|| format!("removing {} for repackaging", package))?
            {
                let outdir = outdir.join("repackaged").join(&package);
                let mut repackager = Repackager::new(manifest, &outdir)
                    .with_context(|| format!("reading existing manifest for {}", package))?;
                for (component, values) in &config.components {
                    repackager
                        .set_component_config(component, values.fields.clone())
                        .with_context(|| format!("setting new config for {}", component))?;
                }
                let new_path = repackager
                    .build()
                    .with_context(|| format!("building repackaged {}", package))?;
                let new_entry = PackageEntry::parse_from(new_path)
                    .with_context(|| format!("parsing repackaged {}", package))?;
                source_package_set.insert(new_entry.name().to_owned(), new_entry);
            }
        }

        if !config_data.is_empty() {
            // Build the config_data package
            let mut config_data_builder = ConfigDataBuilder::default();
            for (package_name, entries) in config_data {
                for entry in entries.into_file_entries() {
                    config_data_builder.add_entry(
                        &package_name,
                        entry.destination.into(),
                        entry.source,
                    )?;
                }
            }
            let manifest_path = config_data_builder
                .build(&outdir)
                .context("Writing the 'config_data' package metafar.")?;
            let entry = PackageEntry::parse_from(manifest_path)
                .context("parsing generated config-data package")?;
            base.try_insert_unique(entry.name().to_owned(), entry).map_err(|_| {
                anyhow!("found a duplicate config_data package when adding generated one.")
            })?;
        }

        // Construct a single "partial" config from the combined fields, and
        // then pass this to the ImageAssemblyConfig::try_from_partials() to get the
        // final validation that it's complete.
        let partial = image_assembly_config::PartialImageAssemblyConfig {
            system: system.into_paths().collect(),
            base: base.into_paths().collect(),
            cache: cache.into_paths().collect(),
            kernel: Some(image_assembly_config::PartialKernelConfig {
                path: kernel_path,
                args: kernel_args.into_iter().collect(),
                clock_backstop: kernel_clock_backstop,
            }),
            boot_args: boot_args.into_iter().collect(),
            bootfs_files: bootfs_files.into_file_entries(),
            bootfs_packages: bootfs_packages.into_paths().collect(),
        };

        let image_assembly_config = image_assembly_config::ImageAssemblyConfig::try_from_partials(
            std::iter::once(partial),
        )?;

        Ok(image_assembly_config)
    }
}

/// Remove a package with a matching name from the provided package sets, returning its parsed
/// manifest and a mutable reference to the set from which it was removed.
fn remove_package_from_sets<'a, 'b: 'a, const N: usize>(
    package_name: &str,
    package_sets: [&'a mut PackageSet; N],
) -> anyhow::Result<Option<(PackageManifest, &'a mut PackageSet)>> {
    let mut matches_name = None;

    for package_set in package_sets {
        if let Some(entry) = package_set.remove(package_name) {
            ensure!(
                matches_name.is_none(),
                "only one package with a given name is allowed per product"
            );
            matches_name = Some((entry.manifest, package_set));
        }
    }

    Ok(matches_name)
}

#[derive(Debug)]
struct PackageEntry {
    path: PathBuf,
    manifest: PackageManifest,
}

impl PackageEntry {
    fn parse_from(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_owned();
        let manifest = PackageManifest::try_load_from(&path)
            .context(format!("parsing {} as a package manifest", path.display()))?;
        Ok(Self { path, manifest })
    }

    fn name(&self) -> &str {
        self.manifest.name().as_ref()
    }
}

#[derive(Default, Debug)]
/// A named set of things, which are mapped by a String key.

struct NamedMap<T> {
    /// The name of the Map.
    name: String,

    /// The entries in the map.
    entries: BTreeMap<String, T>,
}

impl<T> NamedMap<T>
where
    T: std::fmt::Debug,
{
    /// Create a new, named, map.
    fn new(name: &str) -> Self {
        Self { name: name.to_owned(), entries: BTreeMap::new() }
    }

    fn try_insert_unique(&mut self, name: String, value: T) -> Result<()> {
        let result =
            self.entries.try_insert_unique(MapEntry(name, value)).map_err(|e| format!("{:?}", e));
        // The error is mapped a second time to separate the borrow of entries
        // from the borrow of name.
        result.map_err(|e| anyhow!("duplicate entry for {}: {}", self.name, e))
    }
}

impl<T> std::ops::Deref for NamedMap<T> {
    type Target = BTreeMap<String, T>;

    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}

impl<T> std::ops::DerefMut for NamedMap<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.entries
    }
}

impl<T> IntoIterator for NamedMap<T> {
    type Item = T;

    type IntoIter = std::collections::btree_map::IntoValues<String, Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_values()
    }
}

/// A named set of packages with their manifests parsed into memory, keyed by package name.
type PackageSet = NamedMap<PackageEntry>;
impl PackageSet {
    /// Parse the given path as a PackageManifest, and add it to the PackageSet.
    fn add_package_from_path<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
        {
            let entry = PackageEntry::parse_from(path)?;
            self.try_insert_unique(entry.name().to_owned(), entry)
        }
        .with_context(|| format!("Adding package to set: {}", self.name))
    }

    /// Convert the PackageSet into an iterable collection of Paths.
    fn into_paths(self) -> impl Iterator<Item = PathBuf> {
        self.entries.into_values().map(|e| e.path)
    }
}

type FileEntryMap = NamedMap<PathBuf>;
impl FileEntryMap {
    fn add_entry(&mut self, entry: FileEntry) -> Result<()> {
        self.try_insert_unique(entry.destination, entry.source)
            .with_context(|| format!("Adding entry to set: {}", self.name))
    }

    fn into_file_entries(self) -> Vec<FileEntry> {
        self.entries
            .into_iter()
            .map(|(destination, source)| FileEntry { destination, source })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use fuchsia_pkg::{PackageBuilder, PackageManifest};
    use std::fs::File;
    use tempfile::TempDir;

    fn write_empty_pkg(path: impl AsRef<Path>, name: &str) -> Utf8PathBuf {
        let path = path.as_ref();
        let mut builder = PackageBuilder::new(name);
        let manifest_path = path.join(name);
        builder.manifest_path(&manifest_path);
        builder.build(path, path.join(format!("{}_meta.far", name))).unwrap();
        Utf8PathBuf::from_path_buf(manifest_path).unwrap()
    }

    fn make_test_assembly_bundle(bundle_path: &Path) -> AssemblyInputBundle {
        let write_empty_bundle_pkg = |name: &str| write_empty_pkg(bundle_path, name).into();
        AssemblyInputBundle {
            image_assembly: image_assembly_config::PartialImageAssemblyConfig {
                base: vec![write_empty_bundle_pkg("base_package0")],
                system: vec![write_empty_bundle_pkg("sys_package0")],
                cache: vec![write_empty_bundle_pkg("cache_package0")],
                bootfs_packages: vec![write_empty_bundle_pkg("bootfs_package0")],
                kernel: Some(image_assembly_config::PartialKernelConfig {
                    path: Some("kernel/path".into()),
                    args: vec!["kernel_arg0".into()],
                    clock_backstop: Some(56244),
                }),
                boot_args: vec!["boot_arg0".into()],
                bootfs_files: vec![FileEntry {
                    source: "source/path/to/file".into(),
                    destination: "dest/file/path".into(),
                }],
            },
            config_data: BTreeMap::default(),
            blobs: Vec::default(),
        }
    }

    #[test]
    fn test_builder() {
        let outdir = TempDir::new().unwrap();
        let mut builder = ImageAssemblyConfigBuilder::default();
        builder.add_parsed_bundle(outdir.path(), make_test_assembly_bundle(outdir.path())).unwrap();
        let result: image_assembly_config::ImageAssemblyConfig = builder.build(&outdir).unwrap();

        assert_eq!(result.base, vec![outdir.path().join("base_package0")]);
        assert_eq!(result.cache, vec![outdir.path().join("cache_package0")]);
        assert_eq!(result.system, vec![outdir.path().join("sys_package0")]);
        assert_eq!(result.bootfs_packages, vec![outdir.path().join("bootfs_package0")]);
        assert_eq!(result.boot_args, vec!("boot_arg0".to_string()));
        assert_eq!(
            result.bootfs_files,
            vec!(FileEntry {
                source: outdir.path().join("source/path/to/file"),
                destination: "dest/file/path".into()
            })
        );

        assert_eq!(result.kernel.path, outdir.path().join("kernel/path"));
        assert_eq!(result.kernel.args, vec!("kernel_arg0".to_string()));
        assert_eq!(result.kernel.clock_backstop, 56244);
    }

    #[test]
    fn test_builder_with_config_data() {
        let outdir = TempDir::new().unwrap();
        let mut builder = ImageAssemblyConfigBuilder::default();

        // Write a file to the temp dir for use with config_data.
        let bundle_path = outdir.path().join("bundle");
        let config_data_target_package_name = "base_package0";
        let config_data_target_package_dir =
            bundle_path.join("config_data").join(config_data_target_package_name);
        let config_data_file_path = config_data_target_package_dir.join("config_data_source_file");
        std::fs::create_dir_all(&config_data_target_package_dir).unwrap();
        std::fs::write(&config_data_file_path, "configuration data").unwrap();

        // Create an assembly bundle and add a config_data entry to it.
        let mut bundle = make_test_assembly_bundle(&bundle_path);
        bundle.config_data.insert(
            config_data_target_package_name.to_string(),
            vec![FileEntry {
                source: config_data_file_path,
                destination: "dest/file/path".to_owned(),
            }],
        );

        builder.add_parsed_bundle(&bundle_path, bundle).unwrap();
        let result: image_assembly_config::ImageAssemblyConfig = builder.build(&outdir).unwrap();

        // config_data's manifest is in outdir
        let expected_config_data_manifest_path =
            outdir.path().join("config_data").join("package_manifest.json");

        // Validate that the base package set contains config_data.
        assert_eq!(result.base.len(), 2);
        assert!(result.base.contains(&bundle_path.join("base_package0")));
        assert!(result.base.contains(&expected_config_data_manifest_path));

        // Validate the contents of config_data is what is, expected by:
        // 1.  Reading in the package manifest to get the metafar path
        // 2.  Opening the metafar
        // 3.  Reading the config_data entry's file
        // 4.  Validate the contents of the file

        // 1. Read the config_data package manifest
        let config_data_manifest =
            PackageManifest::try_load_from(expected_config_data_manifest_path).unwrap();
        assert_eq!(config_data_manifest.name().as_ref(), "config-data");

        // and get the metafar path.
        let blobs = config_data_manifest.into_blobs();
        let metafar_blobinfo = blobs.get(0).unwrap();
        assert_eq!(metafar_blobinfo.path, "meta/");

        // 2. Read the metafar.
        let mut config_data_metafar = File::open(&metafar_blobinfo.source_path).unwrap();
        let mut far_reader = fuchsia_archive::Reader::new(&mut config_data_metafar).unwrap();

        // 3.  Read the configuration file.
        let config_file_data = far_reader
            .read_file(&format!("meta/data/{}/dest/file/path", config_data_target_package_name))
            .unwrap();

        // 4.  Validate its contents.
        assert_eq!(config_file_data, "configuration data".as_bytes());
    }

    #[test]
    fn test_builder_with_product_packages() {
        let outdir = TempDir::new().unwrap();

        let packages = ProductPackagesConfig {
            base: vec![write_empty_pkg(&outdir, "base_a"), write_empty_pkg(&outdir, "base_b")],
            cache: vec![write_empty_pkg(&outdir, "cache_a"), write_empty_pkg(&outdir, "cache_b")],
        };
        let minimum_bundle = AssemblyInputBundle {
            image_assembly: image_assembly_config::PartialImageAssemblyConfig {
                base: vec![
                    write_empty_pkg(&outdir, "platform_a").into_std_path_buf(),
                    write_empty_pkg(&outdir, "platform_b").into_std_path_buf(),
                ],
                kernel: Some(image_assembly_config::PartialKernelConfig {
                    path: Some("kernel/path".into()),
                    args: Vec::default(),
                    clock_backstop: Some(0),
                }),
                ..image_assembly_config::PartialImageAssemblyConfig::default()
            },
            config_data: BTreeMap::default(),
            blobs: Vec::default(),
        };
        let mut builder = ImageAssemblyConfigBuilder::default();
        builder.add_parsed_bundle(outdir.path().join("minimum_bundle"), minimum_bundle).unwrap();
        builder.add_product_packages(&packages).unwrap();
        let result: image_assembly_config::ImageAssemblyConfig = builder.build(&outdir).unwrap();

        assert_eq!(
            result.base,
            ["base_a", "base_b", "platform_a", "platform_b"]
                .iter()
                .map(|p| outdir.path().join(p))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            result.cache,
            vec![outdir.path().join("cache_a"), outdir.path().join("cache_b")]
        );
    }

    #[test]
    fn test_builder_with_product_packages_catches_duplicates() {
        let outdir = TempDir::new().unwrap();

        let packages = ProductPackagesConfig {
            base: vec![write_empty_pkg(&outdir, "base_a")],
            ..ProductPackagesConfig::default()
        };
        let minimum_bundle = AssemblyInputBundle {
            image_assembly: image_assembly_config::PartialImageAssemblyConfig {
                base: vec![write_empty_pkg(&outdir, "base_a").into_std_path_buf()],
                kernel: Some(image_assembly_config::PartialKernelConfig {
                    path: Some("kernel/path".into()),
                    args: Vec::default(),
                    clock_backstop: Some(0),
                }),
                ..image_assembly_config::PartialImageAssemblyConfig::default()
            },
            config_data: BTreeMap::default(),
            blobs: Vec::default(),
        };
        let mut builder = ImageAssemblyConfigBuilder::default();
        builder.add_parsed_bundle(outdir.path().join("minimum_bundle"), minimum_bundle).unwrap();
        let result = builder.add_product_packages(&packages);
        assert!(result.is_err());
    }

    /// Helper to duplicate the first item in an Vec<T: Clone> and make it also
    /// the last item. This intentionally panics if the Vec is empty.
    fn duplicate_first<T: Clone>(vec: &mut Vec<T>) {
        vec.push(vec.first().unwrap().clone());
    }

    #[test]
    fn test_builder_catches_dupe_base_pkgs_in_aib() {
        let temp = TempDir::new().unwrap();
        let mut aib = make_test_assembly_bundle(temp.path());
        duplicate_first(&mut aib.image_assembly.base);

        let mut builder = ImageAssemblyConfigBuilder::default();
        assert!(builder.add_parsed_bundle(temp.path(), aib).is_err());
    }

    #[test]
    fn test_builder_catches_dupe_cache_pkgs_in_aib() {
        let temp = TempDir::new().unwrap();
        let mut aib = make_test_assembly_bundle(temp.path());
        duplicate_first(&mut aib.image_assembly.cache);

        let mut builder = ImageAssemblyConfigBuilder::default();
        assert!(builder.add_parsed_bundle(temp.path(), aib).is_err());
    }

    #[test]
    fn test_builder_catches_dupe_system_pkgs_in_aib() {
        let temp = TempDir::new().unwrap();
        let mut aib = make_test_assembly_bundle(temp.path());
        duplicate_first(&mut aib.image_assembly.system);

        let mut builder = ImageAssemblyConfigBuilder::default();
        assert!(builder.add_parsed_bundle(temp.path(), aib).is_err());
    }

    #[test]
    fn test_builder_catches_dupe_bootfs_pkgs_in_aib() {
        let temp = TempDir::new().unwrap();
        let mut aib = make_test_assembly_bundle(temp.path());
        duplicate_first(&mut aib.image_assembly.bootfs_packages);

        let mut builder = ImageAssemblyConfigBuilder::default();
        assert!(builder.add_parsed_bundle(temp.path(), aib).is_err());
    }

    fn test_duplicates_across_aibs_impl<
        T: Clone,
        F: Fn(&mut AssemblyInputBundle) -> &mut Vec<T>,
    >(
        accessor: F,
    ) {
        let outdir = TempDir::new().unwrap();
        let mut aib = make_test_assembly_bundle(outdir.path());
        let mut second_aib = AssemblyInputBundle::default();

        let first_list = (accessor)(&mut aib);
        let second_list = (accessor)(&mut second_aib);

        // Clone the first item in the first AIB into the same list in the
        // second AIB to create a duplicate item across the two AIBs.
        let value = first_list.get(0).unwrap();
        second_list.push(value.clone());

        let mut builder = ImageAssemblyConfigBuilder::default();
        builder.add_parsed_bundle(outdir.path(), aib).unwrap();
        assert!(builder.add_parsed_bundle(outdir.path().join("second"), second_aib).is_err());
    }

    #[test]
    #[ignore] // As packages from different bundles have different paths,
              // this isn't currently working
    fn test_builder_catches_dupe_base_pkgs_across_aibs() {
        test_duplicates_across_aibs_impl(|a| &mut a.image_assembly.base);
    }

    #[test]
    #[ignore] // As packages from different bundles have different paths,
              // this isn't currently working
    fn test_builder_catches_dupe_cache_pkgs_across_aibs() {
        test_duplicates_across_aibs_impl(|a| &mut a.image_assembly.cache);
    }

    #[test]
    #[ignore] // As packages from different bundles have different paths,
              // this isn't currently working
    fn test_builder_catches_dupe_system_pkgs_across_aibs() {
        test_duplicates_across_aibs_impl(|a| &mut a.image_assembly.system);
    }

    #[test]
    fn test_builder_catches_dupe_bootfs_files_across_aibs() {
        test_duplicates_across_aibs_impl(|a| &mut a.image_assembly.bootfs_files);
    }

    #[test]
    fn test_builder_catches_dupe_config_data_across_aibs() {
        let temp = TempDir::new().unwrap();
        let mut first_aib = make_test_assembly_bundle(temp.path());
        let mut second_aib = AssemblyInputBundle::default();

        let config_data_file_entry = FileEntry {
            source: "source/path/to/file".into(),
            destination: "dest/file/path".into(),
        };

        first_aib.config_data.insert("base_package0".into(), vec![config_data_file_entry.clone()]);
        second_aib.config_data.insert("base_package0".into(), vec![config_data_file_entry]);

        let mut builder = ImageAssemblyConfigBuilder::default();
        builder.add_parsed_bundle(temp.path(), first_aib).unwrap();
        assert!(builder.add_parsed_bundle(temp.path().join("second"), second_aib).is_err());
    }
}
