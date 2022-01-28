// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// TODO Follow 2018 idioms
#![allow(elided_lifetimes_in_paths)]

//! Crate wlan-common hosts common libraries
//! to be used for WLAN SME, MLME, and binaries written in Rust.

#![cfg_attr(feature = "benchmark", feature(test))]
pub mod appendable;
pub mod big_endian;
pub mod bss;
pub mod buffer_reader;
pub mod buffer_writer;
pub mod channel;
pub mod data_writer;
#[allow(unused)]
pub mod energy;
pub mod error;
pub mod ewma_signal;
pub mod format;
pub mod hasher;
pub mod ie;
pub mod mac;
pub mod mgmt_writer;
pub mod organization;
pub mod scan;
pub mod security;
pub mod sequence;
pub mod signal_velocity;
pub mod sink;
#[allow(unused)]
pub mod stats;
#[cfg(target_os = "fuchsia")]
pub mod test_utils;
pub mod tim;
pub mod time;
#[cfg(target_os = "fuchsia")]
pub mod timer;
pub mod tx_vector;
pub mod unaligned_view;
pub mod wmm;

use {
    channel::{Cbw, Phy},
    fidl_fuchsia_wlan_sme as fidl_sme,
};

use std::fmt;
pub use time::TimeUnit;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RadioConfig {
    pub phy: Option<Phy>,
    pub cbw: Option<Cbw>,
    pub primary_channel: Option<u8>,
}

impl RadioConfig {
    pub fn new(phy: Phy, cbw: Cbw, primary_channel: u8) -> Self {
        RadioConfig { phy: Some(phy), cbw: Some(cbw), primary_channel: Some(primary_channel) }
    }

    // TODO(fxbug.dev/83769): Implement `From `instead.
    pub fn to_fidl(&self) -> fidl_sme::RadioConfig {
        let (channel_bandwidth, _) = self.cbw.or(Some(Cbw::Cbw20)).unwrap().to_fidl();
        fidl_sme::RadioConfig {
            override_phy: self.phy.is_some(),
            phy: self.phy.or(Some(Phy::Ht)).unwrap().to_fidl(),
            override_channel_bandwidth: self.cbw.is_some(),
            channel_bandwidth,
            override_primary_channel: self.primary_channel.is_some(),
            primary_channel: self.primary_channel.unwrap_or(0),
        }
    }

    pub fn from_fidl(radio_cfg: fidl_sme::RadioConfig) -> Self {
        RadioConfig {
            phy: if radio_cfg.override_phy { Some(Phy::from_fidl(radio_cfg.phy)) } else { None },
            cbw: if radio_cfg.override_channel_bandwidth {
                Some(Cbw::from_fidl(radio_cfg.channel_bandwidth, 0))
            } else {
                None
            },
            primary_channel: if radio_cfg.override_primary_channel {
                Some(radio_cfg.primary_channel)
            } else {
                None
            },
        }
    }
}

#[derive(Copy, Clone)]
pub enum StationMode {
    Client,
    Ap,
    Mesh,
}

impl fmt::Display for StationMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StationMode::Client => f.write_str("client"),
            StationMode::Ap => f.write_str("AP"),
            StationMode::Mesh => f.write_str("mesh"),
        }
    }
}
