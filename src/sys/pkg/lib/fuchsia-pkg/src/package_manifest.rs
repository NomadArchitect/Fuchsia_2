// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{
        MetaContents, MetaPackage, Package, PackageManifestError, PackageName, PackagePath,
        PackageVariant,
    },
    fuchsia_hash::Hash,
    serde::{Deserialize, Serialize},
    std::path::PathBuf,
    std::{
        collections::BTreeMap,
        fs::{self, File},
        io,
        io::{Read, Seek, SeekFrom, Write},
        path::Path,
    },
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct PackageManifest(VersionedPackageManifest);

impl PackageManifest {
    pub fn blobs(&self) -> &[BlobInfo] {
        match &self.0 {
            VersionedPackageManifest::Version1(manifest) => &manifest.blobs,
        }
    }

    pub fn into_blobs(self) -> Vec<BlobInfo> {
        match self.0 {
            VersionedPackageManifest::Version1(manifest) => manifest.blobs,
        }
    }

    pub fn name(&self) -> &PackageName {
        match &self.0 {
            VersionedPackageManifest::Version1(manifest) => &manifest.package.name,
        }
    }

    pub async fn archive(
        self,
        root_dir: PathBuf,
        out: impl Write,
    ) -> Result<(), PackageManifestError> {
        let mut contents: BTreeMap<_, (_, Box<dyn Read>)> = BTreeMap::new();
        for blob in self.into_blobs() {
            let source_path = root_dir.join(blob.source_path);
            if blob.path == "meta/" {
                let mut meta_far_blob = File::open(&source_path).map_err(|err| {
                    PackageManifestError::IoErrorWithPath { cause: err, path: source_path }
                })?;
                meta_far_blob.seek(SeekFrom::Start(0))?;
                contents.insert(
                    "meta.far".to_string(),
                    (meta_far_blob.metadata()?.len(), Box::new(meta_far_blob)),
                );
            } else {
                let blob_file = File::open(&source_path).map_err(|err| {
                    PackageManifestError::IoErrorWithPath { cause: err, path: source_path }
                })?;
                contents.insert(
                    blob.merkle.to_string(),
                    (blob_file.metadata()?.len(), Box::new(blob_file)),
                );
            }
        }
        fuchsia_archive::write(out, contents)?;
        Ok(())
    }

    pub fn package_path(&self) -> PackagePath {
        match &self.0 {
            VersionedPackageManifest::Version1(manifest) => PackagePath::from_name_and_variant(
                manifest.package.name.to_owned(),
                manifest.package.version.to_owned(),
            ),
        }
    }

    /// Returns the merkle root of the meta.far.
    ///
    /// # Panics
    ///
    /// Panics if the PackageManifest is missing a "meta/" entry
    pub fn hash(&self) -> Hash {
        self.blobs().iter().find(|blob| blob.path == "meta/").unwrap().merkle
    }

    /// Create a `PackageManifest` from a blobs directory and the meta.far hash.
    ///
    /// This directory must be a flat file that contains all the package blobs.
    pub fn from_blobs_dir(dir: &Path, meta_far_hash: Hash) -> Result<Self, PackageManifestError> {
        let meta_far_path = dir.join(meta_far_hash.to_string());

        let mut meta_far_file = File::open(&meta_far_path)?;
        let meta_far_size = meta_far_file.metadata()?.len();

        let mut meta_far = fuchsia_archive::Reader::new(&mut meta_far_file)?;

        let meta_contents = meta_far.read_file("meta/contents")?;
        let meta_contents = MetaContents::deserialize(meta_contents.as_slice())?.into_contents();

        // The meta contents are unordered, so sort them to keep things consistent.
        let meta_contents = meta_contents.into_iter().collect::<BTreeMap<_, _>>();

        let meta_package = meta_far.read_file("meta/package")?;
        let meta_package = MetaPackage::deserialize(meta_package.as_slice())?;

        // Build the PackageManifest of this package.
        let mut builder = PackageManifestBuilder::new(meta_package);

        for (blob_path, merkle) in meta_contents.into_iter() {
            let source_path = dir.join(&merkle.to_string()).canonicalize()?;

            if !source_path.exists() {
                return Err(PackageManifestError::IoErrorWithPath {
                    cause: io::ErrorKind::NotFound.into(),
                    path: source_path,
                });
            }

            let size = fs::metadata(&source_path)?.len();

            builder = builder.add_blob(BlobInfo {
                source_path: source_path.into_os_string().into_string().map_err(|source_path| {
                    PackageManifestError::InvalidBlobPath {
                        merkle,
                        source_path: source_path.into(),
                    }
                })?,
                path: blob_path,
                merkle,
                size,
            });
        }

        // Add the meta.far blob.
        builder = builder.add_blob(BlobInfo {
            source_path: meta_far_path.into_os_string().into_string().map_err(|source_path| {
                PackageManifestError::InvalidBlobPath {
                    merkle: meta_far_hash,
                    source_path: source_path.into(),
                }
            })?,
            path: "meta/".into(),
            merkle: meta_far_hash,
            size: meta_far_size,
        });

        Ok(builder.build())
    }

    pub fn from_package(package: Package) -> Result<Self, PackageManifestError> {
        let mut blobs = Vec::with_capacity(package.blobs().len());
        for (blob_path, blob_entry) in package.blobs() {
            let source_path = blob_entry.source_path();

            blobs.push(BlobInfo {
                source_path: source_path.into_os_string().into_string().map_err(|source_path| {
                    PackageManifestError::InvalidBlobPath {
                        merkle: blob_entry.hash(),
                        source_path: source_path.into(),
                    }
                })?,
                path: blob_path,
                merkle: blob_entry.hash(),
                size: blob_entry.size(),
            })
        }
        let package_metadata = PackageMetadata {
            name: package.meta_package().name().to_owned(),
            version: package.meta_package().variant().to_owned(),
        };
        let manifest_v1 = PackageManifestV1 {
            package: package_metadata,
            blobs,
            repository: None,
            blob_sources_relative: Default::default(),
        };
        Ok(PackageManifest(VersionedPackageManifest::Version1(manifest_v1)))
    }
}

pub struct PackageManifestBuilder {
    manifest: PackageManifestV1,
}

impl PackageManifestBuilder {
    pub fn new(meta_package: MetaPackage) -> Self {
        Self {
            manifest: PackageManifestV1 {
                package: PackageMetadata {
                    name: meta_package.name().to_owned(),
                    version: meta_package.variant().to_owned(),
                },
                blobs: vec![],
                repository: None,
                blob_sources_relative: Default::default(),
            },
        }
    }

    pub fn repository(mut self, repository: impl Into<String>) -> Self {
        self.manifest.repository = Some(repository.into());
        self
    }

    pub fn add_blob(mut self, info: BlobInfo) -> Self {
        self.manifest.blobs.push(info);
        self
    }

    pub fn build(self) -> PackageManifest {
        PackageManifest(VersionedPackageManifest::Version1(self.manifest))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "version")]
enum VersionedPackageManifest {
    #[serde(rename = "1")]
    Version1(PackageManifestV1),
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
struct PackageManifestV1 {
    package: PackageMetadata,
    blobs: Vec<BlobInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    repository: Option<String>,

    /// Are the blob source_paths relative to the working dir (default, as made
    /// by 'pm') or the file containing the serialized manifest (new, portable,
    /// behavior)
    #[serde(default, skip_serializing_if = "RelativeTo::is_default")]
    blob_sources_relative: RelativeTo,
}

/// If the path is a relative path, what is it relative from?
///
/// If 'RelativeTo::WorkingDir', then the path is assumed to be relative to the
/// working dir, and can be used directly as a path.
///
/// If 'RelativeTo::File', then the path is relative to the file that contained
/// the path.  To use the path, it must be resolved against the path to the
/// file.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum RelativeTo {
    #[serde(rename = "working_dir")]
    WorkingDir,
    #[serde(rename = "file")]
    File,
}

impl Default for RelativeTo {
    fn default() -> Self {
        RelativeTo::WorkingDir
    }
}

impl RelativeTo {
    pub(crate) fn is_default(&self) -> bool {
        matches!(self, RelativeTo::WorkingDir)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
struct PackageMetadata {
    name: PackageName,
    version: PackageVariant,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct BlobInfo {
    pub source_path: String,
    pub path: String,
    pub merkle: fuchsia_merkle::Hash,
    pub size: u64,
}

pub mod host {
    use super::*;
    use crate::PathToStringExt;
    use anyhow::Context;
    use assembly_util::{path_relative_from_file, resolve_path_from_file};
    use std::fs::File;
    use std::io::BufReader;
    use std::path::Path;

    impl PackageManifest {
        pub fn try_load_from(path: impl AsRef<Path>) -> anyhow::Result<Self> {
            let manifest_path = path.as_ref();
            let manifest_str = manifest_path.path_to_string()?;
            let file = File::open(manifest_path)
                .context(format!("Opening package manifest: {}", &manifest_str))?;
            let versioned: VersionedPackageManifest = serde_json::from_reader(BufReader::new(file))
                .context(format!("Reading package manifest: {}", &manifest_str))?;
            let versioned = match versioned {
                VersionedPackageManifest::Version1(manifest) => VersionedPackageManifest::Version1(
                    manifest.resolve_blob_source_paths(&manifest_path)?,
                ),
            };
            Ok(Self(versioned))
        }

        pub fn write_with_relative_blob_paths(
            self,
            path: impl AsRef<Path>,
        ) -> anyhow::Result<Self> {
            let versioned = match self.0 {
                VersionedPackageManifest::Version1(manifest) => VersionedPackageManifest::Version1(
                    manifest.write_with_relative_blob_paths(path)?,
                ),
            };
            Ok(PackageManifest(versioned))
        }
    }

    impl PackageManifestV1 {
        pub fn write_with_relative_blob_paths(
            self,
            manifest_path: impl AsRef<Path>,
        ) -> anyhow::Result<Self> {
            let manifest = if let RelativeTo::WorkingDir = &self.blob_sources_relative {
                // manifest contains working-dir relative source paths, make
                // them relative to the file, instead.
                let blobs = self
                    .blobs
                    .into_iter()
                    .map(|blob| relativize_blob_source_path(blob, &manifest_path))
                    .collect::<anyhow::Result<Vec<BlobInfo>>>()?;
                Self { blobs, blob_sources_relative: RelativeTo::File, ..self }
            } else {
                self
            };
            let versioned_manifest = VersionedPackageManifest::Version1(manifest.clone());
            let file = File::create(manifest_path)?;
            serde_json::to_writer(file, &versioned_manifest)?;
            Ok(manifest)
        }
    }

    impl PackageManifestV1 {
        pub fn resolve_blob_source_paths(
            self,
            manifest_path: impl AsRef<Path>,
        ) -> anyhow::Result<Self> {
            if let RelativeTo::File = &self.blob_sources_relative {
                let blobs = self
                    .blobs
                    .into_iter()
                    .map(|blob| resolve_blob_source_path(blob, &manifest_path))
                    .collect::<anyhow::Result<Vec<BlobInfo>>>()?;
                Ok(Self { blobs, ..self })
            } else {
                Ok(self)
            }
        }
    }

    fn relativize_blob_source_path(
        blob: BlobInfo,
        manifest_path: impl AsRef<Path>,
    ) -> anyhow::Result<BlobInfo> {
        let source_path = path_relative_from_file(blob.source_path, manifest_path)?;
        let source_path = source_path.path_to_string().with_context(|| {
            format!(
                "Path from UTF-8 string, made relative, is no longer utf-8: {}",
                source_path.display()
            )
        })?;

        Ok(BlobInfo { source_path, ..blob })
    }

    fn resolve_blob_source_path(
        blob: BlobInfo,
        manifest_path: impl AsRef<Path>,
    ) -> anyhow::Result<BlobInfo> {
        let source_path = resolve_path_from_file(&blob.source_path, manifest_path)?
            .path_to_string()
            .context(format!("Resolving blob path: {}", &blob.source_path))?;
        Ok(BlobInfo { source_path, ..blob })
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::CreationManifest,
        fuchsia_merkle::Hash,
        pretty_assertions::assert_eq,
        serde_json::json,
        std::{path::PathBuf, str::FromStr},
        tempfile::TempDir,
    };

    #[test]
    fn test_version1_serialization() {
        let manifest = PackageManifest(VersionedPackageManifest::Version1(PackageManifestV1 {
            package: PackageMetadata {
                name: "example".parse().unwrap(),
                version: "0".parse().unwrap(),
            },
            blobs: vec![BlobInfo {
                source_path: "../p1".into(),
                path: "data/p1".into(),
                merkle: "0000000000000000000000000000000000000000000000000000000000000000"
                    .parse()
                    .unwrap(),
                size: 1,
            }],
            repository: None,
            blob_sources_relative: Default::default(),
        }));

        assert_eq!(
            serde_json::to_value(&manifest).unwrap(),
            json!(
                {
                    "version": "1",
                    "package": {
                        "name": "example",
                        "version": "0"
                    },
                    "blobs": [
                        {
                            "source_path": "../p1",
                            "path": "data/p1",
                            "merkle": "0000000000000000000000000000000000000000000000000000000000000000",
                            "size": 1
                        },
                    ]
                }
            )
        );

        let manifest = PackageManifest(VersionedPackageManifest::Version1(PackageManifestV1 {
            package: PackageMetadata {
                name: "example".parse().unwrap(),
                version: "0".parse().unwrap(),
            },
            blobs: vec![BlobInfo {
                source_path: "../p1".into(),
                path: "data/p1".into(),
                merkle: "0000000000000000000000000000000000000000000000000000000000000000"
                    .parse()
                    .unwrap(),
                size: 1,
            }],
            repository: Some("testrepository.org".into()),
            blob_sources_relative: RelativeTo::File,
        }));

        assert_eq!(
            serde_json::to_value(&manifest).unwrap(),
            json!(
                {
                    "version": "1",
                    "repository": "testrepository.org",
                    "package": {
                        "name": "example",
                        "version": "0"
                    },
                    "blobs": [
                        {
                            "source_path": "../p1",
                            "path": "data/p1",
                            "merkle": "0000000000000000000000000000000000000000000000000000000000000000",
                            "size": 1
                        },
                    ],
                    "blob_sources_relative": "file"
                }
            )
        );
    }

    #[test]
    fn test_version1_deserialization() {
        let manifest = serde_json::from_value::<VersionedPackageManifest>(json!(
            {
                "version": "1",
                "repository": "testrepository.org",
                "package": {
                    "name": "example",
                    "version": "0"
                },
                "blobs": [
                    {
                        "source_path": "../p1",
                        "path": "data/p1",
                        "merkle": "0000000000000000000000000000000000000000000000000000000000000000",
                        "size": 1
                    },
                ]
            }
        )).expect("valid json");

        assert_eq!(
            manifest,
            VersionedPackageManifest::Version1(PackageManifestV1 {
                package: PackageMetadata {
                    name: "example".parse().unwrap(),
                    version: "0".parse().unwrap(),
                },
                blobs: vec![BlobInfo {
                    source_path: "../p1".into(),
                    path: "data/p1".into(),
                    merkle: "0000000000000000000000000000000000000000000000000000000000000000"
                        .parse()
                        .unwrap(),
                    size: 1
                }],
                repository: Some("testrepository.org".into()),
                blob_sources_relative: Default::default(),
            })
        );

        let manifest = serde_json::from_value::<VersionedPackageManifest>(json!(
            {
                "version": "1",
                "package": {
                    "name": "example",
                    "version": "0"
                },
                "blobs": [
                    {
                        "source_path": "../p1",
                        "path": "data/p1",
                        "merkle": "0000000000000000000000000000000000000000000000000000000000000000",
                        "size": 1
                    },
                ],
                "blob_sources_relative": "file"
            }
        )).expect("valid json");

        assert_eq!(
            manifest,
            VersionedPackageManifest::Version1(PackageManifestV1 {
                package: PackageMetadata {
                    name: "example".parse().unwrap(),
                    version: "0".parse().unwrap(),
                },
                blobs: vec![BlobInfo {
                    source_path: "../p1".into(),
                    path: "data/p1".into(),
                    merkle: "0000000000000000000000000000000000000000000000000000000000000000"
                        .parse()
                        .unwrap(),
                    size: 1
                }],
                repository: None,
                blob_sources_relative: RelativeTo::File,
            })
        )
    }

    #[test]
    fn test_create_package_manifest_from_package() {
        let mut package_builder = Package::builder("package-name".parse().unwrap());
        package_builder.add_entry(
            String::from("bin/my_prog"),
            Hash::from_str("0000000000000000000000000000000000000000000000000000000000000000")
                .unwrap(),
            PathBuf::from("src/bin/my_prog"),
            1,
        );
        let package = package_builder.build().unwrap();
        let package_manifest = PackageManifest::from_package(package).unwrap();
        assert_eq!(&"package-name".parse::<PackageName>().unwrap(), package_manifest.name());
    }

    #[test]
    fn test_from_blobs_dir() {
        let temp = TempDir::new().unwrap();
        let gen_dir = temp.path().join("gen");
        std::fs::create_dir_all(&gen_dir).unwrap();

        let blobs_dir = temp.path().join("blobs");
        std::fs::create_dir_all(&blobs_dir).unwrap();

        // Helper to write some content into a blob.
        let write_blob = |contents| {
            let mut builder = fuchsia_merkle::MerkleTreeBuilder::new();
            builder.write(contents);
            let hash = builder.finish().root();

            let path = blobs_dir.join(hash.to_string());
            std::fs::write(&path, contents).unwrap();

            (path.to_str().unwrap().to_string(), hash)
        };

        // Create a package.
        let (file1_path, file1_hash) = write_blob(b"file 1");
        let (file2_path, file2_hash) = write_blob(b"file 2");

        std::fs::create_dir_all(gen_dir.join("meta")).unwrap();
        let meta_package_path = gen_dir.join("meta").join("package");
        std::fs::write(&meta_package_path, "{\"name\":\"package\",\"version\":\"0\"}").unwrap();

        let external_contents = BTreeMap::from([
            ("file-1".into(), file1_path.clone()),
            ("file-2".into(), file2_path.clone()),
        ]);

        let far_contents = BTreeMap::from([(
            "meta/package".into(),
            meta_package_path.to_str().unwrap().to_string(),
        )]);

        let creation_manifest =
            CreationManifest::from_external_and_far_contents(external_contents, far_contents)
                .unwrap();

        let gen_meta_far_path = temp.path().join("meta.far");
        let _package_manifest =
            crate::build::build(&creation_manifest, &gen_meta_far_path, "package");

        // Compute the meta.far hash, and copy it into the blobs/ directory.
        let meta_far_bytes = std::fs::read(&gen_meta_far_path).unwrap();
        let mut merkle_builder = fuchsia_merkle::MerkleTreeBuilder::new();
        merkle_builder.write(&meta_far_bytes);
        let meta_far_hash = merkle_builder.finish().root();

        let meta_far_path = blobs_dir.join(meta_far_hash.to_string());
        std::fs::write(&meta_far_path, &meta_far_bytes).unwrap();

        // We should be able to create a manifest from the blob directory that matches the one
        // created by the builder.
        assert_eq!(
            PackageManifest::from_blobs_dir(&blobs_dir, meta_far_hash).unwrap(),
            PackageManifest(VersionedPackageManifest::Version1(PackageManifestV1 {
                package: PackageMetadata {
                    name: "package".parse().unwrap(),
                    version: PackageVariant::zero(),
                },
                blobs: vec![
                    BlobInfo {
                        source_path: file1_path,
                        path: "file-1".into(),
                        merkle: file1_hash,
                        size: 6,
                    },
                    BlobInfo {
                        source_path: file2_path,
                        path: "file-2".into(),
                        merkle: file2_hash,
                        size: 6,
                    },
                    BlobInfo {
                        source_path: meta_far_path.to_str().unwrap().to_string(),
                        path: "meta/".into(),
                        merkle: meta_far_hash,
                        size: 12288,
                    },
                ],
                repository: None,
                blob_sources_relative: RelativeTo::WorkingDir,
            }))
        );
    }
}

#[cfg(all(test, not(target_os = "fuchsia")))]
mod host_tests {
    use super::*;
    use crate::PathToStringExt;
    use serde_json::Value;
    use std::fs::File;
    use tempfile::TempDir;

    #[test]
    fn test_load_from_simple() {
        let temp_dir = TempDir::new().unwrap();

        let data_dir = temp_dir.path().join("data_source");
        let manifest_dir = temp_dir.path().join("manifest_dir");
        let manifest_path = manifest_dir.join("package_manifest.json");
        let expected_blob_source_path = data_dir.join("p1").path_to_string().unwrap();

        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&manifest_dir).unwrap();

        let manifest = PackageManifest(VersionedPackageManifest::Version1(PackageManifestV1 {
            package: PackageMetadata {
                name: "example".parse().unwrap(),
                version: "0".parse().unwrap(),
            },
            blobs: vec![BlobInfo {
                source_path: expected_blob_source_path.clone(),
                path: "data/p1".into(),
                merkle: "0000000000000000000000000000000000000000000000000000000000000000"
                    .parse()
                    .unwrap(),
                size: 1,
            }],
            repository: None,
            blob_sources_relative: RelativeTo::WorkingDir,
        }));

        let manifest_file = File::create(&manifest_path).unwrap();
        serde_json::to_writer(manifest_file, &manifest).unwrap();

        let loaded_manifest = PackageManifest::try_load_from(&manifest_path).unwrap();
        assert_eq!(loaded_manifest.name(), &"example".parse::<PackageName>().unwrap());

        let blobs = loaded_manifest.into_blobs();
        assert_eq!(blobs.len(), 1);
        let blob = blobs.first().unwrap();
        assert_eq!(blob.path, "data/p1");
        assert_eq!(blob.source_path, expected_blob_source_path);
    }

    #[test]
    fn test_load_from_resolves_source_paths() {
        let temp_dir = TempDir::new().unwrap();

        let data_dir = temp_dir.path().join("data_source");
        let manifest_dir = temp_dir.path().join("manifest_dir");
        let manifest_path = manifest_dir.join("package_manifest.json");
        let expected_blob_source_path = data_dir.join("p1").path_to_string().unwrap();

        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&manifest_dir).unwrap();

        let manifest = PackageManifest(VersionedPackageManifest::Version1(PackageManifestV1 {
            package: PackageMetadata {
                name: "example".parse().unwrap(),
                version: "0".parse().unwrap(),
            },
            blobs: vec![BlobInfo {
                source_path: "../data_source/p1".into(),
                path: "data/p1".into(),
                merkle: "0000000000000000000000000000000000000000000000000000000000000000"
                    .parse()
                    .unwrap(),
                size: 1,
            }],
            repository: None,
            blob_sources_relative: RelativeTo::File,
        }));

        let manifest_file = File::create(&manifest_path).unwrap();
        serde_json::to_writer(manifest_file, &manifest).unwrap();

        let loaded_manifest = PackageManifest::try_load_from(&manifest_path).unwrap();
        assert_eq!(loaded_manifest.name(), &"example".parse::<PackageName>().unwrap());

        let blobs = loaded_manifest.into_blobs();
        assert_eq!(blobs.len(), 1);
        let blob = blobs.first().unwrap();
        assert_eq!(blob.path, "data/p1");
        assert_eq!(blob.source_path, expected_blob_source_path);
    }

    #[test]
    fn test_write_package_manifest_already_relative() {
        let temp_dir = TempDir::new().unwrap();

        let data_dir = temp_dir.path().join("data_source");
        let manifest_dir = temp_dir.path().join("manifest_dir");
        let manifest_path = manifest_dir.join("package_manifest.json");

        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&manifest_dir).unwrap();

        let manifest = PackageManifest(VersionedPackageManifest::Version1(PackageManifestV1 {
            package: PackageMetadata {
                name: "example".parse().unwrap(),
                version: "0".parse().unwrap(),
            },
            blobs: vec![BlobInfo {
                source_path: "../data_source/p1".into(),
                path: "data/p1".into(),
                merkle: "0000000000000000000000000000000000000000000000000000000000000000"
                    .parse()
                    .unwrap(),
                size: 1,
            }],
            repository: None,
            blob_sources_relative: RelativeTo::File,
        }));

        let result_manifest =
            manifest.clone().write_with_relative_blob_paths(&manifest_path).unwrap();

        // The manifest should not have been changed in this case.
        assert_eq!(result_manifest, manifest);

        let parsed_manifest: Value =
            serde_json::from_reader(File::open(manifest_path).unwrap()).unwrap();
        let object = parsed_manifest.as_object().unwrap();
        let version = object.get("version").unwrap();
        let blobs_value = object.get("blobs").unwrap();
        let blobs = blobs_value.as_array().unwrap();
        let blob_value = blobs.first().unwrap();
        let blob = blob_value.as_object().unwrap();
        let source_path_value = blob.get("source_path").unwrap();
        let source_path = source_path_value.as_str().unwrap();

        assert_eq!(version, "1");
        assert_eq!(source_path, "../data_source/p1");
    }

    #[test]
    fn test_write_package_manifest_making_paths_relative() {
        let temp_dir = TempDir::new().unwrap();

        let data_dir = temp_dir.path().join("data_source");
        let manifest_dir = temp_dir.path().join("manifest_dir");
        let manifest_path = manifest_dir.join("package_manifest.json");
        let blob_source_path = data_dir.join("p2").path_to_string().unwrap();

        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&manifest_dir).unwrap();

        let manifest = PackageManifest(VersionedPackageManifest::Version1(PackageManifestV1 {
            package: PackageMetadata {
                name: "example".parse().unwrap(),
                version: "0".parse().unwrap(),
            },
            blobs: vec![BlobInfo {
                source_path: blob_source_path,
                path: "data/p2".into(),
                merkle: "0000000000000000000000000000000000000000000000000000000000000000"
                    .parse()
                    .unwrap(),
                size: 1,
            }],
            repository: None,
            blob_sources_relative: RelativeTo::WorkingDir,
        }));

        let result_manifest =
            manifest.clone().write_with_relative_blob_paths(&manifest_path).unwrap();
        let blob = result_manifest.blobs().first().unwrap();
        assert_eq!(blob.source_path, "../data_source/p2");

        let parsed_manifest: serde_json::Value =
            serde_json::from_reader(File::open(manifest_path).unwrap()).unwrap();

        let object = parsed_manifest.as_object().unwrap();
        let blobs_value = object.get("blobs").unwrap();
        let blobs = blobs_value.as_array().unwrap();
        let blob_value = blobs.first().unwrap();
        let blob = blob_value.as_object().unwrap();
        let source_path_value = blob.get("source_path").unwrap();
        let source_path = source_path_value.as_str().unwrap();

        assert_eq!(source_path, "../data_source/p2");
    }
}
