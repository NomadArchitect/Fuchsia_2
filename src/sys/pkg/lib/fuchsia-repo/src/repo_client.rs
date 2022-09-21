// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{
        range::Range,
        repository::{Error, RepoProvider},
        resource::Resource,
    },
    anyhow::{anyhow, Context as _, Result},
    fidl_fuchsia_developer_ffx::{ListFields, PackageEntry, RepositoryPackage},
    fidl_fuchsia_developer_ffx_ext::RepositorySpec,
    fidl_fuchsia_pkg_ext::{
        MirrorConfigBuilder, RepositoryConfig, RepositoryConfigBuilder, RepositoryKey,
        RepositoryStorageType,
    },
    fuchsia_fs::file::Adapter,
    fuchsia_pkg::MetaContents,
    fuchsia_url::RepositoryUrl,
    futures::{
        future::{BoxFuture, Shared},
        io::Cursor,
        stream::{self, BoxStream},
        AsyncRead, AsyncReadExt as _, FutureExt as _, StreamExt as _, TryStreamExt as _,
    },
    std::{
        fmt::{self, Debug},
        sync::atomic::{AtomicUsize, Ordering},
        time::SystemTime,
    },
    tuf::{
        client::{Client as TufClient, Config},
        crypto::KeyType,
        interchange::Json,
        metadata::{
            Metadata as _, MetadataPath, MetadataVersion, RawSignedMetadata, TargetDescription,
            TargetPath, TargetsMetadata,
        },
        repository::{
            EphemeralRepository, RepositoryProvider, RepositoryProvider as TufRepositoryProvider,
        },
        verify::Verified,
    },
};

const LIST_PACKAGE_CONCURRENCY: usize = 5;

/// A unique ID which is given to every repository.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct RepositoryId(usize);

impl RepositoryId {
    fn new() -> Self {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
        RepositoryId(NEXT_ID.fetch_add(1, Ordering::Relaxed))
    }
}

pub struct RepoClient {
    /// The url of the repository.
    repo_url: RepositoryUrl,

    /// A unique ID for the repository, scoped to this instance of the daemon.
    id: RepositoryId,

    /// _tx_on_drop is a channel that will emit a `Cancelled` message to `rx_on_drop` when this
    /// repository is dropped. This is a convenient way to notify any downstream users to clean up
    /// any side tables that associate a repository to some other data.
    _tx_on_drop: futures::channel::oneshot::Sender<()>,

    /// Channel Receiver that receives a `Cancelled` signal when this repository is dropped.
    rx_on_drop: futures::future::Shared<futures::channel::oneshot::Receiver<()>>,

    /// The TUF client for this repository
    tuf_client: TufClient<Json, EphemeralRepository<Json>, Box<dyn RepoProvider>>,
}

impl RepoClient {
    pub async fn new(name: &str, backend: Box<dyn RepoProvider>) -> Result<Self, Error> {
        // Validate that the name can be used as a repo url.
        let repo_url =
            RepositoryUrl::parse_host(name.to_string()).context("creating repository")?;

        let tuf_client = get_tuf_client(backend).await?;

        let (tx_on_drop, rx_on_drop) = futures::channel::oneshot::channel();
        let rx_on_drop = rx_on_drop.shared();

        Ok(Self {
            repo_url,
            id: RepositoryId::new(),
            tuf_client,
            _tx_on_drop: tx_on_drop,
            rx_on_drop,
        })
    }

    /// Returns a receiver that will receive a `Canceled` signal when the repository is dropped.
    pub fn on_dropped_signal(&self) -> Shared<futures::channel::oneshot::Receiver<()>> {
        self.rx_on_drop.clone()
    }

    pub fn id(&self) -> RepositoryId {
        self.id
    }

    pub fn repo_url(&self) -> &RepositoryUrl {
        &self.repo_url
    }

    pub fn name(&self) -> &str {
        self.repo_url.host()
    }

    /// Get a [RepositorySpec] for this [Repository].
    pub fn spec(&self) -> RepositorySpec {
        self.tuf_client.remote_repo().spec()
    }

    /// Returns if the repository supports watching for timestamp changes.
    pub fn supports_watch(&self) -> bool {
        return self.tuf_client.remote_repo().supports_watch();
    }

    /// Return a stream that yields whenever the repository's timestamp changes.
    pub fn watch(&self) -> anyhow::Result<BoxStream<'static, ()>> {
        self.tuf_client.remote_repo().watch()
    }

    /// Update client to the latest available metadata.
    pub async fn update(&mut self) -> Result<bool, Error> {
        Ok(self.tuf_client.update().await?)
    }

    /// Return a stream of bytes for the metadata resource.
    pub async fn fetch_metadata(&self, path: &str) -> Result<Resource, Error> {
        self.fetch_metadata_range(path, Range::Full).await
    }

    /// Return a stream of bytes for the metadata resource in given range.
    pub async fn fetch_metadata_range(&self, path: &str, range: Range) -> Result<Resource, Error> {
        self.tuf_client.remote_repo().fetch_metadata_range(path, range).await
    }

    /// Return a stream of bytes for the blob resource.
    pub async fn fetch_blob(&self, path: &str) -> Result<Resource, Error> {
        self.fetch_blob_range(path, Range::Full).await
    }

    /// Return a stream of bytes for the blob resource in given range.
    pub async fn fetch_blob_range(&self, path: &str, range: Range) -> Result<Resource, Error> {
        self.tuf_client.remote_repo().fetch_blob_range(path, range).await
    }

    /// Return the target description for a TUF target path.
    pub async fn get_target_description(
        &self,
        path: &str,
    ) -> Result<Option<TargetDescription>, Error> {
        match self.tuf_client.database().trusted_targets() {
            Some(trusted_targets) => Ok(trusted_targets
                .targets()
                .get(&TargetPath::new(path).map_err(|e| anyhow::anyhow!(e))?)
                .map(|t| t.clone())),
            None => Ok(None),
        }
    }

    pub async fn get_config(
        &self,
        mirror_url: &str,
        repo_storage_type: Option<RepositoryStorageType>,
    ) -> Result<RepositoryConfig, Error> {
        let mirror_url: http::Uri = format!("http://{}", mirror_url).parse()?;

        let trusted_root = self.tuf_client.database().trusted_root();

        let mut repo_config_builder = RepositoryConfigBuilder::new(self.repo_url().clone())
            .root_version(trusted_root.version())
            .root_threshold(trusted_root.root().threshold())
            .add_mirror(
                MirrorConfigBuilder::new(mirror_url)?
                    .subscribe(self.tuf_client.remote_repo().supports_watch())
                    .build(),
            );

        if let Some(repo_storage_type) = repo_storage_type {
            repo_config_builder = repo_config_builder.repo_storage_type(repo_storage_type);
        }

        for root_key in trusted_root.root_keys().filter(|k| *k.typ() == KeyType::Ed25519) {
            repo_config_builder = repo_config_builder
                .add_root_key(RepositoryKey::Ed25519(root_key.as_bytes().to_vec()));
        }

        let repo_config = repo_config_builder.build();

        Ok(repo_config)
    }

    async fn get_components_for_package(
        &self,
        trusted_targets: &Verified<TargetsMetadata>,
        package: &RepositoryPackage,
    ) -> Result<Option<Vec<PackageEntry>>> {
        let package_entries = self
            .show_target_package(trusted_targets, package.name.as_ref().unwrap().to_string())
            .await?;

        if package_entries.is_none() {
            return Ok(None);
        }

        let components = package_entries
            .unwrap()
            .into_iter()
            .filter(|e| is_component_manifest(&e.path.as_ref().unwrap()))
            .collect();

        Ok(Some(components))
    }

    pub async fn list_packages(
        &self,
        include_fields: ListFields,
    ) -> Result<Vec<RepositoryPackage>, Error> {
        let trusted_targets =
            self.tuf_client.database().trusted_targets().context("missing target information")?;

        let mut packages = vec![];
        for (package_name, package_description) in trusted_targets.targets() {
            let meta_far_size = package_description.length();
            let meta_far_hash =
                if let Some(meta_far_hash) = package_description.custom().get("merkle") {
                    meta_far_hash.as_str().unwrap_or("").to_string()
                } else {
                    continue;
                };

            let mut bytes = vec![];
            self.fetch_blob(&meta_far_hash)
                .await
                .with_context(|| format!("fetching blob {}", meta_far_hash))?
                .read_to_end(&mut bytes)
                .await
                .with_context(|| format!("reading blob {}", meta_far_hash))?;

            let mut archive =
                fuchsia_archive::AsyncUtf8Reader::new(Adapter::new(Cursor::new(bytes))).await?;
            let contents = archive.read_file("meta/contents").await?;
            let contents = MetaContents::deserialize(contents.as_slice())?;

            // Concurrently fetch the package blob sizes.
            // FIXME(http://fxbug.dev/97192): Use work queue so we can globally control the
            // concurrency here, rather than limiting fetches per call.
            let mut tasks =
                stream::iter(contents.contents().into_iter().map(|(_, hash)| async move {
                    self.tuf_client.remote_repo().blob_len(&hash.to_string()).await
                }))
                .buffer_unordered(LIST_PACKAGE_CONCURRENCY);

            let mut size = meta_far_size;
            while let Some(blob_len) = tasks.try_next().await? {
                size += blob_len;
            }

            // Determine when the meta.far was created.
            let meta_far_modified = self
                .tuf_client
                .remote_repo()
                .blob_modification_time(&meta_far_hash)
                .await
                .with_context(|| format!("fetching blob modification time {}", meta_far_hash))?
                .map(|x| -> anyhow::Result<u64> {
                    Ok(x.duration_since(SystemTime::UNIX_EPOCH)?.as_secs())
                })
                .transpose()?;

            packages.push(RepositoryPackage {
                name: Some(package_name.to_string()),
                size: Some(size),
                hash: Some(meta_far_hash),
                modified: meta_far_modified,
                ..RepositoryPackage::EMPTY
            });
        }

        if include_fields.intersects(ListFields::COMPONENTS) {
            for package in packages.iter_mut() {
                match self.get_components_for_package(&trusted_targets, &package).await {
                    Ok(components) => package.entries = components,
                    Err(e) => {
                        tracing::error!(
                            "failed to get components for package '{}': {}",
                            package.name.as_ref().unwrap_or(&String::from("<unknown>")),
                            e
                        )
                    }
                };
            }
        }

        Ok(packages)
    }

    pub async fn show_package(&self, package_name: String) -> Result<Option<Vec<PackageEntry>>> {
        let trusted_targets =
            self.tuf_client.database().trusted_targets().context("expected targets information")?;

        self.show_target_package(trusted_targets, package_name).await
    }

    async fn show_target_package(
        &self,
        trusted_targets: &Verified<TargetsMetadata>,
        package_name: String,
    ) -> Result<Option<Vec<PackageEntry>>> {
        let target_path = TargetPath::new(&package_name)?;
        let target = if let Some(target) = trusted_targets.targets().get(&target_path) {
            target
        } else {
            return Ok(None);
        };

        let size = target.length();
        let custom = target.custom();

        let hash = custom
            .get("merkle")
            .ok_or_else(|| anyhow!("package {:?} is missing the `merkle` field", package_name))?;

        let hash = hash
            .as_str()
            .ok_or_else(|| {
                anyhow!("package {:?} hash should be a string, not {:?}", package_name, hash)
            })?
            .to_string();

        // Read the meta.far.
        let mut meta_far_bytes = vec![];
        self.fetch_blob(&hash)
            .await
            .with_context(|| format!("fetching blob {}", hash))?
            .read_to_end(&mut meta_far_bytes)
            .await
            .with_context(|| format!("reading blob {}", hash))?;

        let mut archive =
            fuchsia_archive::AsyncUtf8Reader::new(Adapter::new(Cursor::new(meta_far_bytes)))
                .await?;

        let modified = self
            .tuf_client
            .remote_repo()
            .blob_modification_time(&hash)
            .await?
            .map(|x| -> anyhow::Result<u64> {
                Ok(x.duration_since(SystemTime::UNIX_EPOCH)?.as_secs())
            })
            .transpose()?;

        // Add entry for meta.far
        let mut entries = vec![PackageEntry {
            path: Some("meta.far".to_string()),
            hash: Some(hash),
            size: Some(size),
            modified,
            ..PackageEntry::EMPTY
        }];

        entries.extend(archive.list().map(|item| PackageEntry {
            path: Some(item.path().to_string()),
            size: Some(item.length()),
            modified,
            ..PackageEntry::EMPTY
        }));

        match archive.read_file("meta/contents").await {
            Ok(c) => {
                let contents = MetaContents::deserialize(c.as_slice())?;
                for (name, hash) in contents.contents() {
                    let hash_string = hash.to_string();
                    let size = self.tuf_client.remote_repo().blob_len(&hash_string).await?;
                    let modified = self
                        .tuf_client
                        .remote_repo()
                        .blob_modification_time(&hash_string)
                        .await?
                        .map(|x| -> anyhow::Result<u64> {
                            Ok(x.duration_since(SystemTime::UNIX_EPOCH)?.as_secs())
                        })
                        .transpose()?;

                    entries.push(PackageEntry {
                        path: Some(name.to_owned()),
                        hash: Some(hash_string),
                        size: Some(size),
                        modified,
                        ..PackageEntry::EMPTY
                    });
                }
            }
            Err(e) => {
                tracing::warn!("failed to read meta/contents for package {}: {}", package_name, e);
            }
        }

        Ok(Some(entries))
    }
}

impl Debug for RepoClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Repository")
            .field("repo_url", &self.repo_url)
            .field("id", &self.id)
            .field("tuf_client", &self.tuf_client)
            .finish_non_exhaustive()
    }
}

impl TufRepositoryProvider<Json> for RepoClient {
    fn fetch_metadata<'a>(
        &'a self,
        meta_path: &MetadataPath,
        version: MetadataVersion,
    ) -> BoxFuture<'a, tuf::Result<Box<dyn AsyncRead + Send + Unpin + 'a>>> {
        self.tuf_client.remote_repo().fetch_metadata(meta_path, version)
    }

    fn fetch_target<'a>(
        &'a self,
        target_path: &TargetPath,
    ) -> BoxFuture<'a, tuf::Result<Box<dyn AsyncRead + Send + Unpin + 'a>>> {
        self.tuf_client.remote_repo().fetch_target(target_path)
    }
}

fn is_component_manifest(s: &str) -> bool {
    return s.ends_with(".cm") || s.ends_with(".cmx");
}

pub(crate) async fn get_tuf_client<R>(
    tuf_repo: R,
) -> Result<TufClient<Json, EphemeralRepository<Json>, R>, anyhow::Error>
where
    R: RepositoryProvider<Json> + Sync,
{
    let metadata_repo = EphemeralRepository::<Json>::new();

    let raw_signed_meta = {
        // FIXME(http://fxbug.dev/92126) we really should be initializing trust, rather than just
        // trusting 1.root.json.
        let root = tuf_repo.fetch_metadata(&MetadataPath::root(), MetadataVersion::Number(1)).await;

        // If we couldn't find 1.root.json, see if root.json exists and try to initialize trust with it.
        let mut root = match root {
            Err(tuf::Error::MetadataNotFound { .. }) => {
                tuf_repo.fetch_metadata(&MetadataPath::root(), MetadataVersion::None).await?
            }
            Err(err) => return Err(err.into()),
            Ok(root) => root,
        };

        let mut buf = Vec::new();
        root.read_to_end(&mut buf).await.context("reading metadata")?;

        RawSignedMetadata::<Json, _>::new(buf)
    };

    let mut client =
        TufClient::with_trusted_root(Config::default(), &raw_signed_meta, metadata_repo, tuf_repo)
            .await?;

    // Update tuf to the latest metadata version.
    client.update().await?;

    Ok(client)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            repository::{FileSystemRepository, PmRepository},
            test_utils::{
                make_pm_repository, make_readonly_empty_repository, make_repository, repo_key,
                repo_private_key, PKG1_BIN_HASH, PKG1_HASH, PKG1_LIB_HASH, PKG2_HASH,
            },
        },
        assert_matches::assert_matches,
        camino::{Utf8Path, Utf8PathBuf},
        fidl_fuchsia_pkg_ext::RepositoryConfigBuilder,
        pretty_assertions::assert_eq,
        std::fs::create_dir_all,
        tuf::{
            database::Database, repo_builder::RepoBuilder, repository::FileSystemRepositoryBuilder,
        },
    };

    const REPO_NAME: &str = "fake-repo";

    fn get_modtime(path: Utf8PathBuf) -> u64 {
        std::fs::metadata(path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_constructor_accepts_valid_names() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();
        let metadata_dir = dir.join("metadata");
        let blobs_dir = dir.join("blobs");
        make_repository(metadata_dir.as_std_path(), blobs_dir.as_std_path()).await;

        for name in ["fuchsia.com", "my-repository", "hello.there.world"] {
            let backend =
                FileSystemRepository::new(metadata_dir.clone(), blobs_dir.clone()).unwrap();

            assert_matches!(RepoClient::new(name, Box::new(backend)).await, Ok(_))
        }
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_constructor_rejects_invalid_names() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();
        let metadata_dir = dir.join("metadata");
        let blobs_dir = dir.join("blobs");
        make_repository(metadata_dir.as_std_path(), blobs_dir.as_std_path()).await;

        for name in ["", "my_repo", "MyRepo", "😀"] {
            let backend =
                FileSystemRepository::new(metadata_dir.clone(), blobs_dir.clone()).unwrap();

            assert_matches!(RepoClient::new(name, Box::new(backend)).await, Err(_))
        }
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_get_config() {
        let repo = make_readonly_empty_repository(REPO_NAME).await.unwrap();

        let repo_url: RepositoryUrl = format!("fuchsia-pkg://{}", REPO_NAME).parse().unwrap();
        let server_url = "some-url:1234";
        let mirror_url: http::Uri = format!("http://{}", server_url).parse().unwrap();

        assert_eq!(
            repo.get_config(server_url, None).await.unwrap(),
            RepositoryConfigBuilder::new(repo_url.clone())
                .add_root_key(repo_key())
                .add_mirror(
                    MirrorConfigBuilder::new(mirror_url.clone()).unwrap().subscribe(true).build()
                )
                .build()
        );

        assert_eq!(
            repo.get_config(server_url, Some(RepositoryStorageType::Persistent)).await.unwrap(),
            RepositoryConfigBuilder::new(repo_url)
                .add_root_key(repo_key())
                .add_mirror(MirrorConfigBuilder::new(mirror_url).unwrap().subscribe(true).build())
                .repo_storage_type(RepositoryStorageType::Persistent)
                .build()
        );
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_list_packages() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();

        let repo =
            RepoClient::new(REPO_NAME, Box::new(make_pm_repository(&dir).await)).await.unwrap();

        // Look up the timestamp for the meta.far for the modified setting.
        let pkg1_modified = get_modtime(dir.join("repository").join("blobs").join(PKG1_HASH));
        let pkg2_modified = get_modtime(dir.join("repository").join("blobs").join(PKG2_HASH));

        let mut packages = repo.list_packages(ListFields::empty()).await.unwrap();

        // list_packages returns the contents out of order. Sort the entries so they are consistent.
        packages.sort_unstable_by(|lhs, rhs| lhs.name.cmp(&rhs.name));

        assert_eq!(
            packages,
            vec![
                RepositoryPackage {
                    name: Some("package1".into()),
                    hash: Some(PKG1_HASH.into()),
                    size: Some(24603),
                    modified: Some(pkg1_modified),
                    entries: None,
                    ..RepositoryPackage::EMPTY
                },
                RepositoryPackage {
                    name: Some("package2".into()),
                    hash: Some(PKG2_HASH.into()),
                    size: Some(24603),
                    modified: Some(pkg2_modified),
                    entries: None,
                    ..RepositoryPackage::EMPTY
                },
            ],
        );
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_list_packages_with_components() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();

        let backend = Box::new(make_pm_repository(&dir).await);
        let repo = RepoClient::new(REPO_NAME, backend).await.unwrap();

        // Look up the timestamp for the meta.far for the modified setting.
        let pkg1_modified = get_modtime(dir.join("repository").join("blobs").join(PKG1_HASH));
        let pkg2_modified = get_modtime(dir.join("repository").join("blobs").join(PKG2_HASH));

        let mut packages = repo.list_packages(ListFields::COMPONENTS).await.unwrap();

        // list_packages returns the contents out of order. Sort the entries so they are consistent.
        packages.sort_unstable_by(|lhs, rhs| lhs.name.cmp(&rhs.name));

        assert_eq!(
            packages,
            vec![
                RepositoryPackage {
                    name: Some("package1".into()),
                    hash: Some(PKG1_HASH.into()),
                    size: Some(24603),
                    modified: Some(pkg1_modified),
                    entries: Some(vec![
                        PackageEntry {
                            path: Some("meta/package1.cm".into()),
                            hash: None,
                            size: Some(11),
                            modified: Some(pkg1_modified),
                            ..PackageEntry::EMPTY
                        },
                        PackageEntry {
                            path: Some("meta/package1.cmx".into()),
                            hash: None,
                            size: Some(12),
                            modified: Some(pkg1_modified),
                            ..PackageEntry::EMPTY
                        },
                    ]),
                    ..RepositoryPackage::EMPTY
                },
                RepositoryPackage {
                    name: Some("package2".into()),
                    hash: Some(PKG2_HASH.into()),
                    size: Some(24603),
                    modified: Some(pkg2_modified),
                    entries: Some(vec![
                        PackageEntry {
                            path: Some("meta/package2.cm".into()),
                            hash: None,
                            size: Some(11),
                            modified: Some(pkg2_modified),
                            ..PackageEntry::EMPTY
                        },
                        PackageEntry {
                            path: Some("meta/package2.cmx".into()),
                            hash: None,
                            size: Some(12),
                            modified: Some(pkg2_modified),
                            ..PackageEntry::EMPTY
                        },
                    ]),
                    ..RepositoryPackage::EMPTY
                },
            ],
        );
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_get_tuf_client() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();
        let metadata_dir = dir.join("repository");
        create_dir_all(&metadata_dir).unwrap();

        let mut repo = FileSystemRepositoryBuilder::<Json>::new(metadata_dir.clone())
            .targets_prefix("targets")
            .build()
            .unwrap();

        let key = repo_private_key();
        let metadata = RepoBuilder::create(&mut repo)
            .trusted_root_keys(&[&key])
            .trusted_targets_keys(&[&key])
            .trusted_snapshot_keys(&[&key])
            .trusted_timestamp_keys(&[&key])
            .commit()
            .await
            .unwrap();

        let database = Database::from_trusted_metadata(&metadata).unwrap();

        RepoBuilder::from_database(&mut repo, &database)
            .trusted_root_keys(&[&key])
            .trusted_targets_keys(&[&key])
            .trusted_snapshot_keys(&[&key])
            .trusted_timestamp_keys(&[&key])
            .stage_root()
            .unwrap()
            .commit()
            .await
            .unwrap();

        let backend = PmRepository::new(dir.to_path_buf()).unwrap();
        let _repo = RepoClient::new("name", Box::new(backend)).await.unwrap();

        std::fs::remove_file(dir.join("repository").join("1.root.json")).unwrap();

        let backend = PmRepository::new(dir.to_path_buf()).unwrap();
        let _repo = RepoClient::new("name", Box::new(backend)).await.unwrap();
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_show_package() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();

        let backend = Box::new(make_pm_repository(&dir).await);
        let repo = RepoClient::new(REPO_NAME, backend).await.unwrap();

        // Look up the timestamps for the blobs.
        let blob_dir = dir.join("repository").join("blobs");
        let meta_far_modified = get_modtime(blob_dir.join(PKG1_HASH));

        let bin_modified = get_modtime(blob_dir.join(PKG1_BIN_HASH));
        let lib_modified = get_modtime(blob_dir.join(PKG1_LIB_HASH));

        let mut entries = repo.show_package("package1".into()).await.unwrap().unwrap();

        // show_packages returns contents out of order. Sort the entries so they are consistent.
        entries.sort_unstable_by(|lhs, rhs| lhs.path.cmp(&rhs.path));

        assert_eq!(
            entries,
            vec![
                PackageEntry {
                    path: Some("bin/package1".into()),
                    hash: Some(PKG1_BIN_HASH.into()),
                    size: Some(15),
                    modified: Some(bin_modified),
                    ..PackageEntry::EMPTY
                },
                PackageEntry {
                    path: Some("lib/package1".into()),
                    hash: Some(PKG1_LIB_HASH.into()),
                    size: Some(12),
                    modified: Some(lib_modified),
                    ..PackageEntry::EMPTY
                },
                PackageEntry {
                    path: Some("meta.far".into()),
                    hash: Some(PKG1_HASH.into()),
                    size: Some(24576),
                    modified: Some(meta_far_modified),
                    ..PackageEntry::EMPTY
                },
                PackageEntry {
                    path: Some("meta/contents".into()),
                    hash: None,
                    size: Some(156),
                    modified: Some(meta_far_modified),
                    ..PackageEntry::EMPTY
                },
                PackageEntry {
                    path: Some("meta/fuchsia.abi/abi-revision".into()),
                    hash: None,
                    size: Some(8),
                    modified: Some(meta_far_modified),
                    ..PackageEntry::EMPTY
                },
                PackageEntry {
                    path: Some("meta/package".into()),
                    hash: None,
                    size: Some(33),
                    modified: Some(meta_far_modified),
                    ..PackageEntry::EMPTY
                },
                PackageEntry {
                    path: Some("meta/package1.cm".into()),
                    hash: None,
                    size: Some(11),
                    modified: Some(meta_far_modified),
                    ..PackageEntry::EMPTY
                },
                PackageEntry {
                    path: Some("meta/package1.cmx".into()),
                    hash: None,
                    size: Some(12),
                    modified: Some(meta_far_modified),
                    ..PackageEntry::EMPTY
                },
            ]
        );
    }
}
