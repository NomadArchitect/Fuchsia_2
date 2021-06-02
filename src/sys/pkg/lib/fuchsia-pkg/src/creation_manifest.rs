// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::errors::CreationManifestError,
    serde::{Deserialize, Serialize},
    std::{
        collections::{btree_map, BTreeMap},
        fs,
        io::{self, Read},
        path::Path,
    },
};

/// A `CreationManifest` lists the files that should be included in a Fuchsia package.
/// Both `external_contents` and `far_contents` are maps from package resource paths in
/// the to-be-created package to paths on the local filesystem.
/// Package resource paths start with "meta/" if and only if they are in `far_contents`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreationManifest {
    external_contents: BTreeMap<String, String>,
    far_contents: BTreeMap<String, String>,
}

impl CreationManifest {
    /// Creates a `CreationManifest` from external and far contents maps.
    ///
    /// `external_contents` is a map from package resource paths to their locations
    /// on the host filesystem. These are the files that will be listed in
    /// `meta/contents`. Resource paths in `external_contents` must *not* start with
    /// `meta/`.
    /// `far_contents` is a map from package resource paths to their locations
    /// on the host filesystem. These are the files that will be included bodily in the
    /// package `meta.far` archive. Resource paths in `far_contents` must start with
    /// `meta/`.
    ///
    /// # Examples
    ///
    /// ```
    /// # use fuchsia_pkg::CreationManifest;
    /// # use maplit::btreemap;
    /// let external_contents = btreemap! {
    ///     "lib/mylib.so".to_string() => "build/system/path/mylib.so".to_string()
    /// };
    /// let far_contents = btreemap! {
    ///     "meta/my_component_manifest.cmx".to_string() =>
    ///         "other/build/system/path/my_component_manifest.cmx".to_string()
    /// };
    /// let creation_manifest =
    ///     CreationManifest::from_external_and_far_contents(external_contents, far_contents)
    ///         .unwrap();
    /// ```
    pub fn from_external_and_far_contents(
        external_contents: BTreeMap<String, String>,
        far_contents: BTreeMap<String, String>,
    ) -> Result<Self, CreationManifestError> {
        for (resource_path, _) in external_contents.iter().chain(far_contents.iter()) {
            crate::path::check_resource_path(&resource_path).map_err(|e| {
                CreationManifestError::ResourcePath { cause: e, path: resource_path.to_string() }
            })?;
        }
        for (resource_path, _) in external_contents.iter() {
            if resource_path.starts_with("meta/") {
                return Err(CreationManifestError::ExternalContentInMetaDirectory {
                    path: resource_path.to_string(),
                });
            }
        }
        for (resource_path, _) in far_contents.iter() {
            if !resource_path.starts_with("meta/") {
                return Err(CreationManifestError::FarContentNotInMetaDirectory {
                    path: resource_path.to_string(),
                });
            }
        }
        Ok(CreationManifest { external_contents, far_contents })
    }

    /// Deserializes a `CreationManifest` from versioned json.
    ///
    /// # Examples
    /// ```
    /// # use fuchsia_pkg::CreationManifest;
    /// let json_string = r#"
    /// {
    ///   "version": "1",
    ///   "content": {
    ///     "/": {
    ///       "lib/mylib.so": "build/system/path/mylib.so"
    ///     },
    ///    "/meta/": {
    ///      "my_component_manifest.cmx": "other/build/system/path/my_component_manifest.cmx"
    ///    }
    /// }"#;
    /// let creation_manifest = CreationManifest::from_json(json_string.as_bytes());
    /// ```
    pub fn from_json<R: io::Read>(reader: R) -> Result<Self, CreationManifestError> {
        match serde_json::from_reader::<R, VersionedCreationManifest>(reader)? {
            VersionedCreationManifest::Version1(v1) => CreationManifest::from_v1(v1),
        }
    }

    fn from_v1(v1: CreationManifestV1) -> Result<Self, CreationManifestError> {
        let mut far_contents = BTreeMap::new();
        // Validate package resource paths in far contents before "meta/" is prepended
        // for better error messages.
        for (resource_path, host_path) in v1.far_contents.into_iter() {
            crate::path::check_resource_path(&resource_path).map_err(|e| {
                CreationManifestError::ResourcePath { cause: e, path: resource_path.to_string() }
            })?;
            far_contents.insert(format!("meta/{}", resource_path), host_path);
        }
        CreationManifest::from_external_and_far_contents(v1.external_contents, far_contents)
    }

    /// Create a `CreationManifest` from a `pm-build`-style Fuchsia INI file (fini). fini is a
    /// simple format where each line is an entry of `$PKG_PATH=$HOST_PATH`. This copies the
    /// parsing algorithm from pm, where:
    ///
    /// * The $PKG_PATH is the string up to the first equals sign.
    /// * If there is a duplicate entry, check if the two entries have the same file contents. If
    ///   not, error out.
    /// * Only check if the files exist upon duplicate entry.
    /// * Ignores lines without an '=' in it.
    ///
    /// Note: This functionality exists only to ease the migration from `pm build`. This will be
    /// removed once there are no more users of `pm build`-style manifests.
    ///
    /// # Examples
    ///
    /// ```
    /// # use fuchsia_pkg::CreationManifest;
    /// let fini_string = "\
    ///     lib/mylib.so=build/system/path/mylib.so\n\
    ///     meta/my_component_manifest.cmx=other/build/system/path/my_component_manifest.cmx\n";
    ///
    /// let creation_manifest = CreationManifest::from_pm_fini(fini_string.as_bytes()).unwrap();
    /// ```
    pub fn from_pm_fini<R: io::BufRead>(mut reader: R) -> Result<Self, CreationManifestError> {
        let mut external_contents = BTreeMap::new();
        let mut far_contents = BTreeMap::new();

        let mut buf = String::new();
        while reader.read_line(&mut buf)? != 0 {
            let line = buf.trim();
            if line.is_empty() {
                buf.clear();
                continue;
            }

            // pm's build manifest finds the first '='. If one doesn't exist the line is ignored.
            let pos = if let Some(pos) = line.find('=') {
                pos
            } else {
                buf.clear();
                continue;
            };

            let package_path = line[..pos].trim().to_string();
            let host_path = line[pos + 1..].trim().to_string();

            let entry = if package_path.starts_with("meta/") {
                far_contents.entry(package_path)
            } else {
                external_contents.entry(package_path)
            };

            match entry {
                btree_map::Entry::Vacant(entry) => {
                    entry.insert(host_path);
                }
                btree_map::Entry::Occupied(entry) => {
                    // `pm build` manifests allow for duplicate entries, as long as they point to
                    // the same file.
                    if !same_file_contents(Path::new(&entry.get()), Path::new(&host_path))? {
                        return Err(CreationManifestError::DuplicateResourcePath {
                            path: entry.key().clone(),
                        });
                    }
                }
            }

            buf.clear();
        }

        Ok(Self { external_contents, far_contents })
    }

    /// `external_contents` lists the blobs that make up the meta/contents file
    pub fn external_contents(&self) -> &BTreeMap<String, String> {
        &self.external_contents
    }

    /// `far_contents` lists the files to be included bodily in the meta.far
    pub fn far_contents(&self) -> &BTreeMap<String, String> {
        &self.far_contents
    }
}

// It's possible for the same host file to be discovered through multiple paths. This is allowed as
// long as the files have the same file contents.
fn same_file_contents(lhs: &Path, rhs: &Path) -> io::Result<bool> {
    println!("same_file_contents: {} ? {}", lhs.display(), rhs.display());

    // First, check if the two paths are the same.
    if lhs == rhs {
        return Ok(true);
    }

    // Next, check if the paths point at the same file. We can quickly check dev/inode equality on
    // unix-style systems.
    #[cfg(unix)]
    fn same_dev_inode(lhs: &Path, rhs: &Path) -> io::Result<bool> {
        use std::os::unix::fs::MetadataExt;

        let lhs = fs::metadata(lhs)?;
        let rhs = fs::metadata(rhs)?;

        Ok(lhs.dev() == rhs.dev() && lhs.ino() == rhs.ino())
    }

    #[cfg(not(unix))]
    fn same_dev_inode(_lhs: &Path, _rhs: &Path) -> io::Result<bool> {
        Ok(false)
    }

    if same_dev_inode(lhs, rhs)? {
        println!("same inode");
        return Ok(true);
    }

    // Next, check if the paths resolve to the same path.
    let lhs = fs::canonicalize(lhs)?;
    let rhs = fs::canonicalize(rhs)?;

    if lhs == rhs {
        println!("same absolute path");
        return Ok(true);
    }

    println!("reading files");
    println!("lhs {:?}", fs::read_to_string(&lhs).unwrap());
    println!("rhs {:?}", fs::read_to_string(&rhs).unwrap());

    // Next, see if the files have different lengths.
    let lhs = fs::File::open(lhs)?;
    let rhs = fs::File::open(rhs)?;

    if lhs.metadata()?.len() != rhs.metadata()?.len() {
        println!("diff length {:?} {:?}", lhs.metadata()?.len(), rhs.metadata()?.len());
        return Ok(false);
    }

    // Finally, check if the files have the same contents.
    let mut lhs = io::BufReader::new(lhs).bytes();
    let mut rhs = io::BufReader::new(rhs).bytes();

    loop {
        match (lhs.next(), rhs.next()) {
            (None, None) => {
                return Ok(true);
            }
            (Some(Ok(_)), None) | (None, Some(Ok(_))) => {
                return Ok(false);
            }
            (Some(Ok(lhs_byte)), Some(Ok(rhs_byte))) => {
                if lhs_byte != rhs_byte {
                    return Ok(false);
                }
            }
            (Some(Err(err)), _) | (_, Some(Err(err))) => {
                return Err(err);
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "version", content = "content", deny_unknown_fields)]
enum VersionedCreationManifest {
    #[serde(rename = "1")]
    Version1(CreationManifestV1),
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
struct CreationManifestV1 {
    #[serde(rename = "/")]
    external_contents: BTreeMap<String, String>,
    #[serde(rename = "/meta/")]
    far_contents: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::ResourcePathError::PathStartsWithSlash;
    use crate::test::*;
    use maplit::btreemap;
    use matches::assert_matches;
    use proptest::prelude::*;
    use serde_json::json;

    fn from_json_value(
        value: serde_json::Value,
    ) -> Result<CreationManifest, CreationManifestError> {
        CreationManifest::from_json(value.to_string().as_bytes())
    }

    #[test]
    fn test_malformed_json() {
        assert_matches!(
            CreationManifest::from_json("<invalid json document>".as_bytes()),
            Err(CreationManifestError::Json(err)) if err.is_syntax()
        );
    }

    #[test]
    fn test_invalid_version() {
        assert_matches!(
            from_json_value(json!({"version": "2", "content": {}})),
            Err(CreationManifestError::Json(err)) if err.is_data()
        );
    }

    #[test]
    fn test_invalid_resource_path() {
        assert_matches!(
            from_json_value(
                json!(
                    {"version": "1",
                     "content":
                     {"/meta/" :
                      {"/starts-with-slash": "host-path"},
                      "/": {
                      }
                     }
                    }
                )
            ),
            Err(CreationManifestError::ResourcePath {
                cause: PathStartsWithSlash,
                path: s
            }) if s == "/starts-with-slash"
        );
    }

    #[test]
    fn test_meta_in_external() {
        assert_matches!(
            from_json_value(
                json!(
                    {"version": "1",
                     "content":
                     {"/meta/" : {},
                      "/": {
                          "meta/foo": "host-path"}
                     }
                    }
                )
            ),
            Err(CreationManifestError::ExternalContentInMetaDirectory{path: s}) if s == "meta/foo"
        );
    }

    #[test]
    fn test_from_v1() {
        assert_eq!(
            from_json_value(json!(
                {"version": "1",
                 "content": {
                     "/": {
                         "this-path": "this-host-path",
                         "that/path": "that/host/path"},
                     "/meta/" : {
                         "some-path": "some-host-path",
                         "other/path": "other/host/path"}
                 }
                }
            ))
            .unwrap(),
            CreationManifest {
                external_contents: btreemap! {
                    "this-path".to_string() => "this-host-path".to_string(),
                    "that/path".to_string() => "that/host/path".to_string()
                },
                far_contents: btreemap! {
                    "meta/some-path".to_string() => "some-host-path".to_string(),
                    "meta/other/path".to_string() => "other/host/path".to_string()
                }
            }
        );
    }

    #[test]
    fn test_from_pm_fini() {
        assert_eq!(
            CreationManifest::from_pm_fini(
                "this-path=this-host-path\n\
                 that/path=that/host/path\n\
                 another/path=another/host=path\n
                   with/white/space = host/white/space \n\

                 meta/some-path=some-host-path\n\
                 meta/other/path=other/host/path\n\
                 meta/another/path=another/host=path\n\
                 ignore lines without equals"
                    .as_bytes()
            )
            .unwrap(),
            CreationManifest {
                external_contents: btreemap! {
                    "this-path".to_string() => "this-host-path".to_string(),
                    "that/path".to_string() => "that/host/path".to_string(),
                    "another/path".to_string() => "another/host=path".to_string(),
                    "with/white/space".to_string() => "host/white/space".to_string(),
                },
                far_contents: btreemap! {
                    "meta/some-path".to_string() => "some-host-path".to_string(),
                    "meta/other/path".to_string() => "other/host/path".to_string(),
                    "meta/another/path".to_string() => "another/host=path".to_string(),
                },
            },
        );
    }

    #[test]
    fn test_from_pm_fini_empty() {
        assert_eq!(
            CreationManifest::from_pm_fini("".as_bytes()).unwrap(),
            CreationManifest { external_contents: btreemap! {}, far_contents: btreemap! {} },
        );
    }

    #[test]
    fn test_from_pm_fini_same_file_contents() {
        let dir = tempfile::tempdir().unwrap();

        let path = dir.path().join("path");
        let same = dir.path().join("same");

        fs::write(&path, b"hello world").unwrap();
        fs::write(&same, b"hello world").unwrap();

        let fini = format!(
            "path={path}\n\
             path={same}\n",
            path = path.to_str().unwrap(),
            same = same.to_str().unwrap(),
        );

        assert_eq!(
            CreationManifest::from_pm_fini(fini.as_bytes()).unwrap(),
            CreationManifest {
                external_contents: btreemap! {
                    "path".to_string() => path.to_str().unwrap().to_string(),
                },
                far_contents: btreemap! {},
            },
        );
    }

    #[test]
    fn test_from_pm_fini_different_contents() {
        let dir = tempfile::tempdir().unwrap();

        let path = dir.path().join("path");
        let different = dir.path().join("different");

        fs::write(&path, b"hello world").unwrap();
        fs::write(&different, b"different").unwrap();

        let fini = format!(
            "path={path}\n\
             path={different}\n",
            path = path.to_str().unwrap(),
            different = different.to_str().unwrap()
        );

        assert_matches!(
            CreationManifest::from_pm_fini(fini.as_bytes()),
            Err(CreationManifestError::DuplicateResourcePath { path }) if path == "path"
        );
    }

    #[test]
    fn test_from_pm_fini_not_found() {
        let dir = tempfile::tempdir().unwrap();

        let path = dir.path().join("path");
        let not_found = dir.path().join("not_found");

        fs::write(&path, b"hello world").unwrap();

        let fini = format!(
            "path={path}\n\
             path={not_found}\n",
            path = path.to_str().unwrap(),
            not_found = not_found.to_str().unwrap()
        );

        assert_matches!(
            CreationManifest::from_pm_fini(fini.as_bytes()),
            Err(CreationManifestError::IoError(err)) if err.kind() == io::ErrorKind::NotFound
        );
    }

    #[cfg(not(target_os = "fuchsia"))]
    #[cfg(unix)]
    #[test]
    fn test_from_pm_fini_link() {
        let dir = tempfile::tempdir().unwrap();

        let path = dir.path().join("path");
        let hard = dir.path().join("hard");
        let sym = dir.path().join("symlink");

        fs::write(&path, b"hello world").unwrap();
        fs::hard_link(&path, &hard).unwrap();
        std::os::unix::fs::symlink(&path, &sym).unwrap();

        let fini = format!(
            "path={path}\n\
             path={hard}\n\
             path={sym}\n",
            path = path.to_str().unwrap(),
            hard = hard.to_str().unwrap(),
            sym = sym.to_str().unwrap(),
        );

        assert_eq!(
            CreationManifest::from_pm_fini(fini.as_bytes()).unwrap(),
            CreationManifest {
                external_contents: btreemap! {
                    "path".to_string() => path.to_str().unwrap().to_string(),
                },
                far_contents: btreemap! {},
            },
        );
    }

    proptest! {
        #[test]
        fn test_from_external_and_far_contents_does_not_modify_valid_maps(
            ref external_resource_path in random_external_resource_path(),
            ref external_host_path in ".{0,30}",
            ref far_resource_path in random_far_resource_path(),
            ref far_host_path in ".{0,30}"
        ) {
            let external_contents = btreemap! {
                external_resource_path.to_string() => external_host_path.to_string()
            };
            let far_resource_path = format!("meta/{}", far_resource_path);
            let far_contents = btreemap! {
                far_resource_path.to_string() => far_host_path.to_string()
            };

            let creation_manifest = CreationManifest::from_external_and_far_contents(
                external_contents.clone(), far_contents.clone())
                .unwrap();

            prop_assert_eq!(creation_manifest.external_contents(), &external_contents);
            prop_assert_eq!(creation_manifest.far_contents(), &far_contents);
        }
    }
}
