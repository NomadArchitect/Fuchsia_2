// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::{Context as _, Error},
    fidl_fuchsia_developer_remotecontrol::{RemoteControlMarker, RemoteControlProxy},
    fuchsia_async as fasync,
    fuchsia_component::client::connect_to_service,
    futures::{future::try_join, prelude::*},
    hoist::{hoist, OvernetInstance},
    std::io::{Read, Write},
};

async fn copy_stdin_to_socket(
    mut tx_socket: futures::io::WriteHalf<fidl::AsyncSocket>,
) -> Result<(), Error> {
    let (mut tx_stdin, mut rx_stdin) = futures::channel::mpsc::channel::<Vec<u8>>(2);
    std::thread::Builder::new()
        .spawn(move || -> Result<(), Error> {
            let mut buf = [0u8; 1024];
            let mut stdin = std::io::stdin();
            loop {
                let n = stdin.read(&mut buf)?;
                if n == 0 {
                    return Ok(());
                }
                let buf = &buf[..n];
                futures::executor::block_on(tx_stdin.send(buf.to_vec()))?;
            }
        })
        .context("Spawning blocking thread")?;
    while let Some(buf) = rx_stdin.next().await {
        tx_socket.write(buf.as_slice()).await?;
    }
    Ok(())
}

async fn copy_socket_to_stdout(
    mut rx_socket: futures::io::ReadHalf<fidl::AsyncSocket>,
) -> Result<(), Error> {
    let (mut tx_stdout, mut rx_stdout) = futures::channel::mpsc::channel::<Vec<u8>>(2);
    std::thread::Builder::new()
        .spawn(move || -> Result<(), Error> {
            let mut stdout = std::io::stdout();
            while let Some(buf) = futures::executor::block_on(rx_stdout.next()) {
                let mut buf = buf.as_slice();
                loop {
                    let n = stdout.write(buf)?;
                    if n == buf.len() {
                        stdout.flush()?;
                        break;
                    }
                    buf = &buf[n..];
                }
            }
            Ok(())
        })
        .context("Spawning blocking thread")?;
    let mut buf = [0u8; 1024];
    loop {
        let n = rx_socket.read(&mut buf).await?;
        tx_stdout.send((&buf[..n]).to_vec()).await?;
    }
}

async fn send_request(proxy: &RemoteControlProxy, id: Option<u64>) -> Result<(), Error> {
    // If the program was launched with a u64, that's our ffx daemon ID, so add it to RCS.
    // The daemon id is used to map the RCS instance back to an ip address or
    // nodename in the daemon, for target merging.
    if let Some(id) = id {
        proxy.add_id(id).await.with_context(|| format!("Failed to add id {} to RCS", id))
    } else {
        // We just need to make a request to the RCS - it doesn't really matter
        // what we choose here so long as there are no side effects.
        let _ = proxy.identify_host().await?;
        Ok(())
    }
}

fn get_id_argument<I>(mut args: I) -> Result<Option<u64>, Error>
where
    I: Iterator<Item = String>,
{
    // Assume the first argument is the name of the binary.
    let _ = args.next();
    if let Some(arg) = args.next() {
        arg.parse::<u64>()
            .with_context(|| format!("Failed to parse {} as u64", arg))
            .map(|n| Some(n))
    } else {
        Ok(None)
    }
}

#[fasync::run_singlethreaded]
async fn main() -> Result<(), Error> {
    let rcs_proxy = connect_to_service::<RemoteControlMarker>()?;
    send_request(&rcs_proxy, get_id_argument(std::env::args())?).await?;
    let (local_socket, remote_socket) = fidl::Socket::create(fidl::SocketOpts::STREAM)?;
    let local_socket = fidl::AsyncSocket::from_socket(local_socket)?;
    let (rx_socket, tx_socket) = futures::AsyncReadExt::split(local_socket);
    hoist().connect_as_mesh_controller()?.attach_socket_link(remote_socket)?;
    try_join(copy_socket_to_stdout(rx_socket), copy_stdin_to_socket(tx_socket)).await?;

    Ok(())
}

#[cfg(test)]
mod test {

    use {
        super::*,
        anyhow::Error,
        fidl::endpoints::create_proxy_and_stream,
        fidl_fuchsia_developer_remotecontrol::{
            IdentifyHostResponse, RemoteControlMarker, RemoteControlProxy, RemoteControlRequest,
        },
        fuchsia_async as fasync,
        std::cell::RefCell,
        std::rc::Rc,
    };

    #[test]
    fn test_get_id_argument() {
        assert_eq!(
            get_id_argument(vec!["foo".to_string(), "1234".to_string()].into_iter()).unwrap(),
            Some(1234u64)
        );
        assert_eq!(
            get_id_argument(vec!["foo".to_string(), "4567".to_string()].into_iter()).unwrap(),
            Some(4567u64)
        );
        assert!(get_id_argument(vec!["foo".to_string(), "foo".to_string()].into_iter()).is_err());
    }

    fn setup_fake_rcs(handle_stream: bool) -> RemoteControlProxy {
        let (proxy, mut stream) = create_proxy_and_stream::<RemoteControlMarker>().unwrap();

        if !handle_stream {
            return proxy;
        }

        fasync::Task::local(async move {
            let last_id = Rc::new(RefCell::new(0));
            while let Ok(req) = stream.try_next().await {
                match req {
                    Some(RemoteControlRequest::IdentifyHost { responder }) => {
                        let _ = responder
                            .send(&mut Ok(IdentifyHostResponse {
                                nodename: Some("".to_string()),
                                addresses: Some(vec![]),
                                ids: Some(vec![last_id.borrow().clone()]),
                                ..IdentifyHostResponse::EMPTY
                            }))
                            .unwrap();
                    }
                    Some(RemoteControlRequest::AddId { id, responder }) => {
                        last_id.replace(id);
                        responder.send().unwrap();
                    }
                    _ => assert!(false),
                }
            }
        })
        .detach();

        proxy
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_handles_successful_response() -> Result<(), Error> {
        let rcs_proxy = setup_fake_rcs(true);
        assert!(send_request(&rcs_proxy, None).await.is_ok());
        Ok(())
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_handles_failed_response() -> Result<(), Error> {
        let rcs_proxy = setup_fake_rcs(false);
        assert!(send_request(&rcs_proxy, None).await.is_err());
        Ok(())
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_sends_id_if_given() -> Result<(), Error> {
        let rcs_proxy = setup_fake_rcs(true);
        send_request(&rcs_proxy, Some(34u64)).await.unwrap();
        let ident = rcs_proxy.identify_host().await?.unwrap();
        assert_eq!(34u64, ident.ids.unwrap()[0]);
        Ok(())
    }
}
