// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {argh::FromArgs, ffx_core::ffx_command, ffx_power_sub_command::Subcommand};

#[ffx_command()]
#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "power", description = "Access power-related information")]
/// Top-level command for "ffx profile power".
pub struct PowerCommand {
    #[argh(subcommand)]
    pub subcommand: Subcommand,
}
