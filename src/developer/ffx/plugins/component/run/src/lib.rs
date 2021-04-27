// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::{anyhow, Context, Error, Result},
    blocking::Unblock,
    cs::{io::Directory, v2::V2Component, Subcommand},
    ffx_component_run_args::RunComponentCommand,
    ffx_core::ffx_plugin,
    fidl::endpoints::create_proxy,
    fidl_fuchsia_developer_remotecontrol as rc, fidl_fuchsia_io as fio,
    fidl_fuchsia_sys::{
        ComponentControllerEvent, ComponentControllerMarker, FileDescriptor, LaunchInfo,
        LauncherProxy, TerminationReason::*,
    },
    fuchsia_zircon_status::Status,
    futures::StreamExt,
    signal_hook::{consts::signal::SIGINT, iterator::Signals},
};

// TODO(fxbug.dev/53159): refactor fuchsia-runtime so we can use the constant from there on the host,
// rather than redefining it here.
const HANDLE_TYPE_FILE_DESCRIPTOR: i32 = 0x30;

async fn get_process_id(url: &str, rcs_proxy: rc::RemoteControlProxy) -> Result<Vec<u32>, Error> {
    let (root, dir_server) = fidl::endpoints::create_proxy::<fio::DirectoryMarker>()
        .context("creating hub root proxy")?;
    rcs_proxy
        .open_hub(dir_server)
        .await?
        .map_err(|i| Status::ok(i).unwrap_err())
        .context("opening hub")?;
    let hub_dir = Directory::from_proxy(root);
    let component = V2Component::explore(hub_dir, Subcommand::Show).await;
    let process_ids = component.get_process_id_recursive(url, &mut vec![]);
    Ok(process_ids)
}

#[ffx_plugin(LauncherProxy = "core/appmgr:out:fuchsia.sys.Launcher")]
pub async fn run_component(
    rcs_proxy: rc::RemoteControlProxy,
    launcher_proxy: LauncherProxy,
    run: RunComponentCommand,
) -> Result<()> {
    let process_ids = get_process_id(&run.url, rcs_proxy).await.expect("failed to get process_ids");
    run_component_cmd(launcher_proxy, process_ids, run).await
}

async fn run_component_cmd(
    launcher_proxy: LauncherProxy,
    process_ids: Vec<u32>,
    run: RunComponentCommand,
) -> Result<()> {
    let (control_proxy, control_server_end) = create_proxy::<ComponentControllerMarker>()?;
    let (sout, cout) =
        fidl::Socket::create(fidl::SocketOpts::STREAM).context("failed to create socket")?;
    let (serr, cerr) =
        fidl::Socket::create(fidl::SocketOpts::STREAM).context("failed to create socket")?;

    let mut stdout = Unblock::new(std::io::stdout());
    let mut stderr = Unblock::new(std::io::stderr());
    let copy_futures = futures::future::try_join(
        futures::io::copy(fidl::AsyncSocket::from_socket(cout)?, &mut stdout),
        futures::io::copy(fidl::AsyncSocket::from_socket(cerr)?, &mut stderr),
    );

    // This is only necessary until Overnet correctly handle setup for passed channels.
    // TODO(jwing) remove this once that is finished.
    control_proxy.detach().unwrap();

    let mut event_stream = control_proxy.take_event_stream();
    let term_event_future = async move {
        while let Some(result) = event_stream.next().await {
            match result? {
                ComponentControllerEvent::OnTerminated { return_code, termination_reason } => {
                    return Ok((return_code, termination_reason));
                }
                _ => {}
            }
        }
        Err(anyhow!("no termination event received"))
    };

    // Force an exit on interrupt.
    let mut signals = Signals::new(&[SIGINT]).unwrap();
    let handle = signals.handle();
    let thread = std::thread::spawn(move || {
        let mut kill_started = false;
        for signal in signals.forever() {
            match signal {
                SIGINT => {
                    if kill_started {
                        println!("\nCaught interrupt, killing remote component.");
                        let _ = control_proxy.kill();
                        kill_started = true;
                    } else {
                        // If for some reason the kill signal hangs, we want to give the user
                        // a way to exit ffx.
                        println!("Received second interrupt. Forcing exit...");
                        std::process::exit(0);
                    }
                }
                _ => unreachable!(),
            }
        }
    });

    let out_fd = FileDescriptor {
        type0: HANDLE_TYPE_FILE_DESCRIPTOR,
        type1: 0,
        type2: 0,
        handle0: Some(sout.into()),
        handle1: None,
        handle2: None,
    };

    let err_fd = FileDescriptor {
        type0: HANDLE_TYPE_FILE_DESCRIPTOR,
        type1: 0,
        type2: 0,
        handle0: Some(serr.into()),
        handle1: None,
        handle2: None,
    };

    let mut info = LaunchInfo {
        url: run.url.clone(),
        arguments: Some(run.args),
        out: Some(Box::new(out_fd)),
        err: Some(Box::new(err_fd)),
        additional_services: None,
        directory_request: None,
        flat_namespace: None,
    };

    launcher_proxy.create_component(&mut info, Some(control_server_end)).map_err(|_| {
        anyhow!(
            "Error starting component: {:?}. Ensure there is a target connected with `ffx list`"
        )
    })?;

    if run.background {
        if process_ids.len() > 0 {
            println!("Started component: {}\nProcess IDs: {:?}", run.url, process_ids);
        } else {
            println!("Started component: {}", run.url);
        }
        std::process::exit(0);
    } else {
        if process_ids.len() > 0 {
            println!(
                "Started component: {}\nProcess IDs: {:?}\nComponent stdout and stderr will be shown below. Press Ctrl+C to exit and kill the component.",
                run.url, process_ids
            );
        } else {
            println!("Started component: {}\nComponent stdout and stderr will be shown below. Press Ctrl+C to exit and kill the component.", run.url);
        }
    }

    let (copy_res, term_event) = futures::join!(copy_futures, term_event_future);
    copy_res?;

    let (exit_code, termination_reason) = term_event?;
    if termination_reason != Exited {
        let message = match termination_reason {
            Unknown => "Unknown",
            UrlInvalid => "Component URL is invalid",
            PackageNotFound => "Package could not be found. Ensure `fx serve` is running",
            InternalError => "Internal error",
            ProcessCreationError => "Process creation error",
            RunnerFailed => "Runner failed to start",
            RunnerTerminated => "Runner crashed",
            Unsupported => "Component uses unsupported feature",
            RealmShuttingDown => "Realm is shutting down. Can't create component",
            AccessDenied => "Component did not have sufficient access to run",
            Exited => unreachable!(),
        };
        eprintln!("Error: {}. \nThere may be a more detailed error in the system logs.", message);
    }

    // Shut down the signal thread.
    handle.close();
    thread.join().expect("thread to shutdown without panic");

    std::process::exit(exit_code as i32);
}

////////////////////////////////////////////////////////////////////////////////
// tests

#[cfg(test)]
mod test {
    use {super::*, fidl_fuchsia_sys::LauncherRequest};

    fn setup_fake_launcher_service() -> LauncherProxy {
        setup_oneshot_fake_launcher_proxy(|req| {
            match req {
                LauncherRequest::CreateComponent {
                    launch_info:
                        LaunchInfo {
                            url: _,
                            arguments: _,
                            out: _,
                            err: _,
                            additional_services: _,
                            directory_request: _,
                            flat_namespace: _,
                        },
                    controller,
                    control_handle: _,
                } => {
                    let (_, handle) = controller.unwrap().into_stream_and_control_handle().unwrap();
                    handle.send_on_terminated(0, Exited).unwrap();
                    // TODO: Add test coverage for FE behavior once fxbug.dev/49063 is resolved.
                }
            }
        })
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_run_component_cmd() -> Result<()> {
        let url = "fuchsia-pkg://fuchsia.com/test#meta/test.cmx".to_string();
        let args = vec!["test1".to_string(), "test2".to_string()];
        let background = true;
        let run_cmd = RunComponentCommand { url, args, background };
        let launcher_proxy = setup_fake_launcher_service();
        let process_ids = vec![12345];
        let _response = run_component_cmd(launcher_proxy, process_ids, run_cmd)
            .await
            .expect("getting tests should not fail");
        Ok(())
    }
}
