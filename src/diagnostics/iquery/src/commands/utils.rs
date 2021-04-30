// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{
        commands::{Command, ListCommand},
        types::Error,
    },
    diagnostics_data::InspectData,
    diagnostics_reader::{ArchiveReader, Inspect},
    fidl::endpoints::DiscoverableService,
    fidl_fuchsia_diagnostics::{ArchiveAccessorMarker, ArchiveAccessorProxy},
    fidl_fuchsia_io as fio,
    files_async::{self, DirentKind},
    fuchsia_component::client,
    futures::StreamExt,
    glob, io_util, selectors,
};

/// If not path is provided, then connects to the global archivist. If one is provided, then
/// connects to first `ArchiveAccessor` that the given glob matches or returns an error if none
/// is found.
pub async fn connect_to_archive_accessor(
    accessor_path: &Option<String>,
) -> Result<ArchiveAccessorProxy, Error> {
    match accessor_path {
        None => client::connect_to_protocol::<ArchiveAccessorMarker>()
            .map_err(|e| Error::ConnectToArchivist(e)),
        Some(path) => connect_to_archive_at(path).await,
    }
}

/// Connects to first `ArchiveAccessor` that the given glob matches.
async fn connect_to_archive_at(glob_path: &str) -> Result<ArchiveAccessorProxy, Error> {
    let path_results =
        glob::glob(&glob_path).map_err(|e| Error::ParsePath(glob_path.to_string(), e.into()))?;
    for path_result in path_results {
        if let Ok(path) = path_result {
            let path_str = path.to_string_lossy().to_string();
            let node = io_util::open_node_in_namespace(
                &path_str,
                fio::OPEN_RIGHT_READABLE | fio::OPEN_FLAG_NODE_REFERENCE,
            )
            .map_err(|e| Error::io_error("open node in namespace", e))?;
            if let Ok(node_info) = node.describe().await {
                match node_info {
                    fio::NodeInfo::Service(_) => {
                        return client::connect_to_protocol_at_path::<ArchiveAccessorMarker>(
                            &path_str,
                        )
                        .map_err(|e| Error::ConnectToArchivist(e));
                    }
                    fio::NodeInfo::Directory(_) => {
                        let directory = io_util::open_directory_in_namespace(
                            &path_str,
                            fio::OPEN_RIGHT_READABLE,
                        )
                        .map_err(|e| Error::io_error("open directory in namespace", e))?;
                        let mut stream = files_async::readdir_recursive(&directory, None);
                        while let Some(result) = stream.next().await {
                            if let Ok(entry) = result {
                                if entry.kind == DirentKind::Service
                                    && entry.name.ends_with(ArchiveAccessorMarker::SERVICE_NAME)
                                {
                                    let accessor_path = format!("{}/{}", path_str, entry.name);
                                    return client::connect_to_protocol_at_path::<
                                        ArchiveAccessorMarker,
                                    >(&accessor_path)
                                    .map_err(|e| Error::ConnectToArchivist(e));
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Err(Error::UnknownArchivePath)
}

/// Returns the selectors for a component whose url contains the `manifest` string.
pub async fn get_selectors_for_manifest(
    manifest: &Option<String>,
    tree_selectors: &Vec<String>,
    accessor_path: &Option<String>,
) -> Result<Vec<String>, Error> {
    match &manifest {
        None => Ok(tree_selectors.clone()),
        Some(manifest) => {
            let list_command = ListCommand {
                manifest: Some(manifest.clone()),
                with_url: false,
                accessor_path: accessor_path.clone(),
            };
            let monikers = list_command
                .execute()
                .await?
                .into_iter()
                .map(|item| item.into_moniker())
                .collect::<Vec<_>>();
            if monikers.is_empty() {
                Err(Error::ManifestNotFound(manifest.clone()))
            } else if tree_selectors.is_empty() {
                Ok(monikers.into_iter().map(|moniker| format!("{}:root", moniker)).collect())
            } else {
                Ok(monikers
                    .into_iter()
                    .flat_map(|moniker| {
                        tree_selectors
                            .iter()
                            .map(move |tree_selector| format!("{}:{}", moniker, tree_selector))
                    })
                    .collect())
            }
        }
    }
}

/// Returns the component "moniker" and the hierarchy data for results of
/// reading from the archive using the given selectors.
pub async fn fetch_data(
    selectors: &[String],
    accessor_path: &Option<String>,
) -> Result<Vec<InspectData>, Error> {
    let archive = connect_to_archive_accessor(accessor_path).await?;
    let mut reader = ArchiveReader::new().with_archive(archive).retry_if_empty(false);
    // We support receiving the moniker or a tree selector
    for selector in selectors {
        match selectors::tokenize_string(selector, selectors::SELECTOR_DELIMITER) {
            Ok(tokens) => {
                if tokens.len() > 1 {
                    reader = reader.add_selector(selector.as_ref());
                } else if tokens.len() == 1 {
                    reader = reader.add_selector(format!("{}:*", selector));
                } else {
                    return Err(Error::InvalidArguments(format!(
                        "Iquery selectors cannot be empty strings: {:?}",
                        selector
                    )));
                }
            }
            Err(e) => {
                return Err(Error::InvalidArguments(format!(
                    "Tokenizing a provided selector failed. Error: {:?} Selector: {:?}",
                    e, selector
                )));
            }
        }
    }
    let mut results = reader.snapshot::<Inspect>().await.map_err(|e| Error::Fetch(e))?;
    for result in results.iter_mut() {
        if let Some(hierarchy) = &mut result.payload {
            hierarchy.sort();
        }
    }
    Ok(results)
}
