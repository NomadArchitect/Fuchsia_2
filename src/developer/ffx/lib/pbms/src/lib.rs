// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Access utilities for product metadata.
//!
//! This is a collection of helper functions wrapping the FMS and GCS libs.
//!
//! The metadata can be loaded from a variety of sources. The initial places are
//! GCS and the local build.
//!
//! Call `product_bundle_urls()` to get a set of URLs for each product bundle.
//!
//! Call `fms_entries_from()` to get FMS entries from a particular repo. The
//! entries include product bundle metadata, physical device specifications, and
//! virtual device specifications. Each FMS entry has a unique name to identify
//! that entry.
//!
//! These FMS entry names are suitable to present to the user. E.g. the name of
//! a product bundle is also the name of the product bundle metadata entry.

use {
    crate::{
        pbms::{
            fetch_product_metadata, get_product_data_from_gcs, local_path_helper,
            path_from_file_url, pb_names_from_path, pbm_repo_list, CONFIG_STORAGE_PATH, GS_SCHEME,
        },
        repo_info::RepoInfo,
    },
    anyhow::{bail, Context, Result},
    fms::Entries,
    futures_lite::stream::StreamExt,
    itertools::Itertools as _,
    std::{
        io::Write,
        path::{Path, PathBuf},
    },
};

mod gcs;
mod pbms;
mod repo_info;

/// For each non-local URL in ffx CONFIG_METADATA, fetch updated info.
pub async fn update_metadata<W>(verbose: bool, writer: &mut W) -> Result<()>
where
    W: Write + Sync,
{
    let repos = pbm_repo_list().await.context("getting repo list")?;
    fetch_product_metadata(&repos, verbose, writer).await.context("fetching product metadata")
}

/// Gather a list of PBM reference URLs which include the product bundle entry
/// name.
///
/// Tip: Call `update_metadata()` to update the info (or not, if the intent is
///      to peek at what's there without updating).
pub async fn product_bundle_urls() -> Result<Vec<url::Url>> {
    let mut result = Vec::new();

    // Collect product bundle URLs from the file paths in ffx config.
    for repo in pbm_repo_list().await.context("getting repo list")? {
        if let Some(path) = &path_from_file_url(&repo) {
            let names =
                pb_names_from_path(&Path::new(&path)).context("loading product bundle names")?;
            for name in names {
                let mut product_url = repo.to_owned();
                product_url.set_fragment(Some(&name));
                result.push(product_url);
            }
        }
    }

    let storage_path: PathBuf =
        ffx_config::get(CONFIG_STORAGE_PATH).await.context("getting CONFIG_STORAGE_PATH")?;
    if !storage_path.is_dir() {
        // Early out before calling read_dir.
        return Ok(result);
    }

    // Collect product bundle URLs from the downloaded information. These
    // entries may not be currently referenced in the ffx config. This is where
    // product bundles from old versions will be pulled in, for example.
    let mut dir_entries = async_fs::read_dir(storage_path).await.context("reading vendors dir")?;
    while let Some(dir_entry) = dir_entries.try_next().await.context("reading directory")? {
        if dir_entry.path().is_dir() {
            if let Ok(repo_info) = RepoInfo::load(&dir_entry.path().join("info")) {
                let names = pb_names_from_path(&dir_entry.path().join("product_bundles.json"))?;
                for name in names {
                    let repo = format!("{}#{}", repo_info.metadata_url, name);
                    result.push(
                        url::Url::parse(&repo)
                            .with_context(|| format!("parsing metadata URL {:?}", repo))?,
                    );
                }
            }
        }
    }
    Ok(result)
}

/// Gather all the fms entries from a given product_url.
///
/// If `product_url` is None or not a URL, then an attempt will be made to find
/// default entries.
pub async fn fms_entries_from(product_url: &url::Url) -> Result<Entries> {
    let path = get_metadata_glob(product_url).await.context("getting metadata")?;
    let mut entries = Entries::new();
    entries.add_from_path(&path).context("adding entries")?;
    Ok(entries)
}

/// Find a product bundle url and name for `product_url`.
///
/// If product_url is
/// - None and there is only one product bundle available, use it.
/// - Some(product name) and a url with that fragment is found, use it.
/// - Some(full product url with fragment) use it.
/// If a match is not found or multiple matches are found, fail with an error
/// message.
///
/// Tip: Call `update_metadata()` to get up to date choices (or not, if the
///      intent is to select from what's already there).
pub async fn select_product_bundle(looking_for: &Option<String>) -> Result<url::Url> {
    let mut urls = product_bundle_urls().await.context("getting product bundle URLs")?;
    urls.sort();
    urls.reverse();
    if let Some(looking_for) = &looking_for {
        let matches = urls.into_iter().filter(|url| {
            return url.as_str() == looking_for
                || url.fragment().expect("product_urls must have fragment") == looking_for;
        });
        match matches.at_most_one() {
            Ok(Some(m)) => Ok(m),
            Ok(None) => bail!(
                "{}",
                "A product bundle with that name was not found, please check the spelling and try again."
            ),
            // This branch can only happen with looking_for matches more than
            // one fragment--full urls can only ever match once.
            Err(matches) => {
                let printable_matches =
                    matches.map(|url| url.to_string()).collect::<Vec<String>>().join("\n");
                bail!(
                    "Multiple product bundles match for: '{}':\n{}",
                    looking_for,
                    printable_matches
                )
            }
        }
    } else {
        match urls.into_iter().at_most_one() {
            Ok(Some(url)) => Ok(url),
            Ok(None) => bail!("There are no product bundles available."),
            Err(urls) => {
                let printable_urls =
                    urls.map(|url| url.to_string()).collect::<Vec<String>>().join("\n");
                bail!(
                    "There is more than one product bundle available, please pass in a product bundle URL:\n{}", printable_urls
                )
            }
        }
    }
}

/// Determine whether the data for `product_url` is downloaded and ready to be
/// used.
pub async fn is_pb_ready(product_url: &url::Url) -> Result<bool> {
    assert!(product_url.as_str().contains("#"));
    Ok(get_images_dir(product_url).await.context("getting images dir")?.is_dir())
}

/// Download data related to the product.
///
/// The emulator may then be run with the data downloaded.
///
/// If `product_bundle_url` is None and only one viable PBM is available, that entry
/// is used.
///
/// `writer` is used to output user messages.
pub async fn get_product_data<W>(
    product_url: &url::Url,
    verbose: bool,
    writer: &mut W,
) -> Result<()>
where
    W: Write + Sync,
{
    if product_url.scheme() == "file" {
        log::info!("There's no data download necessary for local products.");
        return Ok(());
    }
    if product_url.scheme() != GS_SCHEME {
        log::info!("Only GCS downloads are supported at this time.");
        return Ok(());
    }
    get_product_data_from_gcs(product_url, verbose, writer)
        .await
        .context("reading pbms entries")?;
    Ok(())
}

/// Determine the path to the product images data.
pub async fn get_images_dir(product_url: &url::Url) -> Result<PathBuf> {
    assert!(!product_url.as_str().is_empty());
    let name = product_url.fragment().expect("a URI fragment is required");
    assert!(!name.is_empty());
    assert!(!name.contains("/"));
    local_path_helper(product_url, &format!("{}/images", name), /*dir=*/ true).await
}

/// Determine the path to the product packages data.
pub async fn get_packages_dir(product_url: &url::Url) -> Result<PathBuf> {
    assert!(!product_url.as_str().is_empty());
    let name = product_url.fragment().expect("a URI fragment is required");
    assert!(!name.is_empty());
    assert!(!name.contains("/"));
    local_path_helper(product_url, &format!("{}/packages", name), /*dir=*/ true).await
}

/// Determine the path to the local product metadata directory.
pub async fn get_metadata_dir(product_url: &url::Url) -> Result<PathBuf> {
    assert!(!product_url.as_str().is_empty());
    assert!(!product_url.fragment().is_none());
    Ok(get_metadata_glob(product_url)
        .await
        .context("getting metadata")?
        .parent()
        .expect("Metadata files should have a parent")
        .to_path_buf())
}

/// Determine the glob path to the product metadata.
///
/// A glob path may have wildcards, such as "file://foo/*.json".
pub async fn get_metadata_glob(product_url: &url::Url) -> Result<PathBuf> {
    assert!(!product_url.as_str().is_empty());
    assert!(!product_url.fragment().is_none());
    local_path_helper(product_url, "product_bundles.json", /*dir=*/ false).await
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::pbms::CONFIG_METADATA,
        ffx_config::ConfigLevel,
        serde_json,
        std::{fs::File, io::Write},
        tempfile::TempDir,
    };

    const CORE_JSON: &str = include_str!("../test_data/test_core.json");
    const IMAGES_JSON: &str = include_str!("../test_data/test_images.json");
    const PRODUCT_BUNDLE_JSON: &str = include_str!("../test_data/test_product_bundle.json");

    // Disabling this test until a test config can be modified without altering
    // the local user's config.
    #[ignore]
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_get_pbms() {
        ffx_config::test_init().expect("create test config");
        let temp_dir = TempDir::new().expect("temp dir");
        let temp_path = temp_dir.path();

        let manifest_path = temp_dir.path().join("sdk/manifest");
        std::fs::create_dir_all(&manifest_path).expect("create dir");
        let mut core_file = File::create(manifest_path.join("core")).expect("create core");
        core_file.write_all(CORE_JSON.as_bytes()).expect("write core file");
        drop(core_file);

        let mut images_file =
            File::create(temp_path.join("images.json")).expect("create images file");
        images_file.write_all(IMAGES_JSON.as_bytes()).expect("write images file");
        drop(images_file);

        let mut pbm_file =
            File::create(temp_path.join("product_bundle.json")).expect("create images file");
        pbm_file.write_all(PRODUCT_BUNDLE_JSON.as_bytes()).expect("write pbm file");
        drop(pbm_file);

        let sdk_root = temp_path.to_str().expect("path to str");
        ffx_config::set(("sdk.root", ConfigLevel::User), sdk_root.into())
            .await
            .expect("set sdk root path");
        ffx_config::set(("sdk.type", ConfigLevel::User), "in-tree".into())
            .await
            .expect("set sdk type");
        ffx_config::set((CONFIG_METADATA, ConfigLevel::User), serde_json::json!([""]))
            .await
            .expect("set pbms metadata");
        let mut writer = Box::new(std::io::stdout());
        update_metadata(/*verbose=*/ false, &mut writer).await.expect("get pbms");
        let urls = product_bundle_urls().await.expect("get pbms");
        assert!(!urls.is_empty());
    }
}
