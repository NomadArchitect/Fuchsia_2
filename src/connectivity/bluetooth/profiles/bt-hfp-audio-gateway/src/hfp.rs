// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    async_utils::stream::FutureMap,
    fidl::endpoints::{Proxy, ServerEnd},
    fidl_fuchsia_bluetooth_hfp::{CallManagerProxy, PeerHandlerMarker},
    fuchsia_bluetooth::types::PeerId,
    futures::{channel::mpsc::Receiver, select, stream::StreamExt},
    std::{collections::hash_map::Entry, matches},
};

use crate::{
    config::AudioGatewayFeatureSupport,
    error::Error,
    peer::Peer,
    profile::{Profile, ProfileEvent},
};

/// Manages operation of the HFP functionality.
pub struct Hfp {
    config: AudioGatewayFeatureSupport,
    /// The `profile` provides Hfp with a means to drive the fuchsia.bluetooth.bredr related APIs.
    profile: Profile,
    /// The `call_manager` provides Hfp with a means to interact with clients of the
    /// fuchsia.bluetooth.hfp.Hfp and fuchsia.bluetooth.hfp.CallManager protocols.
    call_manager: Option<CallManagerProxy>,
    call_manager_registration: Receiver<CallManagerProxy>,
    /// A collection of Bluetooth peers that support the HFP profile.
    peers: FutureMap<PeerId, Peer>,
}

impl Hfp {
    /// Create a new `Hfp` with the provided `profile`.
    pub fn new(
        profile: Profile,
        call_manager_registration: Receiver<CallManagerProxy>,
        config: AudioGatewayFeatureSupport,
    ) -> Self {
        Self {
            profile,
            call_manager_registration,
            call_manager: None,
            peers: FutureMap::new(),
            config,
        }
    }

    /// Run the Hfp object to completion. Runs until an unrecoverable error occurs or there is no
    /// more work to perform because all managed resource have been closed.
    pub async fn run(mut self) -> Result<(), Error> {
        loop {
            select! {
                // If the profile stream ever terminates, the component should shut down.
                event = self.profile.next() => {
                    if let Some(event) = event {
                        self.handle_profile_event(event?).await?;
                    } else {
                        break;
                    }
                }
                manager = self.call_manager_registration.select_next_some() => {
                    self.handle_new_call_manager(manager).await?;
                }
                removed = self.peers.next() => {
                    if let Some(removed) = removed {
                        log::debug!("peer removed: {}", removed);
                    }
                }
                complete => {
                    break;
                }
            }
        }
        Ok(())
    }

    /// Handle a single `ProfileEvent` from `profile`.
    async fn handle_profile_event(&mut self, event: ProfileEvent) -> Result<(), Error> {
        let id = event.peer_id();
        let peer = match self.peers.inner().entry(id) {
            Entry::Vacant(entry) => {
                let mut peer = Peer::new(id, self.profile.proxy(), self.config)?;
                if let Some(proxy) = self.call_manager.clone() {
                    let server_end = peer.build_handler().await?;
                    if Self::send_peer_connected(&proxy, peer.id(), server_end).await.is_err() {
                        self.call_manager = None;
                    }
                }
                entry.insert(Box::pin(peer))
            }
            Entry::Occupied(entry) => entry.into_mut(),
        };
        peer.profile_event(event).await
    }

    /// Handle a single `CallManagerEvent` from `call_manager`.
    async fn handle_new_call_manager(&mut self, proxy: CallManagerProxy) -> Result<(), Error> {
        if matches!(&self.call_manager, Some(manager) if !manager.is_closed()) {
            log::info!("Call manager already set. Closing new connection");
            return Ok(());
        }

        let mut server_ends = Vec::with_capacity(self.peers.inner().len());
        for (id, peer) in self.peers.inner().iter_mut() {
            server_ends.push((*id, peer.build_handler().await?));
        }

        for (id, server) in server_ends {
            if Self::send_peer_connected(&proxy, id, server).await.is_err() {
                return Ok(());
            }
        }

        self.call_manager = Some(proxy.clone());

        Ok(())
    }

    async fn send_peer_connected(
        proxy: &CallManagerProxy,
        id: PeerId,
        server_end: ServerEnd<PeerHandlerMarker>,
    ) -> Result<(), ()> {
        proxy.peer_connected(&mut id.into(), server_end).await.map_err(|e| {
            if e.is_closed() {
                log::info!("CallManager channel closed.");
            } else {
                log::info!("CallManager channel closed with error: {}", e);
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::profile::test_server::{setup_profile_and_test_server, LocalProfileTestServer},
        fidl_fuchsia_bluetooth as bt,
        fidl_fuchsia_bluetooth_hfp::{
            CallManagerMarker, CallManagerRequest, CallManagerRequestStream, NetworkInformation,
        },
        fuchsia_async as fasync,
        futures::{channel::mpsc, SinkExt, TryStreamExt},
    };

    #[fasync::run_until_stalled(test)]
    async fn profile_error_propagates_error_from_hfp_run() {
        let (profile, server) = setup_profile_and_test_server();
        // dropping the server is expected to produce an error from Hfp::run
        drop(server);

        let (_tx, rx) = mpsc::channel(0);

        let hfp = Hfp::new(profile, rx, AudioGatewayFeatureSupport::default());
        let result = hfp.run().await;
        assert!(result.is_err());
    }

    /// Tests the HFP main run loop from a blackbox perspective by asserting on the FIDL messages
    /// sent and received by the services that Hfp interacts with: A bredr profile server and
    /// a call manager.
    #[fasync::run_until_stalled(test)]
    async fn new_profile_event_initiates_connections_to_profile_and_call_manager_() {
        let (profile, server) = setup_profile_and_test_server();
        let (proxy, stream) =
            fidl::endpoints::create_proxy_and_stream::<CallManagerMarker>().unwrap();

        let (mut sender, receiver) = mpsc::channel(1);
        sender.send(proxy).await.expect("Hfp to receive the proxy");

        // Run hfp in a background task since we are testing that the profile server observes the
        // expected behavior when interacting with hfp.
        let hfp = Hfp::new(profile, receiver, AudioGatewayFeatureSupport::default());
        let _hfp_task = fasync::Task::local(hfp.run());

        // Drive both services to expected steady states without any errors.
        let result = futures::future::join(
            profile_server_init_and_peer_handling(server),
            call_manager_init_and_peer_handling(stream),
        )
        .await;
        matches::assert_matches!(result, (Ok(()), Ok(())));
    }

    /// Respond to all FIDL messages expected during the initialization of the Hfp main run loop
    /// and during the simulation of a new `Peer` being added.
    ///
    /// Returns Ok(()) when all expected messages have been handled normally.
    async fn call_manager_init_and_peer_handling(
        mut stream: CallManagerRequestStream,
    ) -> Result<(), anyhow::Error> {
        match stream.try_next().await? {
            Some(CallManagerRequest::PeerConnected { id: _, handle, responder }) => {
                responder.send()?;
                let mut stream = handle.into_stream()?;
                let responder = stream
                    .next()
                    .await
                    .expect("some request to be received")?
                    .into_watch_network_information()
                    .expect("watch network information request");
                responder.send(NetworkInformation::EMPTY)?;
            }
            x => anyhow::bail!("Unexpected request received: {:?}", x),
        };
        Ok(())
    }

    /// Respond to all FIDL messages expected during the initialization of the Hfp main run loop and
    /// during the simulation of a new `Peer` search result event.
    ///
    /// Returns Ok(()) when all expected messages have been handled normally.
    async fn profile_server_init_and_peer_handling(
        mut server: LocalProfileTestServer,
    ) -> Result<(), anyhow::Error> {
        server.complete_registration().await;

        // Send search result
        server
            .results
            .as_ref()
            .unwrap()
            .service_found(&mut bt::PeerId { value: 1 }, None, &mut vec![].iter_mut())
            .await?;

        Ok(())
    }
}
