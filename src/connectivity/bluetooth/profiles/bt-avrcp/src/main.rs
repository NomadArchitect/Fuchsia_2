// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![recursion_limit = "1024"]

use {
    anyhow::{format_err, Context, Error},
    fidl_fuchsia_bluetooth_bredr as bredr, fuchsia_async as fasync,
    fuchsia_component::server::ServiceFs,
    fuchsia_inspect as inspect,
    fuchsia_inspect_derive::Inspect,
    futures::{channel::mpsc, stream::StreamExt, FutureExt},
    log::{error, info, warn},
    std::convert::TryInto,
};

mod metrics;
mod packets;
mod peer;
mod peer_manager;
mod profile;
mod service;
mod types;

#[cfg(test)]
mod tests;

use crate::{
    metrics::{MetricsNode, METRICS_NODE_NAME},
    peer_manager::PeerManager,
    profile::{protocol_to_channel_type, AvrcpService, ChannelType},
};

#[fasync::run_singlethreaded]
async fn main() -> Result<(), Error> {
    fuchsia_syslog::init_with_tags(&["avrcp", "avctp"]).expect("Unable to initialize logger");

    // Begin searching for AVRCP target/controller SDP records on newly connected remote peers
    // and register our AVRCP service with the BrEdr profile service.
    let (profile_proxy, mut connection_requests, mut search_result_requests) =
        profile::connect_and_advertise().context("Unable to connect to BrEdr Profile Service")?;

    // Create a channel that peer manager will receive requests for peer controllers from the FIDL
    // service runner.
    // TODO(fxbug.dev/44330) handle back pressure correctly and reduce mpsc::channel buffer sizes.
    let (client_sender, mut service_request_receiver) = mpsc::channel(512);

    let mut fs = ServiceFs::new();

    let inspect = inspect::Inspector::new();
    inspect_runtime::serve(&inspect, &mut fs)?;

    let mut peer_manager = PeerManager::new(profile_proxy);
    if let Err(e) = peer_manager.iattach(inspect.root(), "peers") {
        warn!("Failed to attach to inspect: {:?}", e);
    }
    let mut metrics_node = MetricsNode::default();
    if let Err(e) = metrics_node.iattach(inspect.root(), METRICS_NODE_NAME) {
        warn!("Failed to attach to inspect metrics: {:?}", e);
    }
    peer_manager.set_metrics_node(metrics_node);

    let mut service_fut = service::run_services(fs, client_sender)
        .expect("Unable to start AVRCP FIDL service")
        .fuse();

    loop {
        futures::select! {
            request = connection_requests.next() => {
                let connected = match request {
                    None => return Err(format_err!("BR/EDR Profile Service closed connection target")),
                    Some(Err(e)) => return Err(format_err!("BR/EDR Profile connection target error: {}", e)),
                    Some(Ok(request)) => request,
                };
                let bredr::ConnectionReceiverRequest::Connected { peer_id, channel, protocol, .. } = connected;
                let channel = match channel.try_into() {
                    Ok(chan) => chan,
                    Err(e) => {
                        warn!("Couldn't convert channel: {:?}", e);
                        continue;
                    }
                };
                let peer_id = peer_id.into();
                info!("Incoming connection request from {:?} with protocol: {:?}", peer_id, protocol);

                match protocol_to_channel_type(&protocol) {
                    Some(ChannelType::Control) => peer_manager.new_control_connection(&peer_id, channel),
                    Some(ChannelType::Browse) => peer_manager.new_browse_connection(&peer_id, channel),
                    None => {
                        info!("Received connection over non-AVRCP protocol: {:?}", protocol);
                        continue
                    }
                }
            }
            result_request = search_result_requests.next() =>  {
                let result = match result_request {
                    None => return Err(format_err!("BR/EDR Profile Service closed service search")),
                    Some(Err(e)) => return Err(format_err!("BR/EDR Profile service search error: {}", e)),
                    Some(Ok(request)) => request,
                };
                let bredr::SearchResultsRequest::ServiceFound { peer_id, protocol: _, attributes, responder } = result;

                if let Some(service) = AvrcpService::from_attributes(attributes) {
                    info!("Service found on {:?}: {:?}", peer_id, service);
                    peer_manager.services_found(&peer_id.into(), vec![service]);
                }
                responder.send().context("FIDL response for search failed")?;
            },
            request = service_request_receiver.select_next_some() => {
                peer_manager.handle_service_request(request);
            },
            service_result = service_fut => {
                error!("Service task finished unexpectedly: {:?}", service_result);
                break;
            },
            complete => break,
        }
    }
    Ok(())
}
