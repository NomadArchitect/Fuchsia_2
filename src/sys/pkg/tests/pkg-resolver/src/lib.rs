// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::Error,
    blobfs_ramdisk::BlobfsRamdisk,
    cobalt_client::traits::AsEventCodes,
    diagnostics_hierarchy::{testing::TreeAssertion, DiagnosticsHierarchy},
    diagnostics_reader::{ArchiveReader, ComponentSelector, Inspect},
    fidl::endpoints::ClientEnd,
    fidl_fuchsia_boot::{ArgumentsRequest, ArgumentsRequestStream},
    fidl_fuchsia_cobalt::{CobaltEvent, CountEvent, EventPayload},
    fidl_fuchsia_io::{
        DirectoryMarker, DirectoryProxy, CLONE_FLAG_SAME_RIGHTS, OPEN_RIGHT_READABLE,
        OPEN_RIGHT_WRITABLE,
    },
    fidl_fuchsia_pkg::{
        ExperimentToggle as Experiment, FontResolverMarker, FontResolverProxy, LocalMirrorMarker,
        PackageCacheMarker, PackageResolverAdminMarker, PackageResolverAdminProxy,
        PackageResolverMarker, PackageResolverProxy, RepositoryManagerMarker,
        RepositoryManagerProxy,
    },
    fidl_fuchsia_pkg_ext::{BlobId, RepositoryConfig, RepositoryConfigBuilder, RepositoryConfigs},
    fidl_fuchsia_pkg_rewrite::{
        EngineMarker as RewriteEngineMarker, EngineProxy as RewriteEngineProxy,
    },
    fidl_fuchsia_pkg_rewrite_ext::{Rule, RuleConfig},
    fuchsia_async as fasync,
    fuchsia_component::{
        client::{App, AppBuilder},
        server::{NestedEnvironment, ServiceFs},
    },
    fuchsia_merkle::{Hash, MerkleTree},
    fuchsia_pkg_testing::SystemImageBuilder,
    fuchsia_pkg_testing::{serve::ServedRepository, Package, PackageBuilder, Repository},
    fuchsia_url::pkg_url::RepoUrl,
    fuchsia_zircon::{self as zx, Status},
    futures::{future::BoxFuture, prelude::*},
    matches::assert_matches,
    parking_lot::Mutex,
    pkgfs_ramdisk::PkgfsRamdisk,
    serde::Serialize,
    std::{
        convert::TryInto,
        fs::File,
        io::{self, BufWriter, Read},
        path::{Path, PathBuf},
        sync::Arc,
        time::Duration,
    },
    tempfile::TempDir,
};

// If the body of an https response is not large enough, hyper will download the body
// along with the header in the initial fuchsia_hyper::HttpsClient.request(). This means
// that even if the body is implemented with a stream that sends some bytes and then fails
// before the transfer is complete, the error will occur on the initial request instead
// of when looping over the Response body bytes.
// This value probably just needs to be larger than the Hyper buffer, which defaults to 400 kB
// https://docs.rs/hyper/0.13.10/hyper/client/struct.Builder.html#method.http1_max_buf_size
pub const FILE_SIZE_LARGE_ENOUGH_TO_TRIGGER_HYPER_BATCHING: usize = 600_000;

pub mod mock_filesystem;

pub trait PkgFs {
    fn root_dir_handle(&self) -> Result<ClientEnd<DirectoryMarker>, Error>;

    fn blobfs_root_dir_handle(&self) -> Result<ClientEnd<DirectoryMarker>, Error>;
}

impl PkgFs for PkgfsRamdisk {
    fn root_dir_handle(&self) -> Result<ClientEnd<DirectoryMarker>, Error> {
        PkgfsRamdisk::root_dir_handle(self)
    }

    fn blobfs_root_dir_handle(&self) -> Result<ClientEnd<DirectoryMarker>, Error> {
        self.blobfs().root_dir_handle()
    }
}

pub struct Mounts {
    pkg_resolver_data: DirOrProxy,
    pkg_resolver_config_data: DirOrProxy,
}

#[derive(Serialize)]
pub struct Config {
    pub enable_dynamic_configuration: bool,
}

#[derive(Default)]
pub struct MountsBuilder {
    pkg_resolver_data: Option<DirOrProxy>,
    pkg_resolver_config_data: Option<DirOrProxy>,
    config: Option<Config>,
    static_repository: Option<RepositoryConfig>,
    dynamic_rewrite_rules: Option<RuleConfig>,
    dynamic_repositories: Option<RepositoryConfigs>,
    custom_config_data: Vec<(PathBuf, String)>,
}

impl MountsBuilder {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn pkg_resolver_data(mut self, pkg_resolver_data: DirOrProxy) -> Self {
        self.pkg_resolver_data = Some(pkg_resolver_data);
        self
    }
    pub fn pkg_resolver_config_data(mut self, pkg_resolver_config_data: DirOrProxy) -> Self {
        self.pkg_resolver_config_data = Some(pkg_resolver_config_data);
        self
    }
    pub fn config(mut self, config: Config) -> Self {
        self.config = Some(config);
        self
    }
    pub fn static_repository(mut self, static_repository: RepositoryConfig) -> Self {
        self.static_repository = Some(static_repository);
        self
    }
    pub fn dynamic_rewrite_rules(mut self, dynamic_rewrite_rules: RuleConfig) -> Self {
        self.dynamic_rewrite_rules = Some(dynamic_rewrite_rules);
        self
    }
    pub fn dynamic_repositories(mut self, dynamic_repositories: RepositoryConfigs) -> Self {
        self.dynamic_repositories = Some(dynamic_repositories);
        self
    }
    /// Injects a file with custom contents into /config/data. Panics if file already exists.
    pub fn custom_config_data(mut self, path: impl Into<PathBuf>, data: impl Into<String>) -> Self {
        self.custom_config_data.push((path.into(), data.into()));
        self
    }
    pub fn build(self) -> Mounts {
        let mounts = Mounts {
            pkg_resolver_data: self
                .pkg_resolver_data
                .unwrap_or_else(|| DirOrProxy::Dir(tempfile::tempdir().expect("/tmp to exist"))),
            pkg_resolver_config_data: self
                .pkg_resolver_config_data
                .unwrap_or_else(|| DirOrProxy::Dir(tempfile::tempdir().expect("/tmp to exist"))),
        };
        if let Some(config) = self.config {
            mounts.add_config(&config);
        }
        if let Some(config) = self.static_repository {
            mounts.add_static_repository(config);
        }
        if let Some(config) = self.dynamic_rewrite_rules {
            mounts.add_dynamic_rewrite_rules(&config);
        }
        if let Some(config) = self.dynamic_repositories {
            mounts.add_dynamic_repositories(&config);
        }
        for (path, data) in self.custom_config_data {
            mounts.add_custom_config_data(path, data);
        }
        mounts
    }
}

impl Mounts {
    fn add_config(&self, config: &Config) {
        if let DirOrProxy::Dir(ref d) = self.pkg_resolver_config_data {
            let f = File::create(d.path().join("config.json")).unwrap();
            serde_json::to_writer(BufWriter::new(f), &config).unwrap();
        } else {
            panic!("not supported");
        }
    }

    fn add_static_repository(&self, config: RepositoryConfig) {
        if let DirOrProxy::Dir(ref d) = self.pkg_resolver_config_data {
            let static_repo_path = d.path().join("repositories");
            if !static_repo_path.exists() {
                std::fs::create_dir(&static_repo_path).unwrap();
            }
            let f =
                File::create(static_repo_path.join(format!("{}.json", config.repo_url().host())))
                    .unwrap();
            serde_json::to_writer(BufWriter::new(f), &RepositoryConfigs::Version1(vec![config]))
                .unwrap();
        } else {
            panic!("not supported");
        }
    }

    fn add_dynamic_rewrite_rules(&self, rule_config: &RuleConfig) {
        if let DirOrProxy::Dir(ref d) = self.pkg_resolver_data {
            let f = File::create(d.path().join("rewrites.json")).unwrap();
            serde_json::to_writer(BufWriter::new(f), rule_config).unwrap();
        } else {
            panic!("not supported");
        }
    }
    fn add_dynamic_repositories(&self, repo_configs: &RepositoryConfigs) {
        if let DirOrProxy::Dir(ref d) = self.pkg_resolver_data {
            let f = File::create(d.path().join("repositories.json")).unwrap();
            serde_json::to_writer(BufWriter::new(f), repo_configs).unwrap();
        } else {
            panic!("not supported");
        }
    }

    fn add_custom_config_data(&self, path: impl AsRef<Path>, data: String) {
        if let DirOrProxy::Dir(ref d) = self.pkg_resolver_config_data {
            let path = d.path().join(path);
            assert!(!path.exists());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, &data).unwrap();
        } else {
            panic!("not supported");
        }
    }
}

pub enum DirOrProxy {
    Dir(TempDir),
    Proxy(DirectoryProxy),
}

pub trait AppBuilderExt {
    fn add_dir_or_proxy_to_namespace(
        self,
        path: impl Into<String>,
        dir_or_proxy: &DirOrProxy,
    ) -> Self;
}

impl AppBuilderExt for AppBuilder {
    fn add_dir_or_proxy_to_namespace(
        self,
        path: impl Into<String>,
        dir_or_proxy: &DirOrProxy,
    ) -> Self {
        match dir_or_proxy {
            DirOrProxy::Dir(d) => {
                self.add_dir_to_namespace(path.into(), File::open(d.path()).unwrap()).unwrap()
            }
            DirOrProxy::Proxy(p) => {
                self.add_handle_to_namespace(path.into(), clone_directory_proxy(p))
            }
        }
    }
}

pub fn clone_directory_proxy(proxy: &DirectoryProxy) -> zx::Handle {
    let (client, server) = fidl::endpoints::create_endpoints().unwrap();
    proxy.clone(CLONE_FLAG_SAME_RIGHTS, server).unwrap();
    client.into()
}

async fn pkgfs_with_system_image() -> PkgfsRamdisk {
    let system_image_package = SystemImageBuilder::new();
    let system_image_package = system_image_package.build().await;
    pkgfs_with_system_image_and_pkg(&system_image_package, None).await
}

pub async fn pkgfs_with_system_image_and_pkg(
    system_image_package: &Package,
    pkg: Option<&Package>,
) -> PkgfsRamdisk {
    let blobfs = BlobfsRamdisk::start().unwrap();
    system_image_package.write_to_blobfs_dir(&blobfs.root_dir().unwrap());
    if let Some(pkg) = pkg {
        pkg.write_to_blobfs_dir(&blobfs.root_dir().unwrap());
    }
    PkgfsRamdisk::builder()
        .blobfs(blobfs)
        .system_image_merkle(system_image_package.meta_far_merkle_root())
        .start()
        .unwrap()
}

pub struct TestEnvBuilder<PkgFsFn, PkgFsFut, MountsFn>
where
    PkgFsFn: FnOnce() -> PkgFsFut,
    PkgFsFut: Future,
{
    pkgfs: PkgFsFn,
    mounts: MountsFn,
    boot_arguments_service: Option<BootArgumentsService<'static>>,
    local_mirror_repo: Option<(Arc<Repository>, RepoUrl)>,
    allow_local_mirror: bool,
    tuf_metadata_timeout: Option<Duration>,
    blob_network_header_timeout: Option<Duration>,
    blob_network_body_timeout: Option<Duration>,
    blob_download_resumption_attempts_limit: Option<u64>,
}

impl
    TestEnvBuilder<
        fn() -> BoxFuture<'static, PkgfsRamdisk>,
        BoxFuture<'static, PkgfsRamdisk>,
        fn() -> Mounts,
    >
{
    pub fn new() -> Self {
        Self {
            pkgfs: || pkgfs_with_system_image().boxed(),
            // If it's not overriden, the default state of the mounts allows for dynamic configuration.
            // We do this because in the majority of tests, we'll want to use dynamic repos and rewrite rules.
            // Note: this means that we'll produce different envs from TestEnvBuilder::new().build().await
            // vs TestEnvBuilder::new().mounts(MountsBuilder::new().build()).build()
            mounts: || {
                MountsBuilder::new().config(Config { enable_dynamic_configuration: true }).build()
            },
            boot_arguments_service: None,
            local_mirror_repo: None,
            allow_local_mirror: false,
            tuf_metadata_timeout: None,
            blob_network_header_timeout: None,
            blob_network_body_timeout: None,
            blob_download_resumption_attempts_limit: None,
        }
    }
}

impl<PkgFsFn, PkgFsFut, MountsFn> TestEnvBuilder<PkgFsFn, PkgFsFut, MountsFn>
where
    PkgFsFn: FnOnce() -> PkgFsFut,
    PkgFsFut: Future,
    PkgFsFut::Output: PkgFs,
    MountsFn: FnOnce() -> Mounts,
{
    pub fn pkgfs<Pother>(
        self,
        pkgfs: Pother,
    ) -> TestEnvBuilder<impl FnOnce() -> future::Ready<Pother>, future::Ready<Pother>, MountsFn>
    where
        Pother: PkgFs + 'static,
    {
        TestEnvBuilder::<_, _, MountsFn> {
            pkgfs: || future::ready(pkgfs),
            mounts: self.mounts,
            boot_arguments_service: self.boot_arguments_service,
            local_mirror_repo: self.local_mirror_repo,
            allow_local_mirror: self.allow_local_mirror,
            tuf_metadata_timeout: self.tuf_metadata_timeout,
            blob_network_header_timeout: self.blob_network_header_timeout,
            blob_network_body_timeout: self.blob_network_body_timeout,
            blob_download_resumption_attempts_limit: self.blob_download_resumption_attempts_limit,
        }
    }
    pub fn mounts(
        self,
        mounts: Mounts,
    ) -> TestEnvBuilder<PkgFsFn, PkgFsFut, impl FnOnce() -> Mounts> {
        TestEnvBuilder::<PkgFsFn, _, _> {
            pkgfs: self.pkgfs,
            mounts: || mounts,
            boot_arguments_service: self.boot_arguments_service,
            local_mirror_repo: self.local_mirror_repo,
            allow_local_mirror: self.allow_local_mirror,
            tuf_metadata_timeout: self.tuf_metadata_timeout,
            blob_network_header_timeout: self.blob_network_header_timeout,
            blob_network_body_timeout: self.blob_network_body_timeout,
            blob_download_resumption_attempts_limit: self.blob_download_resumption_attempts_limit,
        }
    }
    pub fn boot_arguments_service(self, svc: BootArgumentsService<'static>) -> Self {
        Self {
            pkgfs: self.pkgfs,
            mounts: self.mounts,
            boot_arguments_service: Some(svc),
            local_mirror_repo: self.local_mirror_repo,
            allow_local_mirror: self.allow_local_mirror,
            tuf_metadata_timeout: self.tuf_metadata_timeout,
            blob_network_header_timeout: self.blob_network_header_timeout,
            blob_network_body_timeout: self.blob_network_body_timeout,
            blob_download_resumption_attempts_limit: self.blob_download_resumption_attempts_limit,
        }
    }

    pub fn local_mirror_repo(mut self, repo: &Arc<Repository>, hostname: RepoUrl) -> Self {
        self.local_mirror_repo = Some((repo.clone(), hostname));
        self
    }

    pub fn allow_local_mirror(mut self) -> Self {
        assert_eq!(self.allow_local_mirror, false, "allow_local_mirror should only be set once");
        self.allow_local_mirror = true;
        self
    }

    pub fn tuf_metadata_timeout(mut self, timeout: Duration) -> Self {
        assert!(
            self.tuf_metadata_timeout.is_none(),
            "tuf_metadata_timeout should only be set once"
        );
        assert_eq!(
            timeout,
            Duration::from_secs(timeout.as_secs()),
            "tuf_metadata_timeout must be a whole number of seconds, because the command line \
            flag is in seconds"
        );
        self.tuf_metadata_timeout = Some(timeout);
        self
    }

    pub fn blob_network_header_timeout(mut self, timeout: Duration) -> Self {
        assert!(
            self.blob_network_header_timeout.is_none(),
            "blob_network_header_timeout should only be set once"
        );
        assert_eq!(
            timeout,
            Duration::from_secs(timeout.as_secs()),
            "blob_network_header_timeout must be a whole number of seconds, because the command line \
            flag is in seconds"
        );
        self.blob_network_header_timeout = Some(timeout);
        self
    }

    pub fn blob_network_body_timeout(mut self, timeout: Duration) -> Self {
        assert!(
            self.blob_network_body_timeout.is_none(),
            "blob_network_body_timeout should only be set once"
        );
        assert_eq!(
            timeout,
            Duration::from_secs(timeout.as_secs()),
            "blob_network_body_timeout must be a whole number of seconds, because the command line \
            flag is in seconds"
        );
        self.blob_network_body_timeout = Some(timeout);
        self
    }

    pub fn blob_download_resumption_attempts_limit(mut self, limit: u64) -> Self {
        assert!(
            self.blob_download_resumption_attempts_limit.is_none(),
            "blob_download_resumption_attempts_limit should only be set once"
        );
        self.blob_download_resumption_attempts_limit = Some(limit);
        self
    }

    pub async fn build(self) -> TestEnv<PkgFsFut::Output> {
        let pkgfs = (self.pkgfs)().await;
        let mounts = (self.mounts)();

        let mut pkg_cache = AppBuilder::new(
            "fuchsia-pkg://fuchsia.com/pkg-resolver-integration-tests#meta/pkg-cache.cmx"
                .to_owned(),
        )
        .add_handle_to_namespace(
            "/pkgfs".to_owned(),
            pkgfs.root_dir_handle().expect("pkgfs dir to open").into(),
        )
        .add_handle_to_namespace(
            "/blob".to_owned(),
            pkgfs.blobfs_root_dir_handle().expect("blob dir to open").into(),
        );

        let local_mirror_dir = tempfile::tempdir().unwrap();
        let mut local_mirror = if let Some((repo, url)) = self.local_mirror_repo {
            let proxy = io_util::directory::open_in_namespace(
                local_mirror_dir.path().to_str().unwrap(),
                OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
            )
            .unwrap();
            repo.copy_local_repository_to_dir(&proxy, &url).await;

            Some(AppBuilder::new(
                "fuchsia-pkg://fuchsia.com/pkg-resolver-integration-tests#meta/pkg-local-mirror.cmx"
                    .to_owned(),
            ).add_dir_to_namespace("/usb/0/fuchsia_pkg".to_owned(), std::fs::File::open(local_mirror_dir.path()).unwrap()).unwrap())
        } else {
            None
        };

        let pkg_resolver = AppBuilder::new(RESOLVER_MANIFEST_URL.to_owned())
            .add_handle_to_namespace(
                "/pkgfs".to_owned(),
                pkgfs.root_dir_handle().expect("pkgfs dir to open").into(),
            )
            .add_dir_or_proxy_to_namespace("/data", &mounts.pkg_resolver_data)
            .add_dir_or_proxy_to_namespace("/config/data", &mounts.pkg_resolver_config_data)
            .add_dir_to_namespace("/config/ssl".to_owned(), File::open("/pkg/data/ssl").unwrap())
            .unwrap();

        let pkg_resolver = if self.allow_local_mirror {
            pkg_resolver.args(vec!["--allow-local-mirror", "true"])
        } else {
            pkg_resolver
        };

        let pkg_resolver = if let Some(timeout) = self.tuf_metadata_timeout {
            pkg_resolver.args(vec![
                "--tuf-metadata-timeout-seconds".to_string(),
                timeout.as_secs().to_string(),
            ])
        } else {
            pkg_resolver
        };

        let pkg_resolver = if let Some(timeout) = self.blob_network_header_timeout {
            pkg_resolver.args(vec![
                "--blob-network-header-timeout-seconds".to_string(),
                timeout.as_secs().to_string(),
            ])
        } else {
            pkg_resolver
        };

        let pkg_resolver = if let Some(timeout) = self.blob_network_body_timeout {
            pkg_resolver.args(vec![
                "--blob-network-body-timeout-seconds".to_string(),
                timeout.as_secs().to_string(),
            ])
        } else {
            pkg_resolver
        };

        let pkg_resolver = if let Some(limit) = self.blob_download_resumption_attempts_limit {
            pkg_resolver.args(vec![
                "--blob-download-resumption-attempts-limit".to_string(),
                limit.to_string(),
            ])
        } else {
            pkg_resolver
        };

        let mut fs = ServiceFs::new();
        fs.add_proxy_service::<fidl_fuchsia_net::NameLookupMarker, _>()
            .add_proxy_service::<fidl_fuchsia_posix_socket::ProviderMarker, _>()
            .add_proxy_service::<fidl_fuchsia_logger::LogSinkMarker, _>()
            .add_proxy_service::<fidl_fuchsia_tracing_provider::RegistryMarker, _>()
            .add_proxy_service_to::<PackageCacheMarker, _>(
                pkg_cache.directory_request().unwrap().clone(),
            );
        if let Some(local_mirror) = local_mirror.as_mut() {
            fs.add_proxy_service_to::<LocalMirrorMarker, _>(
                local_mirror.directory_request().unwrap().clone(),
            );
        }

        if let Some(boot_arguments_service) = self.boot_arguments_service {
            let mock_arg_svc = Arc::new(boot_arguments_service);
            fs.add_fidl_service(move |stream: ArgumentsRequestStream| {
                fasync::Task::spawn(Arc::clone(&mock_arg_svc).run_service(stream)).detach();
            });
        }

        let logger_factory = Arc::new(MockLoggerFactory::new());
        let logger_factory_clone = Arc::clone(&logger_factory);
        fs.add_fidl_service(move |stream| {
            fasync::Task::spawn(Arc::clone(&logger_factory_clone).run_logger_factory(stream))
                .detach()
        });

        let mut salt = [0; 4];
        zx::cprng_draw(&mut salt[..]).expect("zx_cprng_draw does not fail");
        let environment_label = format!("pkg-resolver-env_{}", hex::encode(&salt));
        let env = fs
            .create_nested_environment(&environment_label)
            .expect("nested environment to create successfully");
        fasync::Task::spawn(fs.collect()).detach();

        let pkg_cache = pkg_cache.spawn(env.launcher()).expect("package cache to launch");
        let pkg_resolver = pkg_resolver.spawn(env.launcher()).expect("package resolver to launch");
        let local_mirror =
            local_mirror.map(|app| app.spawn(env.launcher()).expect("local mirror to launch"));

        TestEnv {
            env,
            pkgfs,
            proxies: Proxies::from_app(&pkg_resolver),
            apps: Apps { pkg_cache, pkg_resolver, local_mirror },
            _mounts: mounts,
            nested_environment_label: environment_label,
            mocks: Mocks { logger_factory },
            local_mirror_dir,
        }
    }
}

pub struct Apps {
    pub pkg_cache: App,
    pub pkg_resolver: App,
    pub local_mirror: Option<App>,
}

pub struct Proxies {
    pub resolver_admin: PackageResolverAdminProxy,
    pub resolver: PackageResolverProxy,
    pub repo_manager: RepositoryManagerProxy,
    pub rewrite_engine: RewriteEngineProxy,
    pub font_resolver: FontResolverProxy,
}

impl Proxies {
    pub fn from_app(app: &App) -> Self {
        Proxies {
            resolver: app
                .connect_to_service::<PackageResolverMarker>()
                .expect("connect to package resolver"),
            resolver_admin: app
                .connect_to_service::<PackageResolverAdminMarker>()
                .expect("connect to package resolver admin"),
            repo_manager: app
                .connect_to_service::<RepositoryManagerMarker>()
                .expect("connect to repository manager"),
            rewrite_engine: app
                .connect_to_service::<RewriteEngineMarker>()
                .expect("connect to rewrite engine"),
            font_resolver: app
                .connect_to_service::<FontResolverMarker>()
                .expect("connect to font resolver"),
        }
    }
}

pub struct Mocks {
    pub logger_factory: Arc<MockLoggerFactory>,
}

pub struct TestEnv<P = PkgfsRamdisk> {
    pub pkgfs: P,
    pub env: NestedEnvironment,
    pub apps: Apps,
    pub proxies: Proxies,
    pub _mounts: Mounts,
    pub nested_environment_label: String,
    pub mocks: Mocks,
    pub local_mirror_dir: TempDir,
}

impl TestEnv<PkgfsRamdisk> {
    pub fn add_slice_to_blobfs(&self, slice: &[u8]) {
        let merkle = MerkleTree::from_reader(slice).expect("merkle slice").root().to_string();
        let mut blob = self
            .pkgfs
            .blobfs()
            .root_dir()
            .expect("blobfs has root dir")
            .write_file(merkle, 0)
            .expect("create file in blobfs");
        blob.set_len(slice.len() as u64).expect("set_len");
        io::copy(&mut &slice[..], &mut blob).expect("copy from slice to blob");
    }

    pub fn add_file_with_merkle_to_blobfs(&self, mut file: File, merkle: &Hash) {
        let mut blob = self
            .pkgfs
            .blobfs()
            .root_dir()
            .expect("blobfs has root dir")
            .write_file(merkle.to_string(), 0)
            .expect("create file in blobfs");
        blob.set_len(file.metadata().expect("file has metadata").len()).expect("set_len");
        io::copy(&mut file, &mut blob).expect("copy file to blobfs");
    }

    pub fn add_file_to_pkgfs_at_path(&self, mut file: File, path: impl openat::AsPath) {
        let mut blob = self
            .pkgfs
            .root_dir()
            .expect("pkgfs root_dir")
            .new_file(path, 0)
            .expect("create file in pkgfs");
        blob.set_len(file.metadata().expect("file has metadata").len()).expect("set_len");
        io::copy(&mut file, &mut blob).expect("copy file to pkgfs");
    }

    pub fn partially_add_file_to_pkgfs_at_path(&self, mut file: File, path: impl openat::AsPath) {
        let full_len = file.metadata().expect("file has metadata").len();
        assert!(full_len > 1, "can't partially write 1 byte");
        let mut partial_bytes = vec![0; full_len as usize / 2];
        file.read_exact(partial_bytes.as_mut_slice()).expect("partial read of file");
        let mut blob = self
            .pkgfs
            .root_dir()
            .expect("pkgfs root_dir")
            .new_file(path, 0)
            .expect("create file in pkgfs");
        blob.set_len(full_len).expect("set_len");
        io::copy(&mut partial_bytes.as_slice(), &mut blob).expect("copy file to pkgfs");
    }

    pub fn partially_add_slice_to_pkgfs_at_path(&self, slice: &[u8], path: impl openat::AsPath) {
        assert!(slice.len() > 1, "can't partially write 1 byte");
        let partial_slice = &slice[0..slice.len() / 2];
        let mut blob = self
            .pkgfs
            .root_dir()
            .expect("pkgfs root_dir")
            .new_file(path, 0)
            .expect("create file in pkgfs");
        blob.set_len(slice.len() as u64).expect("set_len");
        io::copy(&mut &partial_slice[..], &mut blob).expect("copy file to pkgfs");
    }

    pub async fn stop(self) {
        // Tear down the environment in reverse order, ending with the storage.
        drop(self.proxies);
        drop(self.apps);
        drop(self.env);
        self.pkgfs.stop().await.expect("pkgfs to stop gracefully");
    }
}

pub struct BootArgumentsService<'a> {
    tuf_repo_config: &'a str,
}
impl BootArgumentsService<'_> {
    pub fn new(tuf_repo_config: &'static str) -> Self {
        Self { tuf_repo_config }
    }
    async fn run_service(self: Arc<Self>, mut stream: ArgumentsRequestStream) {
        while let Some(req) = stream.try_next().await.unwrap() {
            match req {
                ArgumentsRequest::GetString { key, responder } => {
                    assert_eq!(key, "tuf_repo_config", "Unexpected GetString key: {}", key);
                    responder.send(Some(self.tuf_repo_config)).unwrap();
                }
                _ => panic!("Unexpected request to mock BootArgumentsService!"),
            };
        }
    }
}

struct MockLogger {
    cobalt_events: Mutex<Vec<CobaltEvent>>,
}

impl MockLogger {
    fn new() -> Self {
        Self { cobalt_events: Mutex::new(vec![]) }
    }

    async fn run_logger(self: Arc<Self>, mut stream: fidl_fuchsia_cobalt::LoggerRequestStream) {
        while let Some(event) = stream.try_next().await.unwrap() {
            match event {
                fidl_fuchsia_cobalt::LoggerRequest::LogCobaltEvent { event, responder } => {
                    self.cobalt_events.lock().push(event);
                    let _ = responder.send(fidl_fuchsia_cobalt::Status::Ok);
                }
                _ => {
                    panic!("unhandled Logger method {:?}", event);
                }
            }
        }
    }
}

pub struct MockLoggerFactory {
    loggers: Mutex<Vec<Arc<MockLogger>>>,
}

impl MockLoggerFactory {
    fn new() -> Self {
        Self { loggers: Mutex::new(vec![]) }
    }

    async fn run_logger_factory(
        self: Arc<Self>,
        mut stream: fidl_fuchsia_cobalt::LoggerFactoryRequestStream,
    ) {
        while let Some(event) = stream.try_next().await.unwrap() {
            match event {
                fidl_fuchsia_cobalt::LoggerFactoryRequest::CreateLoggerFromProjectId {
                    project_id,
                    logger,
                    responder,
                } => {
                    assert_eq!(project_id, cobalt_sw_delivery_registry::PROJECT_ID);
                    let mock_logger = Arc::new(MockLogger::new());
                    self.loggers.lock().push(mock_logger.clone());
                    fasync::Task::spawn(mock_logger.run_logger(logger.into_stream().unwrap()))
                        .detach();
                    let _ = responder.send(fidl_fuchsia_cobalt::Status::Ok);
                }
                _ => {
                    panic!("unhandled LoggerFactory method: {:?}", event);
                }
            }
        }
    }

    pub async fn wait_for_at_least_n_events_with_metric_id(
        &self,
        n: usize,
        id: u32,
    ) -> Vec<CobaltEvent> {
        loop {
            let events: Vec<CobaltEvent> = self
                .loggers
                .lock()
                .iter()
                .flat_map(|logger| logger.cobalt_events.lock().clone().into_iter())
                .filter(|CobaltEvent { metric_id, .. }| *metric_id == id)
                .collect();
            if events.len() >= n {
                return events;
            }
            fasync::Timer::new(fasync::Time::after(zx::Duration::from_millis(10))).await;
        }
    }
}

impl<P: PkgFs> TestEnv<P> {
    pub async fn set_experiment_state(&self, experiment: Experiment, state: bool) {
        self.proxies
            .resolver_admin
            .set_experiment_state(experiment, state)
            .await
            .expect("experiment state to toggle");
    }

    pub async fn register_repo(&self, repo: &ServedRepository) {
        self.register_repo_at_url(repo, "fuchsia-pkg://test").await;
    }

    pub async fn register_repo_at_url<R>(&self, repo: &ServedRepository, url: R)
    where
        R: TryInto<RepoUrl>,
        R::Error: std::fmt::Debug,
    {
        let repo_config = repo.make_repo_config(url.try_into().unwrap());
        let () = self.proxies.repo_manager.add(repo_config.into()).await.unwrap().unwrap();
    }

    pub async fn restart_pkg_resolver(&mut self) {
        // Start a new package resolver component
        let pkg_resolver = AppBuilder::new(RESOLVER_MANIFEST_URL.to_owned())
            .add_handle_to_namespace(
                "/pkgfs".to_owned(),
                self.pkgfs.root_dir_handle().expect("pkgfs dir to open").into(),
            )
            .add_dir_or_proxy_to_namespace("/data", &self._mounts.pkg_resolver_data)
            .add_dir_or_proxy_to_namespace("/config/data", &self._mounts.pkg_resolver_config_data)
            .add_dir_to_namespace("/config/ssl".to_owned(), File::open("/pkg/data/ssl").unwrap())
            .unwrap();
        let pkg_resolver =
            pkg_resolver.spawn(self.env.launcher()).expect("package resolver to launch");

        // Previous pkg-resolver terminated when its app goes out of scope
        self.proxies = Proxies::from_app(&pkg_resolver);
        self.apps.pkg_resolver = pkg_resolver;

        self.wait_for_pkg_resolver_to_start().await;
    }

    pub async fn wait_for_pkg_resolver_to_start(&self) {
        self.proxies
            .rewrite_engine
            .test_apply("fuchsia-pkg://test.com/name")
            .await
            .expect("fidl call succeeds")
            .expect("test apply result is ok");
    }

    pub fn connect_to_resolver(&self) -> PackageResolverProxy {
        self.apps
            .pkg_resolver
            .connect_to_service::<PackageResolverMarker>()
            .expect("connect to package resolver")
    }

    pub fn resolve_package(
        &self,
        url: &str,
    ) -> impl Future<Output = Result<DirectoryProxy, Status>> {
        resolve_package(&self.proxies.resolver, url)
    }

    pub fn get_hash(&self, url: impl Into<String>) -> impl Future<Output = Result<BlobId, Status>> {
        let fut =
            self.proxies.resolver.get_hash(&mut fidl_fuchsia_pkg::PackageUrl { url: url.into() });
        async move { fut.await.unwrap().map(|blob_id| blob_id.into()).map_err(|i| Status::from_raw(i)) }
    }

    pub async fn open_cached_package(&self, hash: BlobId) -> Result<DirectoryProxy, zx::Status> {
        let cache_service = self.apps.pkg_cache.connect_to_service::<PackageCacheMarker>().unwrap();
        let (proxy, server_end) = fidl::endpoints::create_proxy().unwrap();
        let () = cache_service
            .open(&mut hash.into(), &mut std::iter::empty(), server_end)
            .await
            .unwrap()
            .map_err(zx::Status::from_raw)?;
        Ok(proxy)
    }

    pub async fn pkg_resolver_inspect_hierarchy(&self) -> DiagnosticsHierarchy {
        ArchiveReader::new()
            .add_selector(ComponentSelector::new(vec![
                self.nested_environment_label.clone(),
                "pkg-resolver.cmx".to_string(),
            ]))
            .snapshot::<Inspect>()
            .await
            .expect("read inspect hierarchy")
            .into_iter()
            .next()
            .expect("one result")
            .payload
            .expect("payload is not none")
    }

    /// Wait until pkg-resolver inspect state satisfies `desired_state`.
    pub async fn wait_for_pkg_resolver_inspect_state(&self, desired_state: TreeAssertion<String>) {
        while desired_state.run(&self.pkg_resolver_inspect_hierarchy().await).is_err() {
            fasync::Timer::new(fasync::Time::after(zx::Duration::from_millis(10))).await;
        }
    }

    /// Wait until at least `expected_event_codes.len()` events of metric id `expected_metric_id`
    /// are received, then assert that the event codes of the received events correspond, in order,
    /// to the event codes in `expected_event_codes`.
    pub async fn assert_count_events(
        &self,
        expected_metric_id: u32,
        expected_event_codes: Vec<impl AsEventCodes>,
    ) {
        let actual_events = self
            .mocks
            .logger_factory
            .wait_for_at_least_n_events_with_metric_id(
                expected_event_codes.len(),
                expected_metric_id,
            )
            .await;
        assert_eq!(
            actual_events.len(),
            expected_event_codes.len(),
            "event count different than expected, actual_events: {:?}",
            actual_events
        );

        for (event, expected_codes) in actual_events
            .into_iter()
            .zip(expected_event_codes.into_iter().map(|c| c.as_event_codes()))
        {
            assert_matches!(
                event,
                CobaltEvent {
                    metric_id,
                    event_codes,
                    component: None,
                    payload: EventPayload::EventCount(CountEvent {
                        period_duration_micros: 0,
                        count: 1
                    }),
                } if metric_id == expected_metric_id && event_codes == expected_codes
            )
        }
    }
}

pub const EMPTY_REPO_PATH: &str = "/pkg/empty-repo";
const RESOLVER_MANIFEST_URL: &str =
    "fuchsia-pkg://fuchsia.com/pkg-resolver-integration-tests#meta/pkg-resolver.cmx";

// The following functions generate unique test package dummy content. Callers are recommended
// to pass in the name of the test case.
pub fn test_package_bin(s: &str) -> Vec<u8> {
    return format!("!/boot/bin/sh\n{}", s).as_bytes().to_owned();
}

pub fn test_package_cmx(s: &str) -> Vec<u8> {
    return format!("\"{{\"program\":{{\"binary\":\"bin/{}\"}}", s).as_bytes().to_owned();
}

pub fn extra_blob_contents(s: &str, i: u32) -> Vec<u8> {
    format!("contents of file {}-{}", s, i).as_bytes().to_owned()
}

pub async fn make_pkg_with_extra_blobs(s: &str, n: u32) -> Package {
    let mut pkg = PackageBuilder::new(s)
        .add_resource_at(format!("bin/{}", s), &test_package_bin(s)[..])
        .add_resource_at(format!("meta/{}.cmx", s), &test_package_cmx(s)[..]);
    for i in 0..n {
        pkg =
            pkg.add_resource_at(format!("data/{}-{}", s, i), extra_blob_contents(s, i).as_slice());
    }
    pkg.build().await.unwrap()
}

pub fn resolve_package(
    resolver: &PackageResolverProxy,
    url: &str,
) -> impl Future<Output = Result<DirectoryProxy, Status>> {
    let (package, package_server_end) = fidl::endpoints::create_proxy().unwrap();
    let selectors: Vec<&str> = vec![];
    let response_fut = resolver.resolve(url, &mut selectors.into_iter(), package_server_end);
    async move {
        let () = response_fut.await.unwrap().map_err(Status::from_raw)?;
        Ok(package)
    }
}

pub fn make_repo_config(repo: &RepositoryConfig) -> RepositoryConfigs {
    RepositoryConfigs::Version1(vec![repo.clone()])
}

pub fn make_repo() -> RepositoryConfig {
    RepositoryConfigBuilder::new("fuchsia-pkg://example.com".parse().unwrap()).build()
}

pub async fn get_repos(repository_manager: &RepositoryManagerProxy) -> Vec<RepositoryConfig> {
    let (repo_iterator, repo_iterator_server) =
        fidl::endpoints::create_proxy().expect("create repo iterator proxy");
    repository_manager.list(repo_iterator_server).expect("list repos");
    let mut ret = vec![];
    loop {
        let repos = repo_iterator.next().await.expect("advance repo iterator");
        if repos.is_empty() {
            return ret;
        }
        ret.extend(repos.into_iter().map(|r| r.try_into().unwrap()))
    }
}

pub async fn get_rules(rewrite_engine: &RewriteEngineProxy) -> Vec<Rule> {
    let (rule_iterator, rule_iterator_server) =
        fidl::endpoints::create_proxy().expect("create rule iterator proxy");
    rewrite_engine.list(rule_iterator_server).expect("list rules");
    let mut ret = vec![];
    loop {
        let rules = rule_iterator.next().await.expect("advance rule iterator");
        if rules.is_empty() {
            return ret;
        }
        ret.extend(rules.into_iter().map(|r| r.try_into().unwrap()))
    }
}
