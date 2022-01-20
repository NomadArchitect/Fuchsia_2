// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::{self, Context},
    fidl::endpoints::{create_proxy, ClientEnd, Proxy},
    fidl_fuchsia_component_decl as fdecl,
    fidl_fuchsia_io::{self as fio, DirectoryMarker, DirectoryProxy},
    fidl_fuchsia_mem as fmem,
    fidl_fuchsia_pkg::{PackageResolverMarker, PackageResolverProxy},
    fidl_fuchsia_sys2::{self as fsys, ComponentResolverRequest, ComponentResolverRequestStream},
    fuchsia_component::{client::connect_to_protocol, server::ServiceFs},
    fuchsia_url::{
        errors::{ParseError as PkgUrlParseError, ResourcePathError},
        pkg_url::PkgUrl,
    },
    fuchsia_zircon::Status,
    futures::prelude::*,
    log::*,
    thiserror::Error,
};

#[fuchsia::component]
async fn main() -> anyhow::Result<()> {
    info!("started");
    let mut service_fs = ServiceFs::new_local();
    service_fs.dir("svc").add_fidl_service(|stream: ComponentResolverRequestStream| stream);
    service_fs.take_and_serve_directory_handle().context("failed to serve outgoing namespace")?;
    service_fs
        .for_each_concurrent(None, |stream| async move {
            match serve(stream).await {
                Ok(()) => {}
                Err(err) => error!("failed to serve resolve request: {:?}", err),
            }
        })
        .await;

    Ok(())
}

async fn serve(mut stream: ComponentResolverRequestStream) -> anyhow::Result<()> {
    let package_resolver = connect_to_protocol::<PackageResolverMarker>()
        .context("failed to connect to PackageResolver service")?;
    while let Some(ComponentResolverRequest::Resolve { component_url, responder }) =
        stream.try_next().await.context("failed to read request from FIDL stream")?
    {
        match resolve_component(&component_url, &package_resolver).await {
            Ok(result) => responder.send(&mut Ok(result)),
            Err(err) => {
                info!("failed to resolve component URL {}: {}", &component_url, &err);
                responder.send(&mut Err(err.into()))
            }
        }
        .context("failed sending response")?;
    }
    Ok(())
}

async fn resolve_component(
    component_url: &str,
    package_resolver: &PackageResolverProxy,
) -> Result<fsys::Component, ResolverError> {
    let package_url = PkgUrl::parse(component_url)?;
    let cm_path = package_url.resource().ok_or_else(|| {
        ResolverError::InvalidUrl(PkgUrlParseError::InvalidResourcePath(
            ResourcePathError::PathIsEmpty,
        ))
    })?;
    let package_dir = resolve_package(&package_url, package_resolver).await?;

    // Read the component manifest (.cm file) from the package directory.
    let data =
        get_data_from_package_path(&package_dir, cm_path, ResolverError::ManifestNotFound).await?;
    let raw_bytes = raw_bytes_from_data(&data)?;

    let decl: fdecl::Component = fidl::encoding::decode_persistent(&raw_bytes[..])
        .map_err(ResolverError::ParsingManifest)?;
    let config_values = if let Some(config_decl) = decl.config.as_ref() {
        // if we have a config declaration, we need to read the value file from the package dir
        let strategy =
            config_decl.value_source.as_ref().ok_or(ResolverError::MissingConfigSource)?;
        let config_path = match strategy {
            fdecl::ConfigValueSource::PackagePath(path) => path,
            other => return Err(ResolverError::UnsupportedConfigStrategy(other.to_owned())),
        };
        Some(
            get_data_from_package_path(
                &package_dir,
                &config_path,
                ResolverError::ConfigValuesNotFound,
            )
            .await?,
        )
    } else {
        None
    };

    let package_dir = ClientEnd::new(
        package_dir.into_channel().expect("could not convert proxy to channel").into_zx_channel(),
    );
    Ok(fsys::Component {
        resolved_url: Some(component_url.into()),
        decl: Some(data),
        package: Some(fsys::Package {
            package_url: Some(package_url.root_url().to_string()),
            package_dir: Some(package_dir),
            ..fsys::Package::EMPTY
        }),
        config_values,
        ..fsys::Component::EMPTY
    })
}

async fn get_data_from_package_path(
    package_dir: &DirectoryProxy,
    path: &str,
    map_not_found_err: impl FnOnce(io_util::node::OpenError) -> ResolverError,
) -> Result<fmem::Data, ResolverError> {
    let file = io_util::directory::open_file(&package_dir, path, fio::OPEN_RIGHT_READABLE)
        .await
        .map_err(map_not_found_err)?;

    let (status, buffer) =
        file.get_buffer(fio::VMO_FLAG_READ).await.map_err(ResolverError::IoError)?;
    Status::ok(status).map_err(ResolverError::VmoFailure)?;

    Ok(match buffer {
        Some(buffer) => fmem::Data::Buffer(*buffer),
        None => fmem::Data::Bytes(
            io_util::file::read(&file)
                .await
                .map_err(|source| ResolverError::ReadFile { source, path: path.to_owned() })?,
        ),
    })
}

fn raw_bytes_from_data(data: &fmem::Data) -> Result<Vec<u8>, ResolverError> {
    Ok(match data {
        fmem::Data::Buffer(buf) => {
            let size = buf.size as usize;
            let mut raw_bytes = Vec::with_capacity(size);
            raw_bytes.resize(size, 0);
            buf.vmo.read(&mut raw_bytes, 0).map_err(ResolverError::VmoFailure)?;
            raw_bytes
        }
        fmem::Data::Bytes(b) => b.clone(),
        _ => return Err(ResolverError::UnrecognizedDataVariant),
    })
}

async fn resolve_package(
    package_url: &PkgUrl,
    package_resolver: &PackageResolverProxy,
) -> Result<DirectoryProxy, ResolverError> {
    let package_url = package_url.root_url();
    let selectors = Vec::new();
    let (proxy, server_end) =
        create_proxy::<DirectoryMarker>().expect("failed to create channel pair");
    package_resolver
        .resolve(&package_url.to_string(), &mut selectors.into_iter(), server_end)
        .await
        .map_err(ResolverError::IoError)?
        .map_err(|err| match err {
            fidl_fuchsia_pkg::ResolveError::PackageNotFound => ResolverError::PackageNotFound,
            fidl_fuchsia_pkg::ResolveError::RepoNotFound
            | fidl_fuchsia_pkg::ResolveError::UnavailableBlob
            | fidl_fuchsia_pkg::ResolveError::UnavailableRepoMetadata => ResolverError::Unavailable,
            fidl_fuchsia_pkg::ResolveError::NoSpace => ResolverError::NoSpace,
            _ => ResolverError::Internal,
        })?;
    Ok(proxy)
}

#[derive(Error, Debug)]
enum ResolverError {
    #[error("an unexpected error ocurred")]
    Internal,
    #[error("invalid component URL: {}", .0)]
    InvalidUrl(#[from] PkgUrlParseError),
    #[error("manifest not found: {_0}")]
    ManifestNotFound(#[source] io_util::node::OpenError),
    #[error("config values not found: {_0}")]
    ConfigValuesNotFound(#[source] io_util::node::OpenError),
    #[error("package not found")]
    PackageNotFound,
    #[error("error reading {path}: {source}")]
    ReadFile {
        path: String,
        #[source]
        source: io_util::file::ReadError,
    },
    #[error("IO error: {}", .0)]
    IoError(#[source] fidl::Error),
    #[error("failed to get manifest VMO: {}", .0)]
    VmoFailure(#[source] Status),
    #[error("failed to parse compiled manifest to check for config: {_0}")]
    ParsingManifest(#[source] fidl::Error),
    #[error("insufficient space to store package")]
    NoSpace,
    #[error("the component's package is temporarily unavailable")]
    Unavailable,
    #[error("encountered unrecognized fuchsia.mem.Data variant")]
    UnrecognizedDataVariant,
    #[error("component has config fields but does not have a config value lookup strategy")]
    MissingConfigSource,
    #[error("unsupported config value resolution strategy {_0:?}")]
    UnsupportedConfigStrategy(fdecl::ConfigValueSource),
}

impl From<ResolverError> for fsys::ResolverError {
    fn from(err: ResolverError) -> fsys::ResolverError {
        match err {
            ResolverError::Internal => fsys::ResolverError::Internal,
            ResolverError::InvalidUrl(_) => fsys::ResolverError::InvalidArgs,
            ResolverError::ManifestNotFound { .. } => fsys::ResolverError::ManifestNotFound,
            ResolverError::ConfigValuesNotFound { .. } => fsys::ResolverError::ConfigValuesNotFound,
            ResolverError::PackageNotFound => fsys::ResolverError::PackageNotFound,
            ResolverError::ReadFile { .. }
            | ResolverError::VmoFailure(_)
            | ResolverError::IoError(_)
            | ResolverError::UnrecognizedDataVariant => fsys::ResolverError::Io,
            ResolverError::NoSpace => fsys::ResolverError::NoSpace,
            ResolverError::Unavailable => fsys::ResolverError::ResourceUnavailable,
            ResolverError::ParsingManifest(..)
            | ResolverError::MissingConfigSource
            | ResolverError::UnsupportedConfigStrategy(..) => fsys::ResolverError::InvalidManifest,
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        anyhow::Error,
        assert_matches::assert_matches,
        fidl::{encoding::encode_persistent, endpoints::ServerEnd},
        fidl_fuchsia_component_config as fconfig, fidl_fuchsia_component_decl as fdecl,
        fidl_fuchsia_mem,
        fidl_fuchsia_pkg::{PackageResolverRequest, PackageResolverRequestStream},
        fidl_fuchsia_sys2::{self as fsys, ComponentResolverMarker},
        fuchsia_async as fasync,
        fuchsia_component::server as fserver,
        fuchsia_component_test::{
            mock::MockHandles, ChildOptions, RealmBuilder, RouteBuilder, RouteEndpoint,
        },
        fuchsia_zircon::Vmo,
        futures::{channel::mpsc, join, lock::Mutex},
        std::{boxed::Box, sync::Arc},
        vfs::{
            directory::entry::DirectoryEntry,
            execution_scope::ExecutionScope,
            file::{vmo::asynchronous::read_only_static, vmo::asynchronous::NewVmo},
            path::Path,
            pseudo_directory,
        },
    };

    async fn mock_pkg_resolver(
        trigger: Arc<Mutex<Option<mpsc::Sender<Result<(), Error>>>>>,
        handles: MockHandles,
    ) -> Result<(), Error> {
        let mut fs = fserver::ServiceFs::new();
        fs.dir("svc").add_fidl_service(
            move |mut req_stream: fidl_fuchsia_pkg::PackageResolverRequestStream| {
                let tx = trigger.clone();
                fasync::Task::local(async move {
                    while let Some(fidl_fuchsia_pkg::PackageResolverRequest::Resolve {
                        responder,
                        ..
                    }) =
                        req_stream.try_next().await.expect("Serving package resolver stream failed")
                    {
                        responder
                            .send(&mut Err(fidl_fuchsia_pkg::ResolveError::PackageNotFound))
                            .expect("failed sending package resolver response to client");

                        {
                            let mut lock = tx.lock().await;
                            let mut c = lock.take().unwrap();
                            c.send(Ok(())).await.expect("failed sending oneshot to test");
                            lock.replace(c);
                        }
                    }
                })
                .detach();
            },
        );

        fs.serve_connection(handles.outgoing_dir.into_channel())?;
        fs.collect::<()>().await;
        Ok(())
    }

    async fn package_requester(
        trigger: Arc<Mutex<Option<mpsc::Sender<Result<(), Error>>>>>,
        url: String,
        handles: MockHandles,
    ) -> Result<(), Error> {
        let resolver_proxy = handles.connect_to_service::<ComponentResolverMarker>()?;
        let _ = resolver_proxy.resolve(&url).await?;
        fasync::Task::local(async move {
            let mut lock = trigger.lock().await;
            let mut c = lock.take().unwrap();
            c.send(Ok(())).await.expect("sending oneshot from package requester failed");
            lock.replace(c);
        })
        .detach();
        Ok(())
    }

    #[fasync::run_singlethreaded(test)]
    // Test that the default configuration which forwards requests to
    // PackageResolver works properly.
    async fn test_using_pkg_resolver() {
        let (sender, mut receiver) = mpsc::channel(2);
        let tx = Arc::new(Mutex::new(Some(sender)));
        let resolver_url =
            "fuchsia-pkg://fuchsia.com/universe-resolver-unittests#meta/universe-resolver-for-test.cm"
                .to_string();
        let requested_url =
            "fuchsia-pkg://fuchsia.com/test-pkg-request#meta/test-component.cm".to_string();
        let builder = RealmBuilder::new().await.expect("Failed to create test realm builder");
        builder
            .add_child("universe-resolver", resolver_url, ChildOptions::new())
            .await
            .expect("Failed add universe-resolver to test topology")
            .add_mock_child(
                "fake-pkg-resolver",
                {
                    let sender = tx.clone();
                    move |mock_handles: MockHandles| {
                        Box::pin(mock_pkg_resolver(sender.clone(), mock_handles))
                    }
                },
                ChildOptions::new(),
            )
            .await
            .expect("Failed adding base resolver mock")
            .add_mock_child(
                "requesting-component",
                {
                    let sender = tx.clone();
                    move |mock_handles: MockHandles| {
                        Box::pin(package_requester(
                            sender.clone(),
                            requested_url.clone(),
                            mock_handles,
                        ))
                    }
                },
                ChildOptions::new().eager(),
            )
            .await
            .expect("Failed adding mock request component")
            .add_route(
                RouteBuilder::protocol("fuchsia.pkg.PackageResolver")
                    .source(RouteEndpoint::component("fake-pkg-resolver"))
                    .targets(vec![RouteEndpoint::component("universe-resolver")]),
            )
            .await
            .expect("Failed adding resolver route from fake-base-resolver to universe-resolver")
            .add_route(
                RouteBuilder::protocol("fuchsia.sys2.ComponentResolver")
                    .source(RouteEndpoint::component("universe-resolver"))
                    .targets(vec![RouteEndpoint::component("requesting-component")]),
            )
            .await
            .expect("Failed adding resolver route from universe-resolver to requesting-component")
            .add_route(
                RouteBuilder::protocol("fuchsia.logger.LogSink")
                    .source(RouteEndpoint::AboveRoot)
                    .targets(vec![
                        RouteEndpoint::component("universe-resolver"),
                        RouteEndpoint::component("fake-pkg-resolver"),
                        RouteEndpoint::component("requesting-component"),
                    ]),
            )
            .await
            .expect("Failed adding LogSink route to test components");
        let _test_topo = builder.build().await.unwrap();

        receiver.next().await.expect("Unexpected error waiting for response").expect("error sent");
        receiver.next().await.expect("Unexpected error waiting for response").expect("error sent");
    }

    #[fuchsia::test]
    async fn resolve_package_succeeds() {
        let (proxy, mut server) =
            fidl::endpoints::create_proxy_and_stream::<PackageResolverMarker>().unwrap();
        let server = async move {
            let fs = pseudo_directory! {
                "test_file" => read_only_static(b"foo"),
            };
            while let Some(request) = server.try_next().await.unwrap() {
                match request {
                    PackageResolverRequest::Resolve { package_url, selectors, dir, responder } => {
                        assert_eq!(
                            package_url, "fuchsia-pkg://fuchsia.com/test",
                            "unexpected package URL"
                        );
                        assert!(
                            selectors.is_empty(),
                            "Call to Resolve should not contain any selectors"
                        );
                        fs.clone().open(
                            ExecutionScope::new(),
                            fio::OPEN_RIGHT_READABLE,
                            fio::MODE_TYPE_DIRECTORY,
                            Path::dot(),
                            ServerEnd::new(dir.into_channel()),
                        );
                        responder.send(&mut Ok(())).unwrap();
                    }
                    _ => panic!("unexpected API call"),
                }
            }
        };
        let client = async move {
            let result = resolve_package(
                &PkgUrl::new_package("fuchsia.com".into(), "/test".into(), None).unwrap(),
                &proxy,
            )
            .await;
            let directory = result.expect("package resolver failed unexpectedly");
            let file =
                io_util::directory::open_file(&directory, "test_file", fio::OPEN_RIGHT_READABLE)
                    .await
                    .expect("failed to open 'test_file' from package resolver directory");
            let contents = io_util::file::read(&file)
                .await
                .expect("failed to read 'test_file' contents from package resolver directory");
            assert_eq!(&contents, b"foo");
        };
        join!(server, client);
    }

    #[fuchsia::test]
    async fn resolve_component_succeeds() {
        let (proxy, mut server) =
            fidl::endpoints::create_proxy_and_stream::<PackageResolverMarker>().unwrap();
        let server = async move {
            let cm_bytes = encode_persistent(&mut fdecl::Component::EMPTY.clone())
                .expect("failed to encode ComponentDecl FIDL");
            let fs = pseudo_directory! {
                "meta" => pseudo_directory!{
                    "test.cm" => read_only_static(cm_bytes),
                },
            };
            while let Some(request) = server.try_next().await.unwrap() {
                match request {
                    PackageResolverRequest::Resolve { package_url, selectors, dir, responder } => {
                        assert_eq!(
                            package_url, "fuchsia-pkg://fuchsia.com/test",
                            "unexpected package URL"
                        );
                        assert!(
                            selectors.is_empty(),
                            "Call to Resolve should not contain any selectors"
                        );
                        fs.clone().open(
                            ExecutionScope::new(),
                            fio::OPEN_RIGHT_READABLE,
                            fio::MODE_TYPE_DIRECTORY,
                            Path::dot(),
                            ServerEnd::new(dir.into_channel()),
                        );
                        responder.send(&mut Ok(())).unwrap();
                    }
                    _ => panic!("unexpected API call"),
                }
            }
        };
        let client = async move {
            assert_matches!(
                resolve_component("fuchsia-pkg://fuchsia.com/test#meta/test.cm", &proxy).await,
                Ok(fsys::Component {
                    decl: Some(fidl_fuchsia_mem::Data::Buffer(fidl_fuchsia_mem::Buffer { .. })),
                    ..
                })
            );
        };
        join!(server, client);
    }

    #[fuchsia::test]
    async fn resolve_component_succeeds_with_hash() {
        let (proxy, mut server) =
            fidl::endpoints::create_proxy_and_stream::<PackageResolverMarker>().unwrap();
        let server = async move {
            let cm_bytes = encode_persistent(&mut fdecl::Component::EMPTY.clone())
                .expect("failed to encode ComponentDecl FIDL");
            let fs = pseudo_directory! {
                "meta" => pseudo_directory!{
                    "test.cm" => read_only_static(cm_bytes),
                },
            };
            while let Some(request) = server.try_next().await.unwrap() {
                match request {
                    PackageResolverRequest::Resolve { package_url, selectors, dir, responder } => {
                        assert_eq!(package_url, "fuchsia-pkg://fuchsia.com/test?hash=9e3a3f63c018e2a4db0ef93903a87714f036e3e8ff982a7a2020eca86cc4677c", "unexpected package URL");
                        assert!(
                            selectors.is_empty(),
                            "Call to Resolve should not contain any selectors"
                        );
                        fs.clone().open(
                            ExecutionScope::new(),
                            fio::OPEN_RIGHT_READABLE,
                            fio::MODE_TYPE_DIRECTORY,
                            Path::dot(),
                            ServerEnd::new(dir.into_channel()),
                        );
                        responder.send(&mut Ok(())).unwrap();
                    }
                    _ => panic!("unexpected API call"),
                }
            }
        };
        let client = async move {
            assert_matches!(resolve_component("fuchsia-pkg://fuchsia.com/test?hash=9e3a3f63c018e2a4db0ef93903a87714f036e3e8ff982a7a2020eca86cc4677c#meta/test.cm", &proxy).await, Ok(_));
        };
        join!(server, client);
    }

    #[fuchsia::test]
    async fn resolve_component_succeeds_with_vmo_manifest() {
        let (proxy, mut server) =
            fidl::endpoints::create_proxy_and_stream::<PackageResolverMarker>().unwrap();
        let server = async move {
            let fs = pseudo_directory! {
                "meta" => pseudo_directory!{
                    "test.cm" => vfs::file::vmo::read_only(|| async move {
                        let cm_bytes = encode_persistent(&mut fdecl::Component::EMPTY.clone())
                            .expect("failed to encode ComponentDecl FIDL");
                        let capacity = cm_bytes.len() as u64;
                        let vmo = Vmo::create(capacity)?;
                        vmo.write(&cm_bytes, 0).expect("failed to write manifest bytes to vmo");
                        Ok(NewVmo { vmo, size: capacity, capacity })
                    }),
                },
            };
            while let Some(request) = server.try_next().await.unwrap() {
                match request {
                    PackageResolverRequest::Resolve { dir, responder, .. } => {
                        fs.clone().open(
                            ExecutionScope::new(),
                            fio::OPEN_RIGHT_READABLE,
                            fio::MODE_TYPE_DIRECTORY,
                            Path::dot(),
                            ServerEnd::new(dir.into_channel()),
                        );
                        responder.send(&mut Ok(())).unwrap();
                    }
                    _ => panic!("unexpected API call"),
                }
            }
        };
        let client = async move {
            assert_matches!(
                resolve_component("fuchsia-pkg://fuchsia.com/test#meta/test.cm", &proxy).await,
                Ok(fsys::Component { decl: Some(fmem::Data::Buffer(_)), .. })
            );
        };
        join!(server, client);
    }

    #[fuchsia::test]
    async fn resolve_component_fails_bad_connection() {
        let (proxy, server) =
            fidl::endpoints::create_proxy_and_stream::<PackageResolverMarker>().unwrap();
        drop(server);
        assert_matches!(
            resolve_component("fuchsia-pkg://fuchsia.com/test#meta/test.cm", &proxy).await,
            Err(ResolverError::IoError(_))
        );
    }

    #[fuchsia::test]
    async fn resolve_component_fails_with_package_resolver_failure() {
        let (proxy, mut server) =
            fidl::endpoints::create_proxy_and_stream::<PackageResolverMarker>().unwrap();
        let server = async move {
            while let Some(request) = server.try_next().await.unwrap() {
                match request {
                    PackageResolverRequest::Resolve { responder, .. } => {
                        responder.send(&mut Err(fidl_fuchsia_pkg::ResolveError::NoSpace)).unwrap();
                    }
                    _ => panic!("unexpected API call"),
                }
            }
        };
        let client = async move {
            assert_matches!(
                resolve_component("fuchsia-pkg://fuchsia.com/test#meta/test.cm", &proxy).await,
                Err(ResolverError::NoSpace)
            );
        };
        join!(server, client);
    }

    #[fuchsia::test]
    async fn resolve_component_fails_with_component_not_found() {
        let (proxy, mut server) =
            fidl::endpoints::create_proxy_and_stream::<PackageResolverMarker>().unwrap();
        let server = async move {
            let fs = pseudo_directory! {};
            while let Some(request) = server.try_next().await.unwrap() {
                match request {
                    PackageResolverRequest::Resolve { dir, responder, .. } => {
                        fs.clone().open(
                            ExecutionScope::new(),
                            fio::OPEN_RIGHT_READABLE,
                            fio::MODE_TYPE_DIRECTORY,
                            Path::dot(),
                            ServerEnd::new(dir.into_channel()),
                        );
                        responder.send(&mut Ok(())).unwrap();
                    }
                    _ => panic!("unexpected API call"),
                }
            }
        };
        let client = async move {
            assert_matches!(
                resolve_component("fuchsia-pkg://fuchsia.com/test#meta/test.cm", &proxy).await,
                Err(ResolverError::ManifestNotFound(..))
            );
        };
        join!(server, client);
    }

    fn spawn_pkg_resolver(
        fs: Arc<impl vfs::directory::entry::DirectoryEntry>,
        mut server: PackageResolverRequestStream,
    ) -> fasync::Task<()> {
        fasync::Task::spawn(async move {
            while let Some(request) = server.try_next().await.unwrap() {
                match request {
                    PackageResolverRequest::Resolve { package_url, selectors, dir, responder } => {
                        assert_eq!(
                            package_url, "fuchsia-pkg://fuchsia.com/test",
                            "unexpected package URL"
                        );
                        assert!(
                            selectors.is_empty(),
                            "Call to Resolve should not contain any selectors"
                        );
                        fs.clone().open(
                            ExecutionScope::new(),
                            fio::OPEN_RIGHT_READABLE,
                            fio::MODE_TYPE_DIRECTORY,
                            Path::dot(),
                            ServerEnd::new(dir.into_channel()),
                        );
                        responder.send(&mut Ok(())).unwrap();
                    }
                    _ => panic!("unexpected API call"),
                }
            }
        })
    }

    #[fuchsia::test]
    async fn resolve_component_succeeds_with_config() {
        let (proxy, server) =
            fidl::endpoints::create_proxy_and_stream::<PackageResolverMarker>().unwrap();
        let cm_bytes = encode_persistent(&mut fdecl::Component {
            config: Some(fdecl::Config {
                value_source: Some(fdecl::ConfigValueSource::PackagePath(
                    "meta/test_with_config.cvf".to_string(),
                )),
                ..fdecl::Config::EMPTY
            }),
            ..fdecl::Component::EMPTY
        })
        .expect("failed to encode ComponentDecl FIDL");
        let cvf_bytes =
            encode_persistent(&mut fconfig::ValuesData { ..fconfig::ValuesData::EMPTY })
                .expect("failed to encode ValuesData FIDL");
        let fs = pseudo_directory! {
            "meta" => pseudo_directory! {
                "test_with_config.cm" => read_only_static(cm_bytes),
                "test_with_config.cvf" => read_only_static(cvf_bytes),
            },
        };
        let _pkg_resolver = spawn_pkg_resolver(fs, server);
        assert_matches!(
            resolve_component("fuchsia-pkg://fuchsia.com/test#meta/test_with_config.cm", &proxy)
                .await
                .unwrap(),
            fsys::Component {
                decl: Some(fidl_fuchsia_mem::Data::Buffer(fidl_fuchsia_mem::Buffer { .. })),
                config_values: Some(fidl_fuchsia_mem::Data::Buffer(
                    fidl_fuchsia_mem::Buffer { .. }
                )),
                ..
            }
        );
    }

    #[fuchsia::test]
    async fn resolve_component_fails_missing_config_value_file() {
        let (proxy, server) =
            fidl::endpoints::create_proxy_and_stream::<PackageResolverMarker>().unwrap();
        let cm_bytes = encode_persistent(&mut fdecl::Component {
            config: Some(fdecl::Config {
                value_source: Some(fdecl::ConfigValueSource::PackagePath(
                    "meta/test_with_config.cvf".to_string(),
                )),
                ..fdecl::Config::EMPTY
            }),
            ..fdecl::Component::EMPTY
        })
        .expect("failed to encode ComponentDecl FIDL");
        let fs = pseudo_directory! {
            "meta" => pseudo_directory! {
                "test_with_config.cm" => read_only_static(cm_bytes),
            },
        };
        let _pkg_resolver = spawn_pkg_resolver(fs, server);
        assert_matches!(
            resolve_component("fuchsia-pkg://fuchsia.com/test#meta/test_with_config.cm", &proxy)
                .await
                .unwrap_err(),
            ResolverError::ConfigValuesNotFound(_)
        );
    }

    #[fuchsia::test]
    async fn resolve_component_fails_bad_config_strategy() {
        let (proxy, server) =
            fidl::endpoints::create_proxy_and_stream::<PackageResolverMarker>().unwrap();
        let cm_bytes = encode_persistent(&mut fdecl::Component {
            config: Some(fdecl::Config { ..fdecl::Config::EMPTY }),
            ..fdecl::Component::EMPTY
        })
        .expect("failed to encode ComponentDecl FIDL");
        let cvf_bytes =
            encode_persistent(&mut fconfig::ValuesData { ..fconfig::ValuesData::EMPTY })
                .expect("failed to encode ValuesData FIDL");
        let fs = pseudo_directory! {
            "meta" => pseudo_directory! {
                "test_with_config.cm" => read_only_static(cm_bytes),
                "test_with_config.cvf" => read_only_static(cvf_bytes),
            },
        };
        let _pkg_resolver = spawn_pkg_resolver(fs, server);
        assert_matches!(
            resolve_component("fuchsia-pkg://fuchsia.com/test#meta/test_with_config.cm", &proxy)
                .await
                .unwrap_err(),
            ResolverError::MissingConfigSource
        );
    }
}
