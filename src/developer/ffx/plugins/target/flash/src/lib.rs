// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::manifest::{Flash, FlashManifest},
    anyhow::{Context, Result},
    ffx_config::file,
    ffx_core::{ffx_bail, ffx_plugin},
    ffx_flash_args::{FlashCommand, OemFile},
    fidl_fuchsia_developer_bridge::FastbootProxy,
    std::fs::File,
    std::io::{stdout, BufReader, Read, Write},
    std::path::Path,
};

mod manifest;

const SSH_OEM_COMMAND: &str = "add-staged-bootloader-file ssh.authorized_keys";

#[ffx_plugin()]
pub async fn flash(
    fastboot_proxy: FastbootProxy,
    // TODO(fxb/74841): remove allow attribute
    #[allow(unused_mut)] mut cmd: FlashCommand,
) -> Result<()> {
    let path = Path::new(&cmd.manifest);
    if !path.is_file() {
        ffx_bail!("Manifest \"{}\" is not a file.", cmd.manifest);
    }
    match cmd.ssh_key.as_ref() {
        Some(ssh) => {
            let ssh_file = Path::new(ssh);
            if !ssh_file.is_file() {
                ffx_bail!("SSH key \"{}\" is not a file.", ssh);
            }
            if cmd.oem_stage.iter().find(|f| f.command() == SSH_OEM_COMMAND).is_some() {
                ffx_bail!("Both the SSH key and the SSH OEM Stage flags were set. Only use one.");
            }
            cmd.oem_stage.push(OemFile::new(SSH_OEM_COMMAND.to_string(), ssh.to_string()));
        }
        None => {
            if cmd.oem_stage.iter().find(|f| f.command() == SSH_OEM_COMMAND).is_none() {
                let key: Option<String> = file("ssh.pub").await?;
                match key {
                    Some(k) => {
                        println!("No `--ssh-key` flag, using {}", k);
                        cmd.oem_stage.push(OemFile::new(SSH_OEM_COMMAND.to_string(), k));
                    }
                    None => ffx_bail!(
                        "Warning: flashing without a SSH key is not advised. \n\
                              Use the `--ssh-key` to pass a ssh key."
                    ),
                }
            }
        }
    }
    let mut writer = Box::new(stdout());
    let reader = File::open(path).context("opening file for read").map(BufReader::new)?;
    flash_impl(&mut writer, reader, fastboot_proxy, cmd).await
}

async fn flash_impl<W: Write + Send, R: Read>(
    mut writer: W,
    reader: R,
    fastboot_proxy: FastbootProxy,
    cmd: FlashCommand,
) -> Result<()> {
    FlashManifest::load(reader)?.flash(&mut writer, fastboot_proxy, cmd).await
}

////////////////////////////////////////////////////////////////////////////////
// tests

#[cfg(test)]
mod test {
    use super::*;
    use fidl_fuchsia_developer_bridge::FastbootRequest;
    use std::default::Default;
    use std::sync::{Arc, Mutex};
    use tempfile::NamedTempFile;

    #[derive(Default)]
    pub(crate) struct FakeServiceCommands {
        pub(crate) staged_files: Vec<String>,
        pub(crate) oem_commands: Vec<String>,
    }

    pub(crate) fn setup() -> (Arc<Mutex<FakeServiceCommands>>, FastbootProxy) {
        let state = Arc::new(Mutex::new(FakeServiceCommands { ..Default::default() }));
        (
            state.clone(),
            setup_fake_fastboot_proxy(move |req| match req {
                FastbootRequest::Flash { listener, responder, .. } => {
                    listener.into_proxy().unwrap().on_finished().unwrap();
                    responder.send(&mut Ok(())).unwrap();
                }
                FastbootRequest::Erase { responder, .. } => {
                    responder.send(&mut Ok(())).unwrap();
                }
                FastbootRequest::Reboot { responder } => {
                    responder.send(&mut Ok(())).unwrap();
                }
                FastbootRequest::RebootBootloader { listener, responder } => {
                    listener.into_proxy().unwrap().on_reboot().unwrap();
                    responder.send(&mut Ok(())).unwrap();
                }
                FastbootRequest::ContinueBoot { responder } => {
                    responder.send(&mut Ok(())).unwrap();
                }
                FastbootRequest::SetActive { responder, .. } => {
                    responder.send(&mut Ok(())).unwrap();
                }
                FastbootRequest::Stage { path, responder, .. } => {
                    let mut state = state.lock().unwrap();
                    state.staged_files.push(path);
                    responder.send(&mut Ok(())).unwrap();
                }
                FastbootRequest::Oem { command, responder } => {
                    let mut state = state.lock().unwrap();
                    state.oem_commands.push(command);
                    responder.send(&mut Ok(())).unwrap();
                }
            }),
        )
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_nonexistent_file_throws_err() {
        assert!(flash(
            setup().1,
            FlashCommand { manifest: "ffx_test_does_not_exist".to_string(), ..Default::default() }
        )
        .await
        .is_err())
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_nonexistent_ssh_file_throws_err() {
        let tmp_file = NamedTempFile::new().expect("tmp access failed");
        let tmp_file_name = tmp_file.path().to_string_lossy().to_string();
        assert!(flash(
            setup().1,
            FlashCommand {
                manifest: tmp_file_name,
                ssh_key: Some("ssh_does_not_exist".to_string()),
                ..Default::default()
            }
        )
        .await
        .is_err())
    }
}
