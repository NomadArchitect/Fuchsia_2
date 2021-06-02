// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_bluetooth_bredr as bredr;
use fuchsia_bluetooth::types::PeerId;
use fuchsia_zircon as zx;
use futures::{Future, StreamExt};
use log::trace;

use crate::error::ScoConnectError;
use crate::features::CodecId;

/// The components of an active SCO connection.
/// Dropping this struct will close the SCO connection.
pub struct ScoConnection {
    /// The parameters that this connection was set up with.
    pub params: bredr::ScoConnectionParameters,
    /// Socket which holds the connection open. Held so when this is dropped the connection closes.
    _socket: zx::Socket,
}

pub struct ScoConnector {
    proxy: bredr::ProfileProxy,
}

const COMMON_SCO_PARAMS: bredr::ScoConnectionParameters = bredr::ScoConnectionParameters {
    air_frame_size: Some(60), // Chosen to match legacy usage.
    // IO parameters are to fit 16-bit PSM Signed audio input expected from the audio chip.
    io_coding_format: Some(bredr::CodingFormat::LinearPcm),
    io_frame_size: Some(16),
    io_pcm_data_format: Some(fidl_fuchsia_hardware_audio::SampleFormat::PcmSigned),
    path: Some(bredr::DataPath::Offload),
    ..bredr::ScoConnectionParameters::EMPTY
};

/// If all eSCO parameters fail to setup a connection, these parameters are required to be
/// supported by all peers.  HFP 1.8 Section 5.7.1.
const SCO_PARAMS_FALLBACK: bredr::ScoConnectionParameters = bredr::ScoConnectionParameters {
    parameter_set: Some(bredr::HfpParameterSet::CvsdD1),
    air_coding_format: Some(bredr::CodingFormat::Cvsd),
    // IO bandwidth to match an 8khz audio rate.
    io_bandwidth: Some(16000),
    ..COMMON_SCO_PARAMS
};

fn parameters_for_codec(codec_id: CodecId) -> bredr::ScoConnectionParameters {
    match codec_id {
        CodecId::MSBC => bredr::ScoConnectionParameters {
            parameter_set: Some(bredr::HfpParameterSet::MsbcT2),
            air_coding_format: Some(bredr::CodingFormat::Msbc),
            // IO bandwidth to match an 16khz audio rate.
            io_bandwidth: Some(32000),
            ..COMMON_SCO_PARAMS
        },
        // CVSD fallback
        _ => bredr::ScoConnectionParameters {
            parameter_set: Some(bredr::HfpParameterSet::CvsdS4),
            ..SCO_PARAMS_FALLBACK
        },
    }
}

impl ScoConnector {
    pub fn build(proxy: bredr::ProfileProxy) -> Self {
        Self { proxy }
    }

    async fn initiate_sco(
        proxy: bredr::ProfileProxy,
        peer_id: PeerId,
        params: bredr::ScoConnectionParameters,
    ) -> Result<zx::Socket, ScoConnectError> {
        let (client, mut requests) =
            fidl::endpoints::create_request_stream::<bredr::ScoConnectionReceiverMarker>()?;

        proxy.connect_sco(&mut peer_id.into(), true, params.clone(), client)?;

        let socket = match requests.next().await {
            Some(Ok(bredr::ScoConnectionReceiverRequest::Connected { connection, .. })) => {
                connection.socket.ok_or(ScoConnectError::MissingSocket)?
            }
            Some(Ok(bredr::ScoConnectionReceiverRequest::Error { error, .. })) => {
                return Err(error.into())
            }
            Some(Err(e)) => return Err(e.into()),
            None => return Err(ScoConnectError::ScoCanceled),
        };
        Ok(socket)
    }

    pub fn connect(
        &self,
        peer_id: PeerId,
        codecs: Vec<CodecId>,
    ) -> impl Future<Output = Result<ScoConnection, ScoConnectError>> {
        let proxy = self.proxy.clone();
        async move {
            for codec in codecs {
                let params = parameters_for_codec(codec);
                match Self::initiate_sco(proxy.clone(), peer_id.clone(), params.clone()).await {
                    Ok(socket) => return Ok(ScoConnection { params, _socket: socket }),
                    Err(e) => {
                        trace!("Failed to connect SCO with {:?} ({:?}), trying others..", params, e)
                    }
                };
            }

            let params = SCO_PARAMS_FALLBACK.clone();
            match Self::initiate_sco(proxy, peer_id.clone(), params.clone()).await {
                Ok(socket) => Ok(ScoConnection { params, _socket: socket }),
                Err(e) => {
                    trace!("Failed to connect SCO with fallback ({:?}), failing..", e);
                    Err(ScoConnectError::ScoFailed)
                }
            }
        }
    }
}
