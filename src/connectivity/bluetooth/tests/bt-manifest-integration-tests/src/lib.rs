// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::Error,
    fidl::endpoints::{create_proxy, DiscoverableService, ServerEnd, ServiceMarker},
    fidl_fuchsia_io::{self as fio, DirectoryMarker, DirectoryProxy},
    fuchsia_async as fasync,
    fuchsia_component::server::{ServiceFs, ServiceObj},
    fuchsia_component_test::mock::MockHandles,
    futures::{channel::mpsc, SinkExt, StreamExt, TryStream, TryStreamExt},
    log::info,
    std::sync::Arc,
    vfs::{directory::entry::DirectoryEntry, execution_scope::ExecutionScope},
};

// #! Library for common utilities (mocks, definitions) for the manifest integration tests.

/// Process requests received in the `stream` and relay them to the provided `sender`.
/// Logs incoming requests prefixed with the `tag`.
#[track_caller]
pub async fn process_request_stream<S, Event>(
    mut stream: S::RequestStream,
    mut sender: mpsc::Sender<Event>,
) where
    S: DiscoverableService,
    Event: std::convert::From<<S::RequestStream as TryStream>::Ok>,
    <S::RequestStream as TryStream>::Ok: std::fmt::Debug,
{
    while let Some(request) = stream.try_next().await.expect("serving request stream failed") {
        info!("Received {} service request: {:?}", S::SERVICE_NAME, request);
        sender.send(request.into()).await.expect("should send");
    }
}

/// Adds a handler for the FIDL service `S` which relays the ServerEnd of the service
/// connection request to the provided `sender`.
/// Note: This method does not process requests from the service connection. It only relays
/// the stream to the `sender.
#[track_caller]
pub fn add_fidl_service_handler<S, Event: 'static>(
    fs: &mut ServiceFs<ServiceObj<'_, ()>>,
    sender: mpsc::Sender<Event>,
) where
    S: DiscoverableService,
    Event: std::convert::From<S::RequestStream> + std::marker::Send,
{
    fs.dir("svc").add_fidl_service(move |req_stream: S::RequestStream| {
        let mut s = sender.clone();
        fasync::Task::local(async move {
            info!("Received connection for {}", S::SERVICE_NAME);
            s.send(req_stream.into()).await.expect("should send");
        })
        .detach()
    });
}

/// A mock component that provides the generic service `S`. The request stream
/// of the service is processed and any requests relayed to the provided `sender`.
pub async fn mock_component<S, Event: 'static>(
    sender: mpsc::Sender<Event>,
    handles: MockHandles,
) -> Result<(), Error>
where
    S: DiscoverableService,
    Event: std::convert::From<<<S as ServiceMarker>::RequestStream as TryStream>::Ok>
        + std::marker::Send,
    <<S as ServiceMarker>::RequestStream as TryStream>::Ok: std::fmt::Debug,
{
    let mut fs = ServiceFs::new();
    fs.dir("svc").add_fidl_service(move |req_stream: S::RequestStream| {
        let sender_clone = sender.clone();
        info!("Received connection for {}", S::SERVICE_NAME);
        fasync::Task::local(process_request_stream::<S, _>(req_stream, sender_clone)).detach();
    });

    fs.serve_connection(handles.outgoing_dir.into_channel())?;
    fs.collect::<()>().await;
    Ok(())
}

/// Spawns a VFS handler for the provided `dir`.
pub fn spawn_vfs(dir: Arc<dyn DirectoryEntry>) -> DirectoryProxy {
    let (client_end, server_end) = create_proxy::<DirectoryMarker>().unwrap();
    let scope = ExecutionScope::new();
    dir.open(
        scope,
        fio::OPEN_RIGHT_READABLE | fio::OPEN_RIGHT_WRITABLE,
        0,
        vfs::path::Path::empty(),
        ServerEnd::new(server_end.into_channel()),
    );
    client_end
}
