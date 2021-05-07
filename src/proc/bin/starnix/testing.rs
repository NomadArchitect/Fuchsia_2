// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl::endpoints::Proxy;
use fidl_fuchsia_io as fio;
use io_util::directory;
use std::ffi::CString;
use std::sync::Arc;

use crate::auth::Credentials;
use crate::fs::*;
use crate::mm::syscalls::sys_mmap;
use crate::syscalls::SyscallContext;
use crate::syscalls::SyscallResult;
use crate::task::*;
use crate::types::*;

/// Create a FileSystem for use in testing.
///
/// Open "/pkg" and returns a FileSystem rooted in that directory.
fn create_test_file_system() -> Arc<FileSystem> {
    let root =
        directory::open_in_namespace("/pkg", fio::OPEN_RIGHT_READABLE | fio::OPEN_RIGHT_EXECUTABLE)
            .expect("failed to open /pkg");
    return Arc::new(FileSystem::new(fio::DirectorySynchronousProxy::new(
        root.into_channel().unwrap().into_zx_channel(),
    )));
}

/// Creates a `Kernel` and `Task` for testing purposes.
///
/// The `Task` is backed by a real process, and can be used to test syscalls.
pub fn create_kernel_and_task() -> (Arc<Kernel>, TaskOwner) {
    let kernel =
        Kernel::new(&CString::new("test-kernel").unwrap()).expect("failed to create kernel");

    let task = Task::new(
        &kernel,
        &CString::new("test-task").unwrap(),
        FdTable::new(),
        create_test_file_system(),
        Credentials::default(),
    )
    .expect("failed to create first task");

    (kernel, task)
}

/// Maps `length` at `address` with `PROT_READ | PROT_WRITE`, `MAP_ANONYMOUS | MAP_PRIVATE`.
///
/// Returns the address returned by `sys_mmap`.
pub fn map_memory(ctx: &SyscallContext<'_>, address: UserAddress, length: u64) -> UserAddress {
    match sys_mmap(
        &ctx,
        address,
        length as usize,
        PROT_READ | PROT_WRITE,
        MAP_ANONYMOUS | MAP_PRIVATE,
        FdNumber::from_raw(-1),
        0,
    )
    .unwrap()
    {
        SyscallResult::Success(address) => UserAddress::from(address),
        _ => {
            assert!(false, "Could not map memory");
            UserAddress::default()
        }
    }
}
