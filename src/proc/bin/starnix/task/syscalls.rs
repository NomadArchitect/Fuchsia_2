// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_zircon as zx;
use log::info;
use std::ffi::CString;

use crate::mm::*;
use crate::not_implemented;
use crate::runner::*;
use crate::signals::signal_handling::send_signal;
use crate::strace;
use crate::syscalls::*;
use crate::task::UncheckedSignal;
use crate::types::*;

pub fn sys_clone(
    ctx: &SyscallContext<'_>,
    flags: u64,
    user_stack: UserAddress,
    user_parent_tid: UserRef<pid_t>,
    user_child_tid: UserRef<pid_t>,
    user_tls: UserAddress,
) -> Result<SyscallResult, Errno> {
    let task_owner = ctx.task.clone_task(flags, user_parent_tid, user_child_tid)?;
    let tid = task_owner.task.id;

    let mut registers = ctx.registers;
    registers.rax = 0;
    if !user_stack.is_null() {
        registers.rsp = user_stack.ptr() as u64;
    }

    if flags & (CLONE_SETTLS as u64) != 0 {
        registers.fs_base = user_tls.ptr() as u64;
    }

    if flags & (CLONE_THREAD as u64) != 0 {
        spawn_task(task_owner, registers, |_| {
            // TODO: Do threads need a task_complete callback?
        });
    } else {
        let task = ctx.task.clone();
        spawn_task(task_owner, registers, move |_| {
            let _ = send_signal(&task, &UncheckedSignal::from(SIGCHLD));
        });
    }

    Ok(tid.into())
}

fn read_c_string_vector(
    mm: &MemoryManager,
    user_vector: UserRef<UserCString>,
    buf: &mut [u8],
) -> Result<Vec<CString>, Errno> {
    let mut user_current = user_vector;
    let mut vector: Vec<CString> = vec![];
    loop {
        let mut user_string = UserCString::default();
        mm.read_object(user_current, &mut user_string)?;
        if user_string.is_null() {
            break;
        }
        let string = mm.read_c_string(user_string, buf)?;
        vector.push(CString::new(string).map_err(|_| EINVAL)?);
        user_current = user_current.next();
    }
    Ok(vector)
}

pub fn sys_execve(
    ctx: &mut SyscallContext<'_>,
    user_path: UserCString,
    user_argv: UserRef<UserCString>,
    user_environ: UserRef<UserCString>,
) -> Result<SyscallResult, Errno> {
    let mut buf = [0u8; PATH_MAX as usize];
    let path = CString::new(ctx.task.mm.read_c_string(user_path, &mut buf)?).map_err(|_| EINVAL)?;
    // TODO: What is the maximum size for an argument?
    let argv = read_c_string_vector(&ctx.task.mm, user_argv, &mut buf)?;
    let environ = read_c_string_vector(&ctx.task.mm, user_environ, &mut buf)?;
    strace!(ctx.task, "execve({:?}, argv={:?}, environ={:?})", path, argv, environ);
    let start_info = ctx.task.exec(&path, &argv, &environ)?;
    ctx.registers = start_info.to_registers();
    Ok(SUCCESS)
}

pub fn sys_getpid(ctx: &SyscallContext<'_>) -> Result<SyscallResult, Errno> {
    Ok(ctx.task.get_pid().into())
}

pub fn sys_getsid(ctx: &SyscallContext<'_>, pid: pid_t) -> Result<SyscallResult, Errno> {
    if pid == 0 {
        return Ok(ctx.task.get_sid().into());
    }
    Ok(ctx.task.get_task(pid).ok_or(ESRCH)?.get_sid().into())
}

pub fn sys_gettid(ctx: &SyscallContext<'_>) -> Result<SyscallResult, Errno> {
    Ok(ctx.task.get_tid().into())
}

pub fn sys_getppid(ctx: &SyscallContext<'_>) -> Result<SyscallResult, Errno> {
    Ok(ctx.task.parent.into())
}

pub fn sys_getpgrp(ctx: &SyscallContext<'_>) -> Result<SyscallResult, Errno> {
    Ok(ctx.task.get_pgrp().into())
}

pub fn sys_getpgid(ctx: &SyscallContext<'_>, pid: pid_t) -> Result<SyscallResult, Errno> {
    if pid == 0 {
        return Ok(ctx.task.get_pgrp().into());
    }
    Ok(ctx.task.get_task(pid).ok_or(ESRCH)?.get_pgrp().into())
}

pub fn sys_getuid(ctx: &SyscallContext<'_>) -> Result<SyscallResult, Errno> {
    Ok(ctx.task.creds.uid.into())
}

pub fn sys_getgid(ctx: &SyscallContext<'_>) -> Result<SyscallResult, Errno> {
    Ok(ctx.task.creds.gid.into())
}

pub fn sys_geteuid(ctx: &SyscallContext<'_>) -> Result<SyscallResult, Errno> {
    Ok(ctx.task.creds.euid.into())
}

pub fn sys_getegid(ctx: &SyscallContext<'_>) -> Result<SyscallResult, Errno> {
    Ok(ctx.task.creds.egid.into())
}

pub fn sys_exit(ctx: &SyscallContext<'_>, error_code: i32) -> Result<SyscallResult, Errno> {
    info!(target: "exit", "exit: tid={} error_code={}", ctx.task.get_tid(), error_code);
    *ctx.task.exit_code.lock() = Some(error_code);
    Ok(SyscallResult::Exit(error_code))
}

pub fn sys_exit_group(ctx: &SyscallContext<'_>, error_code: i32) -> Result<SyscallResult, Errno> {
    info!(target: "exit", "exit_group: pid={} error_code={}", ctx.task.get_pid(), error_code);
    *ctx.task.exit_code.lock() = Some(error_code);
    Ok(SyscallResult::ExitGroup(error_code))
}

pub fn sys_sched_getscheduler(
    _ctx: &SyscallContext<'_>,
    _pid: i32,
) -> Result<SyscallResult, Errno> {
    Ok(SCHED_NORMAL.into())
}

pub fn sys_sched_getaffinity(
    ctx: &SyscallContext<'_>,
    _pid: pid_t,
    _cpusetsize: usize,
    user_mask: UserAddress,
) -> Result<SyscallResult, Errno> {
    let result = vec![0xFFu8; _cpusetsize];
    ctx.task.mm.write_memory(user_mask, &result)?;
    Ok(SUCCESS)
}

pub fn sys_sched_setaffinity(
    ctx: &SyscallContext<'_>,
    _pid: pid_t,
    _cpusetsize: usize,
    user_mask: UserAddress,
) -> Result<SyscallResult, Errno> {
    let mut mask = vec![0x0u8; _cpusetsize];
    ctx.task.mm.read_memory(user_mask, &mut mask)?;
    // Currently, we ignore the mask and act as if the system reset the mask
    // immediately to allowing all CPUs.
    Ok(SUCCESS)
}

pub fn sys_getitimer(
    ctx: &SyscallContext<'_>,
    which: u32,
    user_curr_value: UserRef<itimerval>,
) -> Result<SyscallResult, Errno> {
    let signal_state = ctx.task.thread_group.signal_state.read();
    match which {
        ITIMER_REAL => {
            ctx.task.mm.write_object(user_curr_value, &signal_state.itimer_real)?;
        }
        ITIMER_VIRTUAL => {
            ctx.task.mm.write_object(user_curr_value, &signal_state.itimer_virtual)?;
        }
        ITIMER_PROF => {
            ctx.task.mm.write_object(user_curr_value, &signal_state.itimer_prof)?;
        }
        _ => {
            return Err(EINVAL);
        }
    }
    Ok(SUCCESS)
}

pub fn sys_setitimer(
    ctx: &SyscallContext<'_>,
    which: u32,
    user_new_value: UserRef<itimerval>,
    user_old_value: UserRef<itimerval>,
) -> Result<SyscallResult, Errno> {
    let mut new_value = itimerval::default();
    ctx.task.mm.read_object(user_new_value, &mut new_value)?;

    let old_value;
    let mut signal_state = ctx.task.thread_group.signal_state.write();

    match which {
        ITIMER_REAL => {
            old_value = signal_state.itimer_real;
            signal_state.itimer_real = new_value;
        }
        ITIMER_VIRTUAL => {
            old_value = signal_state.itimer_virtual;
            signal_state.itimer_virtual = new_value;
        }
        ITIMER_PROF => {
            old_value = signal_state.itimer_prof;
            signal_state.itimer_prof = new_value;
        }
        _ => {
            return Err(EINVAL);
        }
    }

    if !user_old_value.is_null() {
        ctx.task.mm.write_object(user_old_value, &old_value)?;
    }

    Ok(SUCCESS)
}

pub fn sys_prctl(
    ctx: &SyscallContext<'_>,
    option: u32,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    arg5: u64,
) -> Result<SyscallResult, Errno> {
    match option {
        PR_SET_VMA => {
            if arg2 != PR_SET_VMA_ANON_NAME as u64 {
                not_implemented!("prctl: PR_SET_VMA: Unknown arg2: 0x{:x}", arg2);
                return Err(ENOSYS);
            }
            let addr = UserAddress::from(arg3);
            let length = arg4 as usize;
            let name = UserCString::new(UserAddress::from(arg5));
            let mut buf = [0u8; PATH_MAX as usize]; // TODO: How large can these names be?
            let name = ctx.task.mm.read_c_string(name, &mut buf)?;
            let name = CString::new(name).map_err(|_| EINVAL)?;
            ctx.task.mm.set_mapping_name(addr, length, name)?;
            Ok(SUCCESS)
        }
        PR_SET_DUMPABLE => {
            let mut dumpable = ctx.task.mm.dumpable.lock();
            *dumpable = if arg2 == 1 { DumpPolicy::USER } else { DumpPolicy::DISABLE };
            Ok(SUCCESS)
        }
        PR_GET_DUMPABLE => {
            let dumpable = ctx.task.mm.dumpable.lock();
            Ok(match *dumpable {
                DumpPolicy::DISABLE => 0,
                DumpPolicy::USER => 1,
            }
            .into())
        }
        _ => {
            not_implemented!("prctl: Unknown option: 0x{:x}", option);
            Err(ENOSYS)
        }
    }
}

pub fn sys_arch_prctl(
    ctx: &mut SyscallContext<'_>,
    code: u32,
    addr: UserAddress,
) -> Result<SyscallResult, Errno> {
    match code {
        ARCH_SET_FS => {
            ctx.registers.fs_base = addr.ptr() as u64;
            Ok(SUCCESS)
        }
        _ => {
            not_implemented!("arch_prctl: Unknown code: code=0x{:x} addr={}", code, addr);
            Err(ENOSYS)
        }
    }
}

pub fn sys_set_tid_address(
    ctx: &SyscallContext<'_>,
    user_tid: UserRef<pid_t>,
) -> Result<SyscallResult, Errno> {
    *ctx.task.clear_child_tid.lock() = user_tid;
    Ok(ctx.task.get_tid().into())
}

pub fn sys_getrusage(
    ctx: &SyscallContext<'_>,
    who: i32,
    user_usage: UserRef<rusage>,
) -> Result<SyscallResult, Errno> {
    const RUSAGE_SELF: i32 = crate::types::uapi::RUSAGE_SELF as i32;
    const RUSAGE_THREAD: i32 = crate::types::uapi::RUSAGE_THREAD as i32;
    // TODO(fxb/76811): Implement proper rusage.
    match who {
        RUSAGE_CHILDREN => (),
        RUSAGE_SELF => (),
        RUSAGE_THREAD => (),
        _ => return Err(EINVAL),
    };

    if !user_usage.is_null() {
        let usage = rusage::default();
        ctx.task.mm.write_object(user_usage, &usage)?;
    }

    Ok(SUCCESS)
}

pub fn sys_futex(
    ctx: &SyscallContext<'_>,
    addr: UserAddress,
    op: u32,
    value: u32,
    _utime: UserRef<timespec>,
    _addr2: UserAddress,
    _value3: u32,
) -> Result<SyscallResult, Errno> {
    // TODO: Distinguish between public and private futexes.
    let _is_private = op & FUTEX_PRIVATE_FLAG != 0;

    let is_realtime = op & FUTEX_CLOCK_REALTIME != 0;
    if is_realtime {
        not_implemented!("futex: Realtime futex are not implemented.");
        return Err(ENOSYS);
    }

    let cmd = op & (FUTEX_CMD_MASK as u32);
    match cmd {
        FUTEX_WAIT => {
            let deadline = zx::Time::INFINITE;
            ctx.task.mm.futex.wait(&ctx.task.waiter, addr, value, deadline)?;
        }
        FUTEX_WAKE => {
            ctx.task.mm.futex.wake(addr, value as usize);
        }
        _ => {
            not_implemented!("futex: command 0x{:x} not implemented.", cmd);
            return Err(ENOSYS);
        }
    }
    Ok(SUCCESS)
}

#[cfg(test)]
mod tests {
    use std::u64;

    use super::*;
    use fuchsia_async as fasync;

    use crate::mm::syscalls::sys_munmap;
    use crate::testing::*;

    #[fasync::run_singlethreaded(test)]
    async fn test_prctl_set_vma_anon_name() {
        let (_kernel, task_owner) = create_kernel_and_task();
        let ctx = SyscallContext::new(&task_owner.task);

        let mapped_address = map_memory(&ctx, UserAddress::default(), *PAGE_SIZE);
        let name_addr = mapped_address + 128u64;
        let name = "test-name\0";
        ctx.task.mm.write_memory(name_addr, name.as_bytes()).expect("failed to write name");
        sys_prctl(
            &ctx,
            PR_SET_VMA,
            PR_SET_VMA_ANON_NAME as u64,
            mapped_address.ptr() as u64,
            32,
            name_addr.ptr() as u64,
        )
        .expect("failed to set name");
        assert_eq!(
            CString::new("test-name").unwrap(),
            ctx.task.mm.get_mapping_name(mapped_address + 24u64).expect("failed to get address")
        );

        sys_munmap(&ctx, mapped_address, *PAGE_SIZE as usize).expect("failed to unmap memory");
        assert_eq!(Err(EFAULT), ctx.task.mm.get_mapping_name(mapped_address + 24u64));
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_prctl_get_set_dumpable() {
        let (_kernel, task_owner) = create_kernel_and_task();
        let ctx = SyscallContext::new(&task_owner.task);

        assert_eq!(
            SyscallResult::Success(0),
            sys_prctl(&ctx, PR_GET_DUMPABLE, 0, 0, 0, 0).expect("failed to get dumpable")
        );

        sys_prctl(&ctx, PR_SET_DUMPABLE, 1, 0, 0, 0).expect("failed to set dumpable");
        assert_eq!(
            SyscallResult::Success(1),
            sys_prctl(&ctx, PR_GET_DUMPABLE, 0, 0, 0, 0).expect("failed to get dumpable")
        );

        // SUID_DUMP_ROOT not supported.
        sys_prctl(&ctx, PR_SET_DUMPABLE, 2, 0, 0, 0).expect("failed to set dumpable");
        assert_eq!(
            SyscallResult::Success(0),
            sys_prctl(&ctx, PR_GET_DUMPABLE, 0, 0, 0, 0).expect("failed to get dumpable")
        );
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_sys_getsid() {
        let (kernel, task_owner) = create_kernel_and_task();
        let ctx = SyscallContext::new(&task_owner.task);

        assert_eq!(
            SyscallResult::Success(task_owner.task.get_tid() as u64),
            sys_getsid(&ctx, 0).expect("failed to get sid")
        );

        let second_task_owner = create_task(&kernel, "second task");

        assert_eq!(
            SyscallResult::Success(second_task_owner.task.get_tid() as u64),
            sys_getsid(&ctx, second_task_owner.task.get_tid().into()).expect("failed to get sid")
        );
    }
}
