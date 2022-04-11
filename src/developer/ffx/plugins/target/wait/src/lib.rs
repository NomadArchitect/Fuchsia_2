// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::{anyhow, Context as _, Result},
    errors::FfxError,
    ffx_core::ffx_plugin,
    ffx_wait_args::WaitCommand,
    fidl::endpoints::create_proxy,
    fidl_fuchsia_developer_ffx::{DaemonError, TargetCollectionProxy, TargetMarker, TargetQuery},
    fidl_fuchsia_developer_remotecontrol::RemoteControlMarker,
    fuchsia_async::futures::future::Either,
    fuchsia_async::futures::StreamExt,
    selectors::VerboseError,
    std::time::Duration,
    timeout::timeout,
};

#[ffx_plugin(TargetCollectionProxy = "daemon::protocol")]
pub async fn wait_for_device(
    target_collection: TargetCollectionProxy,
    cmd: WaitCommand,
) -> Result<()> {
    let ffx: ffx_lib_args::Ffx = argh::from_env();
    let knock_fut = async {
        loop {
            if knock_target(&ffx, &target_collection)
                .await
                .map_err(|e| log::debug!("failed to knock target: {:?}", e))
                .is_ok()
            {
                return;
            }
        }
    };
    futures_lite::pin!(knock_fut);
    let timeout_fut = fuchsia_async::Timer::new(Duration::from_secs(cmd.timeout as u64));
    let is_default_target = ffx.target.is_none();
    let timeout_err = FfxError::DaemonError {
        err: DaemonError::Timeout,
        target: ffx.target.clone(),
        is_default_target,
    };
    match futures::future::select(knock_fut, timeout_fut).await {
        Either::Left(_) => Ok(()),
        Either::Right(_) => Err(timeout_err.into()),
    }
}

const RCS_TIMEOUT: u64 = 3;
const KNOCK_TIMEOUT: u64 = 1;

async fn knock_target(
    ffx: &ffx_lib_args::Ffx,
    target_collection_proxy: &TargetCollectionProxy,
) -> Result<()> {
    let default_target = ffx.target().await?;
    let (target_proxy, target_remote) = create_proxy::<TargetMarker>()?;
    let (rcs, remote_server_end) = create_proxy::<RemoteControlMarker>()?;
    // If you are reading this plugin for example code, this is an example of what you
    // should generally not be doing to connect to a daemon protocol. This is maintained
    // by the FFX team directly.
    target_collection_proxy
        .open_target(
            TargetQuery { string_matcher: default_target, ..TargetQuery::EMPTY },
            target_remote,
        )
        .await
        .context("opening target")?
        .map_err(|e| anyhow!("open target err: {:?}", e))?;
    timeout(Duration::from_secs(RCS_TIMEOUT), target_proxy.open_remote_control(remote_server_end))
        .await?
        .context("opening remote_control")?
        .map_err(|e| anyhow!("open remote control err: {:?}", e))?;
    let (knock_client, knock_remote) = fidl::handle::Channel::create()?;
    let knock_client = fuchsia_async::Channel::from_channel(knock_client)?;
    let knock_client = fidl::client::Client::new(knock_client, "knock_client");
    rcs.connect(
        selectors::parse_selector::<VerboseError>(
            "core/remote-control:out:fuchsia.developer.remotecontrol.RemoteControl",
        )
        .unwrap(),
        knock_remote,
    )
    .await
    .context("rcs connect fidl conn")?
    .map_err(|e| anyhow!("rcs connect result: {:?}", e))?;
    let mut event_receiver = knock_client.take_event_receiver();
    let res = timeout(Duration::from_secs(KNOCK_TIMEOUT), event_receiver.next()).await;
    match res {
        Err(_) => Ok(()), // timeout is fine here, it means the connection wasn't lost.
        Ok(r) => r
            .ok_or(anyhow!("unable to knock RCS from itself"))?
            .map(drop)
            .context("rcs connection failure"),
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        fidl::endpoints::create_proxy_and_stream,
        fidl_fuchsia_developer_ffx::{
            TargetCollectionMarker, TargetCollectionRequest, TargetRequest, TargetRequestStream,
        },
        fidl_fuchsia_developer_remotecontrol::{
            RemoteControlRequest, RemoteControlRequestStream, ServiceMatch,
        },
        fuchsia_async::futures::TryStreamExt,
    };

    fn spawn_remote_control(mut rcs_stream: RemoteControlRequestStream) {
        fuchsia_async::Task::local(async move {
            while let Ok(Some(req)) = rcs_stream.try_next().await {
                match req {
                    RemoteControlRequest::Connect { responder, service_chan, .. } => {
                        fuchsia_async::Task::local(async move {
                            let _service_chan = service_chan; // just hold the channel open to make the test succeed. No need to actually use it.
                            std::future::pending::<()>().await;
                        })
                        .detach();
                        responder
                            .send(&mut Ok(ServiceMatch {
                                moniker: vec![],
                                subdir: "foo".to_string(),
                                service: "bar".to_string(),
                            }))
                            .unwrap();
                    }
                    e => panic!("unexpected request: {:?}", e),
                }
            }
        })
        .detach();
    }

    fn spawn_target_handler(mut target_stream: TargetRequestStream, responsive_rcs: bool) {
        fuchsia_async::Task::local(async move {
            while let Ok(Some(req)) = target_stream.try_next().await {
                match req {
                    TargetRequest::OpenRemoteControl { responder, remote_control, .. } => {
                        if responsive_rcs {
                            spawn_remote_control(remote_control.into_stream().unwrap());
                            responder.send(&mut Ok(())).expect("responding to open rcs")
                        } else {
                            std::future::pending::<()>().await;
                        }
                    }
                    e => panic!("got unexpected req: {:?}", e),
                }
            }
        })
        .detach();
    }

    fn setup_fake_target_collection_server(responsive_rcs: bool) -> TargetCollectionProxy {
        let (proxy, mut stream) = create_proxy_and_stream::<TargetCollectionMarker>().unwrap();
        fuchsia_async::Task::local(async move {
            while let Ok(Some(req)) = stream.try_next().await {
                match req {
                    TargetCollectionRequest::OpenTarget { responder, target_handle, .. } => {
                        spawn_target_handler(target_handle.into_stream().unwrap(), responsive_rcs);
                        responder.send(&mut Ok(())).unwrap();
                    }
                    e => panic!("unexpected request: {:?}", e),
                }
            }
        })
        .detach();
        proxy
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn able_to_connect_to_device() {
        assert!(wait_for_device(
            setup_fake_target_collection_server(true),
            WaitCommand { timeout: 5 }
        )
        .await
        .is_ok());
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn unable_to_connect_to_device() {
        assert!(wait_for_device(
            setup_fake_target_collection_server(false),
            WaitCommand { timeout: 5 }
        )
        .await
        .is_err());
    }
}
