// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::device::{
    binder::create_binders, logd::create_socket_and_start_server, magma::MagmaDev,
    wayland::serve_wayland,
};
use crate::fs::{devtmpfs::dev_tmp_fs, SpecialNode};
use crate::task::CurrentTask;
use crate::types::*;
use std::collections::HashSet;

/// Parses and runs the features from the provided "program strvec.". Some features,
/// such as Wayland, should be enabled on a per-component basis. We run this when we first
/// make the Galaxy. When we start the component, we run the run_component_features
/// function.
pub fn run_features<'a>(entries: &'a Vec<String>, current_task: &CurrentTask) -> Result<(), Errno> {
    for entry in entries {
        match entry.as_str() {
            // Wayland is enabled on a per-component basis and so skipped here.
            "wayland" => {}
            "binder" => {
                // Creates the various binder drivers (/dev/binder, /dev/hwbinder, /dev/vndbinder).
                create_binders(current_task.kernel())?;
            }
            "logd" => {
                // Creates a socket at /dev/socket/logdw logs anything written to it.
                create_socket_and_start_server(current_task.kernel());
            }
            "selinux_enabled" => {}
            feature => {
                tracing::warn!("Unsupported feature: {:?}", feature);
            }
        }
    }
    Ok(())
}

/// Runs features requested by individual components
pub fn run_component_features<'a>(
    entries: &'a HashSet<String>,
    current_task: &CurrentTask,
    outgoing_dir: &mut Option<fidl::endpoints::ServerEnd<fidl_fuchsia_io::DirectoryMarker>>,
) -> Result<(), Errno> {
    for entry in entries {
        match entry.as_str() {
            "wayland" => {
                let kernel = current_task.kernel();
                let dev = kernel.device_registry.write().register_misc_chrdev(MagmaDev::new())?;
                dev_tmp_fs(kernel).root().add_node_ops_dev(
                    b"magma0",
                    mode!(IFCHR, 0o600),
                    dev,
                    SpecialNode,
                )?;
                // TODO: The paths for the display and memory allocation file currently hard coded
                // to wayland-0 and wayland-1. In the future this will need to match the environment
                // variables set for the component.
                serve_wayland(
                    current_task,
                    b"/data/tmp/wayland-0".to_vec(),
                    b"/data/tmp/wayland-1".to_vec(),
                    outgoing_dir,
                )?;
            }
            "binder" => {}
            "logd" => {}
            "selinux_enabled" => {}
            feature => {
                tracing::warn!("Unsupported feature: {:?}", feature);
            }
        }
    }
    Ok(())
}
