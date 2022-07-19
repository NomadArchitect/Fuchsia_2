// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Private functionality for pbms lib.

use {
    crate::{
        gcs::{
            fetch_from_gcs, get_boto_path, get_gcs_client_with_auth, get_gcs_client_without_auth,
        },
        repo_info::RepoInfo,
    },
    ::gcs::client::{DirectoryProgress, FileProgress, ProgressResponse, ProgressResult, Throttle},
    anyhow::{bail, Context, Result},
    chrono::{DateTime, NaiveDateTime, Utc},
    ffx_config::sdk::SdkVersion,
    fms::{find_product_bundle, Entries},
    fuchsia_hyper::new_https_client,
    futures::{stream::FuturesUnordered, TryStreamExt as _},
    pkg::repository::{GcsRepository, HttpRepository, Repository, RepositoryBackend},
    sdk_metadata::{Metadata, PackageBundle},
    serde_json::Value,
    std::{
        io::Write,
        path::{Path, PathBuf},
    },
    url::Url,
};

pub(crate) const CONFIG_METADATA: &str = "pbms.metadata";
pub(crate) const CONFIG_STORAGE_PATH: &str = "pbms.storage.path";
pub(crate) const GS_SCHEME: &str = "gs";

/// Load FMS Entries for a given SDK `version`.
///
/// Expandable tags (e.g. "{foo}") in `repos` must already be expanded, do not
/// pass in repo URIs with expandable tags.
pub(crate) async fn fetch_product_metadata<F>(repos: &Vec<url::Url>, progress: &mut F) -> Result<()>
where
    F: FnMut(DirectoryProgress<'_>, FileProgress<'_>) -> ProgressResult,
{
    tracing::info!("Getting product metadata.");
    let storage_path: PathBuf =
        ffx_config::get(CONFIG_STORAGE_PATH).await.context("get CONFIG_STORAGE_PATH")?;
    async_fs::create_dir_all(&storage_path).await.context("create directory")?;
    for repo_url in repos {
        if repo_url.scheme() != GS_SCHEME {
            // There's no need to fetch local files or unrecognized schemes.
            continue;
        }
        let hash = pb_dir_name(&repo_url);
        let local_path = storage_path.join(hash).join("product_bundles.json");
        if let Some(local_dir) = local_path.parent() {
            async_fs::create_dir_all(&local_dir).await.context("create directory")?;

            let mut info = RepoInfo::default();
            info.metadata_url = repo_url.to_string();
            info.save(&local_dir.join("info"))?;

            let temp_dir = tempfile::tempdir_in(&local_dir).context("create temp dir")?;
            fetch_bundle_uri(&repo_url, &temp_dir.path(), progress)
                .await
                .context("fetch product bundle by URL")?;
            let the_file = temp_dir.path().join("product_bundles.json");
            if the_file.is_file() {
                async_fs::rename(&the_file, &local_path).await.context("move temp file")?;
            }
        }
    }
    Ok(())
}

/// Replace the {foo} placeholders in repo paths.
///
/// {version} is replaced with the Fuchsia SDK version string.
/// {sdk.root} is replaced with the SDK directory path.
fn expand_placeholders(uri: &str, version: &str, sdk_root: &str) -> Result<url::Url> {
    let expanded = uri.replace("{version}", version).replace("{sdk.root}", sdk_root);
    if uri.contains(":") {
        Ok(url::Url::parse(&expanded).with_context(|| format!("url parse {:?}", expanded))?)
    } else {
        // If there's no colon, assume it's a local path.
        let base_url = url::Url::parse("file:/").context("parsing minimal file URL")?;
        Ok(url::Url::options()
            .base_url(Some(&base_url))
            .parse(&expanded)
            .with_context(|| format!("url parse {:?}", expanded))?)
    }
}

/// Get a list of the urls in the CONFIG_METADATA config with the placeholders
/// expanded.
///
/// I.e. run expand_placeholders() on each element in CONFIG_METADATA.
pub(crate) async fn pbm_repo_list() -> Result<Vec<url::Url>> {
    let sdk = ffx_config::get_sdk().await.context("PBMS ffx config get sdk")?;
    let version = match sdk.get_version() {
        SdkVersion::Version(version) => version,
        SdkVersion::InTree => "",
        SdkVersion::Unknown => bail!("Unable to determine SDK version vs. in-tree"),
    };
    let sdk_root = sdk.get_path_prefix();
    let repos: Vec<String> = ffx_config::get::<Vec<String>, _>(CONFIG_METADATA)
        .await
        .context("get config CONFIG_METADATA")?;
    let repos: Vec<url::Url> = repos
        .iter()
        .map(|s| {
            expand_placeholders(s, &version, &sdk_root.to_string_lossy())
                .expect(&format!("URL for repo {:?}", s))
        })
        .collect();
    Ok(repos)
}

/// Retrieve the path portion of a "file:/" url. Non-file-paths return None.
///
/// If the url has no scheme, the whole string is returned.
/// E.g.
/// - "/foo/bar" -> Some("/foo/bar")
/// - "file://foo/bar" -> Some("/foo/bar")
/// - "http://foo/bar" -> None
pub(crate) fn path_from_file_url(product_url: &url::Url) -> Option<PathBuf> {
    if product_url.scheme() == "file" {
        product_url.to_file_path().ok()
    } else {
        None
    }
}

/// Get a list of product bundle entry names from `path`.
///
/// These are not full product_urls, but just the name that is used in the
/// fragment portion of the URL.
pub(crate) fn pb_names_from_path(path: &Path) -> Result<Vec<String>> {
    let mut entries = Entries::new();
    entries.add_from_path(path).context("adding from path")?;
    Ok(entries
        .iter()
        .filter_map(|entry| match entry {
            Metadata::ProductBundleV1(_) => Some(entry.name().to_string()),
            _ => None,
        })
        .collect::<Vec<String>>())
}

/// Helper function for determining local path.
///
/// if `dir` return a directory path, else may return a glob (file) path.
pub(crate) async fn local_path_helper(
    product_url: &url::Url,
    add_dir: &str,
    dir: bool,
) -> Result<PathBuf> {
    assert!(!product_url.fragment().is_none());
    if let Some(path) = &path_from_file_url(product_url) {
        if dir {
            // TODO(fxbug.dev/98009): Unify the file layout between local and remote
            // product bundles to avoid this hack.
            let sdk = ffx_config::get_sdk().await.context("getting ffx config sdk")?;
            let sdk_root = sdk.get_path_prefix();
            if path.starts_with(&sdk_root) {
                Ok(sdk_root.to_path_buf())
            } else {
                Ok(path.parent().expect("parent of file path").to_path_buf())
            }
        } else {
            Ok(path.to_path_buf())
        }
    } else {
        let url = url_sans_fragment(&product_url)?;
        let storage_path: PathBuf =
            ffx_config::get(CONFIG_STORAGE_PATH).await.context("getting CONFIG_STORAGE_PATH")?;
        Ok(storage_path.join(pb_dir_name(&url)).join(add_dir))
    }
}

/// Separate the URL on the last "#" character.
///
/// If no "#" is found, use the whole input as the url.
///
/// "file://foo#bar" -> "file://foo"
/// "file://foo" -> "file://foo"
pub(crate) fn url_sans_fragment(product_url: &url::Url) -> Result<url::Url> {
    let mut product_url = product_url.to_owned();
    product_url.set_fragment(None);
    Ok(product_url)
}

/// Helper for `get_product_data()`, see docs there.
pub(crate) async fn get_product_data_from_gcs<W>(
    product_url: &url::Url,
    verbose: bool,
    writer: &mut W,
) -> Result<()>
where
    W: Write + Sync,
{
    assert_eq!(product_url.scheme(), GS_SCHEME);
    let product_name = product_url.fragment().expect("URL with trailing product_name fragment.");
    let url = url_sans_fragment(product_url)?;

    fetch_product_metadata(&vec![url.to_owned()], &mut |_d, _f| {
        write!(writer, ".")?;
        writer.flush()?;
        Ok(ProgressResponse::Continue)
    })
    .await
    .context("fetching metadata")?;

    let storage_path: PathBuf =
        ffx_config::get(CONFIG_STORAGE_PATH).await.context("getting CONFIG_STORAGE_PATH")?;
    let local_repo_dir = storage_path.join(pb_dir_name(&url));
    let file_path = local_repo_dir.join("product_bundles.json");
    if !file_path.is_file() {
        bail!("Failed to download metadata.");
    }
    let mut entries = Entries::new();
    entries.add_from_path(&file_path).context("adding entries from gcs")?;
    let product_bundle = find_product_bundle(&entries, &Some(product_name.to_string()))
        .context("finding product bundle")?;

    let start = std::time::Instant::now();
    tracing::info!("Getting product data for {:?}", product_bundle.name);
    let local_dir = local_repo_dir.join(&product_bundle.name).join("images");
    async_fs::create_dir_all(&local_dir).await.context("creating directory")?;

    for image in &product_bundle.images {
        tracing::debug!("    image: {:?}", image);
        let base_url = url::Url::parse(&image.base_uri)
            .with_context(|| format!("parsing image.base_uri {:?}", image.base_uri))?;
        fetch_by_format(&image.format, &base_url, &local_dir, &mut |_d, _f| {
            write!(writer, ".")?;
            writer.flush()?;
            Ok(ProgressResponse::Continue)
        })
        .await
        .with_context(|| format!("fetching images for {}.", product_bundle.name))?;
    }
    tracing::debug!("Total fetch images runtime {} seconds.", start.elapsed().as_secs_f32());

    let start = std::time::Instant::now();
    writeln!(writer, "\nGetting package data for {:?}", product_bundle.name)?;
    let local_dir = local_repo_dir.join(&product_bundle.name).join("packages");
    async_fs::create_dir_all(&local_dir).await.context("creating directory")?;

    fetch_package_repository_from_mirrors(&local_dir, &product_bundle.packages, &mut |_d, _f| {
        write!(writer, ".")?;
        writer.flush()?;
        Ok(ProgressResponse::Continue)
    })
    .await?;

    tracing::debug!("Total fetch packages runtime {} seconds.", start.elapsed().as_secs_f32());

    writeln!(writer, "\nDownload of product data for {:?} is complete.", product_bundle.name)?;
    if verbose {
        if let Some(parent) = local_dir.parent() {
            writeln!(writer, "Data written to \"{}\".", parent.display())?;
        }
    }
    Ok(())
}

/// Generate a (likely) unique name for the URL.
///
/// URLs don't always make good file paths.
fn pb_dir_name(gcs_url: &url::Url) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::Hash;
    use std::hash::Hasher;
    let mut s = DefaultHasher::new();
    gcs_url.as_str().hash(&mut s);
    format!("{}", s.finish())
}

/// Fetch the product bundle package repository mirror list.
///
/// This will try to download a package repository from each mirror in the list, stopping on the
/// first success. Otherwise it will return the last error encountered.
async fn fetch_package_repository_from_mirrors<F>(
    local_dir: &Path,
    packages: &[PackageBundle],
    progress: &mut F,
) -> Result<()>
where
    F: FnMut(DirectoryProgress<'_>, FileProgress<'_>) -> ProgressResult,
{
    // The packages list is a set of mirrors. Try downloading the packages from each one. Only error
    // out if we can't download the packages from any mirror.
    for (i, package) in packages.iter().enumerate() {
        let res = fetch_package_repository(&local_dir, package, progress).await;

        match res {
            Ok(()) => {
                break;
            }
            Err(err) => {
                tracing::warn!("Unable to fetch {:?}: {:?}", package, err);
                if i + 1 == packages.len() {
                    return Err(err);
                }
            }
        }
    }

    Ok(())
}

/// Fetch packages from this package bundle and write them to `local_dir`.
async fn fetch_package_repository<F>(
    local_dir: &Path,
    package: &PackageBundle,
    progress: &mut F,
) -> Result<()>
where
    F: FnMut(DirectoryProgress<'_>, FileProgress<'_>) -> ProgressResult,
{
    if let Some(blob_uri) = &package.blob_uri {
        tracing::debug!("    package repository: {} {}", package.repo_uri, blob_uri);
    } else {
        tracing::debug!("    package repository: {}", package.repo_uri);
    }

    let mut metadata_repo_uri = url::Url::parse(&package.repo_uri)
        .with_context(|| format!("parsing package.repo_uri {:?}", package.repo_uri))?;

    match package.format.as_str() {
        "files" => {
            // `Url::join()` treats urls with a trailing slash as a directory, and without as a
            // file. In the latter case, it will strip off the last segment before joining paths.
            // Since the metadata and blob url are directories, make sure they have a trailing
            // slash.
            if !metadata_repo_uri.path().ends_with('/') {
                metadata_repo_uri.set_path(&format!("{}/", metadata_repo_uri.path()));
            }

            metadata_repo_uri = metadata_repo_uri.join("repository/")?;

            let blob_repo_uri = if let Some(blob_repo_uri) = &package.blob_uri {
                url::Url::parse(blob_repo_uri)
                    .with_context(|| format!("parsing package.repo_uri {:?}", blob_repo_uri))?
            } else {
                // If the blob uri is unspecified, then use `$METADATA_URI/blobs/`.
                metadata_repo_uri.join("blobs/")?
            };

            fetch_package_repository_from_files(
                local_dir,
                metadata_repo_uri,
                blob_repo_uri,
                progress,
            )
            .await
        }
        "tgz" => {
            if package.blob_uri.is_some() {
                // TODO(fxbug.dev/93850): implement pbms.
                unimplemented!();
            }

            fetch_package_repository_from_tgz(local_dir, metadata_repo_uri, progress).await
        }
        _ =>
        // The schema currently defines only "files" or "tgz" (see RFC-100).
        // This error could be a typo in the product bundle or a new image
        // format has been added and this code needs an update.
        {
            bail!(
                "Unexpected image format ({:?}) in product bundle. \
            Supported formats are \"files\" and \"tgz\". \
            Please report as a bug.",
                package.format,
            )
        }
    }
}

/// Fetch a package repository using the `files` package bundle format and writes it to
/// `local_dir`.
///
/// This supports the following URL schemes:
/// * `http://`
/// * `https://`
/// * `gs://`
async fn fetch_package_repository_from_files<F>(
    local_dir: &Path,
    metadata_repo_uri: Url,
    blob_repo_uri: Url,
    progress: &mut F,
) -> Result<()>
where
    F: FnMut(DirectoryProgress<'_>, FileProgress<'_>) -> ProgressResult,
{
    match (metadata_repo_uri.scheme(), blob_repo_uri.scheme()) {
        (GS_SCHEME, GS_SCHEME) => {
            // FIXME(fxbug.dev/103331): we are reproducing the gcs library's authentication flow,
            // where we will prompt for an oauth token if we get a permission denied error. This
            // was done because the pbms library is written to be used a frontend that can prompt
            // for an oauth token, but the `pkg` library is written to be used on the server side,
            // which cannot do the prompt. We should eventually restructure things such that we can
            // deduplicate this logic.

            // First try to fetch with the public client.
            let client = get_gcs_client_without_auth();
            let backend = Box::new(GcsRepository::new(
                client,
                metadata_repo_uri.clone(),
                blob_repo_uri.clone(),
            )?) as Box<dyn RepositoryBackend + Send + Sync + 'static>;

            if fetch_package_repository_from_backend(
                local_dir,
                metadata_repo_uri.clone(),
                blob_repo_uri.clone(),
                backend,
                progress,
            )
            .await
            .is_ok()
            {
                return Ok(());
            }

            let boto_path = get_boto_path().await?;
            let client = get_gcs_client_with_auth(&boto_path)?;

            let backend = Box::new(GcsRepository::new(
                client,
                metadata_repo_uri.clone(),
                blob_repo_uri.clone(),
            )?) as Box<dyn RepositoryBackend + Send + Sync + 'static>;

            fetch_package_repository_from_backend(
                local_dir,
                metadata_repo_uri.clone(),
                blob_repo_uri.clone(),
                backend,
                progress,
            )
            .await
        }
        ("http" | "https", "http" | "https") => {
            let client = new_https_client();
            let backend = Box::new(HttpRepository::new(
                client,
                metadata_repo_uri.clone(),
                blob_repo_uri.clone(),
            )) as Box<dyn RepositoryBackend + Send + Sync + 'static>;

            fetch_package_repository_from_backend(
                local_dir,
                metadata_repo_uri,
                blob_repo_uri,
                backend,
                progress,
            )
            .await
        }
        ("file", "file") => {
            // The files are already local, so we don't need to download them.
            Ok(())
        }
        (_, _) => {
            bail!("Unexpected URI scheme in ({}, {})", metadata_repo_uri, blob_repo_uri);
        }
    }
}

async fn fetch_package_repository_from_backend<F>(
    local_dir: &Path,
    metadata_repo_uri: Url,
    blob_repo_uri: Url,
    backend: Box<dyn RepositoryBackend + Send + Sync + 'static>,
    progress: &mut F,
) -> Result<()>
where
    F: FnMut(DirectoryProgress<'_>, FileProgress<'_>) -> ProgressResult,
{
    let repo = Repository::new("repo", backend).await.with_context(|| {
        format!("creating package repository {} {}", metadata_repo_uri, blob_repo_uri)
    })?;

    let metadata_dir = local_dir.join("repository");
    let blobs_dir = metadata_dir.join("blobs");

    // TUF metadata may be expired, so pretend we're updating relative to the Unix Epoch so the
    // metadata won't expired.
    let start_time = DateTime::<Utc>::from_utc(NaiveDateTime::from_timestamp(0, 0), Utc);

    let trusted_targets = pkg::resolve::resolve_repository_metadata_with_start_time(
        &repo,
        &metadata_dir,
        &start_time,
    )
    .await
    .with_context(|| format!("downloading repository {} {}", metadata_repo_uri, blob_repo_uri))?;

    let mut count = 0;
    // Exit early if there are no targets.
    if let Some(trusted_targets) = trusted_targets {
        // Download all the packages.
        let fetcher = pkg::resolve::PackageFetcher::new(&repo, &blobs_dir, 5).await?;

        let mut futures = FuturesUnordered::new();

        let mut throttle = Throttle::from_duration(std::time::Duration::from_millis(500));
        for (package_name, desc) in trusted_targets.targets().iter() {
            let merkle = desc.custom().get("merkle").context("missing merkle")?;
            let merkle = if let Value::String(hash) = merkle {
                hash.parse()?
            } else {
                bail!("Merkle field is not a String. {:#?}", desc)
            };

            count += 1;
            if throttle.is_ready() {
                match progress(
                    DirectoryProgress { url: blob_repo_uri.as_ref(), at: 0, of: 1 },
                    FileProgress { url: "Packages", at: 0, of: count },
                )
                .context("rendering progress")?
                {
                    ProgressResponse::Cancel => break,
                    _ => (),
                }
            }
            tracing::debug!("    package: {}", package_name.as_str());

            futures.push(fetcher.fetch_package(merkle));
        }

        while let Some(()) = futures.try_next().await? {}
        progress(
            DirectoryProgress { url: blob_repo_uri.as_ref(), at: 1, of: 1 },
            FileProgress { url: "Packages", at: count, of: count },
        )
        .context("rendering progress")?;
    };

    Ok(())
}

/// Fetch a package repository using the `tgz` package bundle format, and automatically expand the
/// tarball into the `local_dir` directory.
async fn fetch_package_repository_from_tgz<F>(
    local_dir: &Path,
    repo_uri: Url,
    progress: &mut F,
) -> Result<()>
where
    F: FnMut(DirectoryProgress<'_>, FileProgress<'_>) -> ProgressResult,
{
    fetch_bundle_uri(&repo_uri, &local_dir, progress)
        .await
        .with_context(|| format!("downloading repo URI {}", repo_uri))?;

    Ok(())
}

/// Download and expand data.
///
/// For a directory, all files in the directory are downloaded.
/// For a .tgz file, the file is downloaded and expanded.
async fn fetch_by_format<F>(
    format: &str,
    uri: &url::Url,
    local_dir: &Path,
    progress: &mut F,
) -> Result<()>
where
    F: FnMut(DirectoryProgress<'_>, FileProgress<'_>) -> ProgressResult,
{
    match format {
        "files" | "tgz" => fetch_bundle_uri(uri, &local_dir, progress).await,
        _ =>
        // The schema currently defines only "files" or "tgz" (see RFC-100).
        // This error could be a typo in the product bundle or a new image
        // format has been added and this code needs an update.
        {
            bail!(
                "Unexpected image format ({:?}) in product bundle. \
            Supported formats are \"files\" and \"tgz\". \
            Please report as a bug.",
                format,
            )
        }
    }
}

/// Download data from any of the supported schemes listed in RFC-100, Product
/// Bundle, "bundle_uri".
///
/// Currently: "pattern": "^(?:http|https|gs|file):\/\/"
async fn fetch_bundle_uri<F>(
    product_url: &url::Url,
    local_dir: &Path,
    progress: &mut F,
) -> Result<()>
where
    F: FnMut(DirectoryProgress<'_>, FileProgress<'_>) -> ProgressResult,
{
    if product_url.scheme() == GS_SCHEME {
        fetch_from_gcs(product_url.as_str(), local_dir, progress)
            .await
            .context("Downloading from GCS.")?;
    } else if product_url.scheme() == "http" || product_url.scheme() == "https" {
        fetch_from_web(product_url, local_dir, progress).await.context("fetching from http(s)")?;
    } else if let Some(_) = &path_from_file_url(product_url) {
        // Since the file is already local, no fetch is necessary.
    } else {
        bail!("Unexpected URI scheme in ({:?})", product_url);
    }
    Ok(())
}

async fn fetch_from_web<F>(
    _product_uri: &url::Url,
    _local_dir: &Path,
    _progress: &mut F,
) -> Result<()>
where
    F: FnMut(DirectoryProgress<'_>, FileProgress<'_>) -> ProgressResult,
{
    // TODO(fxbug.dev/93850): implement pbms.
    unimplemented!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_path_from_file_url() {
        let input = url::Url::parse("fake://foo#bar").expect("url");
        let output = path_from_file_url(&input);
        assert!(output.is_none());

        let input = url::Url::parse("file:///../../foo#bar").expect("url");
        let output = path_from_file_url(&input);
        assert_eq!(output, Some(Path::new("/foo").to_path_buf()));

        let input = url::Url::parse("file://foo#bar").expect("url");
        let output = path_from_file_url(&input);
        assert!(output.is_none());

        let input = url::Url::parse("file:///foo#bar").expect("url");
        let output = path_from_file_url(&input);
        assert_eq!(output, Some(Path::new("/foo").to_path_buf()));

        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let base_url = url::Url::from_directory_path(temp_dir.path().join("a/b/c/d")).expect("url");
        let input =
            url::Url::options().base_url(Some(&base_url)).parse("../../foo#bar").expect("url");
        let output = path_from_file_url(&input);
        assert_eq!(output, Some(temp_dir.path().join("a/b/foo").to_path_buf()));
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_url_sans_fragment() {
        let input = url::Url::parse("fake://foo#bar").expect("url");
        let output = url_sans_fragment(&input).expect("sans fragment");
        assert_eq!(output, url::Url::parse("fake://foo").expect("check url"));

        let input = url::Url::parse("fake://foo").expect("url");
        let output = url_sans_fragment(&input).expect("sans fragment");
        assert_eq!(output, url::Url::parse("fake://foo").expect("check url"));
    }

    // Disabling this test until a test config can be modified without altering
    // the local user's config.
    #[ignore]
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_local_path_helper() {
        let url = url::Url::parse("fake://foo#bar").expect("url");
        let path = local_path_helper(&url, "foo", /*dir=*/ true).await.expect("dir helper");
        assert!(path.to_string_lossy().ends_with("ffx/pbms/951333825719265977/foo"));

        // Note that the hash will be the same even though the fragment is
        // different.
        let url = url::Url::parse("fake://foo#blah").expect("url");
        let path = local_path_helper(&url, "foo", /*dir=*/ true).await.expect("dir helper");
        assert!(path.to_string_lossy().ends_with("ffx/pbms/951333825719265977/foo"));

        let url = url::Url::parse("gs://foo/blah/*.json#bar").expect("url");
        let path = local_path_helper(&url, "foo", /*dir=*/ true).await.expect("dir helper");
        assert!(path.to_string_lossy().ends_with("ffx/pbms/16042545670964745983/foo"));

        let url = url::Url::parse("file:///foo/blah/*.json#bar").expect("url");
        let path = local_path_helper(&url, "foo", /*dir=*/ true).await.expect("dir helper");
        assert_eq!(path.to_string_lossy(), "/foo/blah");

        let url = url::Url::parse("file:///foo/blah/*.json#bar").expect("url");
        let path = local_path_helper(&url, "foo", /*dir=*/ false).await.expect("dir helper");
        assert_eq!(path.to_string_lossy(), "/foo/blah/*.json");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    #[should_panic(expected = "Unexpected image format")]
    async fn test_fetch_by_format() {
        let url = url::Url::parse("fake://foo").expect("url");
        fetch_by_format("bad", &url, &Path::new("unused"), &mut |_d, _f| {
            Ok(ProgressResponse::Continue)
        })
        .await
        .expect("bad fetch");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    #[should_panic(expected = "Unexpected URI scheme")]
    async fn test_fetch_bundle_uri() {
        let url = url::Url::parse("fake://foo").expect("url");
        fetch_bundle_uri(&url, &Path::new("unused"), &mut |_d, _f| Ok(ProgressResponse::Continue))
            .await
            .expect("bad fetch");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_pb_dir_name() {
        let url = url::Url::parse("fake://foo").expect("url");
        let hash = pb_dir_name(&url);
        assert!(url.as_str() != hash);
        assert!(!hash.contains("/"));
        assert!(!hash.contains(" "));
    }
}
