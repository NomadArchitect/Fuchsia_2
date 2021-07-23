// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::format_err,
    fidl::endpoints::Proxy,
    fidl_fuchsia_wlan_device as fidl_wlan_dev, fuchsia_async as fasync,
    fuchsia_vfs_watcher::{WatchEvent, Watcher},
    fuchsia_zircon::Status as zx_Status,
    futures::prelude::*,
    log::{error, warn},
    std::io,
    std::path::{Path, PathBuf},
    std::str::FromStr,
};

pub struct NewPhyDevice {
    pub id: u16,
    pub proxy: fidl_wlan_dev::PhyProxy,
    pub device: wlan_dev::Device,
}

pub fn watch_phy_devices<E: wlan_dev::DeviceEnv>(
) -> io::Result<impl Stream<Item = Result<NewPhyDevice, anyhow::Error>>> {
    Ok(watch_new_devices::<_, E>(E::PHY_PATH)?.try_filter_map(|path| {
        future::ready(Ok(handle_open_error(&path, new_phy::<E>(&path), "phy")))
    }))
}

fn handle_open_error<T>(
    path: &PathBuf,
    r: Result<T, anyhow::Error>,
    device_type: &'static str,
) -> Option<T> {
    if let Err(ref e) = &r {
        if let Some(&zx_Status::ALREADY_BOUND) = e.downcast_ref::<zx_Status>() {
            warn!("Cannot open already-bound device: {} '{}'", device_type, path.display())
        } else {
            error!("Error opening {} '{}': {}", device_type, path.display(), e)
        }
    }
    r.ok()
}

/// Watches a specified device directory for new WLAN PHYs.
///
/// When new entries are discovered in the specified directory the paths to the new devices are
/// sent along the stream that is returned by this function.
///
/// Note that a `DeviceEnv` trait is required in order for this function to work.  This enables
/// wlandevicemonitor to function in real and in simulated environments where devices are presented
/// differently.
///
/// # Arguments
///
/// * `path` - Path struct that represents the path to the device directory.
fn watch_new_devices<P: AsRef<Path>, E: wlan_dev::DeviceEnv>(
    path: P,
) -> io::Result<impl Stream<Item = Result<PathBuf, anyhow::Error>>> {
    let raw_dir = E::open_dir(&path)?;
    let zircon_channel = fdio::clone_channel(&raw_dir)?;
    let async_channel = fasync::Channel::from_channel(zircon_channel)?;
    let directory = fidl_fuchsia_io::DirectoryProxy::from_channel(async_channel);
    Ok(async move {
        let watcher = Watcher::new(directory).await?;
        Ok(watcher
            .try_filter_map(move |msg| {
                future::ready(Ok(match msg.event {
                    WatchEvent::EXISTING | WatchEvent::ADD_FILE => {
                        Some(path.as_ref().join(msg.filename))
                    }
                    _ => None,
                }))
            })
            .err_into())
    }
    .try_flatten_stream())
}

fn new_phy<E: wlan_dev::DeviceEnv>(path: &PathBuf) -> Result<NewPhyDevice, anyhow::Error> {
    let id = id_from_path(path)?;
    let device = E::device_from_path(path)?;
    let proxy = wlan_dev::connect_wlan_phy(&device)?;
    Ok(NewPhyDevice { id, proxy, device })
}

fn id_from_path(path: &PathBuf) -> Result<u16, anyhow::Error> {
    let file_name = path.file_name().ok_or_else(|| format_err!("Invalid device path"))?;
    let file_name_str =
        file_name.to_str().ok_or_else(|| format_err!("Filename is not valid UTF-8"))?;
    let id = u16::from_str(&file_name_str)
        .map_err(|e| format_err!("Failed to parse device filename as a numeric ID: {}", e))?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        fidl_fuchsia_wlan_common as fidl_common,
        fidl_fuchsia_wlan_device::{self as fidl_wlan_dev, SupportedPhy},
        fidl_fuchsia_wlan_internal as fidl_internal, fidl_fuchsia_wlan_tap as fidl_wlantap,
        fuchsia_zircon::prelude::*,
        futures::{poll, task::Poll},
        pin_utils::pin_mut,
        std::convert::TryInto,
        wlan_common::{ie::*, test_utils::ExpectWithin},
        wlantap_client,
        zerocopy::AsBytes,
    };

    #[cfg(feature = "v2")]
    use {fidl_fuchsia_io::DirectoryProxy, wlan_dev::DeviceEnv};

    #[cfg(not(feature = "v2"))]
    use isolated_devmgr::IsolatedDeviceEnv;

    // In Component Framework v1, the isolated device manager build rule allowed for a flag that
    // would prevent serving /dev until a certain file enumerated.  In WLAN's case, that file was
    // /dev/test/wlantapctl.  In Components Framework v2, we need to manually wait until
    // /dev/test/wlantapctl appears before attempting to create a WLAN PHY device.
    #[cfg(feature = "v2")]
    async fn wait_for_file(dir: &DirectoryProxy, name: &str) -> Result<(), anyhow::Error> {
        let mut watcher = fuchsia_vfs_watcher::Watcher::new(io_util::clone_directory(
            dir,
            fidl_fuchsia_io::OPEN_RIGHT_READABLE,
        )?)
        .await?;
        while let Some(msg) = watcher.try_next().await? {
            if msg.event != fuchsia_vfs_watcher::WatchEvent::EXISTING
                && msg.event != fuchsia_vfs_watcher::WatchEvent::ADD_FILE
            {
                continue;
            }
            if msg.filename.to_str().unwrap() == name {
                return Ok(());
            }
        }
        unreachable!();
    }

    #[cfg(feature = "v2")]
    async fn recursive_open_node(
        initial_dir: &DirectoryProxy,
        name: &str,
    ) -> Result<fidl_fuchsia_io::NodeProxy, anyhow::Error> {
        let mut dir = io_util::clone_directory(initial_dir, fidl_fuchsia_io::OPEN_RIGHT_READABLE)?;

        let path = std::path::Path::new(name);
        let components = path.components().collect::<Vec<_>>();

        for i in 0..(components.len() - 1) {
            let component = &components[i];
            match component {
                std::path::Component::Normal(file) => {
                    wait_for_file(&dir, file.to_str().unwrap()).await?;
                    dir = io_util::open_directory(
                        &dir,
                        std::path::Path::new(file),
                        io_util::OPEN_RIGHT_READABLE,
                    )?;
                }
                _ => panic!("Path must contain only normal components"),
            }
        }
        match components[components.len() - 1] {
            std::path::Component::Normal(file) => {
                wait_for_file(&dir, file.to_str().unwrap()).await?;
                io_util::open_node(
                    &dir,
                    std::path::Path::new(file),
                    fidl_fuchsia_io::OPEN_RIGHT_READABLE | fidl_fuchsia_io::OPEN_RIGHT_WRITABLE,
                    fidl_fuchsia_io::MODE_TYPE_SERVICE,
                )
            }
            _ => panic!("Path must contain only normal components"),
        }
    }

    // TODO(78050): When all WLAN components migrate to Component Framework v2 and the v1 manifests
    // are deprecated, enable this test.
    #[test]
    #[cfg(feature = "v2")]
    fn watch_phys() {
        let mut exec = fasync::TestExecutor::new().expect("Failed to create an executor");
        let phy_watcher =
            watch_phy_devices::<wlan_dev::RealDeviceEnv>().expect("Failed to create phy_watcher");
        pin_mut!(phy_watcher);

        // Wait for the wlantap to appear.
        let raw_dir = wlan_dev::RealDeviceEnv::open_dir("/dev").expect("failed to open /dev/test");
        let zircon_channel =
            fdio::clone_channel(&raw_dir).expect("failed to clone directory channel");
        let async_channel = fasync::Channel::from_channel(zircon_channel)
            .expect("failed to create async channel from zircon channel");
        let dir = fidl_fuchsia_io::DirectoryProxy::from_channel(async_channel);
        let monitor_fut = recursive_open_node(&dir, "test/wlantapctl");
        pin_mut!(monitor_fut);
        exec.run_singlethreaded(async {
            monitor_fut
                .expect_within(5.seconds(), "wlantapctl monitor never finished")
                .await
                .expect("error while watching for wlantapctl")
        });

        // Now that the wlantapctl device is present, connect to it.
        let wlantap = wlantap_client::Wlantap::open().expect("Failed to connect to wlantapctl");

        // Create an intentionally unused variable instead of a plain
        // underscore. Otherwise, this end of the channel will be
        // dropped and cause the phy device to begin unbinding.
        let _wlantap_phy =
            wlantap.create_phy(create_wlantap_config()).expect("failed to create PHY");
        exec.run_singlethreaded(async {
            phy_watcher
                .next()
                .expect_within(5.seconds(), "phy_watcher did not respond")
                .await
                .expect("phy_watcher ended without yielding a phy")
                .expect("phy_watcher returned an error");
            if let Poll::Ready(..) = poll!(phy_watcher.next()) {
                panic!("phy_watcher found more than one phy");
            }
        })
    }

    #[test]
    #[cfg(not(feature = "v2"))]
    fn watch_phys() {
        let mut exec = fasync::TestExecutor::new().expect("Failed to create an executor");
        let phy_watcher =
            watch_phy_devices::<IsolatedDeviceEnv>().expect("Failed to create phy_watcher");
        pin_mut!(phy_watcher);
        let wlantap = wlantap_client::Wlantap::open_from_isolated_devmgr()
            .expect("Failed to connect to wlantapctl");
        // Create an intentionally unused variable instead of a plain
        // underscore. Otherwise, this end of the channel will be
        // dropped and cause the phy device to begin unbinding.
        let _wlantap_phy = wlantap.create_phy(create_wlantap_config());
        exec.run_singlethreaded(async {
            phy_watcher
                .next()
                .expect_within(5.seconds(), "phy_watcher did not respond")
                .await
                .expect("phy_watcher ended without yielding a phy")
                .expect("phy_watcher returned an error");
            if let Poll::Ready(..) = poll!(phy_watcher.next()) {
                panic!("phy_watcher found more than one phy");
            }
        })
    }

    #[test]
    fn handle_open_succeeds() {
        assert!(handle_open_error(&PathBuf::new(), Ok(()), "phy").is_some())
    }

    #[test]
    fn handle_open_fails() {
        assert!(handle_open_error::<()>(&PathBuf::new(), Err(format_err!("test failure")), "phy")
            .is_none())
    }

    fn create_wlantap_config() -> fidl_wlantap::WlantapPhyConfig {
        fidl_wlantap::WlantapPhyConfig {
            iface_mac_addr: [1; 6],
            supported_phys: vec![
                SupportedPhy::Dsss,
                SupportedPhy::Cck,
                SupportedPhy::Ofdm,
                SupportedPhy::Ht,
            ],
            driver_features: vec![],
            mac_role: fidl_wlan_dev::MacRole::Client,
            caps: vec![],
            bands: vec![create_2_4_ghz_band_info()],
            name: String::from("devwatchtap"),
            quiet: false,
        }
    }

    fn create_2_4_ghz_band_info() -> fidl_wlan_dev::BandInfo {
        fidl_wlan_dev::BandInfo {
            band_id: fidl_common::Band::WlanBand2Ghz,
            ht_caps: Some(Box::new(fidl_internal::HtCapabilities {
                bytes: fake_ht_capabilities().as_bytes().try_into().unwrap(),
            })),
            vht_caps: None,
            rates: vec![2, 4, 11, 22, 12, 18, 24, 36, 48, 72, 96, 108],
            supported_channels: fidl_wlan_dev::ChannelList {
                base_freq: 2407,
                channels: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14],
            },
        }
    }
}
