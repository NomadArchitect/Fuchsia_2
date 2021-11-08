// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{base_packages::BasePackages, index::PackageIndex, pkgfs_inspect::PkgfsInspectState},
    anyhow::{anyhow, Context as _, Error},
    argh::FromArgs,
    cobalt_sw_delivery_registry as metrics,
    fidl_fuchsia_update::CommitStatusProviderMarker,
    fuchsia_async::{futures::try_join, Task},
    fuchsia_cobalt::{CobaltConnector, ConnectionType},
    fuchsia_component::server::ServiceFs,
    fuchsia_inspect as finspect,
    fuchsia_syslog::{self, fx_log_err, fx_log_info},
    futures::{lock::Mutex, prelude::*},
    std::sync::{atomic::AtomicU32, Arc},
    system_image::StaticPackages,
};

mod base_packages;
mod cache_service;
mod compat;
mod gc_service;
mod index;
mod pkgfs_inspect;

mod retained_packages_service;

#[cfg(test)]
mod test_utils;

const COBALT_CONNECTOR_BUFFER_SIZE: usize = 1000;

#[derive(FromArgs, Debug, PartialEq)]
/// Flags to the package cache.
pub struct Args {
    /// whether to ignore the system image when starting pkg-cache.
    #[argh(switch)]
    ignore_system_image: bool,
}

#[fuchsia_async::run_singlethreaded]
async fn main() -> Result<(), Error> {
    fuchsia_syslog::init_with_tags(&["pkg-cache"]).expect("can't init logger");
    fuchsia_trace_provider::trace_provider_create_with_fdio();

    main_inner().await.map_err(|err| {
        // Use anyhow to print the error chain.
        let err = anyhow!(err);
        fx_log_err!("error running pkg-cache: {:#}", err);
        err
    })
}

async fn main_inner() -> Result<(), Error> {
    fx_log_info!("starting package cache service");

    let Args { ignore_system_image } = argh::from_env();

    let inspector = finspect::Inspector::new();
    let index_node = inspector.root().create_child("index");

    let (cobalt_sender, cobalt_fut) = CobaltConnector { buffer_size: COBALT_CONNECTOR_BUFFER_SIZE }
        .serve(ConnectionType::project_id(metrics::PROJECT_ID));
    let cobalt_fut = Task::spawn(cobalt_fut);

    let pkgfs_system =
        pkgfs::system::Client::open_from_namespace().context("error opening /pkgfs/system")?;
    let pkgfs_versions =
        pkgfs::versions::Client::open_from_namespace().context("error opening /pkgfs/versions")?;
    let pkgfs_ctl =
        pkgfs::control::Client::open_from_namespace().context("error opening /pkgfs/ctl")?;
    let pkgfs_install =
        pkgfs::install::Client::open_from_namespace().context("error opening /pkgfs/install")?;
    let pkgfs_needs =
        pkgfs::needs::Client::open_from_namespace().context("error opening /pkgfs/needs")?;
    let blobfs = blobfs::Client::open_from_namespace().context("error opening blobfs")?;

    let mut package_index = PackageIndex::new(index_node);

    let (_pkgfs_inspect, (), base_packages) = {
        let pkgfs_inspect_fut = async {
            Ok(PkgfsInspectState::new(&pkgfs_system, inspector.root().create_child("pkgfs")).await)
        };

        let load_cache_packages_fut = async {
            index::load_cache_packages(&mut package_index, &pkgfs_system, &pkgfs_versions)
                .unwrap_or_else(|e| fx_log_err!("Failed to load cache packages: {:#}", anyhow!(e)))
                .await;
            Ok(())
        };

        let base_packages_fut = load_base_packages(
            &pkgfs_system,
            &pkgfs_versions,
            inspector.root().create_child("base-packages"),
            ignore_system_image,
        );

        try_join!(pkgfs_inspect_fut, load_cache_packages_fut, base_packages_fut)?
    };

    let commit_status_provider =
        fuchsia_component::client::connect_to_protocol::<CommitStatusProviderMarker>()
            .context("while connecting to commit status provider")?;

    enum IncomingService {
        PackageCache(fidl_fuchsia_pkg::PackageCacheRequestStream),
        RetainedPackages(fidl_fuchsia_pkg::RetainedPackagesRequestStream),
        SpaceManager(fidl_fuchsia_space::ManagerRequestStream),
    }

    let mut fs = ServiceFs::new();
    inspect_runtime::serve(&inspector, &mut fs)?;
    fs.take_and_serve_directory_handle().context("while serving directory handle")?;
    fs.dir("svc")
        .add_fidl_service(IncomingService::PackageCache)
        .add_fidl_service(IncomingService::RetainedPackages)
        .add_fidl_service(IncomingService::SpaceManager);

    let package_index = Arc::new(Mutex::new(package_index));
    let cache_inspect_id = Arc::new(AtomicU32::new(0));
    let cache_inspect_node = inspector.root().create_child("fuchsia.pkg.PackageCache");
    let cache_get_node = Arc::new(cache_inspect_node.create_child("get"));
    let base_packages = Arc::new(base_packages);

    let () = fs
        .for_each_concurrent(None, move |svc| {
            match svc {
                IncomingService::PackageCache(stream) => Task::spawn(
                    cache_service::serve(
                        pkgfs_versions.clone(),
                        pkgfs_ctl.clone(),
                        pkgfs_install.clone(),
                        pkgfs_needs.clone(),
                        Arc::clone(&package_index),
                        blobfs.clone(),
                        Arc::clone(&base_packages),
                        stream,
                        cobalt_sender.clone(),
                        Arc::clone(&cache_inspect_id),
                        Arc::clone(&cache_get_node),
                    )
                    .map(|res| res.context("while serving fuchsia.pkg.PackageCache")),
                ),
                IncomingService::RetainedPackages(stream) => Task::spawn(
                    retained_packages_service::serve(
                        Arc::clone(&package_index),
                        blobfs.clone(),
                        stream,
                    )
                    .map(|res| res.context("while serving fuchsia.pkg.RetainedPackages")),
                ),
                IncomingService::SpaceManager(stream) => Task::spawn(
                    gc_service::serve(
                        blobfs.clone(),
                        Arc::clone(&base_packages),
                        Arc::clone(&package_index),
                        commit_status_provider.clone(),
                        stream,
                    )
                    .map(|res| res.context("while serving fuchsia.space.Manager")),
                ),
            }
            .unwrap_or_else(|e| {
                fx_log_err!("error handling fidl connection: {:#}", anyhow!(e));
            })
        })
        .await;
    cobalt_fut.await;

    Ok(())
}

async fn load_base_packages(
    pkgfs_system: &pkgfs::system::Client,
    pkgfs_versions: &pkgfs::versions::Client,
    node: finspect::Node,
    ignore_system_image: bool,
) -> Result<Option<BasePackages>, Error> {
    // Not all constructions with pkg-cache include a system image (any recovery implementation,
    // for example, as it will be putting blobs into an empty blobfs).
    if ignore_system_image {
        fx_log_info!("Ignoring system image, so not loading base packages");
        return Ok(None);
    }

    let static_packages = get_static_packages(pkgfs_system).await?;
    let pkgfs_system_hash = pkgfs_system.hash().await.context("while getting system image hash")?;

    let base_packages =
        BasePackages::new(pkgfs_versions, static_packages, &pkgfs_system_hash, node)
            .await
            .context("loading base packages")?;
    Ok(Some(base_packages))
}

async fn get_static_packages(
    pkgfs_system: &pkgfs::system::Client,
) -> Result<StaticPackages, Error> {
    let file = pkgfs_system
        .open_file("data/static_packages")
        .await
        .context("failed to open data/static_packages from system image package")?;
    StaticPackages::deserialize(file).context("error deserializing data/static_packages")
}
