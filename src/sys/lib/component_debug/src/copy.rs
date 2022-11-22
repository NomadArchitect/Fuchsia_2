// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{
        io::Directory,
        path::{
            finalize_destination_to_filepath, HostOrRemotePath, NamespacedPath, RemotePath,
            REMOTE_PATH_HELP,
        },
    },
    anyhow::{bail, Result},
    fidl::endpoints::{create_endpoints, ClientEnd},
    fidl_fuchsia_io as fio, fidl_fuchsia_sys2 as fsys,
    regex::Regex,
    std::{
        collections::HashMap,
        fs::{read, write},
        path::PathBuf,
    },
    thiserror::Error,
};

#[derive(Error, Debug)]
pub enum CopyError {
    #[error("Destination can not have a wildcard.")]
    DestinationContainWildcard,

    #[error("At least two paths (host or remote) must be provided.")]
    NotEnoughPaths,

    #[error("Could not write to host: {error}.")]
    FailedToWriteToHost { error: std::io::Error },

    #[error("File name was unexpectedly empty.")]
    EmptyFileName,

    #[error("Path does not contain a parent folder.")]
    NoParentFolder { path: String },

    #[error("Could not find files in device that matched pattern: {pattern}.")]
    NoWildCardMatches { pattern: String },

    #[error("Could not write to device.")]
    FailedToWriteToDevice,

    #[error("Unexpected error. Destination namespace was non empty but destination path is not a remote path.")]
    UnexpectedHostDestination,

    #[error("Could not create Regex pattern \"{pattern}\": {error}.")]
    FailedToCreateRegex { pattern: String, error: regex::Error },

    #[error("At least one path must be a remote path. {}", REMOTE_PATH_HELP)]
    NoRemotePaths,

    #[error(
        "Could not find an instance with the moniker: {moniker}\n\
    Use `ffx component list` or `ffx component show` to find the correct moniker of your instance."
    )]
    InstanceNotFound { moniker: String },

    #[error("Encountered an unexpected error when attempting to retrieve namespace with the provider moniker: {moniker}. {error:?}.")]
    UnexpectedErrorFromMoniker { moniker: String, error: fsys::RealmQueryError },
}

/// Transfer files between a component's namespace to/from the host machine.
///
/// # Arguments
/// * `realm_query`: |RealmQueryProxy| to fetch the component's namespace.
/// * `paths`: The host and remote paths used for file copying.
pub async fn copy(realm_query: &fsys::RealmQueryProxy, mut paths: Vec<String>) -> Result<()> {
    validate_paths(&paths)?;

    let mut namespaces: HashMap<String, fio::DirectoryProxy> = HashMap::new();
    // paths is safe to unwrap as validate_paths ensures that it is non-empty.
    let destination_path = paths.pop().unwrap();

    for source_path in paths {
        let result = match (
            HostOrRemotePath::parse(&source_path),
            HostOrRemotePath::parse(&destination_path),
        ) {
            (HostOrRemotePath::Remote(source), HostOrRemotePath::Host(destination)) => {
                let source_namespace = get_namespace_or_insert(
                    &realm_query,
                    source.clone().remote_id,
                    &mut namespaces,
                )
                .await?;

                let paths = normalize_paths(source, &source_namespace).await?;

                for source_path in paths {
                    copy_remote_file_to_host(
                        NamespacedPath { path: source_path, ns: source_namespace.to_owned() },
                        destination.clone(),
                    )
                    .await?;
                }
                Ok(())
            }

            (HostOrRemotePath::Remote(source), HostOrRemotePath::Remote(destination)) => {
                let source_namespace = get_namespace_or_insert(
                    &realm_query,
                    source.clone().remote_id,
                    &mut namespaces,
                )
                .await?;

                let destination_namespace = get_namespace_or_insert(
                    &realm_query,
                    destination.clone().remote_id,
                    &mut namespaces,
                )
                .await?;
                let paths = normalize_paths(source, &source_namespace).await?;

                for source in paths {
                    copy_remote_file_to_remote(
                        NamespacedPath { path: source, ns: source_namespace.to_owned() },
                        NamespacedPath {
                            path: destination.clone(),
                            ns: destination_namespace.to_owned(),
                        },
                    )
                    .await?;
                }
                Ok(())
            }

            (HostOrRemotePath::Host(source), HostOrRemotePath::Remote(destination)) => {
                let destination_namespace = get_namespace_or_insert(
                    &realm_query,
                    destination.clone().remote_id,
                    &mut namespaces,
                )
                .await?;

                copy_host_file_to_remote(
                    source,
                    NamespacedPath { path: destination, ns: destination_namespace },
                )
                .await
            }

            (HostOrRemotePath::Host(_), HostOrRemotePath::Host(_)) => {
                Err(CopyError::NoRemotePaths.into())
            }
        };

        match result {
            Ok(_) => continue,
            Err(e) => bail!(
                "Copy failed for source path: {} and destination path: {}. {}",
                &source_path,
                &destination_path,
                e
            ),
        };
    }

    Ok(())
}

// Normalizes the remote source path that may contain a wildcard.
// If the source contains a wildcard, the source is expanded to multiple paths.
// # Arguments
// * `source`: A wildcard path or path on a component's namespace.
// * `namespace`: The source path's namespace directory.
pub async fn normalize_paths(
    source: RemotePath,
    namespace: &fio::DirectoryProxy,
) -> Result<Vec<RemotePath>> {
    if !&source.contains_wildcard() {
        return Ok(vec![source]);
    }

    let directory = match &source.relative_path.parent() {
        Some(directory) => PathBuf::from(directory),
        None => {
            return Err(CopyError::NoParentFolder {
                path: source.relative_path.as_path().display().to_string(),
            }
            .into())
        }
    };

    let file_pattern = source
        .clone()
        .relative_path
        .file_name()
        .map_or_else(
            || Err(CopyError::EmptyFileName),
            |file| Ok(file.to_string_lossy().to_string()),
        )?
        .replace("*", ".*"); // Regex syntax requires a . before wildcard.

    let namespace = Directory::from_proxy(namespace.to_owned())
        .open_dir(&directory, fio::OpenFlags::RIGHT_READABLE)?;
    let entries = get_matching_ns_entries(namespace, file_pattern.clone()).await?;

    if entries.len() == 0 {
        return Err(CopyError::NoWildCardMatches { pattern: file_pattern }.into());
    }

    let paths = entries
        .iter()
        .map(|file| {
            RemotePath::parse(&format!(
                "{}::/{}",
                &source.remote_id,
                directory.join(file).as_path().display().to_string()
            ))
        })
        .collect::<Result<Vec<RemotePath>>>()?;

    Ok(paths)
}

// Checks whether the hashmap contains the existing moniker and creates a new (moniker, DirectoryProxy) pair if it doesn't exist.
// # Arguments
// * `realm_query`: |RealmQueryProxy| to fetch the component's namespace.
// * `moniker`: A moniker used to retrieve a namespace directory.
// * `namespaces`: A table of monikers that map to namespace directories.
pub async fn get_namespace_or_insert(
    realm_query: &fsys::RealmQueryProxy,
    moniker: String,
    namespaces: &mut HashMap<String, fio::DirectoryProxy>,
) -> Result<fio::DirectoryProxy> {
    if !namespaces.contains_key(&moniker) {
        let namespace = retrieve_namespace(&realm_query, &moniker).await?;
        namespaces.insert(moniker.clone(), namespace);
    }

    Ok(namespaces.get(&moniker).unwrap().to_owned())
}

// Checks that the paths meet the following conditions:
// Destination path does not contain a wildcard.
// At least two paths are provided.
// # Arguments
// *`paths`: list of filepaths to be processed.
pub fn validate_paths(paths: &Vec<String>) -> Result<()> {
    if paths.len() < 2 {
        Err(CopyError::NotEnoughPaths.into())
    } else if paths.last().unwrap().contains("*") {
        Err(CopyError::DestinationContainWildcard.into())
    } else {
        Ok(())
    }
}

/// Retrieves the directory proxy associated with a component's namespace
/// # Arguments
/// * `realm_query`: |RealmQueryProxy| to retrieve a component instance.
/// * `moniker`: Absolute moniker of a component instance.
pub async fn retrieve_namespace(
    realm_query: &fsys::RealmQueryProxy,
    moniker: &str,
) -> Result<fio::DirectoryProxy> {
    // A relative moniker is required for |fuchsia.sys2/RealmQuery.GetInstanceInfo|
    let relative_moniker = format!(".{moniker}");
    let (_, resolved_state) = match realm_query.get_instance_info(&relative_moniker).await? {
        Ok((info, state)) => (info, state),
        Err(fsys::RealmQueryError::InstanceNotFound) => {
            return Err(CopyError::InstanceNotFound { moniker: moniker.to_string() }.into());
        }
        Err(e) => {
            return Err(CopyError::UnexpectedErrorFromMoniker {
                moniker: moniker.to_string(),
                error: e,
            }
            .into())
        }
    };
    // resolved_state is safe to unwrap as an error would be thrown otherwise in the above statement.
    let resolved_state = resolved_state.unwrap();
    let namespace = (*resolved_state).ns_dir.into_proxy()?;
    Ok(namespace)
}

/// Writes file contents from a directory to a component's namespace.
///
/// # Arguments
/// * `source`: The host filepath.
/// * `destination`: The path and proxy of a namespace directory.
pub async fn copy_host_file_to_remote(source: PathBuf, destination: NamespacedPath) -> Result<()> {
    let destination_namespace = Directory::from_proxy(destination.ns.to_owned());
    let destination_path = finalize_destination_to_filepath(
        &destination_namespace,
        HostOrRemotePath::Host(source.clone()),
        HostOrRemotePath::Remote(destination.path),
    )
    .await?;

    let data = read(&source)?;

    destination_namespace
        .verify_directory_is_read_write(&destination_path.parent().unwrap())
        .await?;
    destination_namespace.create_file(destination_path, data.as_slice()).await?;
    Ok(())
}

/// Writes file contents to a directory from a component's namespace.
///
/// # Arguments
/// * `source`: The path and proxy of a namespace directory.
/// * `destination`: The host filepath.
pub async fn copy_remote_file_to_host(source: NamespacedPath, destination: PathBuf) -> Result<()> {
    let file_path = &source.path.relative_path.clone();
    let source_namespace = Directory::from_proxy(source.ns.to_owned());
    let destination_path = finalize_destination_to_filepath(
        &source_namespace,
        HostOrRemotePath::Remote(source.path),
        HostOrRemotePath::Host(destination),
    )
    .await?;

    let data = source_namespace.read_file_bytes(file_path).await?;
    write(destination_path, data).map_err(|e| CopyError::FailedToWriteToHost { error: e })?;

    Ok(())
}

/// Writes file contents to a component's namespace from a component's namespace.
///
/// # Arguments
/// * `source`: The path and proxy of a namespace directory.
/// * `destination`: The path and proxy of a namespace directory.
pub async fn copy_remote_file_to_remote(
    source: NamespacedPath,
    destination: NamespacedPath,
) -> Result<()> {
    let source_namespace = Directory::from_proxy(source.ns.to_owned());
    let destination_namespace = Directory::from_proxy(destination.ns.to_owned());
    let destination_path = finalize_destination_to_filepath(
        &destination_namespace,
        HostOrRemotePath::Remote(source.path.clone()),
        HostOrRemotePath::Remote(destination.path),
    )
    .await?;

    let data = source_namespace.read_file_bytes(&source.path.relative_path).await?;
    destination_namespace
        .verify_directory_is_read_write(&destination_path.parent().unwrap())
        .await?;
    destination_namespace.create_file(destination_path, data.as_slice()).await?;
    Ok(())
}

// Retrieves all entries within a directory in a namespace containing a file pattern.
///
/// # Arguments
/// * `namespace`: A directory to a component's namespace.
/// * `file_pattern`: A file pattern to match in a component's directory.
pub async fn get_matching_ns_entries(
    namespace: Directory,
    file_pattern: String,
) -> Result<Vec<String>> {
    let mut entries = namespace.entry_names().await?;

    let file_pattern = Regex::new(format!(r"^{}$", file_pattern).as_str()).map_err(|e| {
        CopyError::FailedToCreateRegex { pattern: file_pattern.to_string(), error: e }
    })?;

    entries.retain(|file_name| file_pattern.is_match(file_name.as_str()));

    Ok(entries)
}

// Duplicates the client end of a namespace directory.
///
/// # Arguments
/// * `ns_dir`: A proxy to the component's namespace directory.
pub fn duplicate_namespace_client(ns_dir: &fio::DirectoryProxy) -> Result<fio::DirectoryProxy> {
    let (client, server) = create_endpoints::<fio::NodeMarker>().unwrap();
    ns_dir.clone(fio::OpenFlags::CLONE_SAME_RIGHTS, server).unwrap();
    let client =
        ClientEnd::<fio::DirectoryMarker>::new(client.into_channel()).into_proxy().unwrap();
    Ok(client)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::test_utils::{
            populate_host_with_file_contents, read_data_from_namespace, serve_realm_query,
            serve_realm_query_with_namespace, set_path_to_read_only,
        },
        fidl::endpoints::{create_endpoints, create_proxy, ClientEnd, Proxy},
        fidl_fuchsia_io as fio,
        std::collections::HashMap,
        std::fs::read,
        std::path::Path,
        tempfile::tempdir,
        test_case::test_case,
    };

    const CHANNEL_SIZE_LIMIT: u64 = 64 * 1024;
    const LARGE_FILE_ARRAY: [u8; CHANNEL_SIZE_LIMIT as usize] = [b'a'; CHANNEL_SIZE_LIMIT as usize];
    const OVER_LIMIT_FILE_ARRAY: [u8; (CHANNEL_SIZE_LIMIT + 1) as usize] =
        [b'a'; (CHANNEL_SIZE_LIMIT + 1) as usize];
    const SAMPLE_NAME: &str = "./core/appmgr";
    const SAMPLE_MONIKER: &str = "./core/appmgr";
    const SAMPLE_FILE_NAME: &str = "foo.txt";
    const SAMPLE_FILE_NAME_2: &str = "bar.txt";
    const SAMPLE_FILE_CONTENTS: &str = "Lorem Ipsum";
    const SAMPLE_FILE_CONTENTS_2: &str = "New Data";
    const BLANK_FILE_CONTENTS: &str = "";
    const READ_WRITE: bool = false;
    const READ_ONLY: bool = true;

    fn create_resolved_state(
        exposed_dir: ClientEnd<fio::DirectoryMarker>,
        ns_dir: ClientEnd<fio::DirectoryMarker>,
    ) -> Option<Box<fsys::ResolvedState>> {
        Some(Box::new(fsys::ResolvedState {
            uses: vec![],
            exposes: vec![],
            config: None,
            pkg_dir: None,
            execution: Some(Box::new(fsys::ExecutionState {
                out_dir: None,
                runtime_dir: None,
                start_reason: "Debugging Workflow".to_string(),
            })),
            exposed_dir,
            ns_dir,
        }))
    }

    fn create_hashmap_of_instance_info(
        name: &str,
        moniker: &str,
        ns_dir: ClientEnd<fio::DirectoryMarker>,
    ) -> HashMap<String, (fsys::InstanceInfo, Option<Box<fsys::ResolvedState>>)> {
        let (exposed_dir, _) = create_endpoints::<fio::DirectoryMarker>().unwrap();
        HashMap::from([(
            name.to_string(),
            (
                fsys::InstanceInfo {
                    moniker: moniker.to_string(),
                    url: String::new(),
                    instance_id: None,
                    state: fsys::InstanceState::Started,
                },
                create_resolved_state(exposed_dir, ns_dir),
            ),
        )])
    }

    fn create_realm_query(
        seed_files: Vec<(&'static str, &'static str)>,
        is_read_only: bool,
    ) -> fsys::RealmQueryProxy {
        let (ns_dir, ns_server) = create_endpoints::<fio::DirectoryMarker>().unwrap();
        let seed_files =
            HashMap::from(seed_files.into_iter().collect::<HashMap<&'static str, &'static str>>());
        let () = serve_realm_query_with_namespace(ns_server, seed_files, is_read_only).unwrap();
        let query_instance = create_hashmap_of_instance_info(SAMPLE_NAME, SAMPLE_MONIKER, ns_dir);
        serve_realm_query(query_instance)
    }

    fn create_realm_query_with_ns_client(
        seed_files: Vec<(&'static str, &'static str)>,
        is_read_only: bool,
    ) -> (fsys::RealmQueryProxy, fio::DirectoryProxy) {
        let (ns_dir, ns_server) = create_proxy::<fio::DirectoryMarker>().unwrap();
        let dup_client = duplicate_namespace_client(&ns_dir).unwrap();
        let seed_files =
            HashMap::from(seed_files.into_iter().collect::<HashMap<&'static str, &'static str>>());
        let () = serve_realm_query_with_namespace(ns_server, seed_files, is_read_only).unwrap();
        let ns_dir = ClientEnd::<fio::DirectoryMarker>::new(ns_dir.into_channel().unwrap().into());
        let query_instance = create_hashmap_of_instance_info(SAMPLE_NAME, SAMPLE_MONIKER, ns_dir);
        let realm_query = serve_realm_query(query_instance);

        (realm_query, dup_client)
    }

    #[test_case("/core/appmgr::/data/foo.txt", "/foo.txt", vec![], vec![(SAMPLE_FILE_NAME, SAMPLE_FILE_CONTENTS)], "/foo.txt", SAMPLE_FILE_CONTENTS; "device_to_host")]
    #[test_case("/core/appmgr::/data/foo.txt", "/foo.txt", vec![(SAMPLE_FILE_NAME, SAMPLE_FILE_CONTENTS)], vec![(SAMPLE_FILE_NAME, SAMPLE_FILE_CONTENTS_2)], "/foo.txt", SAMPLE_FILE_CONTENTS_2; "device_to_host_overwrite_file")]
    #[test_case("/core/appmgr::/data/foo.txt", "/foo.txt", vec![], vec![(SAMPLE_FILE_NAME, BLANK_FILE_CONTENTS)], "/foo.txt", BLANK_FILE_CONTENTS; "device_to_host_blank_file")]
    #[test_case("/core/appmgr::/data/foo.txt", "/bar.txt", vec![],  vec![(SAMPLE_FILE_NAME, SAMPLE_FILE_CONTENTS)], "/bar.txt", SAMPLE_FILE_CONTENTS; "device_to_host_different_name")]
    #[test_case("/core/appmgr::/data/foo.txt", "", vec![],  vec![(SAMPLE_FILE_NAME, SAMPLE_FILE_CONTENTS)], "/foo.txt", SAMPLE_FILE_CONTENTS; "device_to_host_infer_path")]
    #[test_case("/core/appmgr::/data/foo.txt", "/", vec![],  vec![(SAMPLE_FILE_NAME, SAMPLE_FILE_CONTENTS)], "/foo.txt", SAMPLE_FILE_CONTENTS; "device_to_host_infer_slash_path")]
    #[test_case("/core/appmgr::/data/foo.txt", "/foo.txt", vec![],  vec![(SAMPLE_FILE_NAME, SAMPLE_FILE_CONTENTS),(SAMPLE_FILE_NAME_2, SAMPLE_FILE_CONTENTS)],
    "/foo.txt", SAMPLE_FILE_CONTENTS; "device_to_host_populated_directory")]
    #[test_case("/core/appmgr::/data/foo.txt", "/foo.txt", vec![],  vec![(SAMPLE_FILE_NAME, std::str::from_utf8(&LARGE_FILE_ARRAY).unwrap())], "/foo.txt", std::str::from_utf8(&LARGE_FILE_ARRAY).unwrap(); "device_to_host_large_file")]
    #[test_case("/core/appmgr::/data/foo.txt", "/foo.txt", vec![],  vec![(SAMPLE_FILE_NAME, std::str::from_utf8(&OVER_LIMIT_FILE_ARRAY).unwrap())], "/foo.txt", std::str::from_utf8(&OVER_LIMIT_FILE_ARRAY).unwrap(); "device_to_host_over_file_limit")]
    #[fuchsia::test]
    async fn copy_device_to_host(
        source_path: &'static str,
        destination_path: &'static str,
        host_seed_files: Vec<(&'static str, &'static str)>,
        device_seed_files: Vec<(&'static str, &'static str)>,
        actual_data_path: &'static str,
        expected_data: &'static str,
    ) {
        let root = tempdir().unwrap();
        let root_path = root.path().to_str().unwrap();
        let destination_path = format!("{}{}", root_path, destination_path);
        populate_host_with_file_contents(&root_path, host_seed_files).unwrap();
        let realm_query = create_realm_query(device_seed_files, READ_ONLY);

        copy(&realm_query, vec![source_path.to_owned(), destination_path.to_owned()])
            .await
            .unwrap();

        let expected_data = expected_data.to_owned().into_bytes();
        let actual_data_path_string = format!("{}{}", root_path, actual_data_path);
        let actual_data_path = Path::new(&actual_data_path_string);
        let actual_data = read(actual_data_path).unwrap();
        assert_eq!(actual_data, expected_data);
    }

    #[test_case("wrong_moniker/core/appmgr::/data/foo.txt", "/foo.txt", vec![(SAMPLE_FILE_NAME, SAMPLE_FILE_CONTENTS)]; "bad_moniker")]
    #[test_case("/core/appmgr::/data/foo.txt", "/core/appmgr::/data/foo.txt",  vec![(SAMPLE_FILE_NAME, SAMPLE_FILE_CONTENTS)]; "device_to_device_not_supported")]
    #[test_case("/core/appmgr::/data/bar.txt", "/foo.txt", vec![(SAMPLE_FILE_NAME, SAMPLE_FILE_CONTENTS)]; "bad_file")]
    #[test_case("/core/appmgr::/data/foo.txt", "/bar/foo.txt", vec![(SAMPLE_FILE_NAME, SAMPLE_FILE_CONTENTS)]; "bad_directory")]
    #[fuchsia::test]
    async fn copy_device_to_host_fails(
        source_path: &'static str,
        destination_path: &'static str,
        seed_files: Vec<(&'static str, &'static str)>,
    ) {
        let root = tempdir().unwrap();
        let root_path = root.path().to_str().unwrap();
        let destination_path = format!("{}{}", root_path, destination_path);
        let realm_query = create_realm_query(seed_files, READ_ONLY);
        let result =
            copy(&realm_query, vec![source_path.to_owned(), destination_path.to_owned()]).await;

        assert!(result.is_err());
    }

    #[test_case("/core/appmgr::/data/foo.txt", "/"; "read_only_root")]
    #[fuchsia::test]
    async fn copy_device_to_host_fails_read_only(
        source_path: &'static str,
        destination_path: &'static str,
    ) {
        let root = tempdir().unwrap();
        let root_path = root.path().to_str().unwrap();
        let destination_path = format!("{}{}", root_path, destination_path);
        set_path_to_read_only(PathBuf::from(&destination_path)).unwrap();
        let realm_query = create_realm_query(vec![], READ_ONLY);

        let result =
            copy(&realm_query, vec![source_path.to_owned(), destination_path.to_owned()]).await;

        assert!(result.is_err());
    }

    #[test_case("/foo.txt", "/core/appmgr::/data/foo.txt", vec![],  "/data/foo.txt", SAMPLE_FILE_CONTENTS; "host_to_device")]
    #[test_case("/foo.txt", "/core/appmgr::/data/bar.txt", vec![],  "/data/bar.txt", SAMPLE_FILE_CONTENTS; "host_to_device_different_name")]
    #[test_case("/foo.txt", "/core/appmgr::/data/foo.txt", vec![(SAMPLE_FILE_NAME, SAMPLE_FILE_CONTENTS_2)],  "/data/foo.txt", SAMPLE_FILE_CONTENTS; "host_to_device_overwrite_file")]
    #[test_case("/foo.txt", "/core/appmgr::/data/foo.txt", vec![],  "/data/foo.txt", BLANK_FILE_CONTENTS; "host_to_device_blank_file")]
    #[test_case("/foo.txt", "/core/appmgr::/data", vec![], "/data/foo.txt", SAMPLE_FILE_CONTENTS; "host_to_device_inferred_path")]
    #[test_case("/foo.txt", "/core/appmgr::/data/", vec![], "/data/foo.txt", SAMPLE_FILE_CONTENTS; "host_to_device_inferred_slash_path")]
    #[test_case("/foo.txt", "/core/appmgr::/data/", vec![], "/data/foo.txt", std::str::from_utf8(&LARGE_FILE_ARRAY).unwrap(); "host_to_device_large_file")]
    #[test_case("/foo.txt", "/core/appmgr::/data/", vec![], "/data/foo.txt", std::str::from_utf8(&OVER_LIMIT_FILE_ARRAY).unwrap(); "host_to_device_over_limit_file")]
    #[fuchsia::test]
    async fn copy_host_to_device(
        source_path: &'static str,
        destination_path: &'static str,
        seed_files: Vec<(&'static str, &'static str)>,
        actual_data_path: &'static str,
        expected_data: &'static str,
    ) {
        let root = tempdir().unwrap();
        let root_path = root.path().to_str().unwrap();
        let source_path = format!("{}{}", root_path, source_path);
        write(&source_path, expected_data.to_owned().into_bytes()).unwrap();
        let (realm_query, ns_dir) = create_realm_query_with_ns_client(seed_files, READ_WRITE);

        copy(&realm_query, vec![source_path.to_owned(), destination_path.to_owned()])
            .await
            .unwrap();

        let actual_data = read_data_from_namespace(&ns_dir, actual_data_path).await.unwrap();
        let expected_data = expected_data.to_owned().into_bytes();
        assert_eq!(actual_data, expected_data);
    }

    #[test_case("/foo.txt", "/core/appmgr::/foo.txt", SAMPLE_FILE_CONTENTS; "root_dir")]
    #[test_case("/foo.txt", "/core/appmgr::", SAMPLE_FILE_CONTENTS; "root_dir_infer_path")]
    #[test_case("/foo.txt", "/core/appmgr::/", SAMPLE_FILE_CONTENTS; "root_dir_infer_path_slash")]
    #[test_case("/foo.txt", "wrong_moniker/core/appmgr::/data/foo.txt", SAMPLE_FILE_CONTENTS; "bad_moniker")]
    #[test_case("/foo.txt", "/core/appmgr::/bar/foo.txt", SAMPLE_FILE_CONTENTS; "bad_directory")]
    #[test_case("/foo.txt", "/core/appmgr/data/foo.txt", SAMPLE_FILE_CONTENTS; "host_to_host_not_supported")]
    #[fuchsia::test]
    async fn copy_host_to_device_fails(
        source_path: &'static str,
        destination_path: &'static str,
        source_data: &'static str,
    ) {
        let root = tempdir().unwrap();
        let root_path = root.path().to_str().unwrap();
        let source_path = format!("{}{}", root_path, source_path);
        write(&source_path, source_data.to_owned().into_bytes()).unwrap();
        let realm_query = create_realm_query(vec![], READ_WRITE);

        let result =
            copy(&realm_query, vec![source_path.to_owned(), destination_path.to_owned()]).await;

        assert!(result.is_err());
    }

    #[test_case("/foo.txt", "/core/appmgr::/read_only/foo.txt", SAMPLE_FILE_CONTENTS; "read_only_folder")]
    #[test_case("/foo.txt", "/core/appmgr::/read_only", SAMPLE_FILE_CONTENTS; "read_only_folder_infer_path")]
    #[test_case("/foo.txt", "/core/appmgr::/read_only/", SAMPLE_FILE_CONTENTS; "read_only_folder_infer_path_slash")]
    #[fuchsia::test]
    async fn copy_host_to_device_fails_read_only(
        source_path: &'static str,
        destination_path: &'static str,
        source_data: &'static str,
    ) {
        let root = tempdir().unwrap();
        let root_path = root.path().to_str().unwrap();
        let source_path = format!("{}{}", root_path, source_path);
        write(&source_path, source_data.to_owned().into_bytes()).unwrap();
        let realm_query = create_realm_query(vec![], READ_ONLY);

        let result =
            copy(&realm_query, vec![source_path.to_owned(), destination_path.to_owned()]).await;

        assert!(result.is_err());
    }
}
