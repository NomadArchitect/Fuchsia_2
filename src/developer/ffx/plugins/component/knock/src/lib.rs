// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::{Context, Result},
    errors::ffx_error,
    ffx_component::SELECTOR_FORMAT_HELP,
    ffx_core::ffx_plugin,
    ffx_knock_args::{KnockCommand, Node},
    fidl::handle::Channel,
    fidl_fuchsia_developer_remotecontrol as rc, fuchsia_zircon_status as zx_status,
    moniker::{AbsoluteMoniker, AbsoluteMonikerBase},
    selectors::{self, VerboseError},
    std::io::{stdout, Write},
};

fn generate_selector(moniker: String, service: String, node: Node) -> Result<String> {
    let mut moniker = AbsoluteMoniker::parse_str(moniker.as_str())
        .map_err(|e| ffx_error!("Moniker could not be parsed: {}", e))?
        .to_string();

    // Remove the leading '/' if present.
    if moniker.chars().next().unwrap() == '/' {
        moniker.remove(0);
    }

    Ok([moniker, node.to_string(), service].join(":"))
}

#[ffx_plugin()]
pub async fn knock_cmd(remote_proxy: rc::RemoteControlProxy, cmd: KnockCommand) -> Result<()> {
    let writer = Box::new(stdout());

    knock(remote_proxy, writer, generate_selector(cmd.moniker, cmd.service, cmd.node)?.as_str())
        .await
}

async fn knock<W: Write>(
    remote_proxy: rc::RemoteControlProxy,
    mut write: W,
    selector_str: &str,
) -> Result<()> {
    let writer = &mut write;
    let selector = selectors::parse_selector::<VerboseError>(selector_str).map_err(|e| {
        ffx_error!("Invalid selector '{}': {}\n{}", selector_str, e, SELECTOR_FORMAT_HELP)
    })?;

    let (client, server) = Channel::create()?;
    match remote_proxy.connect(selector, server).await.context("awaiting connect call")? {
        Ok(m) => {
            match client.read_split(&mut vec![], &mut vec![]) {
                Err(zx_status::Status::PEER_CLOSED) => writeln!(
                    writer,
                    "Failure: service is not up. Connection to '{}:{}:{}' reported PEER_CLOSED.",
                    m.moniker.join("/"),
                    m.subdir,
                    m.service
                )?,
                Err(zx_status::Status::SHOULD_WAIT) => writeln!(
                    writer,
                    "Success: service is up. Connected to '{}:{}:{}'.",
                    m.moniker.join("/"),
                    m.subdir,
                    m.service
                )?,
                Err(e) => writeln!(
                    writer,
                    "Unknown: opened connection to '{}:{}:{}', but channel read reported {:?}.",
                    m.moniker.join("/"),
                    m.subdir,
                    m.service,
                    e
                )?,
                _ => writeln!(
                    writer,
                    "Success: service is up. Connected to '{}:{}:{}'.",
                    m.moniker.join("/"),
                    m.subdir,
                    m.service
                )?,
            };

            Ok(())
        }
        Err(e) => {
            writeln!(writer, "Failed to connect to service `{}`: {:?}", selector_str, e)?;
            Ok(())
        }
    }
}

////////////////////////////////////////////////////////////////////////////////
// tests

#[cfg(test)]
mod test {
    use {
        super::*, anyhow::Result, fidl::endpoints::RequestStream, fidl::handle::AsyncChannel,
        fidl_fuchsia_developer_ffx::DaemonRequestStream, futures::TryStreamExt,
    };

    fn setup_fake_daemon_service(mut stream: DaemonRequestStream) {
        fuchsia_async::Task::local(async move {
            let mut continue_once = true;
            while let Ok(Some(_req)) = stream.try_next().await {
                // We should only get one request per stream. We want subsequent calls to fail if more are
                // made.
                if continue_once {
                    continue_once = false;
                    continue;
                }
                break;
            }
        })
        .detach();
    }

    fn setup_fake_remote_server(connect_chan: bool) -> rc::RemoteControlProxy {
        setup_fake_remote_proxy(move |req| match req {
            rc::RemoteControlRequest::Connect { selector: _, service_chan, responder } => {
                if connect_chan {
                    setup_fake_daemon_service(DaemonRequestStream::from_channel(
                        AsyncChannel::from_channel(service_chan).unwrap(),
                    ));
                }

                let _ = responder
                    .send(&mut Ok(rc::ServiceMatch {
                        moniker: vec![String::from("core"), String::from("test")],
                        subdir: String::from("out"),
                        service: String::from("fuchsia.myservice"),
                    }))
                    .unwrap();
            }
            _ => assert!(false, "got unexpected {:?}", req),
        })
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_knock_invalid_selector() -> Result<()> {
        let mut output = Vec::new();
        let remote_proxy = setup_fake_remote_server(false);
        let response = knock(remote_proxy, &mut output, "a:b:").await;
        let e = response.unwrap_err();
        assert!(e.to_string().contains(SELECTOR_FORMAT_HELP));
        Ok(())
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_knock_working_service() -> Result<()> {
        let mut output_utf8 = Vec::new();
        let remote_proxy = setup_fake_remote_server(true);
        let _response =
            knock(remote_proxy, &mut output_utf8, "*:*:*").await.expect("knock should not fail");

        let output = String::from_utf8(output_utf8).expect("Invalid UTF-8 bytes");
        assert!(output.contains("Success"));
        assert!(output.contains("core/test:out:fuchsia.myservice"));
        Ok(())
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_knock_no_service_connected() -> Result<()> {
        let mut output_utf8 = Vec::new();
        let remote_proxy = setup_fake_remote_server(false);
        let _response =
            knock(remote_proxy, &mut output_utf8, "*:*:*").await.expect("knock should not fail");

        let output = String::from_utf8(output_utf8).expect("Invalid UTF-8 bytes");
        assert!(!output.contains("Success"));
        assert!(output.contains("Failure"));
        assert!(output.contains("core/test:out:fuchsia.myservice"));
        Ok(())
    }

    #[test]
    fn test_generate_selector() {
        assert_eq!(
            generate_selector(
                "/core/cobalt".to_string(),
                "fuchsia.net.http.Loader".to_string(),
                Node::In
            )
            .unwrap(),
            "core/cobalt:in:fuchsia.net.http.Loader"
        );
        assert_eq!(
            generate_selector(
                "INVALID_MONIKER".to_string(),
                "fuchsia.net.http.Loader".to_string(),
                Node::In
            )
            .unwrap_err()
            .to_string(),
            "Moniker could not be parsed: invalid moniker: INVALID_MONIKER".to_string()
        );
    }
}
