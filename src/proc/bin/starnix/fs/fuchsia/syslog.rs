// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use log::info;
use std::sync::Arc;

use crate::fd_impl_nonseekable;
use crate::fs::*;
use crate::task::*;
use crate::uapi::*;

#[derive(FileObject)]
pub struct SyslogFile {
    common: FileCommon,
}

impl SyslogFile {
    pub fn new() -> FileHandle {
        Arc::new(SyslogFile { common: FileCommon::default() })
    }
}

impl FileObject for SyslogFile {
    fd_impl_nonseekable!();

    fn write(&self, task: &Task, data: &[iovec_t]) -> Result<usize, Errno> {
        let mut size = 0;
        for vec in data {
            let mut local = vec![0; vec.iov_len];
            task.mm.read_memory(vec.iov_base, &mut local)?;
            info!(target: "stdio", "{}", String::from_utf8_lossy(&local));
            size += vec.iov_len;
        }
        Ok(size)
    }

    fn read(&self, _task: &Task, _data: &[iovec_t]) -> Result<usize, Errno> {
        Ok(0)
    }

    fn fstat(&self, task: &Task) -> Result<stat_t, Errno> {
        // TODO(tbodt): Replace these random numbers with an anonymous inode
        Ok(stat_t {
            st_dev: 0x16,
            st_ino: 3,
            st_nlink: 1,
            st_mode: 0x2190,
            st_uid: task.creds.uid,
            st_gid: task.creds.gid,
            st_rdev: 0x8800,
            ..stat_t::default()
        })
    }

    fn ioctl(
        &self,
        task: &Task,
        request: u32,
        in_addr: UserAddress,
        out_addr: UserAddress,
    ) -> Result<SyscallResult, Errno> {
        match request {
            TCGETS => Err(ENOTTY),
            _ => self.common.ioctl(task, request, in_addr, out_addr),
        }
    }
}
