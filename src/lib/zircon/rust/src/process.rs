// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Type-safe bindings for Zircon processes.

use crate::ok;
use crate::{object_get_info, ObjectQuery, Topic};
use crate::{AsHandleRef, Handle, HandleBased, HandleRef, Status, Task, Thread};
use bitflags::bitflags;
use fuchsia_zircon_sys::{self as sys, zx_time_t};

bitflags! {
    #[repr(transparent)]
    pub struct ProcessInfoFlags: u32 {
        const STARTED = sys::ZX_INFO_PROCESS_FLAG_STARTED;
        const EXITED = sys::ZX_INFO_PROCESS_FLAG_EXITED;
        const DEBUGGER_ATTACHED = sys::ZX_INFO_PROCESS_FLAG_DEBUGGER_ATTACHED;
    }
}

/// An object representing a Zircon process.
///
/// As essentially a subtype of `Handle`, it can be freely interconverted.
#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(transparent)]
pub struct Process(Handle);
impl_handle_based!(Process);

sys::zx_info_process_t!(ProcessInfo);

impl From<sys::zx_info_process_t> for ProcessInfo {
    fn from(info: sys::zx_info_process_t) -> ProcessInfo {
        let sys::zx_info_process_t { return_code, start_time, flags } = info;
        ProcessInfo { return_code, start_time, flags }
    }
}

// ProcessInfo is able to be safely replaced with a byte representation and is a PoD type.
unsafe impl ObjectQuery for ProcessInfo {
    const TOPIC: Topic = Topic::PROCESS;
    type InfoTy = ProcessInfo;
}

sys::zx_info_task_stats_t!(TaskStatsInfo);

impl From<sys::zx_info_task_stats_t> for TaskStatsInfo {
    fn from(
        sys::zx_info_task_stats_t {
            mem_mapped_bytes,
            mem_private_bytes,
            mem_shared_bytes,
            mem_scaled_shared_bytes,
        }: sys::zx_info_task_stats_t,
    ) -> TaskStatsInfo {
        TaskStatsInfo {
            mem_mapped_bytes,
            mem_private_bytes,
            mem_shared_bytes,
            mem_scaled_shared_bytes,
        }
    }
}

// TaskStatsInfo is able to be safely replaced with a byte representation and is a PoD type.
unsafe impl ObjectQuery for TaskStatsInfo {
    const TOPIC: Topic = Topic::TASK_STATS;
    type InfoTy = TaskStatsInfo;
}

impl Process {
    /// Similar to `Thread::start`, but is used to start the first thread in a process.
    ///
    /// Wraps the
    /// [zx_process_start](https://fuchsia.dev/fuchsia-src/reference/syscalls/process_start.md)
    /// syscall.
    pub fn start(
        &self,
        thread: &Thread,
        entry: usize,
        stack: usize,
        arg1: Handle,
        arg2: usize,
    ) -> Result<(), Status> {
        let process_raw = self.raw_handle();
        let thread_raw = thread.raw_handle();
        let arg1 = arg1.into_raw();
        ok(unsafe { sys::zx_process_start(process_raw, thread_raw, entry, stack, arg1, arg2) })
    }

    /// Create a thread inside a process.
    ///
    /// Wraps the
    /// [zx_thread_create](https://fuchsia.dev/fuchsia-src/reference/syscalls/thread_create.md)
    /// syscall.
    pub fn create_thread(&self, name: &[u8]) -> Result<Thread, Status> {
        let process_raw = self.raw_handle();
        let name_ptr = name.as_ptr();
        let name_len = name.len();
        let options = 0;
        let mut thread_out = 0;
        let status = unsafe {
            sys::zx_thread_create(process_raw, name_ptr, name_len, options, &mut thread_out)
        };
        ok(status)?;
        unsafe { Ok(Thread::from(Handle::from_raw(thread_out))) }
    }

    /// Write memory inside a process.
    ///
    /// Wraps the
    /// [zx_process_write_memory](https://fuchsia.dev/fuchsia-src/reference/syscalls/process_write_memory.md)
    /// syscall.
    pub fn write_memory(&self, vaddr: sys::zx_vaddr_t, bytes: &[u8]) -> Result<usize, Status> {
        let mut actual = 0;
        let status = unsafe {
            sys::zx_process_write_memory(
                self.raw_handle(),
                vaddr,
                bytes.as_ptr(),
                bytes.len(),
                &mut actual,
            )
        };
        ok(status).map(|()| actual)
    }

    /// Read memory from inside a process.
    ///
    /// Wraps the
    /// [zx_process_read_memory](https://fuchsia.dev/fuchsia-src/reference/syscalls/process_read_memory.md)
    /// syscall.
    pub fn read_memory(&self, vaddr: sys::zx_vaddr_t, bytes: &mut [u8]) -> Result<usize, Status> {
        let mut actual = 0;
        let status = unsafe {
            sys::zx_process_read_memory(
                self.raw_handle(),
                vaddr,
                bytes.as_mut_ptr(),
                bytes.len(),
                &mut actual,
            )
        };
        ok(status).map(|()| actual)
    }

    /// Wraps the
    /// [zx_object_get_info](https://fuchsia.dev/fuchsia-src/reference/syscalls/object_get_info.md)
    /// syscall for the ZX_INFO_PROCESS topic.
    pub fn info(&self) -> Result<ProcessInfo, Status> {
        let mut info = ProcessInfo::default();
        object_get_info::<ProcessInfo>(self.as_handle_ref(), std::slice::from_mut(&mut info))
            .map(|_| info)
    }

    /// Wraps the
    /// [zx_object_get_info](https://fuchsia.dev/fuchsia-src/reference/syscalls/object_get_info.md)
    /// syscall for the ZX_INFO_TASK_STATS topic.
    pub fn task_stats(&self) -> Result<TaskStatsInfo, Status> {
        let mut info = TaskStatsInfo::default();
        object_get_info::<TaskStatsInfo>(self.as_handle_ref(), std::slice::from_mut(&mut info))
            .map(|_| info)
    }

    /// Exit the current process with the given return code.
    ///
    /// Wraps the
    /// [zx_process_exit](https://fuchsia.dev/fuchsia-src/reference/syscalls/process_exit.md)
    /// syscall.
    pub fn exit(retcode: i64) -> ! {
        unsafe {
            sys::zx_process_exit(retcode);
            // kazoo generates the syscall returning a unit value. We know it will not proceed
            // past this point however.
            std::hint::unreachable_unchecked()
        }
    }
}

impl Task for Process {}

#[cfg(test)]
mod tests {
    use crate::cprng_draw;
    // The unit tests are built with a different crate name, but fdio and fuchsia_runtime return a
    // "real" fuchsia_zircon::Process that we need to use.
    use fuchsia_zircon::{
        sys, AsHandleRef, ProcessInfo, ProcessInfoFlags, Signals, Task, TaskStatsInfo, Time,
    };
    use matches::assert_matches;
    use std::ffi::CString;

    #[test]
    fn info_self() {
        let process = fuchsia_runtime::process_self();
        let info = process.info().unwrap();
        const STARTED: u32 = ProcessInfoFlags::STARTED.bits();
        assert_matches!(
            info,
            ProcessInfo {
                return_code: 0,
                start_time,
                flags: STARTED,
            } if start_time > 0
        );
    }

    #[test]
    fn stats_self() {
        let process = fuchsia_runtime::process_self();
        let task_stats = process.task_stats().unwrap();

        // Values greater than zero should be reported back for all memory usage
        // types.
        assert!(matches!(task_stats,
            TaskStatsInfo {
                mem_mapped_bytes,
                mem_private_bytes,
                mem_shared_bytes,
                mem_scaled_shared_bytes
            }
            if mem_mapped_bytes > 0
                && mem_private_bytes > 0
                && mem_shared_bytes > 0
                && mem_scaled_shared_bytes > 0));
    }

    #[test]
    fn exit_and_info() {
        let mut randbuf = [0; 8];
        cprng_draw(&mut randbuf).unwrap();
        let expected_code = i64::from_le_bytes(randbuf);
        let arg = CString::new(format!("{}", expected_code)).unwrap();

        // This test utility will exercise zx::Process::exit, using the provided argument as the
        // return code.
        let binpath = CString::new("/pkg/bin/exit_with_code_util").unwrap();
        let process = fdio::spawn(
            &fuchsia_runtime::job_default(),
            fdio::SpawnOptions::DEFAULT_LOADER,
            &binpath,
            &[&arg],
        )
        .expect("Failed to spawn process");

        process
            .wait_handle(Signals::PROCESS_TERMINATED, Time::INFINITE)
            .expect("Wait for process termination failed");
        let info = process.info().unwrap();
        const STARTED_AND_EXITED: u32 =
            ProcessInfoFlags::STARTED.bits() | ProcessInfoFlags::EXITED.bits();
        assert_matches!(
            info,
            ProcessInfo {
                return_code,
                start_time,
                flags: STARTED_AND_EXITED,
            } if return_code == expected_code && start_time > 0
        );
    }

    #[test]
    fn kill_and_info() {
        // This test utility will sleep "forever" without exiting, so that we can kill it..
        let binpath = CString::new("/pkg/bin/sleep_forever_util").unwrap();
        let process = fdio::spawn(
            &fuchsia_runtime::job_default(),
            // Careful not to clone stdio here, or the test runner can hang.
            fdio::SpawnOptions::DEFAULT_LOADER,
            &binpath,
            &[&binpath],
        )
        .expect("Failed to spawn process");

        let info = process.info().unwrap();
        const STARTED: u32 = ProcessInfoFlags::STARTED.bits();
        assert_matches!(
            info,
            ProcessInfo {
                return_code: 0,
                start_time,
                flags: STARTED,
             } if start_time > 0
        );

        process.kill().expect("Failed to kill process");
        process
            .wait_handle(Signals::PROCESS_TERMINATED, Time::INFINITE)
            .expect("Wait for process termination failed");

        let info = process.info().unwrap();
        const STARTED_AND_EXITED: u32 =
            ProcessInfoFlags::STARTED.bits() | ProcessInfoFlags::EXITED.bits();
        assert_matches!(
            info,
            ProcessInfo {
                return_code: sys::ZX_TASK_RETCODE_SYSCALL_KILL,
                start_time,
                flags: STARTED_AND_EXITED,
            } if start_time > 0
        );
    }
}
