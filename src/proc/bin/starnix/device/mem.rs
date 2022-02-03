// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_zircon::cprng_draw;

use crate::error;
use crate::fd_impl_nonblocking;
use crate::fs::*;
use crate::task::*;
use crate::types::*;

pub fn open_mem_device(minor: u32) -> Result<Box<dyn FileOps>, Errno> {
    match minor {
        DevNull::MINOR => Ok(Box::new(DevNull)),
        DevZero::MINOR => Ok(Box::new(DevZero)),
        DevFull::MINOR => Ok(Box::new(DevFull)),
        DevRandom::MINOR => Ok(Box::new(DevRandom)),
        DevRandom::URANDOM_MINOR => Ok(Box::new(DevRandom)),
        DevKmsg::MINOR => Ok(Box::new(DevKmsg)),
        _ => error!(ENODEV),
    }
}

macro_rules! fd_impl_seekless {
    () => {
        fn read(
            &self,
            file: &FileObject,
            current_task: &CurrentTask,
            data: &[UserBuffer],
        ) -> Result<usize, Errno> {
            self.read_at(file, current_task, 0, data)
        }
        fn write(
            &self,
            file: &FileObject,
            current_task: &CurrentTask,
            data: &[UserBuffer],
        ) -> Result<usize, Errno> {
            self.write_at(file, current_task, 0, data)
        }
        fn seek(
            &self,
            _file: &FileObject,
            _current_task: &CurrentTask,
            _offset: off_t,
            _whence: SeekOrigin,
        ) -> Result<off_t, Errno> {
            Ok(0)
        }
    };
}

pub struct DevNull;

impl DevNull {
    const MINOR: u32 = 3;
}

impl FileOps for DevNull {
    fd_impl_seekless!();
    fd_impl_nonblocking!();

    fn write_at(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        data: &[UserBuffer],
    ) -> Result<usize, Errno> {
        UserBuffer::get_total_length(data)
    }

    fn read_at(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        _data: &[UserBuffer],
    ) -> Result<usize, Errno> {
        Ok(0)
    }
}

struct DevZero;

impl DevZero {
    pub const MINOR: u32 = 5;
}

impl FileOps for DevZero {
    fd_impl_seekless!();
    fd_impl_nonblocking!();

    fn write_at(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        data: &[UserBuffer],
    ) -> Result<usize, Errno> {
        UserBuffer::get_total_length(data)
    }

    fn read_at(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        _offset: usize,
        data: &[UserBuffer],
    ) -> Result<usize, Errno> {
        let mut actual = 0;
        current_task.mm.write_each(data, |bytes| {
            actual += bytes.len();
            Ok(bytes)
        })?;
        Ok(actual)
    }
}

struct DevFull;

impl DevFull {
    pub const MINOR: u32 = 7;
}

impl FileOps for DevFull {
    fd_impl_seekless!();
    fd_impl_nonblocking!();

    fn write_at(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        _data: &[UserBuffer],
    ) -> Result<usize, Errno> {
        error!(ENOSPC)
    }

    fn read_at(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        _offset: usize,
        data: &[UserBuffer],
    ) -> Result<usize, Errno> {
        let mut actual = 0;
        current_task.mm.write_each(data, |bytes| {
            actual += bytes.len();
            Ok(bytes)
        })?;
        Ok(actual)
    }
}

struct DevRandom;

impl DevRandom {
    pub const MINOR: u32 = 8;
    pub const URANDOM_MINOR: u32 = 9;
}

impl FileOps for DevRandom {
    fd_impl_seekless!();
    fd_impl_nonblocking!();

    fn write_at(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        data: &[UserBuffer],
    ) -> Result<usize, Errno> {
        UserBuffer::get_total_length(data)
    }

    fn read_at(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        _offset: usize,
        data: &[UserBuffer],
    ) -> Result<usize, Errno> {
        let mut actual = 0;
        current_task.mm.write_each(data, |bytes| {
            actual += bytes.len();
            cprng_draw(bytes);
            Ok(bytes)
        })?;
        Ok(actual)
    }
}

struct DevKmsg;

impl DevKmsg {
    pub const MINOR: u32 = 11;
}

impl FileOps for DevKmsg {
    fd_impl_seekless!();
    fd_impl_nonblocking!();

    fn read_at(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        _data: &[UserBuffer],
    ) -> Result<usize, Errno> {
        Ok(0)
    }

    fn write_at(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        _offset: usize,
        data: &[UserBuffer],
    ) -> Result<usize, Errno> {
        let total = UserBuffer::get_total_length(data)?;
        let mut bytes = vec![0; total];
        current_task.mm.read_all(data, &mut bytes)?;
        log::info!(target: "kmsg", "{}", String::from_utf8_lossy(&bytes).trim_end_matches('\n'));
        Ok(total)
    }
}
