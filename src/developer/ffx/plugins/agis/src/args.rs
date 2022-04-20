// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {argh::FromArgs, ffx_core::ffx_command};

#[ffx_command()]
#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "agis", description = "Agis Service")]
pub struct AgisCommand {
    #[argh(subcommand)]
    pub operation: Operation,
}

#[derive(FromArgs, PartialEq, Debug)]
#[argh(subcommand)]
pub enum Operation {
    Register(RegisterOp),
    Unregister(UnregisterOp),
    Connections(ConnectionsOp),
}

#[derive(FromArgs, PartialEq, Debug)]
#[argh(subcommand, name = "register", description = "register a process with AGIS for tracing")]
pub struct RegisterOp {
    #[argh(positional, description = "process ID")]
    pub process_id: u64,

    #[argh(positional, description = "process name")]
    pub process_name: String,
}
#[derive(FromArgs, PartialEq, Debug)]
#[argh(subcommand, name = "unregister", description = "unregister a process from AGIS")]
pub struct UnregisterOp {
    #[argh(positional, description = "process ID")]
    pub process_id: u64,
}

#[derive(FromArgs, PartialEq, Debug)]
#[argh(subcommand, name = "connections", description = "list all connected processes")]
pub struct ConnectionsOp {}
