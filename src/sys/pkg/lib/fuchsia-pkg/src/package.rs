// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{MetaContents, MetaContentsError, MetaPackage, MetaPackageError};
use anyhow::Result;
use fuchsia_merkle::Hash;
use std::collections::BTreeMap;
use std::io::{Read, Seek};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Package {
    meta_contents: MetaContents,
    meta_package: MetaPackage,
    blobs: BTreeMap<Hash, BlobEntry>,
}

impl Package {
    /// Get the meta_contents.
    pub fn meta_contents(&self) -> MetaContents {
        self.meta_contents.clone()
    }

    /// Get the meta_package.
    pub fn meta_package(&self) -> MetaPackage {
        self.meta_package.clone()
    }

    /// Get the meta_package.
    pub fn blobs(&self) -> BTreeMap<Hash, BlobEntry> {
        self.blobs.clone()
    }

    /// Create a new `PackageBuilder` from name and variant.
    pub fn builder(
        name: impl Into<String>,
        variant: impl Into<String>,
    ) -> Result<PackageBuilder, MetaPackageError> {
        PackageBuilder::new(name, variant)
    }

    /// Generate a Package from a meta.far file.
    pub fn from_meta_far<R: Read + Seek>(
        mut meta_far: R,
        blobs: BTreeMap<Hash, BlobEntry>,
    ) -> Result<Self> {
        let mut meta_far = fuchsia_archive::Reader::new(&mut meta_far)?;
        let meta_contents =
            MetaContents::deserialize(meta_far.read_file("meta/contents")?.as_slice())?;
        let meta_package =
            MetaPackage::deserialize(meta_far.read_file("meta/package")?.as_slice())?;
        Ok(Package { meta_contents, meta_package, blobs })
    }
}

pub struct PackageBuilder {
    contents: BTreeMap<String, Hash>,
    meta_package: MetaPackage,
    blobs: BTreeMap<Hash, BlobEntry>,
}

impl PackageBuilder {
    pub fn new(
        name: impl Into<String>,
        variant: impl Into<String>,
    ) -> Result<Self, MetaPackageError> {
        Ok(Self {
            contents: BTreeMap::new(),
            meta_package: MetaPackage::from_name_and_variant(name, variant)?,
            blobs: BTreeMap::new(),
        })
    }

    pub fn add_entry(&mut self, blob_path: String, hash: Hash, source_path: PathBuf, size: u64) {
        &self.contents.insert(blob_path, hash);
        &self.blobs.insert(hash, BlobEntry { path: source_path, size });
    }

    pub fn build(self) -> Result<Package, MetaContentsError> {
        Ok(Package {
            meta_contents: MetaContents::from_map(self.contents)?,
            meta_package: self.meta_package,
            blobs: self.blobs,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobEntry {
    path: PathBuf,
    size: u64,
}

impl BlobEntry {
    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn from_path_and_size(path: PathBuf, size: u64) -> Self {
        BlobEntry { path, size }
    }
}

#[cfg(test)]
mod test_package {
    use super::*;
    use crate::build::{build_with_file_system, FileSystem};
    use crate::CreationManifest;

    use fuchsia_merkle::Hash;
    use maplit::{btreemap, hashmap};
    use std::collections::HashMap;
    use std::fs::File;
    use std::io;
    use std::str::FromStr;
    use tempfile::tempdir;

    #[test]
    fn test_create_package() {
        let meta_package =
            MetaPackage::from_name_and_variant("package-name", "package-variant").unwrap();

        let map = btreemap! {
        "bin/my_prog".to_string() =>
            Hash::from_str(
             "0000000000000000000000000000000000000000000000000000000000000000")
            .unwrap(),
        "lib/mylib.so".to_string() =>
            Hash::from_str(
               "1111111111111111111111111111111111111111111111111111111111111111")
            .unwrap(),
            };
        let meta_contents = MetaContents::from_map(map).unwrap();
        let blob_entry = BlobEntry { path: PathBuf::from("src/bin/my_prog"), size: 1 };
        let blobs = btreemap! {
        Hash::from_str(
         "0000000000000000000000000000000000000000000000000000000000000000")
        .unwrap() => blob_entry.clone(),
        Hash::from_str(
           "1111111111111111111111111111111111111111111111111111111111111111")
        .unwrap() => blob_entry.clone(),
        };
        let package = Package {
            meta_contents: meta_contents.clone(),
            meta_package: meta_package.clone(),
            blobs: blobs.clone(),
        };
        assert_eq!(meta_package, package.meta_package());
        assert_eq!(meta_contents, package.meta_contents());
        assert_eq!(blobs, package.blobs());
    }

    struct FakeFileSystem {
        content_map: HashMap<String, Vec<u8>>,
    }

    impl<'a> FileSystem<'a> for FakeFileSystem {
        type File = &'a [u8];
        fn open(&'a self, path: &str) -> Result<Self::File, io::Error> {
            Ok(self.content_map.get(path).unwrap().as_slice())
        }
        fn len(&self, path: &str) -> Result<u64, io::Error> {
            Ok(self.content_map.get(path).unwrap().len() as u64)
        }
        fn read(&self, path: &str) -> Result<Vec<u8>, io::Error> {
            Ok(self.content_map.get(path).unwrap().clone())
        }
    }

    #[test]
    fn test_from_meta_far_valid_meta_far() {
        let creation_manifest = CreationManifest::from_external_and_far_contents(
            btreemap! {
                "lib/mylib.so".to_string() => "host/mylib.so".to_string()
            },
            btreemap! {
                "meta/my_component.cmx".to_string() => "host/my_component.cmx".to_string(),
                "meta/package".to_string() => "host/meta/package".to_string()
            },
        )
        .unwrap();
        let mut meta_far_writer = Vec::new();
        let component_manifest_contents = "my_component.cmx contents";
        let mut v = vec![];
        let meta_package =
            MetaPackage::from_name_and_variant("my-package-name", "my-package-variant").unwrap();
        meta_package.serialize(&mut v).unwrap();
        let file_system = FakeFileSystem {
            content_map: hashmap! {
                "host/mylib.so".to_string() => "mylib.so contents".as_bytes().to_vec(),
                "host/my_component.cmx".to_string() => component_manifest_contents.as_bytes().to_vec(),
                "host/meta/package".to_string() => v
            },
        };

        build_with_file_system(&creation_manifest, &mut meta_far_writer, &file_system).unwrap();

        let mut cursor = io::Cursor::new(meta_far_writer);
        let blob_entry = BlobEntry { path: PathBuf::from("src/bin/my_prog"), size: 1 };
        let blobs = btreemap! {
        Hash::from_str(
         "0000000000000000000000000000000000000000000000000000000000000000")
        .unwrap() => blob_entry.clone(),
        Hash::from_str(
           "1111111111111111111111111111111111111111111111111111111111111111")
        .unwrap() => blob_entry.clone(),
        };
        let package = Package::from_meta_far(&mut cursor, blobs.clone()).unwrap();
        assert_eq!(blobs, package.blobs());
        assert_eq!("my-package-name", package.meta_package().name());
    }

    #[test]
    fn test_from_meta_far_empty_meta_far() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("meta.far");
        File::create(&file_path).unwrap();
        let file = File::open(&file_path).unwrap();
        let blob_entry = BlobEntry { path: PathBuf::from("src/bin/my_prog"), size: 1 };
        let blobs = btreemap! {
        Hash::from_str(
         "0000000000000000000000000000000000000000000000000000000000000000")
        .unwrap() => blob_entry.clone(),
        Hash::from_str(
           "1111111111111111111111111111111111111111111111111111111111111111")
        .unwrap() => blob_entry.clone(),
        };
        let package = Package::from_meta_far(file, blobs.clone());
        assert!(package.is_err());
    }
}
