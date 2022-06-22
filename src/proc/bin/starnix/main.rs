// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::execution::create_galaxy;
use anyhow::Error;
use fuchsia_async as fasync;
use fuchsia_component::server::ServiceFs;
use futures::StreamExt;
use std::sync::Arc;

mod auth;
mod collections;
mod device;
mod execution;
mod fs;
mod loader;
mod lock;
mod logging;
mod mm;
mod mutable_state;
mod selinux;
mod signals;
mod syscalls;
mod task;
mod types;
mod vmex_resource;

#[cfg(test)]
mod testing;

#[fuchsia::main(logging_tags = ["starnix"])]
async fn main() -> Result<(), Error> {
    let galaxy = Arc::new(create_galaxy().await?);
    let serve_galaxy = galaxy.clone();

    let mut fs = ServiceFs::new_local();
    fs.dir("svc").add_fidl_service(move |stream| {
        let galaxy = galaxy.clone();
        fasync::Task::local(async move {
            execution::serve_component_runner(stream, galaxy)
                .await
                .expect("failed to start runner.")
        })
        .detach();
    });
    fs.dir("svc").add_fidl_service(move |stream| {
        let galaxy = serve_galaxy.clone();
        fasync::Task::local(async move {
            execution::serve_starnix_manager(stream, galaxy)
                .await
                .expect("failed to start manager.")
        })
        .detach();
    });
    fs.take_and_serve_directory_handle()?;
    fs.collect::<()>().await;

    Ok(())
}
