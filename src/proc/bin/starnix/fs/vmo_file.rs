// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_zircon::{self as zx, HandleBased};
use std::sync::Arc;

use super::*;
use crate::fd_impl_seekable;
use crate::logging::impossible_error;
use crate::mm::vmo::round_up_to_system_page_size;
use crate::task::*;
use crate::types::*;
use crate::vmex_resource::VMEX_RESOURCE;

pub struct VmoFileObject {
    vmo: Arc<zx::Vmo>,
}

impl VmoFileObject {
    pub fn new(vmo: Arc<zx::Vmo>) -> Self {
        VmoFileObject { vmo }
    }
}

impl FileOps for VmoFileObject {
    fd_impl_seekable!();

    fn read_at(
        &self,
        file: &FileObject,
        task: &Task,
        offset: usize,
        data: &[UserBuffer],
    ) -> Result<usize, Errno> {
        let mut info = file.node().info_write();
        let file_length = info.size;
        let want_read = UserBuffer::get_total_length(data);
        let to_read =
            if file_length < offset + want_read { file_length - offset } else { want_read };
        let mut buf = vec![0u8; to_read];
        self.vmo.read(&mut buf[..], offset as u64).map_err(|_| EIO)?;
        // TODO(steveaustin) - write_each might might be more efficient
        task.mm.write_all(data, &mut buf[..])?;
        // TODO(steveaustin) - omit updating time_access to allow info to be immutable
        // and thus allow simultaneous reads.
        info.time_access = fuchsia_runtime::utc_time();
        Ok(to_read)
    }

    fn write_at(
        &self,
        file: &FileObject,
        task: &Task,
        offset: usize,
        data: &[UserBuffer],
    ) -> Result<usize, Errno> {
        let mut info = file.node().info_write();
        let want_write = UserBuffer::get_total_length(data);
        let write_end = offset + want_write;
        let mut update_content_size = false;
        if write_end > info.size {
            if write_end > info.storage_size {
                let new_size = round_up_to_system_page_size(write_end);
                self.vmo.set_size(new_size as u64).map_err(|_| ENOMEM)?;
                info.storage_size = new_size as usize;
            }
            update_content_size = true;
        }

        let mut buf = vec![0u8; want_write];
        task.mm.read_all(data, &mut buf[..])?;
        self.vmo.write(&mut buf[..], offset as u64).map_err(|_| EIO)?;
        if update_content_size {
            info.size = write_end;
        }
        let now = fuchsia_runtime::utc_time();
        info.time_access = now;
        info.time_modify = now;
        Ok(want_write)
    }

    fn get_vmo(
        &self,
        _file: &FileObject,
        _task: &Task,
        prot: zx::VmarFlags,
    ) -> Result<zx::Vmo, Errno> {
        let mut vmo =
            self.vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).map_err(impossible_error)?;
        if prot.contains(zx::VmarFlags::PERM_EXECUTE) {
            vmo = vmo.replace_as_executable(&VMEX_RESOURCE).map_err(impossible_error)?;
        }
        Ok(vmo)
    }
}
