// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// TODO(andresoportus): Remove this as usage of it is added.
#![allow(dead_code)]

use {
    anyhow::{format_err, Error},
    fidl::endpoints::Proxy,
    fidl_fuchsia_hardware_audio::*,
    fidl_fuchsia_io as fio,
    futures::TryFutureExt,
    std::path::{Path, PathBuf},
};

pub struct CodecInterface {
    /// The proxy to the devfs "/dev".
    dev_proxy: fio::DirectoryProxy,
    /// The path under "/dev" used to connect to the device.
    path: PathBuf,
    /// The proxy to the device if connected.
    proxy: Option<CodecProxy>,
}

impl CodecInterface {
    /// A new interface that will connect to the device at the `path` within the `dev_proxy`
    /// directory. The interface is unconnected when created.
    pub fn new(dev_proxy: fio::DirectoryProxy, path: &Path) -> Self {
        Self { dev_proxy: dev_proxy, path: path.to_path_buf(), proxy: None }
    }

    /// Get the codec proxy.
    pub fn get_proxy(&self) -> Result<&CodecProxy, Error> {
        self.proxy.as_ref().ok_or(format_err!("Proxy not connected"))
    }

    /// Connect to the CodecInterface.
    pub fn connect(&mut self) -> Result<(), Error> {
        let path = self.path.to_str().ok_or(format_err!("invalid codec path"))?;
        let (codec_connect_proxy, codec_connect_server) =
            fidl::endpoints::create_proxy::<CodecConnectorMarker>()?;
        fdio::service_connect_at(
            self.dev_proxy.as_channel().as_ref(),
            path,
            codec_connect_server.into_channel(),
        )?;

        let (ours, theirs) = fidl::endpoints::create_proxy::<CodecMarker>()?;
        codec_connect_proxy.connect(theirs)?;

        self.proxy = Some(ours);
        Ok(())
    }

    /// Get information from the codec.
    pub async fn get_info(&self) -> Result<CodecInfo, Error> {
        self.get_proxy()?.clone().get_info().err_into().await
    }

    /// Reset codec.
    pub async fn reset(&self) -> Result<(), Error> {
        self.get_proxy()?.clone().reset().err_into().await
    }

    /// Set the gain state.
    pub async fn set_gain_state(&self, gain_state: GainState) -> Result<(), Error> {
        Ok(self.get_proxy()?.clone().set_gain_state(gain_state)?)
    }

    /// Get supported DAI formats.
    pub async fn get_dai_formats(&self) -> Result<DaiGetDaiFormatsResult, Error> {
        self.get_proxy()?.clone().get_dai_formats().err_into().await
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::config::Config,
        crate::configurator::Configurator,
        crate::discover::find_codecs,
        crate::testing::tests::get_dev_proxy,
        anyhow::{anyhow, Context},
        async_trait::async_trait,
        futures::lock::Mutex,
        std::sync::Arc,
    };

    pub struct TestConfigurator {}

    #[async_trait]
    impl Configurator for TestConfigurator {
        fn new(_config: Config) -> Result<Self, Error> {
            Ok(Self {})
        }

        async fn process_new_codec(
            &mut self,
            mut device: crate::codec::CodecInterface,
        ) -> Result<(), Error> {
            let _ = device.connect().context("Couldn't connect to codec")?;
            let info = device.get_info().await?;
            assert_eq!(info.unique_id, "123");
            assert_eq!(info.manufacturer, "456");
            assert_eq!(info.product_name, "789");

            let formats = device.get_dai_formats().await?.map_err(|e| anyhow!(e.to_string()))?;
            // We have 2 test codecs, one with good behavior (formats listed) and one with bad
            // behavior (empty formats), we only set_dai_formats for the the one that reported
            // at least one format. Hence, we return Ok(()) here.
            if formats.len() == 0
                || formats[0].number_of_channels.len() == 0
                || formats[0].sample_formats.len() == 0
                || formats[0].frame_formats.len() == 0
                || formats[0].frame_rates.len() == 0
                || formats[0].bits_per_slot.len() == 0
                || formats[0].bits_per_sample.len() == 0
            {
                return Ok(());
            }

            // Good test codec checks.
            assert_eq!(formats[0].number_of_channels[0], 2);
            Ok(())
        }

        async fn process_new_dai(
            &mut self,
            mut _device: crate::dai::DaiInterface,
        ) -> Result<(), Error> {
            Ok(())
        }

        fn serve_interface(&mut self) -> Result<Vec<fuchsia_async::Task<()>>, Error> {
            Ok(vec![])
        }
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_codec_api() -> Result<(), Error> {
        let (_realm_instance, dev_proxy) = get_dev_proxy("class/codec").await?;
        let config = Config::new()?;
        let configurator = Arc::new(Mutex::new(TestConfigurator::new(config)?));
        find_codecs(dev_proxy, 2, configurator).await?;
        Ok(())
    }
}
