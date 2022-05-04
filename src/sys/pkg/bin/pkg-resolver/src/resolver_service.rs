// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{
        cache::{
            BasePackageIndex, BlobFetcher, CacheError::*, MerkleForError, MerkleForError::*,
            ToResolveError as _, ToResolveStatus as _,
        },
        eager_package_manager::EagerPackageManager,
        font_package_manager::FontPackageManager,
        repository_manager::RepositoryManager,
        repository_manager::{GetPackageError, GetPackageError::*, GetPackageHashError},
        rewrite_manager::RewriteManager,
    },
    anyhow::{anyhow, Context as _, Error},
    async_lock::RwLock as AsyncRwLock,
    async_trait::async_trait,
    cobalt_sw_delivery_registry as metrics,
    fidl::endpoints::ServerEnd,
    fidl_fuchsia_io as fio,
    fidl_fuchsia_pkg::{
        FontResolverRequest, FontResolverRequestStream, PackageResolverRequest,
        PackageResolverRequestStream,
    },
    fidl_fuchsia_pkg_ext::{self as pkg, BlobId},
    fuchsia_cobalt::CobaltSender,
    fuchsia_pkg::PackageDirectory,
    fuchsia_syslog::{fx_log_err, fx_log_info, fx_log_warn},
    fuchsia_trace as trace,
    fuchsia_url::pkg_url::{ParseError, PkgUrl},
    fuchsia_zircon::Status,
    futures::{future::Future, stream::TryStreamExt as _},
    std::sync::Arc,
    std::time::Instant,
    system_image::CachePackages,
};

mod inspect;
pub use inspect::ResolverService as ResolverServiceInspectState;

/// Work-queue based package resolver. When all clones of
/// [`QueuedResolver`] are dropped, the queue will resolve all remaining
/// packages and terminate its output stream.
#[derive(Clone, Debug)]
pub struct QueuedResolver {
    queue: work_queue::WorkSender<PkgUrl, (), Result<PackageDirectory, Arc<GetPackageError>>>,
    cache: pkg::cache::Client,
    base_package_index: Arc<BasePackageIndex>,
    rewriter: Arc<AsyncRwLock<RewriteManager>>,
    system_cache_list: Arc<CachePackages>,
    inspect: Arc<ResolverServiceInspectState>,
}

/// This trait represents an instance which can be used to resolve package URLs, which typically
/// ensures that packages are available on the local filesystem and/or fetch them from remote
/// repository if necessary and possible.
#[async_trait]
pub trait Resolver: std::fmt::Debug + Sync + Sized {
    async fn resolve(
        &self,
        url: PkgUrl,
        eager_package_manager: Option<&EagerPackageManager<Self>>,
    ) -> Result<PackageDirectory, pkg::ResolveError>;
}

#[async_trait]
impl Resolver for QueuedResolver {
    async fn resolve(
        &self,
        pkg_url: PkgUrl,
        eager_package_manager: Option<&EagerPackageManager<Self>>,
    ) -> Result<PackageDirectory, pkg::ResolveError> {
        trace::duration_begin!("app", "resolve", "url" => pkg_url.to_string().as_str());
        // While the fuchsia-pkg:// spec allows resource paths, the package resolver should not be
        // given one.
        if pkg_url.resource().is_some() {
            fx_log_err!("package url should not contain a resource name: {}", pkg_url);
            return Err(pkg::ResolveError::InvalidUrl);
        }

        // Base pin.
        let package_inspect = self.inspect.resolve(&pkg_url);
        if let Some(blob) = self.base_package_index.is_unpinned_base_package(&pkg_url) {
            let dir = self.cache.open(blob).await.map_err(|e| {
                let error = e.to_resolve_error();
                fx_log_err!("failed to open base package url {:?}: {:#}", pkg_url, anyhow!(e));
                error
            })?;
            fx_log_info!("resolved {} to {} with base pin", pkg_url, blob);
            return Ok(dir);
        }

        // Rewrite the url.
        let rewritten_url =
            rewrite_url(&self.rewriter, &pkg_url).await.map_err(|e| e.to_resolve_error())?;
        let _package_inspect = package_inspect.rewritten_url(&rewritten_url);

        // Attempt to use EagerPackageManager to resolve the package.
        if let Some(eager_package_manager) = eager_package_manager {
            if let Some(dir) =
                eager_package_manager.get_package_dir(&rewritten_url).map_err(|e| {
                    fx_log_err!(
                        "failed to resolve eager package at {} as {}: {:#}",
                        pkg_url,
                        rewritten_url,
                        e
                    );
                    pkg::ResolveError::PackageNotFound
                })?
            {
                fx_log_info!(
                    "resolved {} as {} with eager package manager",
                    pkg_url,
                    rewritten_url,
                );
                return Ok(dir);
            }
        }

        // Fetch from TUF.
        let queued_fetch = self.queue.push(rewritten_url.clone(), ());
        let pkg_or_status = match queued_fetch.await.expect("expected queue to be open") {
            Ok(v) => Ok(v),
            Err(e) => self.handle_cache_fallbacks(e, &pkg_url, &rewritten_url).await,
        };

        let err_str = match pkg_or_status {
            Ok(_) => "no error".to_string(),
            Err(ref e) => e.to_string(),
        };
        trace::duration_end!("app", "resolve", "status" => err_str.as_str());
        pkg_or_status
    }
}

impl QueuedResolver {
    /// Creates an unbounded queue that will resolve up to `max_concurrency`
    /// packages at once.
    pub fn new(
        cache_client: pkg::cache::Client,
        base_package_index: Arc<BasePackageIndex>,
        system_cache_list: Arc<CachePackages>,
        repo_manager: Arc<AsyncRwLock<RepositoryManager>>,
        rewriter: Arc<AsyncRwLock<RewriteManager>>,
        blob_fetcher: BlobFetcher,
        max_concurrency: usize,
        inspect: Arc<ResolverServiceInspectState>,
    ) -> (impl Future<Output = ()>, Self) {
        let cache = cache_client.clone();
        let (package_fetch_queue, queue) =
            work_queue::work_queue(max_concurrency, move |rewritten_url: PkgUrl, _: ()| {
                let cache = cache_client.clone();
                let repo_manager = Arc::clone(&repo_manager);
                let blob_fetcher = blob_fetcher.clone();
                async move {
                    Ok(package_from_repo(&repo_manager, &rewritten_url, cache, blob_fetcher)
                        .await
                        .map_err(Arc::new)?)
                }
            });
        let fetcher =
            Self { queue, inspect, base_package_index, cache, rewriter, system_cache_list };
        (package_fetch_queue.into_future(), fetcher)
    }

    async fn handle_cache_fallbacks(
        &self,
        error: Arc<GetPackageError>,
        pkg_url: &PkgUrl,
        rewritten_url: &PkgUrl,
    ) -> Result<PackageDirectory, pkg::ResolveError> {
        // NB: Combination of {:#} formatting applied to an `anyhow::Error` allows to print the
        // display impls of all the source errors.

        let error_clone = error.clone();
        // Declare an error to extend lifetime of errors generated inside match.
        let outer_error;
        let maybe_package: Result<PackageDirectory, &GetPackageError> = match &*error {
            Cache(MerkleFor(NotFound)) => {
                // If we can get metadata but the repo doesn't know about the package,
                // it shouldn't be in the cache, BUT some SDK customers currently rely on this
                // behavior.
                // TODO(fxbug.dev/50764): remove this behavior.
                match missing_cache_package_disk_fallback(
                    &rewritten_url,
                    pkg_url,
                    &self.system_cache_list,
                    &self.inspect,
                ) {
                    Some(blob) => match self.cache.open(blob).await {
                        Ok(pkg) => {
                            fx_log_info!(
                                "resolved {} as {} to {} with cache_packages due to {:#}",
                                pkg_url,
                                rewritten_url,
                                blob,
                                anyhow!(error)
                            );
                            Ok(pkg)
                        }
                        Err(open_error) => {
                            outer_error = Into::into(open_error);
                            Err(&outer_error)
                        }
                    },
                    None => Err(&error),
                }
            }
            RepoNotFound(..)
            | OpenRepo(..)
            | Cache(Fidl(..))
            | Cache(ListNeeds(..))
            | Cache(MerkleFor(FetchTargetDescription(..)))
            | Cache(MerkleFor(InvalidTargetPath(..)))
            | Cache(MerkleFor(NoCustomMetadata))
            | Cache(MerkleFor(SerdeError(..))) => {
                // If we couldn't get TUF metadata, we might not have networking. Check in
                // system/data/cache_packages (not to be confused with pkg-cache). The
                // cache_packages manifest pkg URLs are for fuchsia.com, so do not use the
                // rewritten URL.
                match hash_from_cache_packages_manifest(&pkg_url, &self.system_cache_list) {
                    Some(blob) => match self.cache.open(blob).await {
                        Ok(pkg) => {
                            fx_log_info!(
                                "resolved {} as {} to {} with cache_packages due to {:#}",
                                pkg_url,
                                rewritten_url,
                                blob,
                                anyhow!(error)
                            );
                            Ok(pkg)
                        }
                        Err(open_error) => {
                            outer_error = Into::into(open_error);
                            Err(&outer_error)
                        }
                    },
                    None => Err(&error),
                }
            }
            OpenPackage(..)
            | Cache(FetchContentBlob(..))
            | Cache(FetchMetaFar(..))
            | Cache(Get(_)) => {
                // We could talk to TUF and we know there's a new version of this package,
                // but we coldn't retrieve its blobs for some reason. Refuse to fall back to
                // cache_packages and instead return an error for the resolve, which is consistent
                // with the path for packages which are not in cache_packages.
                //
                // We don't use cache_packages in production, and when developers resolve a package
                // on a bench they expect the newest version, or a failure. cache_packages are great
                // for running packages before networking is up, but for these two error conditions,
                // we know we have networking because we could talk to TUF.
                Err(&error)
            }
        };
        maybe_package.map_err(|e| {
            fx_log_warn!(
                "error resolving {} as {}: {:#}",
                pkg_url,
                rewritten_url,
                anyhow!(error_clone)
            );
            e.to_resolve_error()
        })
    }
}

#[derive(Clone, Debug)]
#[cfg(test)]
/// Creates a mocked PackageResolver that resolves any url using the given callback.
pub struct MockResolver {
    queue: work_queue::WorkSender<PkgUrl, (), Result<PackageDirectory, Arc<GetPackageError>>>,
}

#[cfg(test)]
impl MockResolver {
    pub fn new<W, F>(callback: W) -> Self
    where
        W: Fn(PkgUrl) -> F + Send + 'static,
        F: Future<Output = Result<PackageDirectory, Arc<GetPackageError>>> + Send,
    {
        let (package_fetch_queue, queue) =
            work_queue::work_queue(1, move |url, _: ()| callback(url));
        fuchsia_async::Task::spawn(package_fetch_queue.into_future()).detach();
        Self { queue }
    }
}

#[cfg(test)]
#[async_trait]
impl Resolver for MockResolver {
    async fn resolve(
        &self,
        url: PkgUrl,
        _eager_package_manager: Option<&EagerPackageManager<Self>>,
    ) -> Result<PackageDirectory, pkg::ResolveError> {
        let queued_fetch = self.queue.push(url, ());
        queued_fetch.await.expect("expected queue to be open").map_err(|e| e.to_resolve_error())
    }
}

pub async fn run_resolver_service(
    repo_manager: Arc<AsyncRwLock<RepositoryManager>>,
    rewriter: Arc<AsyncRwLock<RewriteManager>>,
    package_resolver: QueuedResolver,
    base_package_index: Arc<BasePackageIndex>,
    system_cache_list: Arc<CachePackages>,
    stream: PackageResolverRequestStream,
    cobalt_sender: CobaltSender,
    inspect: Arc<ResolverServiceInspectState>,
    eager_package_manager: Arc<Option<EagerPackageManager<QueuedResolver>>>,
) -> Result<(), Error> {
    stream
        .map_err(anyhow::Error::new)
        .try_for_each_concurrent(None, |event| async {
            let mut cobalt_sender = cobalt_sender.clone();
            match event {
                PackageResolverRequest::Resolve { package_url, dir, responder } => {
                    let start_time = Instant::now();
                    let response = resolve_and_reopen(
                        &package_resolver,
                        package_url.clone(),
                        dir,
                        eager_package_manager.as_ref().as_ref(),
                    )
                    .await;

                    cobalt_sender.log_event_count(
                        metrics::RESOLVE_STATUS_METRIC_ID,
                        (resolve_result_to_resolve_status_code(&response),),
                        0,
                        1,
                    );

                    cobalt_sender.log_elapsed_time(
                        metrics::RESOLVE_DURATION_METRIC_ID,
                        (
                            resolve_result_to_resolve_duration_code(&response),
                            metrics::ResolveDurationMetricDimensionResolverType::Regular,
                        ),
                        Instant::now().duration_since(start_time).as_micros() as i64,
                    );
                    responder.send(&mut response.map_err(|status| status.into())).with_context(
                        || {
                            format!(
                                "sending fuchsia.pkg/PackageResolver.Resolve response for {:?}",
                                package_url
                            )
                        },
                    )?;
                    Ok(())
                }

                PackageResolverRequest::GetHash { package_url, responder } => {
                    match get_hash(
                        &rewriter,
                        &repo_manager,
                        &base_package_index,
                        &system_cache_list,
                        &package_url.url,
                        &inspect,
                        eager_package_manager.as_ref().as_ref(),
                    )
                    .await
                    {
                        Ok(blob_id) => {
                            responder.send(&mut Ok(blob_id.into())).with_context(|| {
                                format!(
                                    "sending fuchsia.pkg/PackageResolver.GetHash success \
                                         response for {:?}",
                                    package_url.url
                                )
                            })?;
                        }
                        Err(status) => {
                            responder.send(&mut Err(status.into_raw())).with_context(|| {
                                format!(
                                    "sending fuchsia.pkg/PackageResolver.GetHash failure \
                                         response for {:?}",
                                    package_url.url
                                )
                            })?;
                        }
                    }
                    Ok(())
                }
            }
        })
        .await
}

async fn rewrite_url(
    rewriter: &AsyncRwLock<RewriteManager>,
    url: &PkgUrl,
) -> Result<PkgUrl, Status> {
    Ok(rewriter.read().await.rewrite(url))
}

fn missing_cache_package_disk_fallback(
    rewritten_url: &PkgUrl,
    pkg_url: &PkgUrl,
    system_cache_list: &CachePackages,
    inspect: &ResolverServiceInspectState,
) -> Option<BlobId> {
    let possible_fallback = hash_from_cache_packages_manifest(&pkg_url, system_cache_list);
    if possible_fallback.is_some() {
        fx_log_warn!(
            "Did not find {} at URL {}, but did find a matching package name in the \
            built-in cache packages set, so falling back to it. Your package \
            repository may not be configured to serve the package correctly, or may \
            be overriding the domain for the repository which would normally serve \
            this package. This will be an error in a future version of Fuchsia, see \
            fxbug.dev/50748.",
            rewritten_url.name(),
            rewritten_url
        );
        inspect.cache_fallback_due_to_not_found();
    }
    possible_fallback
}

enum HashSource<TufError> {
    Tuf(BlobId),
    SystemImageCachePackages(BlobId, TufError),
}

async fn hash_from_base_or_repo_or_cache(
    repo_manager: &AsyncRwLock<RepositoryManager>,
    rewriter: &AsyncRwLock<RewriteManager>,
    base_package_index: &BasePackageIndex,
    system_cache_list: &CachePackages,
    pkg_url: &PkgUrl,
    inspect_state: &ResolverServiceInspectState,
    eager_package_manager: Option<&EagerPackageManager<QueuedResolver>>,
) -> Result<BlobId, Status> {
    if let Some(blob) = base_package_index.is_unpinned_base_package(pkg_url) {
        fx_log_info!("get_hash for {} to {} with base pin", pkg_url, blob);
        return Ok(blob);
    }

    let rewritten_url = rewrite_url(rewriter, &pkg_url).await?;

    // Attempt to use EagerPackageManager to resolve the package.
    if let Some(eager_package_manager) = eager_package_manager {
        if let Some(dir) = eager_package_manager.get_package_dir(&pkg_url).map_err(|err| {
            fx_log_err!("retrieval error eager package url {}: {:#}", pkg_url, anyhow!(err));
            Status::NOT_FOUND
        })? {
            return dir.merkle_root().await.map(Into::into).map_err(|err| {
                fx_log_err!("package hash error eager package url {}: {:#}", pkg_url, anyhow!(err));
                Status::INTERNAL
            });
        }
    }

    hash_from_repo_or_cache(repo_manager, system_cache_list, pkg_url, &rewritten_url, inspect_state)
        .await
        .map_err(|e| {
            let status = e.to_resolve_status();
            fx_log_warn!("error getting hash {} as {}: {:#}", pkg_url, rewritten_url, anyhow!(e));
            status
        })
        .map(|hash| match hash {
            HashSource::Tuf(blob) => {
                fx_log_info!("get_hash for {} as {} to {} with TUF", pkg_url, rewritten_url, blob);
                blob
            }
            HashSource::SystemImageCachePackages(blob, tuf_err) => {
                fx_log_info!(
                    "get_hash for {} as {} to {} with cache_packages due to {:#}",
                    pkg_url,
                    rewritten_url,
                    blob,
                    anyhow!(tuf_err)
                );
                blob
            }
        })
}
async fn hash_from_repo_or_cache(
    repo_manager: &AsyncRwLock<RepositoryManager>,
    system_cache_list: &CachePackages,
    pkg_url: &PkgUrl,
    rewritten_url: &PkgUrl,
    inspect_state: &ResolverServiceInspectState,
) -> Result<HashSource<GetPackageHashError>, GetPackageHashError> {
    // The RwLock created by `.read()` must not exist across the `.await` (to e.g. prevent
    // deadlock). Rust temporaries are kept alive for the duration of the innermost enclosing
    // statement, so the following two lines should not be combined.
    let fut = repo_manager.read().await.get_package_hash(&rewritten_url);
    match fut.await {
        Ok(b) => Ok(HashSource::Tuf(b)),
        Err(e @ GetPackageHashError::MerkleFor(MerkleForError::NotFound)) => {
            // If we can get metadata but the repo doesn't know about the package,
            // it shouldn't be in the cache, BUT some SDK customers currently rely on this behavior.
            // TODO(fxbug.dev/50764): remove this behavior.
            match missing_cache_package_disk_fallback(
                &rewritten_url,
                pkg_url,
                system_cache_list,
                inspect_state,
            ) {
                Some(blob) => Ok(HashSource::SystemImageCachePackages(blob, e)),
                None => Err(e),
            }
        }
        Err(e) => {
            // If we couldn't get TUF metadata, we might not have networking. Check in
            // system/data/cache_packages (not to be confused with pkg-cache). The cache_packages
            // manifest pkg URLs are for fuchsia.com, so do not use the rewritten URL.
            match hash_from_cache_packages_manifest(&pkg_url, system_cache_list) {
                Some(blob) => Ok(HashSource::SystemImageCachePackages(blob, e)),
                None => Err(e),
            }
        }
    }
}

async fn package_from_repo(
    repo_manager: &AsyncRwLock<RepositoryManager>,
    rewritten_url: &PkgUrl,
    cache: pkg::cache::Client,
    blob_fetcher: BlobFetcher,
) -> Result<PackageDirectory, GetPackageError> {
    // The RwLock created by `.read()` must not exist across the `.await` (to e.g. prevent
    // deadlock). Rust temporaries are kept alive for the duration of the innermost enclosing
    // statement, so the following two lines should not be combined.
    let fut = repo_manager.read().await.get_package(&rewritten_url, &cache, &blob_fetcher);
    match fut.await {
        Ok((blob, dir)) => {
            fx_log_info!("resolved {} to {} with TUF", rewritten_url, blob);
            Ok(dir)
        }
        Err(e) => {
            // We could talk to TUF and we know there's a new version of this package,
            // but we coldn't retrieve its blobs for some reason. Refuse to fall back to
            // cache_packages and instead return an error for the resolve, which is consistent with
            // the path for packages which are not in cache_packages.
            //
            // We don't use cache_packages in production, and when developers resolve a package on
            // a bench they expect the newest version, or a failure. cache_packages are great for
            // running packages before networking is up, but for these two error conditions,
            // we know we have networking because we could talk to TUF.
            Err(e)
        }
    }
}

// Attempts to lookup the hash of a package from `system_cache_list`, which is populated from the
// cache_packages manifest of the system_image package. The cache_packages manifest only includes
// the package path and assumes all hosts are "fuchsia.com", so this fn can only succeed on
// `PkgUrl`s with a host of "fuchsia.com".
fn hash_from_cache_packages_manifest<'a>(
    url: &PkgUrl,
    system_cache_list: &'a CachePackages,
) -> Option<BlobId> {
    if url.host() != "fuchsia.com" {
        return None;
    }
    // We are in the process of removing the concept of package variant
    // (generalizing fuchsia-pkg URL paths to be `(first-segment)(/more-segments)*`
    // instead of requiring that paths are `(name)/(variant)`. Towards this goal,
    // the PkgUrls the pkg-resolver gets from the pkg-cache from `PackageCache.CachePackageIndex`
    // do not have variants. However, they are intended to match only URLs with variant of "0".
    // Additionally, pkg-resolver allows clients to not specify a variant, in which case a
    // variant of "0" will be assumed. This means that if the URL we are resolving has a
    // variant that is not "0", it should never match anything in the cache packages manifest,
    // and if the URL has a variant of "0", we should remove it before checking the cache manifest.
    let url = match url.variant() {
        None => url.clone(),
        Some(variant) if !variant.is_zero() => {
            return None;
        }
        Some(_) => url.strip_variant(),
    };

    let cache_hash = system_cache_list.hash_for_package(&url).map(Into::into);
    match (cache_hash, url.package_hash()) {
        // This arm is less useful than (Some, None) b/c generally metadata lookup for pinned URLs
        // succeeds (because generally the package from the pinned URL exists in the repo), and if
        // the blob fetching failed, then even if this fn returns success, the resolve will still
        // end up failing when we try to open the package from pkg-cache. The arm is still useful
        // if initial creation of the TUF client for fuchsia.com fails (e.g. because networking is
        // down/not yet available).
        (Some(cache), Some(url)) if cache == BlobId::from(*url) => Some(cache),
        (Some(cache), None) => Some(cache),
        _ => None,
    }
}

async fn get_hash(
    rewriter: &AsyncRwLock<RewriteManager>,
    repo_manager: &AsyncRwLock<RepositoryManager>,
    base_package_index: &BasePackageIndex,
    system_cache_list: &CachePackages,
    url: &str,
    inspect_state: &ResolverServiceInspectState,
    eager_package_manager: Option<&EagerPackageManager<QueuedResolver>>,
) -> Result<BlobId, Status> {
    let pkg_url = PkgUrl::parse(url).map_err(|e| handle_bad_package_url(e, url))?;
    // While the fuchsia-pkg:// spec allows resource paths, the package resolver should not be
    // given one.
    if pkg_url.resource().is_some() {
        fx_log_err!("package url should not contain a resource name: {}", pkg_url);
        return Err(Status::INVALID_ARGS);
    }

    trace::duration_begin!("app", "get-hash", "url" => pkg_url.to_string().as_str());
    let hash_or_status = hash_from_base_or_repo_or_cache(
        &repo_manager,
        &rewriter,
        &base_package_index,
        &system_cache_list,
        &pkg_url,
        inspect_state,
        eager_package_manager,
    )
    .await;
    trace::duration_end!("app", "get-hash",
        "status" => hash_or_status.err().unwrap_or(Status::OK).to_string().as_str());
    hash_or_status
}

async fn resolve_and_reopen(
    package_resolver: &QueuedResolver,
    url: String,
    dir_request: ServerEnd<fio::DirectoryMarker>,
    eager_package_manager: Option<&EagerPackageManager<QueuedResolver>>,
) -> Result<(), pkg::ResolveError> {
    let pkg_url = PkgUrl::parse(&url).map_err(|e| handle_bad_package_url_error(e, &url))?;
    let pkg = package_resolver.resolve(pkg_url.clone(), eager_package_manager).await?;

    pkg.reopen(dir_request).map_err(|clone_err| {
        fx_log_err!("failed to open package url {:?}: {:#}", pkg_url, anyhow!(clone_err));
        pkg::ResolveError::Internal
    })
}

/// Run a service that only resolves registered font packages.
pub async fn run_font_resolver_service(
    font_package_manager: Arc<FontPackageManager>,
    package_resolver: QueuedResolver,
    stream: FontResolverRequestStream,
    cobalt_sender: CobaltSender,
) -> Result<(), Error> {
    stream
        .map_err(anyhow::Error::new)
        .try_for_each_concurrent(None, |event| async {
            let mut cobalt_sender = cobalt_sender.clone();
            let FontResolverRequest::Resolve { package_url, directory_request, responder } = event;
            let start_time = Instant::now();
            let response = resolve_font(
                &font_package_manager,
                &package_resolver,
                package_url,
                directory_request,
                cobalt_sender.clone(),
            )
            .await;

            let response_legacy = response.clone().map_err(|s| s.to_resolve_status());
            cobalt_sender.log_event_count(
                metrics::RESOLVE_METRIC_ID,
                (
                    resolve_result_to_resolve_code(response_legacy),
                    metrics::ResolveMetricDimensionResolverType::Font,
                ),
                0,
                1,
            );
            cobalt_sender.log_elapsed_time(
                metrics::RESOLVE_DURATION_METRIC_ID,
                (
                    resolve_result_to_resolve_duration_code(&response),
                    metrics::ResolveDurationMetricDimensionResolverType::Font,
                ),
                Instant::now().duration_since(start_time).as_micros() as i64,
            );
            responder.send(&mut response_legacy.map_err(|s| s.into_raw()))?;
            Ok(())
        })
        .await
}

/// Resolve a single font package.
async fn resolve_font<'a>(
    font_package_manager: &'a Arc<FontPackageManager>,
    package_resolver: &'a QueuedResolver,
    package_url: String,
    directory_request: ServerEnd<fio::DirectoryMarker>,
    mut cobalt_sender: CobaltSender,
) -> Result<(), pkg::ResolveError> {
    let parsed_package_url =
        PkgUrl::parse(&package_url).map_err(|e| handle_bad_package_url_error(e, &package_url))?;
    let is_font_package = font_package_manager.is_font_package(&parsed_package_url);
    cobalt_sender.log_event_count(
        metrics::IS_FONT_PACKAGE_CHECK_METRIC_ID,
        if is_font_package {
            metrics::IsFontPackageCheckMetricDimensionResult::Font
        } else {
            metrics::IsFontPackageCheckMetricDimensionResult::NotFont
        },
        0,
        1,
    );
    if is_font_package {
        resolve_and_reopen(&package_resolver, package_url, directory_request, None).await
    } else {
        fx_log_err!("font resolver asked to resolve non-font package: {}", package_url);
        Err(pkg::ResolveError::PackageNotFound)
    }
}

fn handle_bad_package_url_error(parse_error: ParseError, pkg_url: &str) -> pkg::ResolveError {
    fx_log_err!("failed to parse package url {:?}: {:#}", pkg_url, anyhow!(parse_error));
    pkg::ResolveError::InvalidUrl
}

fn handle_bad_package_url(parse_error: ParseError, pkg_url: &str) -> Status {
    fx_log_err!("failed to parse package url {:?}: {:#}", pkg_url, anyhow!(parse_error));
    Status::INVALID_ARGS
}

fn resolve_result_to_resolve_duration_code<T>(
    res: &Result<(), T>,
) -> metrics::ResolveDurationMetricDimensionResult {
    match res {
        Ok(_) => metrics::ResolveDurationMetricDimensionResult::Success,
        Err(_) => metrics::ResolveDurationMetricDimensionResult::Failure,
    }
}

fn resolve_result_to_resolve_status_code(
    result: &Result<(), pkg::ResolveError>,
) -> metrics::ResolveStatusMetricDimensionResult {
    match *result {
        Ok(()) => metrics::ResolveStatusMetricDimensionResult::Success,
        Err(pkg::ResolveError::Internal) => metrics::ResolveStatusMetricDimensionResult::Internal,
        Err(pkg::ResolveError::AccessDenied) => {
            metrics::ResolveStatusMetricDimensionResult::AccessDenied
        }
        Err(pkg::ResolveError::Io) => metrics::ResolveStatusMetricDimensionResult::Io,
        Err(pkg::ResolveError::BlobNotFound) => {
            metrics::ResolveStatusMetricDimensionResult::BlobNotFound
        }
        Err(pkg::ResolveError::PackageNotFound) => {
            metrics::ResolveStatusMetricDimensionResult::PackageNotFound
        }
        Err(pkg::ResolveError::RepoNotFound) => {
            metrics::ResolveStatusMetricDimensionResult::RepoNotFound
        }
        Err(pkg::ResolveError::NoSpace) => metrics::ResolveStatusMetricDimensionResult::NoSpace,
        Err(pkg::ResolveError::UnavailableBlob) => {
            metrics::ResolveStatusMetricDimensionResult::UnavailableBlob
        }
        Err(pkg::ResolveError::UnavailableRepoMetadata) => {
            metrics::ResolveStatusMetricDimensionResult::UnavailableRepoMetadata
        }
        Err(pkg::ResolveError::InvalidUrl) => {
            metrics::ResolveStatusMetricDimensionResult::InvalidUrl
        }
    }
}

fn resolve_result_to_resolve_code(
    result: Result<(), Status>,
) -> metrics::ResolveMetricDimensionResult {
    match result {
        Ok(()) => metrics::ResolveMetricDimensionResult::ZxOk,
        Err(Status::INTERNAL) => metrics::ResolveMetricDimensionResult::ZxErrInternal,
        Err(Status::NOT_SUPPORTED) => metrics::ResolveMetricDimensionResult::ZxErrNotSupported,
        Err(Status::NO_RESOURCES) => metrics::ResolveMetricDimensionResult::ZxErrNoResources,
        Err(Status::NO_MEMORY) => metrics::ResolveMetricDimensionResult::ZxErrNoMemory,
        Err(Status::INTERRUPTED_RETRY) => {
            metrics::ResolveMetricDimensionResult::ZxErrInternalIntrRetry
        }
        Err(Status::INVALID_ARGS) => metrics::ResolveMetricDimensionResult::ZxErrInvalidArgs,
        Err(Status::BAD_HANDLE) => metrics::ResolveMetricDimensionResult::ZxErrBadHandle,
        Err(Status::WRONG_TYPE) => metrics::ResolveMetricDimensionResult::ZxErrWrongType,
        Err(Status::BAD_SYSCALL) => metrics::ResolveMetricDimensionResult::ZxErrBadSyscall,
        Err(Status::OUT_OF_RANGE) => metrics::ResolveMetricDimensionResult::ZxErrOutOfRange,
        Err(Status::BUFFER_TOO_SMALL) => metrics::ResolveMetricDimensionResult::ZxErrBufferTooSmall,
        Err(Status::BAD_STATE) => metrics::ResolveMetricDimensionResult::ZxErrBadState,
        Err(Status::TIMED_OUT) => metrics::ResolveMetricDimensionResult::ZxErrTimedOut,
        Err(Status::SHOULD_WAIT) => metrics::ResolveMetricDimensionResult::ZxErrShouldWait,
        Err(Status::CANCELED) => metrics::ResolveMetricDimensionResult::ZxErrCanceled,
        Err(Status::PEER_CLOSED) => metrics::ResolveMetricDimensionResult::ZxErrPeerClosed,
        Err(Status::NOT_FOUND) => metrics::ResolveMetricDimensionResult::ZxErrNotFound,
        Err(Status::ALREADY_EXISTS) => metrics::ResolveMetricDimensionResult::ZxErrAlreadyExists,
        Err(Status::ALREADY_BOUND) => metrics::ResolveMetricDimensionResult::ZxErrAlreadyBound,
        Err(Status::UNAVAILABLE) => metrics::ResolveMetricDimensionResult::ZxErrUnavailable,
        Err(Status::ACCESS_DENIED) => metrics::ResolveMetricDimensionResult::ZxErrAccessDenied,
        Err(Status::IO) => metrics::ResolveMetricDimensionResult::ZxErrIo,
        Err(Status::IO_REFUSED) => metrics::ResolveMetricDimensionResult::ZxErrIoRefused,
        Err(Status::IO_DATA_INTEGRITY) => {
            metrics::ResolveMetricDimensionResult::ZxErrIoDataIntegrity
        }
        Err(Status::IO_DATA_LOSS) => metrics::ResolveMetricDimensionResult::ZxErrIoDataLoss,
        Err(Status::IO_NOT_PRESENT) => metrics::ResolveMetricDimensionResult::ZxErrIoNotPresent,
        Err(Status::IO_OVERRUN) => metrics::ResolveMetricDimensionResult::ZxErrIoOverrun,
        Err(Status::IO_MISSED_DEADLINE) => {
            metrics::ResolveMetricDimensionResult::ZxErrIoMissedDeadline
        }
        Err(Status::IO_INVALID) => metrics::ResolveMetricDimensionResult::ZxErrIoInvalid,
        Err(Status::BAD_PATH) => metrics::ResolveMetricDimensionResult::ZxErrBadPath,
        Err(Status::NOT_DIR) => metrics::ResolveMetricDimensionResult::ZxErrNotDir,
        Err(Status::NOT_FILE) => metrics::ResolveMetricDimensionResult::ZxErrNotFile,
        Err(Status::FILE_BIG) => metrics::ResolveMetricDimensionResult::ZxErrFileBig,
        Err(Status::NO_SPACE) => metrics::ResolveMetricDimensionResult::ZxErrNoSpace,
        Err(Status::NOT_EMPTY) => metrics::ResolveMetricDimensionResult::ZxErrNotEmpty,
        Err(Status::STOP) => metrics::ResolveMetricDimensionResult::ZxErrStop,
        Err(Status::NEXT) => metrics::ResolveMetricDimensionResult::ZxErrNext,
        Err(Status::ASYNC) => metrics::ResolveMetricDimensionResult::ZxErrAsync,
        Err(Status::PROTOCOL_NOT_SUPPORTED) => {
            metrics::ResolveMetricDimensionResult::ZxErrProtocolNotSupported
        }
        Err(Status::ADDRESS_UNREACHABLE) => {
            metrics::ResolveMetricDimensionResult::ZxErrAddressUnreachable
        }
        Err(Status::ADDRESS_IN_USE) => metrics::ResolveMetricDimensionResult::ZxErrAddressInUse,
        Err(Status::NOT_CONNECTED) => metrics::ResolveMetricDimensionResult::ZxErrNotConnected,
        Err(Status::CONNECTION_REFUSED) => {
            metrics::ResolveMetricDimensionResult::ZxErrConnectionRefused
        }
        Err(Status::CONNECTION_RESET) => {
            metrics::ResolveMetricDimensionResult::ZxErrConnectionReset
        }
        Err(Status::CONNECTION_ABORTED) => {
            metrics::ResolveMetricDimensionResult::ZxErrConnectionAborted
        }
        Err(_) => metrics::ResolveMetricDimensionResult::UnexpectedZxStatusValue,
    }
}

#[cfg(test)]
mod tests {
    use {super::*, fuchsia_url::pkg_url::PinnedPkgUrl};

    #[test]
    fn test_hash_from_cache_packages_manifest() {
        let hash =
            "0000000000000000000000000000000000000000000000000000000000000000".parse().unwrap();
        let cache_packages = CachePackages::from_entries(vec![PinnedPkgUrl::new_package(
            "fuchsia.com".to_string(),
            "/potato".to_string(),
            hash,
        )
        .unwrap()]);
        let empty_cache_packages = CachePackages::from_entries(vec![]);

        let fuchsia_url = PkgUrl::parse("fuchsia-pkg://fuchsia.com/potato").unwrap();
        let variant_nonzero_fuchsia_url =
            PkgUrl::parse("fuchsia-pkg://fuchsia.com/potato/1").unwrap();
        let variant_zero_fuchsia_url = PkgUrl::parse("fuchsia-pkg://fuchsia.com/potato/0").unwrap();
        let other_repo_url = PkgUrl::parse("fuchsia-pkg://nope.com/potato").unwrap();
        assert_eq!(
            hash_from_cache_packages_manifest(&fuchsia_url, &cache_packages),
            Some(hash.into())
        );
        assert_eq!(
            hash_from_cache_packages_manifest(&variant_zero_fuchsia_url, &cache_packages),
            Some(hash.into())
        );
        assert_eq!(
            hash_from_cache_packages_manifest(&variant_nonzero_fuchsia_url, &cache_packages),
            None
        );
        assert_eq!(hash_from_cache_packages_manifest(&other_repo_url, &cache_packages), None);
        assert_eq!(hash_from_cache_packages_manifest(&fuchsia_url, &empty_cache_packages), None);
    }

    #[test]
    fn test_hash_from_cache_packages_manifest_with_zero_variant() {
        let hash =
            "0000000000000000000000000000000000000000000000000000000000000000".parse().unwrap();
        let cache_packages = CachePackages::from_entries(vec![PinnedPkgUrl::new_package(
            "fuchsia.com".to_string(),
            "/potato/0".to_string(),
            hash,
        )
        .unwrap()]);
        let empty_cache_packages = CachePackages::from_entries(vec![]);

        let fuchsia_url = PkgUrl::parse("fuchsia-pkg://fuchsia.com/potato").unwrap();
        let variant_nonzero_fuchsia_url =
            PkgUrl::parse("fuchsia-pkg://fuchsia.com/potato/1").unwrap();
        let variant_zero_fuchsia_url = PkgUrl::parse("fuchsia-pkg://fuchsia.com/potato/0").unwrap();
        let other_repo_url = PkgUrl::parse("fuchsia-pkg://nope.com/potato/0").unwrap();
        // hash_from_cache_packages_manifest removes variant from URL provided, and
        // since CachePackages is initialized with a variant and will only resolve url to a hash
        // if the /0 variant is provided.
        assert_eq!(hash_from_cache_packages_manifest(&fuchsia_url, &cache_packages), None);
        assert_eq!(
            hash_from_cache_packages_manifest(&variant_zero_fuchsia_url, &cache_packages),
            None
        );
        assert_eq!(
            hash_from_cache_packages_manifest(&variant_nonzero_fuchsia_url, &cache_packages),
            None
        );
        assert_eq!(hash_from_cache_packages_manifest(&other_repo_url, &cache_packages), None);
        assert_eq!(hash_from_cache_packages_manifest(&fuchsia_url, &empty_cache_packages), None);
    }
}
