// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{queue, repository::Repository, repository_manager::Stats},
    anyhow::anyhow,
    cobalt_client::traits::AsEventCode as _,
    cobalt_sw_delivery_registry as metrics,
    fidl::endpoints::ServerEnd,
    fidl_fuchsia_io::DirectoryMarker,
    fidl_fuchsia_pkg::{LocalMirrorProxy, PackageCacheProxy},
    fidl_fuchsia_pkg_ext::{BlobId, MirrorConfig, RepositoryConfig},
    fuchsia_async::TimeoutExt as _,
    fuchsia_cobalt::CobaltSender,
    fuchsia_syslog::{fx_log_err, fx_log_info},
    fuchsia_trace as trace,
    fuchsia_url::pkg_url::PkgUrl,
    fuchsia_zircon::Status,
    futures::{lock::Mutex as AsyncMutex, prelude::*, stream::FuturesUnordered},
    http_uri_ext::HttpUriExt as _,
    hyper::{body::HttpBody, Body, Request, StatusCode},
    parking_lot::Mutex,
    pkgfs::install::BlobKind,
    std::{
        collections::HashSet,
        hash::Hash,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        time::Duration,
    },
    tuf::metadata::TargetPath,
};

mod base_package_index;
pub use base_package_index::BasePackageIndex;

mod inspect;
mod retry;

pub type BlobFetcher = queue::WorkSender<BlobId, FetchBlobContext, Result<(), Arc<FetchError>>>;

/// Root of typesafe builder for BlobNetworkTimeouts.
#[derive(Clone, Copy, Debug)]
pub struct BlobNetworkTimeoutsBuilderNeedsHeader;

impl BlobNetworkTimeoutsBuilderNeedsHeader {
    pub fn header(self, header: Duration) -> BlobNetworkTimeoutsBuilderNeedsBody {
        BlobNetworkTimeoutsBuilderNeedsBody { header }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct BlobNetworkTimeoutsBuilderNeedsBody {
    header: Duration,
}

impl BlobNetworkTimeoutsBuilderNeedsBody {
    pub fn body(self, body: Duration) -> BlobNetworkTimeouts {
        BlobNetworkTimeouts { header: self.header, body }
    }
}

/// Timeouts for blob network operations.
#[derive(Clone, Copy, Debug)]
pub struct BlobNetworkTimeouts {
    header: Duration,
    body: Duration,
}

impl BlobNetworkTimeouts {
    pub fn builder() -> BlobNetworkTimeoutsBuilderNeedsHeader {
        BlobNetworkTimeoutsBuilderNeedsHeader
    }

    pub fn header(&self) -> Duration {
        self.header
    }

    pub fn body(&self) -> Duration {
        self.body
    }
}

/// Provides access to the package cache components.
#[derive(Clone)]
pub struct PackageCache {
    cache: PackageCacheProxy,
    pkgfs_install: pkgfs::install::Client,
    pkgfs_needs: pkgfs::needs::Client,
}

impl PackageCache {
    /// Constructs a new [`PackageCache`].
    pub fn new(
        cache: PackageCacheProxy,
        pkgfs_install: pkgfs::install::Client,
        pkgfs_needs: pkgfs::needs::Client,
    ) -> Self {
        Self { cache, pkgfs_install, pkgfs_needs }
    }

    /// Open the requested package by merkle root using the given selectors, serving the package
    /// directory on the given directory request on success.
    pub async fn open(
        &self,
        merkle: BlobId,
        selectors: &[String],
        dir_request: ServerEnd<DirectoryMarker>,
    ) -> Result<(), PackageOpenError> {
        let fut = self.cache.open(
            &mut merkle.into(),
            &mut selectors.iter().map(|s| s.as_str()),
            dir_request,
        );
        match fut.await?.map_err(Status::from_raw) {
            Ok(()) => Ok(()),
            Err(Status::NOT_FOUND) => Err(PackageOpenError::NotFound),
            Err(status) => Err(PackageOpenError::UnexpectedStatus(status)),
        }
    }

    /// Check to see if a package with the given merkle root exists and is readable.
    pub async fn package_exists(&self, merkle: BlobId) -> Result<bool, PackageOpenError> {
        let (_dir, server_end) = fidl::endpoints::create_proxy()?;
        let selectors = vec![];
        match self.open(merkle, &selectors, server_end).await {
            Ok(()) => Ok(true),
            Err(PackageOpenError::NotFound) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Loads the base package index from pkg-cache.
    pub async fn base_package_index(&self) -> Result<BasePackageIndex, anyhow::Error> {
        BasePackageIndex::from_proxy(self.cache.clone()).await
    }

    /// Create a new blob with the given install intent.
    ///
    /// Returns None if the blob already exists and is readable.
    async fn create_blob(
        &self,
        merkle: BlobId,
        blob_kind: BlobKind,
    ) -> Result<
        Option<(pkgfs::install::Blob<pkgfs::install::NeedsTruncate>, pkgfs::install::BlobCloser)>,
        pkgfs::install::BlobCreateError,
    > {
        match self.pkgfs_install.create_blob(merkle.into(), blob_kind).await {
            Ok((file, closer)) => Ok(Some((file, closer))),
            Err(pkgfs::install::BlobCreateError::AlreadyExists) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Returns a stream of chunks of blobs that are needed to resolve the package specified by
    /// `pkg_merkle` provided that the `pkg_merkle` blob has previously been written to
    /// /pkgfs/install/pkg/. The package should be available in /pkgfs/versions when this stream
    /// terminates without error.
    fn list_needs(
        &self,
        pkg_merkle: BlobId,
    ) -> impl Stream<Item = Result<HashSet<BlobId>, pkgfs::needs::ListNeedsError>> + '_ {
        self.pkgfs_needs
            .list_needs(pkg_merkle.into())
            .map(|item| item.map(|needs| needs.into_iter().map(Into::into).collect()))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PackageOpenError {
    #[error("fidl error")]
    Fidl(#[from] fidl::Error),

    #[error("package not found")]
    NotFound,

    #[error("package cache returned unexpected status: {0}")]
    UnexpectedStatus(Status),
}

impl From<&PackageOpenError> for Status {
    fn from(x: &PackageOpenError) -> Self {
        match x {
            PackageOpenError::NotFound => Status::NOT_FOUND,
            _ => Status::INTERNAL,
        }
    }
}

pub async fn cache_package<'a>(
    repo: Arc<AsyncMutex<Repository>>,
    config: &'a RepositoryConfig,
    url: &'a PkgUrl,
    cache: &'a PackageCache,
    blob_fetcher: &'a BlobFetcher,
    cobalt_sender: CobaltSender,
) -> Result<BlobId, CacheError> {
    let (merkle, size) =
        merkle_for_url(repo, url, cobalt_sender).await.map_err(CacheError::MerkleFor)?;
    // If a merkle pin was specified, use it, but only after having verified that the name and
    // variant exist in the TUF repo.  Note that this doesn't guarantee that the merkle pinned
    // package ever actually existed in the repo or that the merkle pin refers to the named
    // package.
    let (merkle, size) = if let Some(merkle_pin) = url.package_hash() {
        (BlobId::from(*merkle_pin), None)
    } else {
        (merkle, Some(size))
    };

    // If the package already exists, we are done.
    if cache.package_exists(merkle).await.unwrap_or_else(|e| {
        fx_log_err!(
            "unable to check if {} is already cached, assuming it isn't: {:#}",
            url,
            anyhow!(e)
        );
        false
    }) {
        return Ok(merkle);
    }

    let mirrors = config.mirrors().to_vec().into();

    // Fetch the meta.far.
    blob_fetcher
        .push(
            merkle,
            FetchBlobContext {
                blob_kind: BlobKind::Package,
                mirrors: Arc::clone(&mirrors),
                expected_len: size,
                use_local_mirror: config.use_local_mirror(),
            },
        )
        .await
        .expect("processor exists")
        .map_err(|e| CacheError::FetchMetaFar(e, merkle))?;

    cache
        .list_needs(merkle)
        .err_into::<CacheError>()
        .try_for_each(|needs| {
            // Fetch the blobs with some amount of concurrency.
            fx_log_info!("Fetching blobs for {}: {:#?}", url, needs);
            blob_fetcher
                .push_all(needs.into_iter().map(|need| {
                    (
                        need,
                        FetchBlobContext {
                            blob_kind: BlobKind::Data,
                            mirrors: Arc::clone(&mirrors),
                            expected_len: None,
                            use_local_mirror: config.use_local_mirror(),
                        },
                    )
                }))
                .collect::<FuturesUnordered<_>>()
                .map(|res| res.expect("processor exists"))
                .try_collect::<()>()
                .map_err(|e| CacheError::FetchContentBlob(e, merkle))
        })
        .await?;

    Ok(merkle)
}

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("fidl error")]
    Fidl(#[from] fidl::Error),

    #[error("while looking up merkle root for package")]
    MerkleFor(#[source] MerkleForError),

    #[error("while listing needed blobs for package")]
    ListNeeds(#[from] pkgfs::needs::ListNeedsError),

    #[error("while fetching the meta.far: {1}")]
    FetchMetaFar(#[source] Arc<FetchError>, BlobId),

    #[error("while fetching content blob for meta.far {1}")]
    FetchContentBlob(#[source] Arc<FetchError>, BlobId),
}

pub(crate) trait ToResolveStatus {
    fn to_resolve_status(&self) -> Status;
}

// From resolver.fidl:
// * `ZX_ERR_ACCESS_DENIED` if the resolver does not have permission to fetch a package blob.
// * `ZX_ERR_IO` if there is some other unspecified error during I/O.
// * `ZX_ERR_NOT_FOUND` if the package or a package blob does not exist.
// * `ZX_ERR_NO_SPACE` if there is no space available to store the package.
// * `ZX_ERR_UNAVAILABLE` if the resolver is currently unable to fetch a package blob.
impl ToResolveStatus for CacheError {
    fn to_resolve_status(&self) -> Status {
        match self {
            CacheError::Fidl(_) => Status::IO,
            CacheError::MerkleFor(err) => err.to_resolve_status(),
            CacheError::ListNeeds(err) => err.to_resolve_status(),
            CacheError::FetchMetaFar(err, ..) => err.to_resolve_status(),
            CacheError::FetchContentBlob(err, _) => err.to_resolve_status(),
        }
    }
}
impl ToResolveStatus for MerkleForError {
    fn to_resolve_status(&self) -> Status {
        match self {
            MerkleForError::NotFound => Status::NOT_FOUND,
            MerkleForError::InvalidTargetPath(_) => Status::INTERNAL,
            // FIXME(42326) when tuf::Error gets an HTTP error variant, this should be mapped to Status::UNAVAILABLE
            MerkleForError::FetchTargetDescription(..) => Status::INTERNAL,
            MerkleForError::NoCustomMetadata => Status::INTERNAL,
            MerkleForError::SerdeError(_) => Status::INTERNAL,
        }
    }
}
impl ToResolveStatus for pkgfs::needs::ListNeedsError {
    fn to_resolve_status(&self) -> Status {
        match self {
            pkgfs::needs::ListNeedsError::OpenDir(_) => Status::IO,
            pkgfs::needs::ListNeedsError::ReadDir(_) => Status::IO,
            pkgfs::needs::ListNeedsError::ParseError(_) => Status::INTERNAL,
        }
    }
}
impl ToResolveStatus for pkgfs::install::BlobTruncateError {
    fn to_resolve_status(&self) -> Status {
        match self {
            pkgfs::install::BlobTruncateError::Fidl(_) => Status::IO,
            pkgfs::install::BlobTruncateError::NoSpace => Status::NO_SPACE,
            pkgfs::install::BlobTruncateError::UnexpectedResponse(_) => Status::IO,
        }
    }
}
impl ToResolveStatus for pkgfs::install::BlobWriteError {
    fn to_resolve_status(&self) -> Status {
        match self {
            pkgfs::install::BlobWriteError::Fidl(_) => Status::IO,
            pkgfs::install::BlobWriteError::Overwrite => Status::IO,
            pkgfs::install::BlobWriteError::Corrupt => Status::IO,
            pkgfs::install::BlobWriteError::NoSpace => Status::NO_SPACE,
            pkgfs::install::BlobWriteError::UnexpectedResponse(_) => Status::IO,
        }
    }
}
impl ToResolveStatus for FetchError {
    fn to_resolve_status(&self) -> Status {
        match self {
            FetchError::CreateBlob(_) => Status::IO,
            FetchError::BadHttpStatus { code: hyper::StatusCode::UNAUTHORIZED, .. } => {
                Status::ACCESS_DENIED
            }
            FetchError::BadHttpStatus { code: hyper::StatusCode::FORBIDDEN, .. } => {
                Status::ACCESS_DENIED
            }
            FetchError::BadHttpStatus { .. } => Status::UNAVAILABLE,
            FetchError::ContentLengthMismatch { .. } => Status::UNAVAILABLE,
            FetchError::UnknownLength { .. } => Status::UNAVAILABLE,
            FetchError::BlobTooSmall { .. } => Status::UNAVAILABLE,
            FetchError::BlobTooLarge { .. } => Status::UNAVAILABLE,
            FetchError::Hyper { .. } => Status::UNAVAILABLE,
            FetchError::Http { .. } => Status::UNAVAILABLE,
            FetchError::Truncate(e) => e.to_resolve_status(),
            FetchError::Write(e) => e.to_resolve_status(),
            FetchError::NoMirrors => Status::INTERNAL,
            FetchError::BlobUrl(_) => Status::INTERNAL,
            FetchError::FidlError(_) => Status::INTERNAL,
            FetchError::IoError(_) => Status::IO,
            FetchError::LocalMirror(_) => Status::INTERNAL,
            FetchError::NoBlobSource { .. } => Status::INTERNAL,
            FetchError::ConflictingBlobSources => Status::INTERNAL,
            FetchError::BlobHeaderTimeout { .. } => Status::UNAVAILABLE,
            FetchError::BlobBodyTimeout { .. } => Status::UNAVAILABLE,
        }
    }
}

impl From<&MerkleForError> for metrics::MerkleForUrlMetricDimensionResult {
    fn from(e: &MerkleForError) -> metrics::MerkleForUrlMetricDimensionResult {
        match e {
            MerkleForError::NotFound => metrics::MerkleForUrlMetricDimensionResult::NotFound,
            MerkleForError::FetchTargetDescription(..) => {
                metrics::MerkleForUrlMetricDimensionResult::TufError
            }
            MerkleForError::InvalidTargetPath(_) => {
                metrics::MerkleForUrlMetricDimensionResult::InvalidTargetPath
            }
            MerkleForError::NoCustomMetadata => {
                metrics::MerkleForUrlMetricDimensionResult::NoCustomMetadata
            }
            MerkleForError::SerdeError(_) => metrics::MerkleForUrlMetricDimensionResult::SerdeError,
        }
    }
}

pub async fn merkle_for_url<'a>(
    repo: Arc<AsyncMutex<Repository>>,
    url: &'a PkgUrl,
    mut cobalt_sender: CobaltSender,
) -> Result<(BlobId, u64), MerkleForError> {
    let target_path = TargetPath::new(format!("{}/{}", url.name(), url.variant().unwrap_or("0")))
        .map_err(MerkleForError::InvalidTargetPath)?;
    let mut repo = repo.lock().await;
    let res = repo.get_merkle_at_path(&target_path).await;
    cobalt_sender.log_event_count(
        metrics::MERKLE_FOR_URL_METRIC_ID,
        match &res {
            Ok(_) => metrics::MerkleForUrlMetricDimensionResult::Success,
            Err(res) => res.into(),
        },
        0,
        1,
    );
    res.map(|custom| (custom.merkle(), custom.size()))
}

#[derive(Debug, thiserror::Error)]
pub enum MerkleForError {
    #[error("the package was not found in the repository")]
    NotFound,

    #[error("unexpected tuf error when fetching target description for {0:?}")]
    FetchTargetDescription(String, #[source] tuf::error::Error),

    #[error("the target path is not safe")]
    InvalidTargetPath(#[source] tuf::error::Error),

    #[error("the target description does not have custom metadata")]
    NoCustomMetadata,

    #[error("serde value could not be converted")]
    SerdeError(#[source] serde_json::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FetchBlobContext {
    blob_kind: BlobKind,
    mirrors: Arc<[MirrorConfig]>,
    expected_len: Option<u64>,
    use_local_mirror: bool,
}

impl queue::TryMerge for FetchBlobContext {
    fn try_merge(&mut self, other: Self) -> Result<(), Self> {
        // Unmergeable if both contain different expected lengths. One of these instances will
        // fail, but we can't know which one here.
        let expected_len = match (self.expected_len, other.expected_len) {
            (Some(x), None) | (None, Some(x)) => Some(x),
            (None, None) => None,
            (Some(x), Some(y)) if x == y => Some(x),
            _ => return Err(other),
        };

        // Installing a blob as a package will fulfill any pending needs of that blob as a data
        // blob as well, so upgrade Data to Package.
        let blob_kind =
            if self.blob_kind == BlobKind::Package || other.blob_kind == BlobKind::Package {
                BlobKind::Package
            } else {
                BlobKind::Data
            };

        // For now, don't attempt to merge mirrors, but do merge these contexts if the mirrors are
        // equivalent.
        if self.mirrors != other.mirrors {
            return Err(other);
        }

        // Contexts are mergeable, apply the merged state.
        self.expected_len = expected_len;
        self.blob_kind = blob_kind;
        Ok(())
    }
}

pub fn make_blob_fetch_queue(
    node: fuchsia_inspect::Node,
    cache: PackageCache,
    max_concurrency: usize,
    stats: Arc<Mutex<Stats>>,
    cobalt_sender: CobaltSender,
    local_mirror_proxy: Option<LocalMirrorProxy>,
    blob_network_timeouts: BlobNetworkTimeouts,
) -> (impl Future<Output = ()>, BlobFetcher) {
    let http_client = Arc::new(fuchsia_hyper::new_https_client());
    let inspect = inspect::BlobFetcher::from_node_and_timeouts(node, &blob_network_timeouts);

    let (blob_fetch_queue, blob_fetcher) =
        queue::work_queue(max_concurrency, move |merkle: BlobId, context: FetchBlobContext| {
            let inspect = inspect.fetch(&merkle);
            let http_client = Arc::clone(&http_client);
            let cache = cache.clone();
            let stats = Arc::clone(&stats);
            let cobalt_sender = cobalt_sender.clone();
            let local_mirror_proxy = local_mirror_proxy.clone();

            async move {
                let res = fetch_blob(
                    inspect,
                    &http_client,
                    cache,
                    stats,
                    cobalt_sender,
                    merkle,
                    context,
                    local_mirror_proxy.as_ref(),
                    blob_network_timeouts,
                )
                .map_err(Arc::new)
                .await;
                res
            }
        });

    (blob_fetch_queue.into_future(), blob_fetcher)
}

async fn fetch_blob(
    inspect: inspect::NeedsRemoteType,
    http_client: &fuchsia_hyper::HttpsClient,
    cache: PackageCache,
    stats: Arc<Mutex<Stats>>,
    cobalt_sender: CobaltSender,
    merkle: BlobId,
    context: FetchBlobContext,
    local_mirror_proxy: Option<&LocalMirrorProxy>,
    blob_network_timeouts: BlobNetworkTimeouts,
) -> Result<(), FetchError> {
    let use_remote_mirror = context.mirrors.len() != 0;
    let use_local_mirror = context.use_local_mirror;

    match (use_remote_mirror, use_local_mirror, local_mirror_proxy) {
        (true, true, _) => Err(FetchError::ConflictingBlobSources),
        (false, true, Some(local_mirror)) => {
            trace::duration_begin!("app", "fetch_blob_local", "merkle" => merkle.to_string().as_str());
            let res = fetch_blob_local(
                inspect.local_mirror(),
                local_mirror,
                merkle,
                context.blob_kind,
                context.expected_len,
                &cache,
            )
            .await;
            trace::duration_end!("app", "fetch_blob_local", "result" => format!("{:?}", res).as_str());
            res
        }
        (true, false, _) => {
            trace::duration_begin!("app", "fetch_blob_http", "merkle" => merkle.to_string().as_str());
            let res = fetch_blob_http(
                inspect.http(),
                http_client,
                &context.mirrors,
                merkle,
                context.blob_kind,
                context.expected_len,
                blob_network_timeouts,
                &cache,
                stats,
                cobalt_sender,
            )
            .await;
            trace::duration_end!("app", "fetch_blob_http", "result" => format!("{:?}", res).as_str());
            res
        }
        (use_remote_mirror, use_local_mirror, local_mirror) => Err(FetchError::NoBlobSource {
            use_remote_mirror,
            use_local_mirror,
            allow_local_mirror: local_mirror.is_some(),
        }),
    }
}

async fn fetch_blob_http(
    inspect: inspect::NeedsMirror,
    client: &fuchsia_hyper::HttpsClient,
    mirrors: &[MirrorConfig],
    merkle: BlobId,
    blob_kind: BlobKind,
    expected_len: Option<u64>,
    blob_network_timeouts: BlobNetworkTimeouts,
    cache: &PackageCache,
    stats: Arc<Mutex<Stats>>,
    cobalt_sender: CobaltSender,
) -> Result<(), FetchError> {
    // TODO try the other mirrors depending on the errors encountered trying this one.
    let blob_mirror_url = if let Some(mirror) = mirrors.get(0) {
        mirror.blob_mirror_url().to_owned()
    } else {
        return Err(FetchError::NoMirrors);
    };
    let mirror_stats = stats.lock().for_mirror(blob_mirror_url.to_string());
    let blob_url = make_blob_url(blob_mirror_url, &merkle).map_err(|e| FetchError::BlobUrl(e))?;
    let inspect = inspect.mirror(&blob_url.to_string());
    let flaked = Arc::new(AtomicBool::new(false));

    fuchsia_backoff::retry_or_first_error(retry::blob_fetch(), || {
        let flaked = Arc::clone(&flaked);
        let mirror_stats = &mirror_stats;
        let mut cobalt_sender = cobalt_sender.clone();

        async {
            let inspect = inspect.attempt();
            inspect.state(inspect::Http::CreateBlob);
            if let Some((blob, blob_closer)) =
                cache.create_blob(merkle, blob_kind).await.map_err(FetchError::CreateBlob)?
            {
                inspect.state(inspect::Http::DownloadBlob);
                let res = download_blob(
                    &inspect,
                    client,
                    &blob_url,
                    expected_len,
                    blob,
                    blob_network_timeouts,
                )
                .await;
                inspect.state(inspect::Http::CloseBlob);
                blob_closer.close().await;
                res?;
            }

            Ok(())
        }
        .inspect(move |res| match res.as_ref().map_err(FetchError::kind) {
            Err(FetchErrorKind::NetworkRateLimit) => {
                mirror_stats.network_rate_limits().increment();
            }
            Err(FetchErrorKind::Network) => {
                flaked.store(true, Ordering::SeqCst);
            }
            Err(FetchErrorKind::Other) => {}
            Ok(()) => {
                if flaked.load(Ordering::SeqCst) {
                    mirror_stats.network_blips().increment();
                }
            }
        })
        .inspect(move |res| {
            let event_code = match res {
                Ok(()) => metrics::FetchBlobMetricDimensionResult::Success,
                Err(e) => e.into(),
            }
            .as_event_code();
            cobalt_sender.log_event_count(metrics::FETCH_BLOB_METRIC_ID, event_code, 0, 1);
        })
    })
    .await
}

async fn fetch_blob_local(
    inspect: inspect::TriggerAttempt<inspect::LocalMirror>,
    local_mirror: &LocalMirrorProxy,
    merkle: BlobId,
    blob_kind: BlobKind,
    expected_len: Option<u64>,
    cache: &PackageCache,
) -> Result<(), FetchError> {
    let inspect = inspect.attempt();
    inspect.state(inspect::LocalMirror::CreateBlob);
    if let Some((blob, blob_closer)) =
        cache.create_blob(merkle, blob_kind).await.map_err(FetchError::CreateBlob)?
    {
        let res = read_local_blob(&inspect, local_mirror, merkle, expected_len, blob).await;
        inspect.state(inspect::LocalMirror::CloseBlob);
        blob_closer.close().await;
        res?;
    }
    Ok(())
}

async fn read_local_blob(
    inspect: &inspect::Attempt<inspect::LocalMirror>,
    proxy: &LocalMirrorProxy,
    merkle: BlobId,
    expected_len: Option<u64>,
    dest: pkgfs::install::Blob<pkgfs::install::NeedsTruncate>,
) -> Result<(), FetchError> {
    let (local_file, remote) = fidl::endpoints::create_proxy::<fidl_fuchsia_io::FileMarker>()
        .map_err(FetchError::FidlError)?;

    inspect.state(inspect::LocalMirror::GetBlob);
    proxy
        .get_blob(&mut merkle.into(), remote)
        .await
        .map_err(FetchError::FidlError)?
        .map_err(FetchError::LocalMirror)?;

    let (status, info) = local_file.get_attr().await.map_err(FetchError::FidlError)?;
    Status::ok(status).map_err(FetchError::IoError)?;

    if let Some(ref val) = expected_len {
        if val > &info.content_size {
            return Err(FetchError::BlobTooSmall { uri: merkle.to_string() });
        } else if val < &info.content_size {
            return Err(FetchError::BlobTooLarge { uri: merkle.to_string() });
        }
    }

    inspect.state(inspect::LocalMirror::TruncateBlob);
    let mut dest = dest.truncate(info.content_size).await.map_err(FetchError::Truncate)?;

    loop {
        inspect.state(inspect::LocalMirror::ReadBlob);
        let (status, data) =
            local_file.read(fidl_fuchsia_io::MAX_BUF).await.map_err(FetchError::FidlError)?;
        Status::ok(status).map_err(FetchError::IoError)?;
        if data.len() == 0 {
            return Err(FetchError::BlobTooSmall { uri: merkle.to_string() });
        }
        inspect.state(inspect::LocalMirror::WriteBlob);
        dest = match dest.write(&data).await.map_err(FetchError::Write)? {
            pkgfs::install::BlobWriteSuccess::MoreToWrite(blob) => blob,
            pkgfs::install::BlobWriteSuccess::Done => break,
        };
        inspect.write_bytes(data.len());
    }
    Ok(())
}

fn make_blob_url(
    blob_mirror_url: http::Uri,
    merkle: &BlobId,
) -> Result<hyper::Uri, http_uri_ext::Error> {
    blob_mirror_url.extend_dir_with_path(&merkle.to_string())
}

async fn download_blob(
    inspect: &inspect::Attempt<inspect::Http>,
    client: &fuchsia_hyper::HttpsClient,
    uri: &http::Uri,
    expected_len: Option<u64>,
    dest: pkgfs::install::Blob<pkgfs::install::NeedsTruncate>,
    blob_network_timeouts: BlobNetworkTimeouts,
) -> Result<(), FetchError> {
    inspect.state(inspect::Http::HttpGet);
    let request = Request::get(uri)
        .body(Body::empty())
        .map_err(|e| FetchError::Http { e, uri: uri.to_string() })?;
    let response = client
        .request(request)
        .map_err(|e| FetchError::Hyper { e, uri: uri.to_string() })
        .on_timeout(blob_network_timeouts.header(), || {
            Err(FetchError::BlobHeaderTimeout { uri: uri.to_string() })
        })
        .await?;

    if response.status() != StatusCode::OK {
        return Err(FetchError::BadHttpStatus { code: response.status(), uri: uri.to_string() });
    }

    let expected_len = match (expected_len, response.size_hint().exact()) {
        (Some(expected), Some(actual)) => {
            if expected != actual {
                return Err(FetchError::ContentLengthMismatch {
                    expected,
                    actual,
                    uri: uri.to_string(),
                });
            } else {
                expected
            }
        }
        (Some(length), None) | (None, Some(length)) => length,
        (None, None) => return Err(FetchError::UnknownLength { uri: uri.to_string() }),
    };
    inspect.expected_size_bytes(expected_len);

    inspect.state(inspect::Http::TruncateBlob);
    let mut dest = dest.truncate(expected_len).await.map_err(FetchError::Truncate)?;

    inspect.state(inspect::Http::ReadHttpBody);
    let mut chunks = response.into_body();
    let mut written = 0u64;
    while let Some(chunk) = chunks
        .try_next()
        .map_err(|e| FetchError::Hyper { e, uri: uri.to_string() })
        .on_timeout(blob_network_timeouts.body(), || {
            Err(FetchError::BlobBodyTimeout { uri: uri.to_string() })
        })
        .await?
    {
        if written + chunk.len() as u64 > expected_len {
            return Err(FetchError::BlobTooLarge { uri: uri.to_string() });
        }

        inspect.state(inspect::Http::WriteBlob);
        dest = match dest.write(&chunk).await.map_err(FetchError::Write)? {
            pkgfs::install::BlobWriteSuccess::MoreToWrite(blob) => {
                written += chunk.len() as u64;
                blob
            }
            pkgfs::install::BlobWriteSuccess::Done => {
                written += chunk.len() as u64;
                break;
            }
        };
        inspect.state(inspect::Http::ReadHttpBody);
        inspect.write_bytes(chunk.len());
    }
    inspect.state(inspect::Http::WriteComplete);

    if expected_len != written {
        return Err(FetchError::BlobTooSmall { uri: uri.to_string() });
    }

    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("could not create blob")]
    CreateBlob(#[source] pkgfs::install::BlobCreateError),

    #[error("Blob fetch of {uri}: http request expected 200, got {code}")]
    BadHttpStatus { code: hyper::StatusCode, uri: String },

    #[error("repository has no configured mirrors")]
    NoMirrors,

    #[error("Blob fetch of {uri}: expected blob length of {expected}, got {actual}")]
    ContentLengthMismatch { expected: u64, actual: u64, uri: String },

    #[error("Blob fetch of {uri}: blob length not known or provided by server")]
    UnknownLength { uri: String },

    #[error("Blob fetch of {uri}: downloaded blob was too small")]
    BlobTooSmall { uri: String },

    #[error("Blob fetch of {uri}: downloaded blob was too large")]
    BlobTooLarge { uri: String },

    #[error("failed to truncate blob")]
    Truncate(#[source] pkgfs::install::BlobTruncateError),

    #[error("failed to write blob data")]
    Write(#[source] pkgfs::install::BlobWriteError),

    #[error("hyper error while fetching {uri}")]
    Hyper {
        #[source]
        e: hyper::Error,
        uri: String,
    },

    #[error("http error while fetching {uri}")]
    Http {
        #[source]
        e: hyper::http::Error,
        uri: String,
    },

    #[error("blob url error")]
    BlobUrl(#[source] http_uri_ext::Error),

    #[error("FIDL error while fetching blob")]
    FidlError(#[source] fidl::Error),

    #[error("IO error while reading blob")]
    IoError(#[source] Status),

    #[error("LocalMirror error while fetching {0:?}")]
    LocalMirror(
        // The FIDL error type doesn't derive Error, so we can't use #[source].
        fidl_fuchsia_pkg::GetBlobError,
    ),

    #[error(
        "No valid source could be found for the requested blob. \
        use_remote_mirror={use_remote_mirror}, use_local_mirror={use_local_mirror}, \
        allow_local_mirror={allow_local_mirror}"
    )]
    NoBlobSource { use_remote_mirror: bool, use_local_mirror: bool, allow_local_mirror: bool },

    #[error("Tried to request a blob with HTTP and local mirrors")]
    ConflictingBlobSources,

    #[error("timed out waiting for http response header while downloading blob: {uri}")]
    BlobHeaderTimeout { uri: String },

    #[error(
        "timed out waiting for bytes from the http response body while downloading blob: {uri}"
    )]
    BlobBodyTimeout { uri: String },
}

impl From<&FetchError> for metrics::FetchBlobMetricDimensionResult {
    fn from(error: &FetchError) -> Self {
        use metrics::FetchBlobMetricDimensionResult as EventCodes;
        match error {
            FetchError::CreateBlob { .. } => EventCodes::CreateBlob,
            FetchError::BadHttpStatus { code, .. } => match *code {
                StatusCode::BAD_REQUEST => EventCodes::HttpBadRequest,
                StatusCode::UNAUTHORIZED => EventCodes::HttpUnauthorized,
                StatusCode::FORBIDDEN => EventCodes::HttpForbidden,
                StatusCode::NOT_FOUND => EventCodes::HttpNotFound,
                StatusCode::METHOD_NOT_ALLOWED => EventCodes::HttpMethodNotAllowed,
                StatusCode::REQUEST_TIMEOUT => EventCodes::HttpRequestTimeout,
                StatusCode::PRECONDITION_FAILED => EventCodes::HttpPreconditionFailed,
                StatusCode::RANGE_NOT_SATISFIABLE => EventCodes::HttpRangeNotSatisfiable,
                StatusCode::TOO_MANY_REQUESTS => EventCodes::HttpTooManyRequests,
                StatusCode::INTERNAL_SERVER_ERROR => EventCodes::HttpInternalServerError,
                StatusCode::BAD_GATEWAY => EventCodes::HttpBadGateway,
                StatusCode::SERVICE_UNAVAILABLE => EventCodes::HttpServiceUnavailable,
                StatusCode::GATEWAY_TIMEOUT => EventCodes::HttpGatewayTimeout,
                _ => match code.as_u16() {
                    100..=199 => EventCodes::Http1xx,
                    200..=299 => EventCodes::Http2xx,
                    300..=399 => EventCodes::Http3xx,
                    400..=499 => EventCodes::Http4xx,
                    500..=599 => EventCodes::Http5xx,
                    _ => EventCodes::BadHttpStatus,
                },
            },
            FetchError::NoMirrors => EventCodes::NoMirrors,
            FetchError::ContentLengthMismatch { .. } => EventCodes::ContentLengthMismatch,
            FetchError::UnknownLength { .. } => EventCodes::UnknownLength,
            FetchError::BlobTooSmall { .. } => EventCodes::BlobTooSmall,
            FetchError::BlobTooLarge { .. } => EventCodes::BlobTooLarge,
            FetchError::Truncate { .. } => EventCodes::Truncate,
            FetchError::Write { .. } => EventCodes::Write,
            FetchError::Hyper { .. } => EventCodes::Hyper,
            FetchError::Http { .. } => EventCodes::Http,
            FetchError::BlobUrl { .. } => EventCodes::BlobUrl,
            FetchError::FidlError { .. } => EventCodes::FidlError,
            FetchError::IoError { .. } => EventCodes::IoError,
            FetchError::LocalMirror { .. } => EventCodes::LocalMirror,
            FetchError::NoBlobSource { .. } => EventCodes::NoBlobSource,
            FetchError::ConflictingBlobSources => EventCodes::ConflictingBlobSources,
            FetchError::BlobHeaderTimeout { .. } => EventCodes::BlobHeaderDeadlineExceeded,
            FetchError::BlobBodyTimeout { .. } => EventCodes::BlobBodyDeadlineExceeded,
        }
    }
}

impl FetchError {
    fn kind(&self) -> FetchErrorKind {
        match self {
            FetchError::BadHttpStatus { code: StatusCode::TOO_MANY_REQUESTS, uri: _ } => {
                FetchErrorKind::NetworkRateLimit
            }
            FetchError::Hyper { .. }
            | FetchError::Http { .. }
            | FetchError::BadHttpStatus { .. } => FetchErrorKind::Network,
            _ => FetchErrorKind::Other,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum FetchErrorKind {
    NetworkRateLimit,
    Network,
    Other,
}

#[cfg(test)]
mod tests {
    use {super::*, http::Uri};

    #[test]
    fn test_make_blob_url() {
        let merkle = "00112233445566778899aabbccddeeffffeeddccbbaa99887766554433221100"
            .parse::<BlobId>()
            .unwrap();

        assert_eq!(
            make_blob_url("http://example.com".parse::<Uri>().unwrap(), &merkle).unwrap(),
            format!("http://example.com/{}", merkle).parse::<Uri>().unwrap()
        );

        assert_eq!(
            make_blob_url("http://example.com/noslash".parse::<Uri>().unwrap(), &merkle).unwrap(),
            format!("http://example.com/noslash/{}", merkle).parse::<Uri>().unwrap()
        );

        assert_eq!(
            make_blob_url("http://example.com/slash/".parse::<Uri>().unwrap(), &merkle).unwrap(),
            format!("http://example.com/slash/{}", merkle).parse::<Uri>().unwrap()
        );

        assert_eq!(
            make_blob_url("http://example.com/twoslashes//".parse::<Uri>().unwrap(), &merkle)
                .unwrap(),
            format!("http://example.com/twoslashes//{}", merkle).parse::<Uri>().unwrap()
        );

        // IPv6 zone id
        assert_eq!(
            make_blob_url(
                "http://[fe80::e022:d4ff:fe13:8ec3%252]:8083/blobs/".parse::<Uri>().unwrap(),
                &merkle
            )
            .unwrap(),
            format!("http://[fe80::e022:d4ff:fe13:8ec3%252]:8083/blobs/{}", merkle)
                .parse::<Uri>()
                .unwrap()
        );
    }
}
