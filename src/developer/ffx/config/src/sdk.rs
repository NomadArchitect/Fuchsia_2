// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{anyhow, Context, Result};
use log::warn;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::convert::TryInto;

enum RealPaths {
    Map(HashMap<String, String>),
    Prefix(std::path::PathBuf),
}

impl RealPaths {
    fn produce(&self, path: &str) -> Result<std::path::PathBuf> {
        match self {
            RealPaths::Map(m) => m
                .get(path)
                .map(std::path::PathBuf::from)
                .ok_or(anyhow!("SDK File '{}' has no source in the build directory", path)),
            RealPaths::Prefix(p) => Ok(p.join(path)),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum SdkVersion {
    Version(String),
    InTree,
    Unknown,
}

pub struct Sdk {
    metas: Vec<Value>,
    real_paths: RealPaths,
    version: SdkVersion,
}

#[derive(Deserialize)]
struct SdkAtoms {
    #[cfg(test)]
    ids: Vec<Value>,
    atoms: Vec<Atom>,
}

#[derive(Deserialize)]
struct Atom {
    #[cfg(test)]
    category: String,
    #[cfg(test)]
    deps: Vec<String>,
    files: Vec<File>,
    #[serde(rename = "gn-label")]
    #[cfg(test)]
    gn_label: String,
    #[cfg(test)]
    id: String,
    meta: String,
    #[serde(rename = "type")]
    #[cfg(test)]
    ty: String,
}

#[derive(Deserialize)]
struct File {
    destination: String,
    source: String,
}

#[derive(Deserialize)]
struct Manifest {
    #[allow(unused)]
    arch: Value,
    id: Option<String>,
    parts: Vec<Part>,
    #[allow(unused)]
    schema_version: String,
}

#[derive(Deserialize)]
struct Part {
    meta: String,
    #[serde(rename = "type")]
    #[allow(unused)]
    ty: String,
}

impl Sdk {
    pub fn from_build_dir(mut path: std::path::PathBuf) -> Result<Self> {
        path.push("sdk/manifest/core");

        let sdk = Self::atoms_from_core_manifest(std::io::BufReader::new(
            std::fs::File::open(path.clone()).context(format!("opening sdk path: {:?}", path))?,
        ))?
        .try_into();

        sdk.map(|mut x: Sdk| {
            x.version = SdkVersion::InTree;
            x
        })
    }

    fn atoms_from_core_manifest<T>(reader: std::io::BufReader<T>) -> Result<SdkAtoms>
    where
        T: std::io::Read,
    {
        let atoms: SdkAtoms = serde_json::from_reader(reader)?;

        Ok(atoms)
    }

    pub fn from_sdk_dir(path: std::path::PathBuf) -> Result<Self> {
        let manifest_path = path.join("meta/manifest.json");
        let mut version = SdkVersion::Unknown;

        Self::metas_from_sdk_manifest(
            std::io::BufReader::new(
                std::fs::File::open(manifest_path.clone())
                    .context(format!("opening sdk manifest path: {:?}", manifest_path))?,
            ),
            &mut version,
            |meta| {
                let meta_path = path.join(meta);

                std::fs::File::open(meta_path.clone())
                    .context(format!("opening sdk path: {:?}", meta_path))
                    .map(std::io::BufReader::new)
            },
        )
        .map(|metas| Sdk { metas, real_paths: RealPaths::Prefix(path), version })
    }

    fn metas_from_sdk_manifest<M, T>(
        manifest: std::io::BufReader<T>,
        version: &mut SdkVersion,
        get_meta: M,
    ) -> Result<Vec<Value>>
    where
        M: Fn(&str) -> Result<std::io::BufReader<T>>,
        T: std::io::Read,
    {
        let manifest: Manifest = serde_json::from_reader(manifest)?;
        // TODO: Check the schema version and log a warning if it's not what we expect.

        if let Some(id) = manifest.id {
            *version = SdkVersion::Version(id.clone());
        }

        let metas = manifest
            .parts
            .into_iter()
            .map(|x| {
                get_meta(&x.meta)
                    .and_then(|reader| serde_json::from_reader(reader).map_err(|x| x.into()))
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(metas)
    }

    pub fn get_host_tool(&self, name: &str) -> Result<std::path::PathBuf> {
        self.real_paths.produce(
            self.metas
                .iter()
                .filter(|x| {
                    x.get("name")
                        .filter(|n| *n == name)
                        .and(x.get("type").filter(|t| *t == "host_tool"))
                        .is_some()
                })
                .map(|x| -> Result<_> {
                    let arr = x
                        .get("files")
                        .ok_or(anyhow!(
                            "No executable provided for tool '{}' (no file list present)",
                            name
                        ))?
                        .as_array()
                        .ok_or(anyhow!(
                            "Malformed manifest for tool '{}': file list wasn't an array",
                            name
                        ))?;

                    if arr.len() > 1 {
                        warn!("Tool '{}' provides multiple files in manifest", name);
                    }

                    arr.get(0)
                        .ok_or(anyhow!(
                            "No executable provided for tool '{}' (file list was empty)",
                            name
                        ))?
                        .as_str()
                        .ok_or(anyhow!(
                            "Malformed manifest for tool '{}': file name wasn't a string",
                            name
                        ))
                })
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .min_by_key(|x| x.len()) // Shortest path is the one with no arch specifier, i.e. the default arch, i.e. the current arch (we hope.)
                .ok_or(anyhow!("Tool '{}' not found", name))?,
        )
    }

    pub fn get_version(&self) -> &SdkVersion {
        &self.version
    }

    /// For tests only
    #[doc(hidden)]
    pub fn get_empty_sdk_with_version(version: SdkVersion) -> Self {
        Sdk { metas: Vec::new(), real_paths: RealPaths::Prefix(std::path::PathBuf::new()), version }
    }
}

impl SdkAtoms {
    fn to_sdk<T, U>(self, get_meta: T) -> Result<Sdk>
    where
        T: Fn(&str) -> Result<std::io::BufReader<U>>,
        U: std::io::Read,
    {
        let mut metas = Vec::new();
        let mut real_paths = HashMap::new();

        for atom in self.atoms.iter() {
            for file in atom.files.iter() {
                real_paths.insert(file.destination.clone(), file.source.clone());
            }

            let meta = real_paths
                .get(&atom.meta)
                .ok_or(anyhow!("Atom did not specify source for its metadata."))?;

            metas.push(serde_json::from_reader(get_meta(meta)?)?);
        }

        Ok(Sdk { metas, real_paths: RealPaths::Map(real_paths), version: SdkVersion::Unknown })
    }
}

impl TryFrom<SdkAtoms> for Sdk {
    type Error = anyhow::Error;

    fn try_from(atoms: SdkAtoms) -> Result<Sdk> {
        atoms.to_sdk(|meta| Ok(std::io::BufReader::new(std::fs::File::open(meta)?)))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    const CORE_MANIFEST: &str = r#"{
      "atoms": [
        {
          "category": "partner",
          "deps": [],
          "files": [
            {
              "destination": "device/generic-arm64.json",
              "source": "/fuchsia/out/default/gen/sdk/devices/generic-arm64.meta.json"
            }
          ],
          "gn-label": "//sdk/devices:generic-arm64(//build/toolchain/fuchsia:x64)",
          "id": "sdk://device/generic-arm64",
          "meta": "device/generic-arm64.json",
          "type": "device_profile"
        },
        {
          "category": "partner",
          "deps": [],
          "files": [
            {
              "destination": "tools/x64/zxdb",
              "source": "/fuchsia/out/default/host_x64/zxdb"
            },
            {
              "destination": "tools/x64/zxdb-meta.json",
              "source": "/fuchsia/out/default/host_x64/gen/src/developer/debug/zxdb/zxdb_sdk.meta.json"
            }
          ],
          "gn-label": "//src/developer/debug/zxdb:zxdb_sdk(//build/toolchain:host_x64)",
          "id": "sdk://tools/x64/zxdb",
          "meta": "tools/x64/zxdb-meta.json",
          "type": "host_tool"
        },
        {
          "category": "partner",
          "deps": [],
          "files": [
            {
              "destination": "tools/arm64/symbol-index",
              "source": "/fuchsia/out/default/host_arm64/symbol-index"
            },
            {
              "destination": "tools/arm64/symbol-index-meta.json",
              "source": "/fuchsia/out/default/host_arm64/gen/tools/symbol-index/symbol_index_sdk.meta.json"
            }
          ],
          "gn-label": "//tools/symbol-index:symbol_index_sdk(//build/toolchain:host_arm64)",
          "id": "sdk://tools/arm64/symbol-index",
          "meta": "tools/arm64/symbol-index-meta.json",
          "type": "host_tool"
        },
        {
          "category": "partner",
          "deps": [],
          "files": [
            {
              "destination": "tools/symbol-index",
              "source": "/fuchsia/out/default/host_x64/symbol-index"
            },
            {
              "destination": "tools/symbol-index-meta.json",
              "source": "/fuchsia/out/default/host_x64/gen/tools/symbol-index/symbol_index_sdk_legacy.meta.json"
            }
          ],
          "gn-label": "//tools/symbol-index:symbol_index_sdk_legacy(//build/toolchain:host_x64)",
          "id": "sdk://tools/symbol-index",
          "meta": "tools/symbol-index-meta.json",
          "type": "host_tool"
        },
        {
          "category": "partner",
          "deps": [],
          "files": [
            {
              "destination": "tools/x64/symbol-index",
              "source": "/fuchsia/out/default/host_x64/symbol-index"
            },
            {
              "destination": "tools/x64/symbol-index-meta.json",
              "source": "/fuchsia/out/default/host_x64/gen/tools/symbol-index/symbol_index_sdk.meta.json"
            }
          ],
          "gn-label": "//tools/symbol-index:symbol_index_sdk(//build/toolchain:host_x64)",
          "id": "sdk://tools/x64/symbol-index",
          "meta": "tools/x64/symbol-index-meta.json",
          "type": "host_tool"
        }
      ],
      "ids": []
    }"#;

    fn get_core_manifest_meta(name: &str) -> Result<std::io::BufReader<&'static [u8]>> {
        if name == "/fuchsia/out/default/gen/sdk/devices/generic-arm64.meta.json" {
            const META: &str = r#"{
              "description": "A generic arm64 device",
              "images_url": "gs://fuchsia/development//images/generic-arm64.tgz",
              "name": "generic-arm64",
              "packages_url": "gs://fuchsia/development//packages/generic-arm64.tar.gz",
              "type": "device_profile"
            }"#;

            Ok(std::io::BufReader::new(META.as_bytes()))
        } else if name
            == "/fuchsia/out/default/host_x64/gen/src/developer/debug/zxdb/zxdb_sdk.meta.json"
        {
            const META: &str = r#"{
              "files": [
                "tools/x64/zxdb"
              ],
              "name": "zxdb",
              "root": "tools",
              "type": "host_tool"
            }"#;

            Ok(std::io::BufReader::new(META.as_bytes()))
        } else if name
            == "/fuchsia/out/default/host_x64/gen/tools/symbol-index/symbol_index_sdk.meta.json"
        {
            const META: &str = r#"{
              "files": [
                "tools/x64/symbol-index"
              ],
              "name": "symbol-index",
              "root": "tools",
              "type": "host_tool"
            }"#;

            Ok(std::io::BufReader::new(META.as_bytes()))
        } else if name
            == "/fuchsia/out/default/host_x64/gen/tools/symbol-index/symbol_index_sdk_legacy.meta.json"
        {
            const META: &str = r#"{
              "files": [
                "tools/x64/symbol-index"
              ],
              "name": "symbol-index",
              "root": "tools",
              "type": "host_tool"
            }"#;

            Ok(std::io::BufReader::new(META.as_bytes()))
        } else if name
            == "/fuchsia/out/default/host_arm64/gen/tools/symbol-index/symbol_index_sdk.meta.json"
        {
            const META: &str = r#"{
              "files": [
                "tools/arm64/symbol-index"
              ],
              "name": "symbol-index",
              "root": "tools",
              "type": "host_tool"
            }"#;

            Ok(std::io::BufReader::new(META.as_bytes()))
        } else {
            Err(anyhow!("No such manifest: {}", name))
        }
    }

    const SDK_MANIFEST: &str = r#"{
	  "arch": {
		"host": "x86_64-linux-gnu",
		"target": [
		  "arm64",
		  "x64"
		]
	  },
	  "id": "0.20201005.4.1",
	  "parts": [
		{
		  "meta": "fidl/fuchsia.data/meta.json",
		  "type": "fidl_library"
		},
		{
		  "meta": "tools/zxdb-meta.json",
		  "type": "host_tool"
		}
	  ],
	  "schema_version": "1"
    }"#;

    fn get_sdk_manifest_meta(name: &str) -> Result<std::io::BufReader<&'static [u8]>> {
        if name == "fidl/fuchsia.data/meta.json" {
            const META: &str = r#"{
              "deps": [],
              "name": "fuchsia.data",
              "root": "fidl/fuchsia.data",
              "sources": [
                "fidl/fuchsia.data/data.fidl"
              ],
              "type": "fidl_library"
            }"#;

            Ok(std::io::BufReader::new(META.as_bytes()))
        } else if name == "tools/zxdb-meta.json" {
            const META: &str = r#"{
              "files": [
                "tools/zxdb"
              ],
              "name": "zxdb",
              "root": "tools",
              "target_files": {},
              "type": "host_tool"
            }"#;

            Ok(std::io::BufReader::new(META.as_bytes()))
        } else {
            Err(anyhow!("No such manifest: {}", name))
        }
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_core_manifest() {
        let atoms =
            Sdk::atoms_from_core_manifest(std::io::BufReader::new(CORE_MANIFEST.as_bytes()))
                .unwrap();

        assert!(atoms.ids.is_empty());

        let atoms = atoms.atoms;
        assert_eq!(5, atoms.len());
        assert_eq!("partner", atoms[0].category);
        assert!(atoms[0].deps.is_empty());
        assert_eq!("//sdk/devices:generic-arm64(//build/toolchain/fuchsia:x64)", atoms[0].gn_label);
        assert_eq!("sdk://device/generic-arm64", atoms[0].id);
        assert_eq!("device_profile", atoms[0].ty);
        assert_eq!(1, atoms[0].files.len());
        assert_eq!("device/generic-arm64.json", atoms[0].files[0].destination);
        assert_eq!(
            "/fuchsia/out/default/gen/sdk/devices/generic-arm64.meta.json",
            atoms[0].files[0].source
        );

        assert_eq!(2, atoms[1].files.len());
        assert_eq!("tools/x64/zxdb", atoms[1].files[0].destination);
        assert_eq!("/fuchsia/out/default/host_x64/zxdb", atoms[1].files[0].source);
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_core_manifest_to_sdk() {
        let atoms =
            Sdk::atoms_from_core_manifest(std::io::BufReader::new(CORE_MANIFEST.as_bytes()))
                .unwrap();

        let sdk = atoms.to_sdk(get_core_manifest_meta).unwrap();
        assert_eq!(SdkVersion::Unknown, sdk.version);

        assert_eq!(5, sdk.metas.len());
        assert_eq!(
            "A generic arm64 device",
            sdk.metas[0].get("description").unwrap().as_str().unwrap()
        );
        assert_eq!("host_tool", sdk.metas[1].get("type").unwrap().as_str().unwrap());
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_core_manifest_host_tool() {
        let atoms =
            Sdk::atoms_from_core_manifest(std::io::BufReader::new(CORE_MANIFEST.as_bytes()))
                .unwrap();

        let zxdb = atoms.to_sdk(get_core_manifest_meta).unwrap().get_host_tool("zxdb").unwrap();

        assert_eq!(std::path::PathBuf::from("/fuchsia/out/default/host_x64/zxdb"), zxdb);
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_core_manifest_host_tool_multi_arch() {
        let atoms =
            Sdk::atoms_from_core_manifest(std::io::BufReader::new(CORE_MANIFEST.as_bytes()))
                .unwrap();

        let symbol_index =
            atoms.to_sdk(get_core_manifest_meta).unwrap().get_host_tool("symbol-index").unwrap();

        assert_eq!(
            std::path::PathBuf::from("/fuchsia/out/default/host_x64/symbol-index"),
            symbol_index
        );
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_sdk_manifest() {
        let mut version = SdkVersion::Unknown;
        let metas = Sdk::metas_from_sdk_manifest(
            std::io::BufReader::new(SDK_MANIFEST.as_bytes()),
            &mut version,
            get_sdk_manifest_meta,
        )
        .unwrap();

        assert_eq!(SdkVersion::Version("0.20201005.4.1".to_owned()), version);

        assert_eq!(2, metas.len());
        assert_eq!("fidl_library", metas[0].get("type").unwrap().as_str().unwrap());
        assert_eq!("host_tool", metas[1].get("type").unwrap().as_str().unwrap());
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_sdk_manifest_host_tool() {
        let metas = Sdk::metas_from_sdk_manifest(
            std::io::BufReader::new(SDK_MANIFEST.as_bytes()),
            &mut SdkVersion::Unknown,
            get_sdk_manifest_meta,
        )
        .unwrap();

        let sdk = Sdk {
            metas,
            real_paths: RealPaths::Prefix(std::path::PathBuf::from("/foo/bar")),
            version: SdkVersion::Unknown,
        };
        let zxdb = sdk.get_host_tool("zxdb").unwrap();

        assert_eq!(std::path::PathBuf::from("/foo/bar/tools/zxdb"), zxdb);
    }
}
