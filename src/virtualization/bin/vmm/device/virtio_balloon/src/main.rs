// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod balloon_device;
mod wire;

use {
    crate::balloon_device::BalloonDevice,
    anyhow::{anyhow, Context},
    fidl::endpoints::RequestStream,
    fidl_fuchsia_virtualization_hardware::VirtioBalloonRequestStream,
    fuchsia_component::server,
    futures::channel::mpsc,
    futures::{future, StreamExt, TryFutureExt, TryStreamExt},
    machina_virtio_device::GuestBellTrap,
    tracing,
    virtio_device::chain::ReadableChain,
};

async fn run_virtio_balloon(
    mut virtio_balloon_fidl: VirtioBalloonRequestStream,
) -> Result<(), anyhow::Error> {
    // Receive start info as first message.
    let (start_info, responder) = virtio_balloon_fidl
        .try_next()
        .await?
        .ok_or(anyhow!("Failed to read fidl message from the channel."))?
        .into_start()
        .ok_or(anyhow!("Start should be the first message sent."))?;

    // Prepare the device builder
    let (mut device_builder, guest_mem) = machina_virtio_device::from_start_info(start_info)?;
    responder.send()?;
    let vmo = device_builder.take_vmo().expect("VMO must be provided to virtio_balloon device");

    // Complete the setup of queues and get a device.
    let mut virtio_device_fidl = virtio_balloon_fidl.cast_stream();
    let (device, ready_responder) = machina_virtio_device::config_builder_from_stream(
        device_builder,
        &mut virtio_device_fidl,
        &[wire::INFLATEQ, wire::DEFLATEQ, wire::STATSQ][..],
        &guest_mem,
    )
    .await
    .context("Failed to initialize device.")?;

    // Initialize all queues.
    let inflate_stream = device.take_stream(wire::INFLATEQ)?;
    let deflate_stream = device.take_stream(wire::DEFLATEQ)?;
    let stats_stream = device.take_stream(wire::STATSQ)?;
    ready_responder.send()?;

    let negotiated_features =
        wire::VirtioBalloonFeatureFlags::from_bits(device.get_features()).unwrap();
    let balloon_device = BalloonDevice::new(vmo);

    let virtio_balloon_fidl: VirtioBalloonRequestStream = virtio_device_fidl.cast_stream();
    let bell = GuestBellTrap::complete_or_pending(device.take_bell_traps(), &device)
        .map_err(|e| anyhow!("GuestBellTrap: {}", e));

    let (sender, receiver) = mpsc::channel(10);
    futures::try_join!(
        BalloonDevice::run_virtio_balloon_stream(virtio_balloon_fidl, &device, sender),
        BalloonDevice::run_mem_stats_receiver(
            stats_stream,
            &guest_mem,
            &negotiated_features,
            receiver,
        ),
        bell,
        inflate_stream.map(|chain| Ok(chain)).try_for_each(|chain| future::ready({
            if let Err(e) =
                balloon_device.process_inflate_chain(ReadableChain::new(chain, &guest_mem))
            {
                tracing::warn!("Failed to inflate chain {}", e);
            }
            Ok(())
        })),
        deflate_stream.map(|chain| Ok(chain)).try_for_each(|chain| future::ready({
            balloon_device.process_deflate_chain(ReadableChain::new(chain, &guest_mem));
            Ok(())
        })),
    )?;
    Ok(())
}

#[fuchsia::main(logging = true, threads = 1)]
async fn main() -> Result<(), anyhow::Error> {
    let mut fs = server::ServiceFs::new();
    fs.dir("svc").add_fidl_service(|stream: VirtioBalloonRequestStream| stream);
    fs.take_and_serve_directory_handle().context("Error starting server")?;
    fs.for_each_concurrent(None, |stream| async {
        if let Err(e) = run_virtio_balloon(stream).await {
            tracing::error!("Error running virtio_balloon service: {}", e);
        }
    })
    .await;
    Ok(())
}
