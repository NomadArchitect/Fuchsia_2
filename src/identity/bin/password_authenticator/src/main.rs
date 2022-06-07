// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod account;
mod account_manager;
mod account_metadata;
mod constants;
mod disk_management;
mod keys;
mod pinweaver;
mod scrypt;
#[cfg(test)]
mod testing;

use anyhow::{anyhow, Context, Error};
use fidl::endpoints::RequestStream;
use fidl_fuchsia_identity_account::AccountManagerRequestStream;
use fidl_fuchsia_io as fio;
use fidl_fuchsia_process_lifecycle::LifecycleRequestStream;
use fuchsia_async as fasync;
use fuchsia_component::server::ServiceFs;
use fuchsia_fs::directory::open_in_namespace;
use fuchsia_runtime::{self as fruntime, HandleInfo, HandleType};
use futures::StreamExt;
use log::{error, info};
use std::sync::Arc;

use crate::{
    account_manager::{AccountManager, EnvCredManagerProvider},
    account_metadata::DataDirAccountMetadataStore,
    disk_management::DevDiskManager,
};

enum Services {
    AccountManager(AccountManagerRequestStream),
}

#[fasync::run_singlethreaded]
async fn main() -> Result<(), Error> {
    fuchsia_syslog::init_with_tags(&["auth"]).expect("Can't init logger");
    info!("Starting password authenticator");

    let config = password_authenticator_config::Config::take_from_startup_handle();
    // validate that at least one account metadata type is enabled
    if !config.allow_scrypt && !config.allow_pinweaver {
        let err = anyhow!("No account types allowed by config, exiting");
        error!("{}", err);
        return Err(err);
    }

    let dev_root =
        open_in_namespace("/dev", fio::OpenFlags::RIGHT_READABLE | fio::OpenFlags::RIGHT_WRITABLE)?;
    let disk_manager = DevDiskManager::new(dev_root);

    let metadata_root = open_in_namespace(
        "/data/accounts",
        fio::OpenFlags::RIGHT_READABLE
            | fio::OpenFlags::RIGHT_WRITABLE
            | fio::OpenFlags::DIRECTORY
            | fio::OpenFlags::CREATE,
    )?;
    let mut account_metadata_store = DataDirAccountMetadataStore::new(metadata_root);
    // Clean up any not-committed files laying around in the account metadata directory.
    let cleanup_res = account_metadata_store.cleanup_stale_files().await;
    // If any cleanup fails, ignore it -- we can still perform our primary function with
    // stale files laying around.
    // TODO(zarvox): someday, make an inspect entry for this failure mode
    drop(cleanup_res);

    let cred_manager_provider = EnvCredManagerProvider {};
    let account_manager = Arc::new(AccountManager::new(
        config,
        disk_manager,
        account_metadata_store,
        cred_manager_provider,
    ));

    let mut fs = ServiceFs::new();
    fs.dir("svc").add_fidl_service(Services::AccountManager);
    fs.take_and_serve_directory_handle().context("serving directory handle")?;

    let lifecycle_handle_info = HandleInfo::new(HandleType::Lifecycle, 0);
    let lifecycle_handle = fruntime::take_startup_handle(lifecycle_handle_info)
        .expect("must have been provided a lifecycle channel in procargs");
    let async_chan = fasync::Channel::from_channel(lifecycle_handle.into())
        .expect("Async channel conversion failed.");
    let lifecycle_req_stream = LifecycleRequestStream::from_channel(async_chan);

    let account_manager_for_lifecycle = account_manager.clone();
    let _lifecycle_task = fasync::Task::spawn(async move {
        account_manager_for_lifecycle.handle_requests_for_lifecycle(lifecycle_req_stream).await
    });

    fs.for_each_concurrent(None, |service| match service {
        Services::AccountManager(stream) => {
            account_manager.handle_requests_for_account_manager(stream)
        }
    })
    .await;

    Ok(())
}
