// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg(test)]

use {
    anyhow::Error,
    fidl_fuchsia_amber_ext::{self as types, SourceConfigBuilder},
    fidl_fuchsia_pkg::{
        PackageCacheRequest, PackageCacheRequestStream, PackageIndexEntry,
        PackageIndexIteratorRequest, RepositoryManagerMarker, RepositoryManagerProxy,
    },
    fidl_fuchsia_pkg_ext::{
        MirrorConfigBuilder, RepositoryConfig, RepositoryConfigBuilder, RepositoryKey,
    },
    fidl_fuchsia_pkg_rewrite::{
        EngineMarker as RewriteEngineMarker, EngineProxy as RewriteEngineProxy,
    },
    fidl_fuchsia_pkg_rewrite_ext::Rule,
    fuchsia_async as fasync,
    fuchsia_component::{
        client::{App, AppBuilder, Output},
        server::{NestedEnvironment, ServiceFs},
    },
    fuchsia_url::pkg_url::RepoUrl,
    futures::prelude::*,
    http::Uri,
    parking_lot::Mutex,
    serde::Serialize,
    std::sync::Arc,
    std::{convert::TryInto, fs::File},
};

const ROOT_KEY_1: &str = "be0b983f7396da675c40c6b93e47fced7c1e9ea8a32a1fe952ba8f519760b307";
const ROOT_KEY_2: &str = "00112233445566778899aabbccddeeffffeeddccbbaa99887766554433221100";

fn amberctl() -> AppBuilder {
    AppBuilder::new("fuchsia-pkg://fuchsia.com/amberctl-tests#meta/amberctl.cmx".to_owned())
}

struct Mounts {
    _misc: tempfile::TempDir,
    pkgfs: tempfile::TempDir,
    config_data: tempfile::TempDir,
}

impl Mounts {
    fn new() -> Self {
        let misc = tempfile::tempdir().expect("/tmp to exist");
        let config_data = tempfile::tempdir().expect("/tmp to exist");
        let pkgfs = tempfile::tempdir().expect("/tmp to exist");
        std::fs::create_dir(pkgfs.path().join("install")).expect("mkdir pkgfs/install");
        std::fs::create_dir(pkgfs.path().join("needs")).expect("mkdir pkgfs/needs");
        Self { _misc: misc, pkgfs, config_data }
    }
}

struct Proxies {
    repo_manager: RepositoryManagerProxy,
    rewrite_engine: RewriteEngineProxy,
}

struct MockUpdateManager {
    called: Mutex<u32>,
    check_now_result: Mutex<fidl_fuchsia_update::ManagerCheckNowResult>,
}
impl MockUpdateManager {
    fn new_with_check_now_result(res: fidl_fuchsia_update::ManagerCheckNowResult) -> Self {
        Self { called: Mutex::new(0), check_now_result: Mutex::new(res) }
    }

    async fn run(
        self: Arc<Self>,
        mut stream: fidl_fuchsia_update::ManagerRequestStream,
    ) -> Result<(), Error> {
        while let Some(event) = stream.try_next().await? {
            match event {
                fidl_fuchsia_update::ManagerRequest::CheckNow { options, monitor, responder } => {
                    eprintln!("TEST: Got update check request with options {:?}", options);
                    assert_eq!(
                        options,
                        fidl_fuchsia_update::CheckOptions {
                            initiator: Some(fidl_fuchsia_update::Initiator::User),
                            allow_attaching_to_existing_update_check: Some(false),
                            ..fidl_fuchsia_update::CheckOptions::EMPTY
                        }
                    );
                    assert_eq!(monitor, None);
                    *self.called.lock() += 1;
                    responder.send(&mut *self.check_now_result.lock())?;
                }

                fidl_fuchsia_update::ManagerRequest::PerformPendingReboot { responder: _ } => {
                    panic!("amberctl should never call PerformPendingReboot");
                }
            }
        }

        Ok(())
    }
}

struct MockSpaceManager {
    called: Mutex<u32>,
}
impl MockSpaceManager {
    fn new() -> Self {
        Self { called: Mutex::new(0) }
    }

    async fn run(
        self: Arc<Self>,
        mut stream: fidl_fuchsia_space::ManagerRequestStream,
    ) -> Result<(), Error> {
        while let Some(event) = stream.try_next().await? {
            *self.called.lock() += 1;
            let fidl_fuchsia_space::ManagerRequest::Gc { responder } = event;
            responder.send(&mut Ok(()))?;
        }
        Ok(())
    }
}

struct TestEnv {
    _pkg_cache: Arc<MockPackageCacheService>,
    _pkg_resolver: App,
    _mounts: Mounts,
    env: NestedEnvironment,
    proxies: Proxies,
}

#[derive(Serialize)]
struct Config {
    enable_dynamic_configuration: bool,
}

impl TestEnv {
    fn new() -> Self {
        Self::new_with_mounts(Mounts::new())
    }

    fn new_with_mounts(mounts: Mounts) -> Self {
        let mut pkg_resolver = AppBuilder::new(
            "fuchsia-pkg://fuchsia.com/amberctl-tests#meta/pkg-resolver-isolated.cmx".to_owned(),
        )
        .add_dir_to_namespace(
            "/pkgfs".to_owned(),
            File::open(mounts.pkgfs.path()).expect("/pkgfs temp dir to open"),
        )
        .expect("/pkgfs to mount")
        .add_dir_to_namespace("/config/ssl".to_owned(), File::open("/pkg/data/ssl").unwrap())
        .expect("/config/ssl to mount")
        .add_dir_to_namespace(
            "/config/data".to_owned(),
            File::open(mounts.config_data.path()).unwrap(),
        )
        .expect("/config/data to mount");
        let f = File::create(mounts.config_data.path().join("config.json")).unwrap();
        serde_json::to_writer(
            std::io::BufWriter::new(f),
            &Config { enable_dynamic_configuration: true },
        )
        .unwrap();

        let mut fs = ServiceFs::new();
        fs.add_proxy_service_to::<RepositoryManagerMarker, _>(
            pkg_resolver.directory_request().unwrap().clone(),
        )
        .add_proxy_service_to::<RewriteEngineMarker, _>(
            pkg_resolver.directory_request().unwrap().clone(),
        );

        let pkg_cache = Arc::new(MockPackageCacheService::new());
        let pkg_cache_clone = Arc::clone(&pkg_cache);
        fs.add_fidl_service(move |stream: PackageCacheRequestStream| {
            fasync::Task::spawn(
                Arc::clone(&pkg_cache_clone)
                    .run_service(stream)
                    .unwrap_or_else(|e| panic!("error running mock cache service: {:?}", e)),
            )
            .detach()
        });

        let env = fs
            .create_salted_nested_environment("amberctl_env")
            .expect("nested environment to create successfully");
        fasync::Task::spawn(fs.collect()).detach();

        let pkg_resolver = pkg_resolver.spawn(env.launcher()).expect("package resolver to launch");

        let repo_manager_proxy = env
            .connect_to_protocol::<RepositoryManagerMarker>()
            .expect("connect to repository manager");
        let rewrite_engine_proxy =
            env.connect_to_protocol::<RewriteEngineMarker>().expect("connect to rewrite engine");

        Self {
            _pkg_cache: pkg_cache,
            _pkg_resolver: pkg_resolver,
            _mounts: mounts,
            env,
            proxies: Proxies {
                repo_manager: repo_manager_proxy,
                rewrite_engine: rewrite_engine_proxy,
            },
        }
    }

    async fn _run_amberctl(&self, builder: AppBuilder) -> String {
        let fut = builder.output(self.env.launcher()).expect("amberctl to launch");
        let output = fut.await.expect("amberctl to run");
        output.ok().expect("amberctl to succeed");
        String::from_utf8(output.stdout).unwrap()
    }

    async fn run_amberctl<'a>(&'a self, args: &'a [impl std::fmt::Debug + AsRef<str>]) -> String {
        self._run_amberctl(amberctl().args(args.into_iter().map(|s| s.as_ref()))).await
    }

    // Runs "amberctl list_srcs" and returns a vec of fuchsia-pkg URIs from the output
    async fn run_amberctl_list_srcs(&self) -> Vec<String> {
        let mut res = vec![];
        let output = self.run_amberctl(&["list_srcs"]).await;
        for (pos, _) in output.match_indices("\"fuchsia-pkg") {
            let (_, suffix) = output.split_at(pos + 1);
            let url = suffix.split('"').next().unwrap();
            res.push(url.to_owned());
        }
        res
    }

    async fn run_amberctl_add_static_src(&self, name: &'static str) {
        self._run_amberctl(
            amberctl()
                .add_dir_to_namespace(
                    "/configs".to_string(),
                    File::open("/pkg/data/sources").expect("/pkg/data/sources to exist"),
                )
                .expect("static /configs to mount")
                .arg("add_src")
                .arg(format!("-f=/configs/{}", name)),
        )
        .await;
    }

    async fn run_amberctl_add_src(&self, source: types::SourceConfig) {
        let config_dir = tempfile::tempdir().expect("temp config dir to create");
        let file_path = config_dir.path().join("test.json");
        let mut config_file = File::create(file_path).expect("temp config file to create");
        serde_json::to_writer(&mut config_file, &source).expect("source config to serialize");
        drop(config_file);

        self._run_amberctl(
            amberctl()
                .add_dir_to_namespace(
                    "/configs".to_string(),
                    File::open(config_dir.path()).expect("temp config dir to exist"),
                )
                .expect("static /configs to mount")
                .arg("add_src")
                .arg("-f=/configs/test.json"),
        )
        .await;
    }

    async fn run_amberctl_add_repo_config(&self, source: types::SourceConfig) {
        let config_dir = tempfile::tempdir().expect("temp config dir to create");
        let file_path = config_dir.path().join("test.json");
        let mut config_file = File::create(file_path).expect("temp config file to create");
        serde_json::to_writer(&mut config_file, &source).expect("source config to serialize");
        drop(config_file);

        self._run_amberctl(
            amberctl()
                .add_dir_to_namespace(
                    "/configs".to_string(),
                    File::open(config_dir.path()).expect("temp config dir to exist"),
                )
                .expect("static /configs to mount")
                .arg("add_repo_cfg")
                .arg("-f=/configs/test.json"),
        )
        .await;
    }

    async fn resolver_list_repos(&self) -> Vec<RepositoryConfig> {
        let (iterator, iterator_server_end) = fidl::endpoints::create_proxy().unwrap();
        self.proxies.repo_manager.list(iterator_server_end).unwrap();
        collect_iterator(|| iterator.next()).await.unwrap()
    }

    async fn rewrite_engine_list_rules(&self) -> Vec<Rule> {
        let (iterator, iterator_server_end) = fidl::endpoints::create_proxy().unwrap();
        self.proxies.rewrite_engine.list(iterator_server_end).unwrap();
        collect_iterator(|| iterator.next()).await.unwrap()
    }
}

async fn collect_iterator<F, E, I, O>(mut next: impl FnMut() -> F) -> Result<Vec<O>, Error>
where
    F: Future<Output = Result<Vec<I>, fidl::Error>>,
    I: TryInto<O, Error = E>,
    Error: From<E>,
{
    let mut res = Vec::new();
    loop {
        let more = next().await?;
        if more.is_empty() {
            break;
        }
        res.extend(more.into_iter().map(|cfg| cfg.try_into()).collect::<Result<Vec<_>, _>>()?);
    }
    Ok(res)
}

struct MockPackageCacheService {}
impl MockPackageCacheService {
    fn new() -> Self {
        Self {}
    }
    async fn run_service(
        self: Arc<Self>,
        mut stream: PackageCacheRequestStream,
    ) -> Result<(), Error> {
        while let Some(req) = stream.try_next().await? {
            match req {
                PackageCacheRequest::Open { responder, .. } => {
                    responder.send(&mut Ok(()))?;
                }
                PackageCacheRequest::Get { .. } => {
                    panic!("PackageCacheRequest::Get should not be called");
                }
                PackageCacheRequest::Sync { .. } => {
                    panic!("PackageCacheRequest::Sync should not be called");
                }
                PackageCacheRequest::BasePackageIndex { iterator, control_handle: _ } => {
                    let mut stream = iterator.into_stream()?;
                    fasync::Task::spawn(
                        async move {
                            while let Some(PackageIndexIteratorRequest::Next { responder }) =
                                stream.try_next().await?
                            {
                                let mut eof = Vec::<PackageIndexEntry>::new();
                                responder.send(&mut eof.iter_mut())?;
                            }
                            Ok(())
                        }
                        .unwrap_or_else(|e: anyhow::Error| {
                            panic!("error serving base package index: {:?}", e)
                        }),
                    )
                    .detach()
                }
            }
        }
        Ok(())
    }
}

struct SourceConfigGenerator {
    id_prefix: String,
    n: usize,
}

impl SourceConfigGenerator {
    fn new(id_prefix: impl Into<String>) -> Self {
        Self { id_prefix: id_prefix.into(), n: 0 }
    }
}

impl Iterator for SourceConfigGenerator {
    type Item = (types::SourceConfigBuilder, RepositoryConfigBuilder);

    fn next(&mut self) -> Option<Self::Item> {
        let id = format!("{}{:02}", &self.id_prefix, self.n);
        let repo_url = format!("fuchsia-pkg://{}", &id);
        let mirror_url = format!("http://example.com/{}", &id);
        self.n += 1;

        Some((
            SourceConfigBuilder::new(id)
                .repo_url(mirror_url.clone())
                .add_root_key(ROOT_KEY_1)
                .auto(true),
            RepositoryConfigBuilder::new(RepoUrl::parse(&repo_url).unwrap())
                .add_root_key(RepositoryKey::Ed25519(hex::decode(ROOT_KEY_1).unwrap()))
                .add_mirror(
                    MirrorConfigBuilder::new(mirror_url.parse::<Uri>().unwrap())
                        .unwrap()
                        .subscribe(true),
                ),
        ))
    }
}

fn make_test_repo_config() -> RepositoryConfig {
    RepositoryConfigBuilder::new("fuchsia-pkg://test".parse().unwrap())
        .add_root_key(RepositoryKey::Ed25519(hex::decode(ROOT_KEY_1).unwrap()))
        .add_mirror(
            MirrorConfigBuilder::new("http://example.com".parse::<Uri>().unwrap())
                .unwrap()
                .subscribe(true),
        )
        .build()
}

fn make_test_repo_config_with_versions() -> RepositoryConfig {
    RepositoryConfigBuilder::new("fuchsia-pkg://test".parse().unwrap())
        .add_root_key(RepositoryKey::Ed25519(hex::decode(ROOT_KEY_1).unwrap()))
        .add_root_key(RepositoryKey::Ed25519(hex::decode(ROOT_KEY_2).unwrap()))
        .root_version(2)
        .root_threshold(2)
        .add_mirror(
            MirrorConfigBuilder::new("http://example.com".parse::<Uri>().unwrap())
                .unwrap()
                .subscribe(true),
        )
        .build()
}

#[fasync::run_singlethreaded(test)]
async fn test_services_start_with_no_config() {
    let env = TestEnv::new();

    assert_eq!(env.run_amberctl_list_srcs().await, Vec::<String>::new());
    assert_eq!(env.resolver_list_repos().await, vec![]);
    assert_eq!(env.rewrite_engine_list_rules().await, vec![]);
}

#[fasync::run_singlethreaded(test)]
async fn test_add_src() {
    let env = TestEnv::new();

    env.run_amberctl_add_static_src("test.json").await;

    assert_eq!(env.run_amberctl_list_srcs().await, vec!["fuchsia-pkg://test"]);
    assert_eq!(env.resolver_list_repos().await, vec![make_test_repo_config()]);
    assert_eq!(
        env.rewrite_engine_list_rules().await,
        vec![Rule::new("fuchsia.com", "test", "/", "/").unwrap()]
    );
}

#[fasync::run_singlethreaded(test)]
async fn test_add_src_with_versions() {
    let env = TestEnv::new();

    env.run_amberctl_add_static_src("test-with-versions.json").await;

    assert_eq!(env.run_amberctl_list_srcs().await, vec!["fuchsia-pkg://test"]);
    assert_eq!(env.resolver_list_repos().await, vec![make_test_repo_config_with_versions()]);
    assert_eq!(
        env.rewrite_engine_list_rules().await,
        vec![Rule::new("fuchsia.com", "test", "/", "/").unwrap()]
    );
}

#[fasync::run_singlethreaded(test)]
async fn test_add_repo() {
    let env = TestEnv::new();

    let source = SourceConfigBuilder::new("localhost")
        .repo_url("http://127.0.0.1:8083")
        .add_root_key(ROOT_KEY_1)
        .build();

    let repo = RepositoryConfigBuilder::new("fuchsia-pkg://localhost".parse().unwrap())
        .add_root_key(RepositoryKey::Ed25519(hex::decode(ROOT_KEY_1).unwrap()))
        .add_mirror(
            MirrorConfigBuilder::new("http://127.0.0.1:8083".parse::<Uri>().unwrap()).unwrap(),
        )
        .build();

    env.run_amberctl_add_repo_config(source).await;

    assert_eq!(env.resolver_list_repos().await, vec![repo]);
    assert_eq!(env.rewrite_engine_list_rules().await, vec![]);
}

#[fasync::run_singlethreaded(test)]
async fn test_add_src_with_ipv4_id() {
    let env = TestEnv::new();

    let source = SourceConfigBuilder::new("http://10.0.0.1:8083")
        .repo_url("http://10.0.0.1:8083")
        .add_root_key(ROOT_KEY_1)
        .build();

    let repo = RepositoryConfigBuilder::new("fuchsia-pkg://http___10_0_0_1_8083".parse().unwrap())
        .add_root_key(RepositoryKey::Ed25519(hex::decode(ROOT_KEY_1).unwrap()))
        .add_mirror(
            MirrorConfigBuilder::new("http://10.0.0.1:8083".parse::<Uri>().unwrap()).unwrap(),
        )
        .build();

    env.run_amberctl_add_src(source).await;

    assert_eq!(env.resolver_list_repos().await, vec![repo]);
    assert_eq!(
        env.rewrite_engine_list_rules().await,
        vec![Rule::new("fuchsia.com", "http___10_0_0_1_8083", "/", "/").unwrap()]
    );
}

#[fasync::run_singlethreaded(test)]
async fn test_add_src_with_ipv6_id() {
    let env = TestEnv::new();

    let source = SourceConfigBuilder::new("http://[fe80::1122:3344]:8083")
        .repo_url("http://[fe80::1122:3344]:8083")
        .add_root_key(ROOT_KEY_1)
        .build();

    let repo = RepositoryConfigBuilder::new(
        "fuchsia-pkg://http____fe80__1122_3344__8083".parse().unwrap(),
    )
    .add_root_key(RepositoryKey::Ed25519(hex::decode(ROOT_KEY_1).unwrap()))
    .add_mirror(
        MirrorConfigBuilder::new("http://[fe80::1122:3344]:8083".parse::<Uri>().unwrap()).unwrap(),
    )
    .build();

    env.run_amberctl_add_src(source).await;

    assert_eq!(env.resolver_list_repos().await, vec![repo]);
    assert_eq!(
        env.rewrite_engine_list_rules().await,
        vec![Rule::new("fuchsia.com", "http____fe80__1122_3344__8083", "/", "/").unwrap()]
    );
}

#[fasync::run_singlethreaded(test)]
async fn test_add_src_disables_other_sources() {
    let env = TestEnv::new();

    let configs = SourceConfigGenerator::new("testgen").take(3).collect::<Vec<_>>();

    for (config, _) in &configs {
        env.run_amberctl_add_src(config.clone().build().into()).await;
    }

    env.run_amberctl_add_static_src("test.json").await;

    let mut repo_configs = vec![make_test_repo_config()];
    for (_, repo_config) in configs {
        repo_configs.push(repo_config.build());
    }

    assert_eq!(env.resolver_list_repos().await, repo_configs);
    assert_eq!(
        env.rewrite_engine_list_rules().await,
        vec![Rule::new("fuchsia.com", "test", "/", "/").unwrap()]
    );
}

#[fasync::run_singlethreaded(test)]
async fn test_add_repo_retains_existing_state() {
    let env = TestEnv::new();

    // start with an existing source.
    env.run_amberctl_add_static_src("test.json").await;

    // add a repo.
    let source = SourceConfigBuilder::new("devhost")
        .repo_url("http://10.0.0.1:8083")
        .add_root_key(ROOT_KEY_1)
        .build();
    let repo = RepositoryConfigBuilder::new("fuchsia-pkg://devhost".parse().unwrap())
        .add_root_key(RepositoryKey::Ed25519(hex::decode(ROOT_KEY_1).unwrap()))
        .add_mirror(
            MirrorConfigBuilder::new("http://10.0.0.1:8083".parse::<Uri>().unwrap()).unwrap(),
        )
        .build();
    env.run_amberctl_add_repo_config(source).await;

    // ensure adding the repo didn't remove state configured when adding the source.
    assert_eq!(env.resolver_list_repos().await, vec![repo, make_test_repo_config()]);
    assert_eq!(
        env.rewrite_engine_list_rules().await,
        vec![Rule::new("fuchsia.com", "test", "/", "/").unwrap()]
    );
}

#[fasync::run_singlethreaded(test)]
async fn test_rm_src() {
    let env = TestEnv::new();

    let cfg_a = SourceConfigBuilder::new("http://[fe80::1122:3344]:8083")
        .repo_url("http://example.com/a")
        .rate_period(60)
        .add_root_key(ROOT_KEY_1)
        .build();

    let cfg_b = SourceConfigBuilder::new("b")
        .repo_url("http://example.com/b")
        .rate_period(60)
        .add_root_key(ROOT_KEY_2)
        .build();

    env.run_amberctl_add_src(cfg_a.into()).await;
    env.run_amberctl_add_src(cfg_b.into()).await;

    env.run_amberctl(&["rm_src", "-n", "http://[fe80::1122:3344]:8083"]).await;
    assert_eq!(
        env.resolver_list_repos().await,
        vec![RepositoryConfigBuilder::new("fuchsia-pkg://b".parse().unwrap())
            .add_root_key(RepositoryKey::Ed25519(hex::decode(ROOT_KEY_2).unwrap()))
            .add_mirror(
                MirrorConfigBuilder::new("http://example.com/b".parse::<Uri>().unwrap()).unwrap()
            )
            .build()]
    );
    // rm_src removes all rules, so no source remains enabled.
    assert_eq!(env.rewrite_engine_list_rules().await, vec![]);

    env.run_amberctl(&["rm_src", "-n", "b"]).await;
    assert_eq!(env.resolver_list_repos().await, vec![]);
    assert_eq!(env.rewrite_engine_list_rules().await, vec![]);
}

#[fasync::run_singlethreaded(test)]
async fn test_enable_src() {
    let env = TestEnv::new();

    let source = SourceConfigBuilder::new("test")
        .repo_url("http://example.com")
        .enabled(false)
        .add_root_key(ROOT_KEY_1)
        .build();

    let repo = RepositoryConfigBuilder::new("fuchsia-pkg://test".parse().unwrap())
        .add_root_key(RepositoryKey::Ed25519(hex::decode(ROOT_KEY_1).unwrap()))
        .add_mirror(MirrorConfigBuilder::new("http://example.com".parse::<Uri>().unwrap()).unwrap())
        .build();

    env.run_amberctl_add_src(source.into()).await;

    assert_eq!(env.resolver_list_repos().await, vec![repo.clone()]);
    // Adding a disabled source does not add a rewrite rule for it.
    assert_eq!(env.rewrite_engine_list_rules().await, vec![]);

    env.run_amberctl(&["enable_src", "-n", "test"]).await;

    assert_eq!(env.resolver_list_repos().await, vec![repo]);
    assert_eq!(
        env.rewrite_engine_list_rules().await,
        vec![Rule::new("fuchsia.com", "test", "/", "/").unwrap()]
    );
}

#[fasync::run_singlethreaded(test)]
async fn test_enable_src_disables_other_sources() {
    let env = TestEnv::new();

    // add some enabled sources
    let mut gen = SourceConfigGenerator::new("test");
    let configs = gen.by_ref().take(3).collect::<Vec<_>>();
    for (config, _) in &configs {
        env.run_amberctl_add_src(config.clone().build().into()).await;
    }

    // add an initially disabled source
    let (config, repo) = gen.next().unwrap();
    let c = config.enabled(false).build();
    let id = c.id().to_owned();
    env.run_amberctl_add_src(c.into()).await;

    // verify the previously added source is still the enabled one
    assert_eq!(
        env.rewrite_engine_list_rules().await,
        vec![Rule::new("fuchsia.com", "test02", "/", "/").unwrap()]
    );

    // enable the new source source and verify the repos and rules
    let args = ["enable_src", "-n", &id];
    env.run_amberctl(&args).await;

    let mut repo_configs = vec![];
    for (_, repo_config) in configs {
        repo_configs.push(repo_config.build());
    }
    repo_configs.push(repo.build());
    assert_eq!(env.resolver_list_repos().await, repo_configs);
    assert_eq!(
        env.rewrite_engine_list_rules().await,
        vec![Rule::new("fuchsia.com", id, "/", "/").unwrap()]
    );
}

#[fasync::run_singlethreaded(test)]
async fn test_disable_src_disables_all_sources() {
    let env = TestEnv::new();

    env.run_amberctl_add_src(
        SourceConfigBuilder::new("a")
            .repo_url("http://example.com/a")
            .rate_period(60)
            .add_root_key(ROOT_KEY_1)
            .build()
            .into(),
    )
    .await;
    env.run_amberctl_add_src(
        SourceConfigBuilder::new("b")
            .repo_url("http://example.com/b")
            .rate_period(60)
            .add_root_key(ROOT_KEY_2)
            .build()
            .into(),
    )
    .await;

    env.run_amberctl(&["disable_src"]).await;

    assert_eq!(
        env.resolver_list_repos().await,
        vec![
            RepositoryConfigBuilder::new("fuchsia-pkg://a".parse().unwrap())
                .add_root_key(RepositoryKey::Ed25519(hex::decode(ROOT_KEY_1).unwrap()))
                .add_mirror(
                    MirrorConfigBuilder::new("http://example.com/a".parse::<Uri>().unwrap())
                        .unwrap()
                )
                .build(),
            RepositoryConfigBuilder::new("fuchsia-pkg://b".parse().unwrap())
                .add_root_key(RepositoryKey::Ed25519(hex::decode(ROOT_KEY_2).unwrap()))
                .add_mirror(
                    MirrorConfigBuilder::new("http://example.com/b".parse::<Uri>().unwrap())
                        .unwrap()
                )
                .build(),
        ]
    );
    // disabling any source clears all rewrite rules.
    assert_eq!(env.rewrite_engine_list_rules().await, vec![]);
}

async fn test_system_update_impl(
    check_now_result: fidl_fuchsia_update::ManagerCheckNowResult,
) -> Output {
    // skip using TestEnv because we don't need to start pkg_resolver here.
    let mut fs = ServiceFs::new();

    let update_manager = Arc::new(MockUpdateManager::new_with_check_now_result(check_now_result));
    let update_manager_clone = update_manager.clone();
    fs.add_fidl_service(move |stream| {
        let update_manager_clone = update_manager_clone.clone();
        fasync::Task::spawn(
            update_manager_clone
                .run(stream)
                .unwrap_or_else(|e| panic!("error running mock update manager: {:?}", e)),
        )
        .detach()
    });

    let env = fs
        .create_salted_nested_environment("amberctl_env")
        .expect("nested environment to create successfully");
    fasync::Task::spawn(fs.collect()).detach();

    let output = amberctl()
        .arg("system_update")
        .output(env.launcher())
        .expect("amberctl to launch")
        .await
        .expect("amberctl to run");
    assert_eq!(*update_manager.called.lock(), 1);
    output
}

fn assert_stdout(output: &Output, stdout: &str, exit_code: i64) {
    assert_eq!(output.exit_status.reason(), fidl_fuchsia_sys::TerminationReason::Exited);
    assert_eq!(output.exit_status.code(), exit_code);
    assert_eq!(std::str::from_utf8(&output.stdout).unwrap(), stdout);
    assert_eq!(std::str::from_utf8(&output.stderr).unwrap(), "");
}

#[fasync::run_singlethreaded(test)]
async fn test_system_update_start_update() {
    let output = test_system_update_impl(Ok(())).await;
    assert_stdout(&output, "triggered a system update check\n", 0);
}

#[fasync::run_singlethreaded(test)]
async fn test_system_update_already_in_progress() {
    let output =
        test_system_update_impl(Err(fidl_fuchsia_update::CheckNotStartedReason::AlreadyInProgress))
            .await;
    assert_stdout(&output, "system update check already in progress\n", 0);
}

#[fasync::run_singlethreaded(test)]
async fn test_system_update_throttled() {
    let output =
        test_system_update_impl(Err(fidl_fuchsia_update::CheckNotStartedReason::Throttled)).await;
    assert_stdout(&output, "system update check failed: Throttled\n", 1);
}

#[fasync::run_singlethreaded(test)]
async fn test_gc() {
    // skip using TestEnv because we don't need to start pkg_resolver here.
    let mut fs = ServiceFs::new();

    let space_manager = Arc::new(MockSpaceManager::new());
    let space_manager_clone = Arc::clone(&space_manager);
    fs.add_fidl_service(move |stream| {
        let space_manager_clone = Arc::clone(&space_manager_clone);
        fasync::Task::spawn(
            space_manager_clone
                .run(stream)
                .unwrap_or_else(|e| panic!("error running mock space manager: {:?}", e)),
        )
        .detach()
    });

    let env = fs
        .create_salted_nested_environment("amberctl_env")
        .expect("nested environment to create successfully");
    fasync::Task::spawn(fs.collect()).detach();

    amberctl()
        .arg("gc")
        .output(env.launcher())
        .expect("amberctl to launch")
        .await
        .expect("amberctl to run")
        .ok()
        .expect("amberctl to succeed");

    assert_eq!(*space_manager.called.lock(), 1);
}
