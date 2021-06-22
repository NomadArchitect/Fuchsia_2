// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{
        file::FileResolver,
        manifest::{
            v1::FlashManifest as FlashManifestV1, v2::FlashManifest as FlashManifestV2,
            v3::FlashManifest as FlashManifestV3,
        },
    },
    anyhow::{anyhow, bail, Context, Error, Result},
    async_trait::async_trait,
    chrono::{DateTime, Duration, Utc},
    errors::ffx_bail,
    ffx_flash_args::FlashCommand,
    fidl::{
        endpoints::{create_endpoints, ServerEnd},
        Error as FidlError,
    },
    fidl_fuchsia_developer_bridge::{
        FastbootProxy, RebootListenerMarker, RebootListenerRequest, UploadProgressListenerMarker,
        UploadProgressListenerRequest,
    },
    futures::prelude::*,
    futures::try_join,
    serde::{Deserialize, Serialize},
    serde_json::{from_value, Value},
    std::io::{Read, Write},
    termion::{color, style},
};

pub(crate) mod v1;
pub(crate) mod v2;
pub(crate) mod v3;

pub(crate) const UNKNOWN_VERSION: &str = "Unknown flash manifest version";
const LARGE_FILE: &str = "large file, please wait... ";
const REVISION_VAR: &str = "hw-revision";

#[async_trait(?Send)]
pub(crate) trait Flash {
    async fn flash<W, F>(
        &self,
        writer: &mut W,
        file_resolver: &mut F,
        fastboot_proxy: FastbootProxy,
        cmd: FlashCommand,
    ) -> Result<()>
    where
        W: Write + Send,
        F: FileResolver + Send + Sync;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ManifestFile {
    version: u64,
    manifest: Value,
}

pub(crate) enum FlashManifest {
    V1(FlashManifestV1),
    V2(FlashManifestV2),
    V3(FlashManifestV3),
}

impl FlashManifest {
    pub(crate) fn load<R: Read>(reader: R) -> Result<Self> {
        let value: Value = serde_json::from_reader::<R, Value>(reader)
            .context("reading flash manifest from disk")?;
        // GN generated JSON always comes from a list
        let manifest: ManifestFile = match value {
            Value::Array(v) => from_value(v[0].clone())?,
            Value::Object(_) => from_value(value)?,
            _ => ffx_bail!("Could not parse flash manifest."),
        };
        match manifest.version {
            1 => Ok(Self::V1(from_value(manifest.manifest.clone())?)),
            2 => Ok(Self::V2(from_value(manifest.manifest.clone())?)),
            3 => Ok(Self::V3(from_value(manifest.manifest.clone())?)),
            _ => ffx_bail!("{}", UNKNOWN_VERSION),
        }
    }
}

#[async_trait(?Send)]
impl Flash for FlashManifest {
    async fn flash<W, F>(
        &self,
        writer: &mut W,
        file_resolver: &mut F,
        fastboot_proxy: FastbootProxy,
        cmd: FlashCommand,
    ) -> Result<()>
    where
        W: Write + Send,
        F: FileResolver + Send + Sync,
    {
        let total_time = Utc::now();
        prepare(writer, &fastboot_proxy).await?;
        match self {
            Self::V1(v) => v.flash(writer, file_resolver, fastboot_proxy, cmd).await?,
            Self::V2(v) => v.flash(writer, file_resolver, fastboot_proxy, cmd).await?,
            Self::V3(v) => v.flash(writer, file_resolver, fastboot_proxy, cmd).await?,
        };
        let duration = Utc::now().signed_duration_since(total_time);
        writeln!(
            writer,
            "{}Total Time{} [{}{:.2}s{}]",
            color::Fg(color::Green),
            style::Reset,
            color::Fg(color::Blue),
            (duration.num_milliseconds() as f32) / (1000 as f32),
            style::Reset
        )?;
        Ok(())
    }
}

pub(crate) fn map_fidl_error(e: FidlError) -> Error {
    log::error!("FIDL Communication error: {}", e);
    anyhow!(
        "There was an error communcation with the daemon. Try running\n\
        `ffx doctor` for further diagnositcs."
    )
}

pub(crate) fn done_time<W: Write + Send>(
    writer: &mut W,
    duration: Duration,
) -> std::io::Result<()> {
    writeln!(
        writer,
        "{}Done{} [{}{:.2}s{}]",
        color::Fg(color::Green),
        style::Reset,
        color::Fg(color::Blue),
        (duration.num_milliseconds() as f32) / (1000 as f32),
        style::Reset
    )?;
    writer.flush()
}

async fn handle_upload_progress_for_staging<W: Write + Send>(
    writer: &mut W,
    prog_server: ServerEnd<UploadProgressListenerMarker>,
) -> Result<Option<DateTime<Utc>>> {
    let mut stream = prog_server.into_stream()?;
    let mut start_time: Option<DateTime<Utc>> = None;
    let mut finish_time: Option<DateTime<Utc>> = None;
    loop {
        match stream.try_next().await? {
            Some(UploadProgressListenerRequest::OnStarted { size, .. }) => {
                start_time.replace(Utc::now());
                log::debug!("Upload started: {}", size);
                write!(writer, "Uploading... ")?;
                if size > (1 << 24) {
                    write!(writer, "{}", LARGE_FILE)?;
                }
                writer.flush()?;
            }
            Some(UploadProgressListenerRequest::OnFinished { .. }) => {
                if let Some(st) = start_time {
                    let d = Utc::now().signed_duration_since(st);
                    log::debug!("Upload duration: {:.2}s", (d.num_milliseconds() / 1000));
                    done_time(writer, d)?;
                } else {
                    // Write done without the time .
                    writeln!(writer, "{}Done{}", color::Fg(color::Green), style::Reset)?;
                    writer.flush()?;
                }
                finish_time.replace(Utc::now());
                log::debug!("Upload finished");
            }
            Some(UploadProgressListenerRequest::OnError { error, .. }) => {
                log::error!("{}", error);
                bail!(error)
            }
            Some(UploadProgressListenerRequest::OnProgress { bytes_written, .. }) => {
                log::debug!("Upload progress: {}", bytes_written);
            }
            None => return Ok(finish_time),
        }
    }
}

async fn handle_upload_progress_for_flashing<W: Write + Send>(
    name: &str,
    writer: &mut W,
    prog_server: ServerEnd<UploadProgressListenerMarker>,
) -> Result<Option<DateTime<Utc>>> {
    let mut stream = prog_server.into_stream()?;
    let mut start_time: Option<DateTime<Utc>> = None;
    let mut finish_time: Option<DateTime<Utc>> = None;
    let mut is_large: bool = false;
    loop {
        match stream.try_next().await? {
            Some(UploadProgressListenerRequest::OnStarted { size, .. }) => {
                start_time.replace(Utc::now());
                log::debug!("Upload started: {}", size);
                write!(writer, "Uploading... ")?;
                if size > (1 << 24) {
                    is_large = true;
                    write!(writer, "{}", LARGE_FILE)?;
                }
                writer.flush()?;
            }
            Some(UploadProgressListenerRequest::OnFinished { .. }) => {
                if let Some(st) = start_time {
                    let d = Utc::now().signed_duration_since(st);
                    log::debug!("Upload duration: {:.2}s", (d.num_milliseconds() / 1000));
                    done_time(writer, d)?;
                } else {
                    // Write done without the time .
                    writeln!(writer, "{}Done{}", color::Fg(color::Green), style::Reset)?;
                    writer.flush()?;
                }
                write!(writer, "Partitioning {}... ", name)?;
                if is_large {
                    write!(writer, "{}", LARGE_FILE)?;
                }
                writer.flush()?;
                finish_time.replace(Utc::now());
                log::debug!("Upload finished");
            }
            Some(UploadProgressListenerRequest::OnError { error, .. }) => {
                log::error!("{}", error);
                ffx_bail!("{}", error)
            }
            Some(UploadProgressListenerRequest::OnProgress { bytes_written, .. }) => {
                log::debug!("Upload progress: {}", bytes_written);
            }
            None => return Ok(finish_time),
        }
    }
}

pub(crate) async fn stage_file<W: Write + Send, F: FileResolver + Send + Sync>(
    writer: &mut W,
    file_resolver: &mut F,
    resolve: bool,
    file: &str,
    fastboot_proxy: &FastbootProxy,
) -> Result<()> {
    let (prog_client, prog_server) = create_endpoints::<UploadProgressListenerMarker>()?;
    let file_to_upload = if resolve {
        file_resolver.get_file(writer, file).context("reconciling file for upload")?
    } else {
        file.to_string()
    };
    writeln!(writer, "Preparing to stage {}", file_to_upload)?;
    try_join!(
        fastboot_proxy.stage(&file_to_upload, prog_client).map_err(map_fidl_error),
        handle_upload_progress_for_staging(writer, prog_server),
    )
    .and_then(|(stage, _)| {
        stage.map_err(|e| anyhow!("There was an error staging {}: {:?}", file_to_upload, e))
    })
}

pub(crate) async fn flash_partition<W: Write + Send, F: FileResolver + Send + Sync>(
    writer: &mut W,
    file_resolver: &mut F,
    name: &str,
    file: &str,
    fastboot_proxy: &FastbootProxy,
) -> Result<()> {
    let (prog_client, prog_server) = create_endpoints::<UploadProgressListenerMarker>()?;
    let file_to_upload =
        file_resolver.get_file(writer, file).context("reconciling file for upload")?;
    writeln!(writer, "Preparing to upload {}", file_to_upload)?;
    try_join!(
        fastboot_proxy.flash(name, &file_to_upload, prog_client).map_err(map_fidl_error),
        handle_upload_progress_for_flashing(name, writer, prog_server),
    )
    .and_then(|(flash, prog)| {
        if let Some(p) = prog {
            let d = Utc::now().signed_duration_since(p);
            log::debug!("Partition duration: {:.2}s", (d.num_milliseconds() / 1000));
            done_time(writer, d)?;
        } else {
            // Write a line break otherwise
            writeln!(writer, "{}Done{}", color::Fg(color::Green), style::Reset)?;
            writer.flush()?;
        }
        flash.map_err(|e| {
            anyhow!("There was an error flashing \"{}\" - {}: {:?}", name, file_to_upload, e)
        })
    })
}

pub(crate) async fn verify_hardware(
    revision: &String,
    fastboot_proxy: &FastbootProxy,
) -> Result<()> {
    let rev = fastboot_proxy
        .get_var(REVISION_VAR)
        .await
        .map_err(map_fidl_error)?
        .map_err(|e| anyhow!("Communication error with the device: {:?}", e))?;
    if let Some(r) = rev.split("-").next() {
        if r != *revision {
            ffx_bail!("Hardware mismatch! Trying to flash images built for {}", revision);
        }
    } else {
        ffx_bail!("Could not verify hardware revision of target device");
    }
    Ok(())
}

pub(crate) async fn verify_variable_value(
    var: &str,
    value: &str,
    fastboot_proxy: &FastbootProxy,
) -> Result<bool> {
    fastboot_proxy
        .get_var(var)
        .await
        .map_err(map_fidl_error)?
        .map_err(|e| anyhow!("Communication error with the device: {:?}", e))
        .map(|res| res == value)
}

pub(crate) async fn reboot_bootloader<W: Write + Send>(
    writer: &mut W,
    fastboot_proxy: &FastbootProxy,
) -> Result<()> {
    write!(writer, "Rebooting to bootloader... ")?;
    writer.flush()?;
    let (reboot_client, reboot_server) = create_endpoints::<RebootListenerMarker>()?;
    let mut stream = reboot_server.into_stream()?;
    let start_time = Utc::now();
    try_join!(fastboot_proxy.reboot_bootloader(reboot_client).map_err(map_fidl_error), async move {
        if let Some(RebootListenerRequest::OnReboot { control_handle: _ }) =
            stream.try_next().await?
        {
            Ok(())
        } else {
            bail!("Did not receive reboot signal");
        }
    })
    .and_then(|(reboot, _)| {
        let d = Utc::now().signed_duration_since(start_time);
        log::debug!("Reboot duration: {:.2}s", (d.num_milliseconds() / 1000));
        done_time(writer, d)?;
        reboot.map_err(|e| anyhow!("failed booting to bootloader: {:?}", e))
    })
}

pub(crate) async fn prepare<W: Write + Send>(
    writer: &mut W,
    fastboot_proxy: &FastbootProxy,
) -> Result<()> {
    writer.flush()?;
    let (reboot_client, reboot_server) = create_endpoints::<RebootListenerMarker>()?;
    let mut stream = reboot_server.into_stream()?;
    let mut start_time = None;
    writer.flush()?;
    try_join!(fastboot_proxy.prepare(reboot_client).map_err(map_fidl_error), async {
        if let Some(RebootListenerRequest::OnReboot { control_handle: _ }) =
            stream.try_next().await?
        {
            start_time.replace(Utc::now());
            write!(writer, "Rebooting to bootloader... ")?;
            writer.flush()?;
        }
        Ok(())
    })
    .and_then(|(prepare, _)| {
        if let Some(s) = start_time {
            let d = Utc::now().signed_duration_since(s);
            log::debug!("Reboot duration: {:.2}s", (d.num_milliseconds() / 1000));
            done_time(writer, d)?;
        }
        prepare.map_err(|e| anyhow!("failed preparing target for flashing: {:?}", e))
    })
}

////////////////////////////////////////////////////////////////////////////////
// tests

#[cfg(test)]
mod test {
    use super::*;
    use serde_json::from_str;
    use std::io::BufReader;

    const UNKNOWN_VERSION: &'static str = r#"{
        "version": 99999,
        "manifest": "test"
    }"#;

    const MANIFEST: &'static str = r#"{
        "version": 1,
        "manifest": []
    }"#;

    const ARRAY_MANIFEST: &'static str = r#"[{
        "version": 1,
        "manifest": []
    }]"#;

    #[test]
    fn test_deserialization() -> Result<()> {
        let _manifest: ManifestFile = from_str(MANIFEST)?;
        Ok(())
    }

    #[test]
    fn test_loading_unknown_version() {
        let manifest_contents = UNKNOWN_VERSION.to_string();
        let result = FlashManifest::load(BufReader::new(manifest_contents.as_bytes()));
        assert!(result.is_err());
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_loading_version_1() -> Result<()> {
        let manifest_contents = MANIFEST.to_string();
        FlashManifest::load(BufReader::new(manifest_contents.as_bytes())).map(|_| ())
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_loading_version_1_from_array() -> Result<()> {
        let manifest_contents = ARRAY_MANIFEST.to_string();
        FlashManifest::load(BufReader::new(manifest_contents.as_bytes())).map(|_| ())
    }
}
