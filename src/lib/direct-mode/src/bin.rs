// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::Result,
    direct_mode_vmm::{start, take_direct_vdso},
    fdio::{clone_fd, Namespace},
    fuchsia_async as fasync,
    fuchsia_runtime::{take_startup_handle, HandleInfo, HandleType},
    process_builder::StartupHandle,
    std::{env, ffi::CString, io},
};

/// A helper binary that will load the ELF binary provided as its first
/// argument into direct mode.
#[fasync::run_singlethreaded]
async fn main() -> Result<()> {
    let vdso_vmo = take_direct_vdso();
    let args = env::args().skip(1).map(|x| CString::new(x).unwrap()).collect::<Vec<_>>();
    let vars = env::vars()
        .map(|(key, val)| CString::new(format!("{}={}", key, val)).unwrap())
        .collect::<Vec<_>>();
    let mut paths = vec![];
    let mut handles = vec![
        StartupHandle {
            handle: clone_fd(io::stdout())?,
            info: HandleInfo::new(HandleType::FileDescriptor, 1),
        },
        StartupHandle {
            handle: clone_fd(io::stderr())?,
            info: HandleInfo::new(HandleType::FileDescriptor, 2),
        },
    ];
    let config_vmo_info = HandleInfo::new(HandleType::ComponentConfigVmo, 0);
    if let Some(config_vmo) = take_startup_handle(config_vmo_info) {
        handles.push(StartupHandle { handle: config_vmo, info: config_vmo_info })
    }
    for entry in Namespace::installed()?.export()? {
        paths.push(CString::new(entry.path)?);
        handles.push(StartupHandle { handle: entry.handle, info: entry.info })
    }
    start(vdso_vmo, args, vars, paths, handles).await
}
