// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::audio::types::{AudioSettingSource, AudioStream, AudioStreamType},
    crate::base::{SettingInfo, SettingType},
    crate::fidl_hanging_get_responder,
    crate::fidl_process,
    crate::fidl_processor::settings::RequestContext,
    crate::handler::base::Request,
    crate::request_respond,
    crate::switchboard::base::FidlResponseErrorLogger,
    fidl::endpoints::ServiceMarker,
    fidl_fuchsia_media::AudioRenderUsage,
    fidl_fuchsia_settings::{
        AudioInput, AudioMarker, AudioRequest, AudioSettings, AudioStreamSettingSource,
        AudioStreamSettings, AudioWatchResponder, Volume,
    },
    fuchsia_async as fasync,
    fuchsia_syslog::fx_log_err,
};

fidl_hanging_get_responder!(AudioMarker, AudioSettings, AudioWatchResponder,);

impl From<SettingInfo> for AudioSettings {
    fn from(response: SettingInfo) -> Self {
        if let SettingInfo::Audio(info) = response {
            let mut streams = Vec::new();
            for stream in info.streams.iter() {
                streams.push(AudioStreamSettings::from(stream.clone()));
            }

            let mut audio_input = AudioInput::EMPTY;
            audio_input.muted = Some(info.input.mic_mute);

            let mut audio_settings = AudioSettings::EMPTY;
            audio_settings.streams = Some(streams);
            audio_settings.input = Some(audio_input);
            audio_settings
        } else {
            panic!("incorrect value sent to audio");
        }
    }
}

impl From<AudioStream> for AudioStreamSettings {
    fn from(stream: AudioStream) -> Self {
        AudioStreamSettings {
            stream: Some(AudioRenderUsage::from(stream.stream_type)),
            source: Some(AudioStreamSettingSource::from(stream.source)),
            user_volume: Some(Volume {
                level: Some(stream.user_volume_level),
                muted: Some(stream.user_volume_muted),
                ..Volume::EMPTY
            }),
            ..AudioStreamSettings::EMPTY
        }
    }
}

impl From<AudioRenderUsage> for AudioStreamType {
    fn from(usage: AudioRenderUsage) -> Self {
        match usage {
            AudioRenderUsage::Background => AudioStreamType::Background,
            AudioRenderUsage::Media => AudioStreamType::Media,
            AudioRenderUsage::Interruption => AudioStreamType::Interruption,
            AudioRenderUsage::SystemAgent => AudioStreamType::SystemAgent,
            AudioRenderUsage::Communication => AudioStreamType::Communication,
        }
    }
}

impl From<AudioStreamType> for AudioRenderUsage {
    fn from(usage: AudioStreamType) -> Self {
        match usage {
            AudioStreamType::Background => AudioRenderUsage::Background,
            AudioStreamType::Media => AudioRenderUsage::Media,
            AudioStreamType::Interruption => AudioRenderUsage::Interruption,
            AudioStreamType::SystemAgent => AudioRenderUsage::SystemAgent,
            AudioStreamType::Communication => AudioRenderUsage::Communication,
        }
    }
}

impl From<AudioStreamSettingSource> for AudioSettingSource {
    fn from(source: AudioStreamSettingSource) -> Self {
        match source {
            AudioStreamSettingSource::User => AudioSettingSource::User,
            AudioStreamSettingSource::System => AudioSettingSource::System,
        }
    }
}

impl From<AudioSettingSource> for AudioStreamSettingSource {
    fn from(source: AudioSettingSource) -> Self {
        match source {
            AudioSettingSource::User => AudioStreamSettingSource::User,
            AudioSettingSource::System => AudioStreamSettingSource::System,
        }
    }
}

#[derive(thiserror::Error, Debug)]
enum Error {
    #[error("missing user_volume at stream {0}")]
    NoUserVolume(usize),
    #[error("missing user_volume.level at stream {0}")]
    NoUserVolumeLevel(usize),
    #[error("missing user_volume.muted at stream {0}")]
    NoUserVolumeMuted(usize),
    #[error("missing stream at stream {0}")]
    NoStreamType(usize),
    #[error("missing source at stream {0}")]
    NoSource(usize),
}

fn to_request(settings: AudioSettings) -> Option<Result<Request, Error>> {
    settings.streams.map(|streams| {
        streams
            .into_iter()
            .enumerate()
            .map(|(i, stream)| {
                let user_volume = stream.user_volume.ok_or_else(|| Error::NoUserVolume(i))?;
                let user_volume_level =
                    user_volume.level.ok_or_else(|| Error::NoUserVolumeLevel(i))?;
                let user_volume_muted =
                    user_volume.muted.ok_or_else(|| Error::NoUserVolumeMuted(i))?;
                let stream_type = stream.stream.ok_or_else(|| Error::NoStreamType(i))?.into();
                let source = stream.source.ok_or_else(|| Error::NoSource(i))?.into();
                Ok(AudioStream { stream_type, source, user_volume_level, user_volume_muted })
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Request::SetVolume)
    })
}

fidl_process!(Audio, SettingType::Audio, process_request,);

async fn process_request(
    context: RequestContext<AudioSettings, AudioWatchResponder>,
    req: AudioRequest,
) -> Result<Option<AudioRequest>, anyhow::Error> {
    // Support future expansion of FIDL.
    #[allow(unreachable_patterns)]
    match req {
        AudioRequest::Set { settings, responder } => {
            if let Some(request) = to_request(settings) {
                match request {
                    Ok(request) => fasync::Task::spawn(async move {
                        request_respond!(
                            context,
                            responder,
                            SettingType::Audio,
                            request,
                            Ok(()),
                            Err(fidl_fuchsia_settings::Error::Failed),
                            AudioMarker
                        );
                    })
                    .detach(),
                    Err(err) => {
                        fx_log_err!(
                            "{}: Failed to process request: {:?}",
                            AudioMarker::DEBUG_NAME,
                            err
                        );
                        responder
                            .send(&mut Err(fidl_fuchsia_settings::Error::Failed))
                            .log_fidl_response_error(AudioMarker::DEBUG_NAME);
                    }
                }
            } else {
                responder
                    .send(&mut Err(fidl_fuchsia_settings::Error::Unsupported))
                    .log_fidl_response_error(AudioMarker::DEBUG_NAME);
            }
        }
        AudioRequest::Watch { responder } => {
            context.watch(responder, true).await;
        }
        _ => {
            return Ok(Some(req));
        }
    }
    return Ok(None);
}
