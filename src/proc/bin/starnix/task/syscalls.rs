// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_runtime::duplicate_utc_clock_handle;
use fuchsia_zircon as zx;
use fuchsia_zircon::AsHandleRef;
use std::ffi::CString;
use std::sync::Arc;
use tracing::info;
use zerocopy::AsBytes;

use crate::auth::{Credentials, SecureBits};
use crate::execution::*;
use crate::logging::{not_implemented, strace};
use crate::mm::*;
use crate::syscalls::*;
use crate::task::*;
use crate::types::*;

pub fn sys_clone(
    current_task: &CurrentTask,
    flags: u64,
    user_stack: UserAddress,
    user_parent_tid: UserRef<pid_t>,
    user_child_tid: UserRef<pid_t>,
    user_tls: UserAddress,
) -> Result<pid_t, Errno> {
    let mut new_task = current_task.clone_task(flags, user_parent_tid, user_child_tid)?;
    let tid = new_task.id;

    new_task.registers = current_task.registers;
    new_task.registers.rax = 0;
    if !user_stack.is_null() {
        new_task.registers.rsp = user_stack.ptr() as u64;
    }
    if flags & (CLONE_SETTLS as u64) != 0 {
        new_task.registers.fs_base = user_tls.ptr() as u64;
    }

    execute_task(new_task, |_| {});
    Ok(tid)
}

pub fn sys_vfork(current_task: &CurrentTask) -> Result<pid_t, Errno> {
    not_implemented!(current_task, "vfork is not implemented. A normal fork is executed instead.");
    let mut new_task =
        current_task.clone_task(SIGCHLD.number() as u64, UserRef::default(), UserRef::default())?;
    let tid = new_task.id;

    new_task.registers = current_task.registers;
    new_task.registers.rax = 0;

    execute_task(new_task, |_| {});
    // TODO: The process must wait for the child to run exec.
    Ok(tid)
}

fn read_c_string_vector(
    mm: &MemoryManager,
    user_vector: UserRef<UserCString>,
    buf: &mut [u8],
) -> Result<Vec<CString>, Errno> {
    let mut user_current = user_vector;
    let mut vector: Vec<CString> = vec![];
    loop {
        let user_string = mm.read_object(user_current)?;
        if user_string.is_null() {
            break;
        }
        let string = mm.read_c_string(user_string, buf)?;
        vector.push(CString::new(string).map_err(|_| errno!(EINVAL))?);
        user_current = user_current.next();
    }
    Ok(vector)
}

pub fn sys_execve(
    current_task: &mut CurrentTask,
    user_path: UserCString,
    user_argv: UserRef<UserCString>,
    user_environ: UserRef<UserCString>,
) -> Result<(), Errno> {
    let mut buf = [0u8; PATH_MAX as usize];
    let path = CString::new(current_task.mm.read_c_string(user_path, &mut buf)?)
        .map_err(|_| errno!(EINVAL))?;
    // TODO: What is the maximum size for an argument?
    let argv = if user_argv.is_null() {
        Vec::new()
    } else {
        read_c_string_vector(&current_task.mm, user_argv, &mut buf)?
    };
    let environ = if user_environ.is_null() {
        Vec::new()
    } else {
        read_c_string_vector(&current_task.mm, user_environ, &mut buf)?
    };
    strace!(current_task, "execve({:?}, argv={:?}, environ={:?})", path, argv, environ);
    current_task.exec(path, argv, environ)?;
    Ok(())
}

pub fn sys_getpid(current_task: &CurrentTask) -> Result<pid_t, Errno> {
    Ok(current_task.get_pid())
}

pub fn sys_gettid(current_task: &CurrentTask) -> Result<pid_t, Errno> {
    Ok(current_task.get_tid())
}

pub fn sys_getppid(current_task: &CurrentTask) -> Result<pid_t, Errno> {
    Ok(current_task.thread_group.read().get_ppid())
}

fn get_task_or_current(current_task: &CurrentTask, pid: pid_t) -> Result<Arc<Task>, Errno> {
    if pid == 0 {
        Ok(current_task.task_arc_clone())
    } else {
        current_task.get_task(pid).ok_or(errno!(ESRCH))
    }
}

pub fn sys_getsid(current_task: &CurrentTask, pid: pid_t) -> Result<pid_t, Errno> {
    Ok(get_task_or_current(current_task, pid)?.thread_group.read().process_group.session.leader)
}

pub fn sys_getpgrp(current_task: &CurrentTask) -> Result<pid_t, Errno> {
    Ok(current_task.thread_group.read().process_group.leader)
}

pub fn sys_getpgid(current_task: &CurrentTask, pid: pid_t) -> Result<pid_t, Errno> {
    Ok(get_task_or_current(current_task, pid)?.thread_group.read().process_group.leader)
}

pub fn sys_setpgid(current_task: &CurrentTask, pid: pid_t, pgid: pid_t) -> Result<(), Errno> {
    let task = get_task_or_current(current_task, pid)?;
    current_task.thread_group.setpgid(&task, pgid)?;
    Ok(())
}

// A non-root process is allowed to set any of its three uids to the value of any other. The
// CAP_SETUID capability bypasses these checks and allows setting any uid to any integer. Likewise
// for gids.
fn new_uid_allowed(creds: &Credentials, uid: uid_t) -> bool {
    creds.has_capability(CAP_SETUID)
        || uid == creds.uid
        || uid == creds.euid
        || uid == creds.saved_uid
}

fn new_gid_allowed(creds: &Credentials, gid: gid_t) -> bool {
    creds.has_capability(CAP_SETGID)
        || gid == creds.gid
        || gid == creds.egid
        || gid == creds.saved_gid
}

pub fn sys_getuid(current_task: &CurrentTask) -> Result<uid_t, Errno> {
    Ok(current_task.read().creds.uid)
}

pub fn sys_getgid(current_task: &CurrentTask) -> Result<gid_t, Errno> {
    Ok(current_task.read().creds.gid)
}

pub fn sys_setuid(current_task: &CurrentTask, uid: uid_t) -> Result<(), Errno> {
    let creds = &mut current_task.write().creds;
    if uid == gid_t::MAX {
        return error!(EINVAL);
    }
    if !new_uid_allowed(creds, uid) {
        return error!(EPERM);
    }
    let has_cap_setuid = creds.has_capability(CAP_SETUID);
    creds.euid = uid;
    if has_cap_setuid {
        creds.uid = uid;
        creds.saved_uid = uid;
    }
    Ok(())
}

pub fn sys_setgid(current_task: &CurrentTask, gid: gid_t) -> Result<(), Errno> {
    let creds = &mut current_task.write().creds;
    if gid == gid_t::MAX {
        return error!(EINVAL);
    }
    if !new_gid_allowed(creds, gid) {
        return error!(EPERM);
    }
    creds.egid = gid;
    if creds.has_capability(CAP_SETGID) {
        creds.gid = gid;
        creds.saved_gid = gid;
    }
    Ok(())
}

pub fn sys_geteuid(current_task: &CurrentTask) -> Result<uid_t, Errno> {
    Ok(current_task.read().creds.euid)
}

pub fn sys_getegid(current_task: &CurrentTask) -> Result<gid_t, Errno> {
    Ok(current_task.read().creds.egid)
}

pub fn sys_getresuid(
    current_task: &CurrentTask,
    ruid_addr: UserRef<uid_t>,
    euid_addr: UserRef<uid_t>,
    suid_addr: UserRef<uid_t>,
) -> Result<(), Errno> {
    let creds = &current_task.read().creds;
    current_task.mm.write_object(ruid_addr, &creds.uid)?;
    current_task.mm.write_object(euid_addr, &creds.euid)?;
    current_task.mm.write_object(suid_addr, &creds.saved_uid)?;
    Ok(())
}

pub fn sys_getresgid(
    current_task: &CurrentTask,
    rgid_addr: UserRef<gid_t>,
    egid_addr: UserRef<gid_t>,
    sgid_addr: UserRef<gid_t>,
) -> Result<(), Errno> {
    let creds = &current_task.read().creds;
    current_task.mm.write_object(rgid_addr, &creds.gid)?;
    current_task.mm.write_object(egid_addr, &creds.egid)?;
    current_task.mm.write_object(sgid_addr, &creds.saved_gid)?;
    Ok(())
}

pub fn sys_setresuid(
    current_task: &CurrentTask,
    ruid: uid_t,
    euid: uid_t,
    suid: uid_t,
) -> Result<(), Errno> {
    let creds = &mut current_task.write().creds;
    let allowed = |uid| uid == u32::MAX || new_uid_allowed(creds, uid);
    if !allowed(ruid) || !allowed(euid) || !allowed(suid) {
        return error!(EPERM);
    }
    if ruid != u32::MAX {
        creds.uid = ruid;
    }
    if euid != u32::MAX {
        creds.euid = euid;
    }
    if suid != u32::MAX {
        creds.saved_uid = suid;
    }
    Ok(())
}

pub fn sys_setresgid(
    current_task: &CurrentTask,
    rgid: gid_t,
    egid: gid_t,
    sgid: gid_t,
) -> Result<(), Errno> {
    let creds = &mut current_task.write().creds;
    let allowed = |gid| gid == u32::MAX || new_gid_allowed(creds, gid);
    if !allowed(rgid) || !allowed(egid) || !allowed(sgid) {
        return error!(EPERM);
    }
    if rgid != u32::MAX {
        creds.gid = rgid;
    }
    if egid != u32::MAX {
        creds.egid = egid;
    }
    if sgid != u32::MAX {
        creds.saved_gid = sgid;
    }
    Ok(())
}

pub fn sys_exit(current_task: &CurrentTask, code: i32) -> Result<(), Errno> {
    info!(target: "exit", "{:?} exit({})", current_task, code);
    // Only change the current exit status if this has not been already set by exit_group, as
    // otherwise it has priority.
    current_task.write().exit_status.get_or_insert(ExitStatus::Exit(code as u8));
    Ok(())
}

pub fn sys_exit_group(current_task: &CurrentTask, code: i32) -> Result<(), Errno> {
    info!(target: "exit", "{:?} exit_group({})", current_task, code);
    current_task.thread_group.exit(ExitStatus::Exit(code as u8));
    Ok(())
}

pub fn sys_sched_getscheduler(_ctx: &CurrentTask, _pid: i32) -> Result<u32, Errno> {
    Ok(SCHED_NORMAL)
}

pub fn sys_sched_getaffinity(
    current_task: &CurrentTask,
    _pid: pid_t,
    cpusetsize: u32,
    user_mask: UserAddress,
) -> Result<(), Errno> {
    let mask: u64 = u64::MAX;
    if cpusetsize < (std::mem::size_of_val(&mask) as u32) {
        return error!(EINVAL);
    }

    current_task.mm.write_memory(user_mask, &mask.to_ne_bytes())?;
    Ok(())
}

pub fn sys_sched_setaffinity(
    current_task: &CurrentTask,
    _pid: pid_t,
    cpusetsize: u32,
    user_mask: UserAddress,
) -> Result<(), Errno> {
    let mut mask: u64 = 0;
    if cpusetsize < (std::mem::size_of_val(&mask) as u32) {
        return error!(EINVAL);
    }

    current_task.mm.read_memory(user_mask, &mut mask.as_bytes_mut())?;
    // Currently, we ignore the mask and act as if the system reset the mask
    // immediately to allowing all CPUs.
    Ok(())
}

pub fn sys_sched_getparam(
    current_task: &CurrentTask,
    _pid: pid_t,
    param: UserAddress,
) -> Result<(), Errno> {
    // Scheduling parameter cannot be changed right now. Always return 0.
    let param_value = sched_param { sched_priority: 0 };
    current_task.mm.write_object(param.into(), &param_value)?;
    Ok(())
}

pub fn sys_getitimer(
    current_task: &CurrentTask,
    which: u32,
    user_curr_value: UserRef<itimerval>,
) -> Result<(), Errno> {
    let itimers = current_task.thread_group.read().itimers;
    let timer = itimers.get(which as usize).ok_or(errno!(EINVAL))?;
    current_task.mm.write_object(user_curr_value, timer)?;
    Ok(())
}

pub fn sys_setitimer(
    current_task: &CurrentTask,
    which: u32,
    user_new_value: UserRef<itimerval>,
    user_old_value: UserRef<itimerval>,
) -> Result<(), Errno> {
    let new_value = current_task.mm.read_object(user_new_value)?;

    let old_value = current_task.thread_group.set_itimer(which, new_value)?;

    if !user_old_value.is_null() {
        current_task.mm.write_object(user_old_value, &old_value)?;
    }

    Ok(())
}

pub fn sys_prctl(
    current_task: &CurrentTask,
    option: u32,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    arg5: u64,
) -> Result<SyscallResult, Errno> {
    match option {
        PR_SET_VMA => {
            if arg2 != PR_SET_VMA_ANON_NAME as u64 {
                not_implemented!(current_task, "prctl: PR_SET_VMA: Unknown arg2: 0x{:x}", arg2);
                return error!(ENOSYS);
            }
            let addr = UserAddress::from(arg3);
            let length = arg4 as usize;
            let name = UserCString::new(UserAddress::from(arg5));
            let mut buf = [0u8; PATH_MAX as usize]; // TODO: How large can these names be?
            let name = current_task.mm.read_c_string(name, &mut buf)?;
            let name = CString::new(name).map_err(|_| errno!(EINVAL))?;
            current_task.mm.set_mapping_name(addr, length, name)?;
            Ok(().into())
        }
        PR_SET_DUMPABLE => {
            let mut dumpable = current_task.mm.dumpable.lock();
            *dumpable = if arg2 == 1 { DumpPolicy::USER } else { DumpPolicy::DISABLE };
            Ok(().into())
        }
        PR_GET_DUMPABLE => {
            let dumpable = current_task.mm.dumpable.lock();
            Ok(match *dumpable {
                DumpPolicy::DISABLE => 0.into(),
                DumpPolicy::USER => 1.into(),
            })
        }
        PR_SET_PDEATHSIG => {
            not_implemented!(current_task, "PR_SET_PDEATHSIG");
            Ok(().into())
        }
        PR_SET_NAME => {
            let addr = UserAddress::from(arg2);
            let mut name = [0u8; 16];
            current_task.mm.read_memory(addr, &mut name)?;
            // The name is truncated to 16 bytes (including the nul)
            name[15] = 0;
            // this will succeed, because we set 0 at end above
            let string_end = name.iter().position(|&c| c == 0).unwrap();

            let name_str = CString::new(&mut name[0..string_end]).or_else(|_| error!(EINVAL))?;
            current_task.thread.set_name(&name_str).map_err(|_| errno!(EINVAL))?;
            current_task.set_command_name(name_str);
            Ok(0.into())
        }
        PR_GET_NAME => {
            let addr = UserAddress::from(arg2);
            current_task.mm.write_memory(addr, current_task.command().to_bytes_with_nul())?;
            Ok(().into())
        }
        PR_SET_PTRACER => {
            not_implemented!(current_task, "prctl(PR_SET_PTRACER, {})", arg2);
            Ok(().into())
        }
        PR_SET_KEEPCAPS => {
            not_implemented!(current_task, "prctl(PR_SET_KEEPCAPS, {})", arg2);
            Ok(().into())
        }
        PR_SET_NO_NEW_PRIVS => {
            not_implemented!(current_task, "prctl(PR_SET_NO_NEW_PRIVS, {})", arg2);
            Ok(().into())
        }
        PR_SET_SECCOMP => {
            not_implemented!(current_task, "prctl(PR_SET_SECCOMP, {})", arg2);
            Ok(().into())
        }
        PR_GET_CHILD_SUBREAPER => {
            let addr = UserAddress::from(arg2);
            let value: i32 =
                if current_task.thread_group.read().is_child_subreaper { 1 } else { 0 };
            current_task.mm.write_object(addr.into(), &value)?;
            Ok(().into())
        }
        PR_SET_CHILD_SUBREAPER => {
            current_task.thread_group.write().is_child_subreaper = arg2 != 0;
            Ok(().into())
        }
        PR_GET_SECUREBITS => {
            let value = current_task.read().creds.securebits.bits();
            Ok(value.into())
        }
        PR_SET_SECUREBITS => {
            // TODO(security): This does not yet respect locked flags.
            if !current_task.read().creds.has_capability(CAP_SETPCAP) {
                return error!(EPERM);
            }

            let securebits = SecureBits::from_bits(arg2 as u32).ok_or_else(|| {
                not_implemented!(current_task, "PR_SET_SECUREBITS: bits 0x{:x}", arg2);
                errno!(ENOSYS)
            })?;
            current_task.write().creds.securebits = securebits;
            Ok(().into())
        }
        PR_CAPBSET_READ => {
            // TODO(security): We don't currently validate capabilities, but this should return an
            // error if the checked capability is invalid.
            let has_cap =
                current_task.read().creds.cap_bounding.contains(Capabilities::try_from(arg2)?);
            Ok(has_cap.into())
        }
        PR_CAPBSET_DROP => {
            if !current_task.read().creds.has_capability(CAP_SETPCAP) {
                return error!(EPERM);
            }

            current_task.write().creds.cap_bounding.remove(Capabilities::try_from(arg2)?);
            Ok(().into())
        }
        _ => {
            not_implemented!(current_task, "prctl: Unknown option: 0x{:x}", option);
            error!(ENOSYS)
        }
    }
}

pub fn sys_arch_prctl(
    current_task: &mut CurrentTask,
    code: u32,
    addr: UserAddress,
) -> Result<(), Errno> {
    match code {
        ARCH_SET_FS => {
            current_task.registers.fs_base = addr.ptr() as u64;
            Ok(())
        }
        ARCH_SET_GS => {
            current_task.registers.gs_base = addr.ptr() as u64;
            Ok(())
        }
        _ => {
            not_implemented!(
                current_task,
                "arch_prctl: Unknown code: code=0x{:x} addr={}",
                code,
                addr
            );
            error!(ENOSYS)
        }
    }
}

pub fn sys_set_tid_address(
    current_task: &CurrentTask,
    user_tid: UserRef<pid_t>,
) -> Result<pid_t, Errno> {
    current_task.write().clear_child_tid = user_tid;
    Ok(current_task.get_tid())
}

pub fn sys_getrusage(
    current_task: &CurrentTask,
    who: i32,
    user_usage: UserRef<rusage>,
) -> Result<(), Errno> {
    const RUSAGE_SELF: i32 = crate::types::uapi::RUSAGE_SELF as i32;
    const RUSAGE_THREAD: i32 = crate::types::uapi::RUSAGE_THREAD as i32;
    // TODO(fxb/76811): Implement proper rusage.
    match who {
        RUSAGE_CHILDREN => (),
        RUSAGE_SELF => (),
        RUSAGE_THREAD => (),
        _ => return error!(EINVAL),
    };

    if !user_usage.is_null() {
        let usage = rusage::default();
        current_task.mm.write_object(user_usage, &usage)?;
    }

    Ok(())
}

pub fn sys_getrlimit(
    current_task: &CurrentTask,
    resource: u32,
    user_rlimit: UserRef<rlimit>,
) -> Result<(), Errno> {
    sys_prlimit64(current_task, 0, resource, Default::default(), user_rlimit)
}

pub fn sys_setrlimit(
    current_task: &CurrentTask,
    resource: u32,
    user_rlimit: UserRef<rlimit>,
) -> Result<(), Errno> {
    sys_prlimit64(current_task, 0, resource, user_rlimit, Default::default())
}

pub fn sys_prlimit64(
    current_task: &CurrentTask,
    pid: pid_t,
    resource: u32,
    new_limit: UserRef<rlimit>,
    old_limit: UserRef<rlimit>,
) -> Result<(), Errno> {
    if pid != 0 {
        not_implemented!(current_task, "prlimit64 with non 0 pid");
        return error!(ENOSYS);
    }
    if !new_limit.is_null() {
        not_implemented!(current_task, "prlimit64 to edit limits");
        return error!(ENOSYS);
    }
    if !old_limit.is_null() {
        let limit = match resource {
            resource if resource == RLIMIT_NOFILE => {
                Ok(rlimit { rlim_cur: RLIMIT_NOFILE_MAX, rlim_max: RLIMIT_NOFILE_MAX })
            }
            resource if resource == RLIMIT_STACK => {
                // The stack size is fixed at the moment, but
                // if MAP_GROWSDOWN is implemented this should
                // report the limit that it can be grown.
                let mm_state = current_task.mm.state.read();
                let stack_size = mm_state.stack_size as u64;
                Ok(rlimit { rlim_cur: stack_size, rlim_max: stack_size })
            }
            _ => {
                not_implemented!(current_task, "getrlimit: {:?}", resource);
                error!(ENOSYS)
            }
        }?;
        current_task.mm.write_object(old_limit, &limit)?;
    }
    Ok(())
}

fn realtime_deadline_to_monotonic(deadline: timespec) -> Result<zx::Time, Errno> {
    let utc_clock = duplicate_utc_clock_handle(zx::Rights::READ).map_err(|_| errno!(EACCES))?;
    let details = utc_clock.get_details().map_err(|_| errno!(EACCES))?;
    let utc_time = time_from_timespec(deadline)?;
    Ok(details.mono_to_synthetic.apply_inverse(utc_time))
}

pub fn sys_futex(
    current_task: &CurrentTask,
    addr: UserAddress,
    op: u32,
    value: u32,
    utime: UserRef<timespec>,
    addr2: UserAddress,
    value3: u32,
) -> Result<(), Errno> {
    // TODO: Distinguish between public and private futexes.
    let _is_private = op & FUTEX_PRIVATE_FLAG != 0;

    let is_realtime = op & FUTEX_CLOCK_REALTIME != 0;
    let cmd = op & (FUTEX_CMD_MASK as u32);
    match cmd {
        FUTEX_WAIT => {
            let deadline = if utime.is_null() {
                zx::Time::INFINITE
            } else {
                // In theory, we should adjust this for a realtime
                // futex when the system gets suspended, but Zircon
                // does  not give us a way to do this.
                let duration = current_task.mm.read_object(utime)?;
                zx::Time::after(duration_from_timespec(duration)?)
            };
            current_task.mm.futex.wait(
                &current_task,
                addr,
                value,
                FUTEX_BITSET_MATCH_ANY,
                deadline,
            )?;
        }
        FUTEX_WAKE => {
            current_task.mm.futex.wake(addr, value as usize, FUTEX_BITSET_MATCH_ANY);
        }
        FUTEX_WAIT_BITSET => {
            if value3 == 0 {
                return error!(EINVAL);
            }
            // The timeout is interpreted differently by WAIT and WAIT_BITSET: WAIT takes a
            // timeout and WAIT_BITSET takes a deadline.
            let deadline = if utime.is_null() {
                zx::Time::INFINITE
            } else if is_realtime {
                realtime_deadline_to_monotonic(current_task.mm.read_object(utime)?)?
            } else {
                let deadline = current_task.mm.read_object(utime)?;
                time_from_timespec(deadline)?
            };
            current_task.mm.futex.wait(&current_task, addr, value, value3, deadline)?;
        }
        FUTEX_WAKE_BITSET => {
            if value3 == 0 {
                return error!(EINVAL);
            }
            current_task.mm.futex.wake(addr, value as usize, value3);
        }
        FUTEX_REQUEUE => {
            current_task.mm.futex.requeue(addr, value as usize, addr2);
        }
        _ => {
            not_implemented!(current_task, "futex: command 0x{:x} not implemented.", cmd);
            return error!(ENOSYS);
        }
    }
    Ok(())
}

fn cap_header_version(
    current_task: &CurrentTask,
    user_header: UserRef<__user_cap_header_struct>,
) -> Result<u32, Errno> {
    let header = current_task.mm.read_object(user_header)?;
    if header.pid != 0 && header.pid != current_task.id {
        not_implemented!(current_task, "Cap header version only implemented for current task.");
        return error!(EINVAL);
    }
    Ok(header.version)
}

pub fn sys_capget(
    current_task: &CurrentTask,
    user_header: UserRef<__user_cap_header_struct>,
    user_data: UserRef<__user_cap_data_struct>,
) -> Result<(), Errno> {
    if user_data.is_null() {
        current_task.mm.write_object(
            user_header,
            &__user_cap_header_struct { version: _LINUX_CAPABILITY_VERSION_3, pid: 0 },
        )?;
        return Ok(());
    }

    let (permitted, effective, inheritable) = {
        let creds = &current_task.read().creds;
        (creds.cap_permitted, creds.cap_effective, creds.cap_inheritable)
    };
    match cap_header_version(current_task, user_header)? {
        _LINUX_CAPABILITY_VERSION_3 => {
            // Return 64 bit capabilities as two sets of 32 bit capabilities, little endian
            let (permitted, effective, inheritable) =
                (permitted.as_abi_v3(), effective.as_abi_v3(), inheritable.as_abi_v3());
            let data: [__user_cap_data_struct; 2] = [
                __user_cap_data_struct {
                    effective: effective.0,
                    inheritable: inheritable.0,
                    permitted: permitted.0,
                },
                __user_cap_data_struct {
                    effective: effective.1,
                    inheritable: inheritable.1,
                    permitted: permitted.1,
                },
            ];
            current_task.mm.write_objects(user_data, &data)?;
        }
        _ => return error!(EINVAL),
    }
    Ok(())
}

pub fn sys_capset(
    current_task: &CurrentTask,
    user_header: UserRef<__user_cap_header_struct>,
    user_data: UserRef<__user_cap_data_struct>,
) -> Result<(), Errno> {
    let (new_permitted, new_effective, new_inheritable) =
        match cap_header_version(current_task, user_header)? {
            _LINUX_CAPABILITY_VERSION_3 => {
                let mut data: [__user_cap_data_struct; 2] = Default::default();
                current_task.mm.read_objects(user_data, &mut data)?;
                (
                    Capabilities::from_abi_v3((data[0].permitted, data[1].permitted)),
                    Capabilities::from_abi_v3((data[0].effective, data[1].effective)),
                    Capabilities::from_abi_v3((data[0].inheritable, data[1].inheritable)),
                )
            }
            _ => return error!(EINVAL),
        };

    // Permission checks. Copied out of TLPI section 39.7.
    {
        let creds = &current_task.read().creds;
        strace!(current_task, "Capabilities({{permitted={:?} from {:?}, effective={:?} from {:?}, inheritable={:?} from {:?}}}, bounding={:?})", new_permitted, creds.cap_permitted, new_effective, creds.cap_effective, new_inheritable, creds.cap_inheritable, creds.cap_bounding);
        if !creds.has_capability(CAP_SETPCAP)
            && !creds.cap_inheritable.union(creds.cap_permitted).contains(new_inheritable)
        {
            return error!(EPERM);
        }

        if !creds.cap_inheritable.union(creds.cap_bounding).contains(new_inheritable) {
            return error!(EPERM);
        }
        if !creds.cap_permitted.contains(new_permitted) {
            return error!(EPERM);
        }
        if !new_permitted.contains(new_effective) {
            return error!(EPERM);
        }
    }

    let creds = &mut current_task.write().creds;
    creds.cap_permitted = new_permitted;
    creds.cap_effective = new_effective;
    creds.cap_inheritable = new_inheritable;
    Ok(())
}

pub fn sys_setgroups(
    current_task: &CurrentTask,
    size: usize,
    groups_addr: UserAddress,
) -> Result<(), Errno> {
    if size > NGROUPS_MAX as usize {
        return error!(EINVAL);
    }
    let mut groups: Vec<gid_t> = vec![0; size];
    current_task.mm.read_memory(groups_addr, groups.as_mut_slice().as_bytes_mut())?;
    let mut state = current_task.write();
    if !state.creds.is_superuser() {
        return error!(EPERM);
    }
    state.creds.groups = groups;
    Ok(())
}

pub fn sys_getgroups(
    current_task: &CurrentTask,
    size: usize,
    groups_addr: UserAddress,
) -> Result<usize, Errno> {
    let state = current_task.read();
    let groups = &state.creds.groups;
    if size != 0 {
        if size < groups.len() {
            return error!(EINVAL);
        }
        current_task.mm.write_memory(groups_addr, groups.as_slice().as_bytes())?;
    }
    Ok(groups.len())
}

pub fn sys_setsid(current_task: &CurrentTask) -> Result<pid_t, Errno> {
    current_task.thread_group.setsid()?;
    Ok(current_task.get_pid())
}

pub fn sys_getpriority(current_task: &CurrentTask, which: u32, who: i32) -> Result<u8, Errno> {
    match which {
        PRIO_PROCESS => {}
        _ => return error!(EINVAL),
    }
    // TODO(tbodt): check permissions
    let task = get_task_or_current(current_task, who)?;
    let state = task.thread_group.read();
    Ok(state.priority)
}

pub fn sys_setpriority(
    current_task: &CurrentTask,
    which: u32,
    who: i32,
    priority: i32,
) -> Result<(), Errno> {
    match which {
        PRIO_PROCESS => {}
        _ => return error!(EINVAL),
    }
    // TODO(tbodt): check permissions
    let task = get_task_or_current(current_task, who)?;
    // The priority passed into setpriority is actually in the -19...20 range and is not
    // transformed into the 1...40 range. The man page is lying. (I sent a patch, so it might not
    // be lying anymore by the time you read this.)
    let priority = 20 - priority;
    task.thread_group.write().priority = priority.clamp(1, 40) as u8;
    Ok(())
}

pub fn sys_unshare(current_task: &CurrentTask, flags: u32) -> Result<(), Errno> {
    const IMPLEMENTED_FLAGS: u32 = CLONE_NEWNS;
    if flags & !IMPLEMENTED_FLAGS != 0 {
        not_implemented!(
            current_task,
            "unshare does not implement flags: 0x{:x}",
            flags & !IMPLEMENTED_FLAGS
        );
        return error!(EINVAL);
    }

    if (flags & CLONE_NEWNS) != 0 {
        if !current_task.read().creds.has_capability(CAP_SYS_ADMIN) {
            return error!(EPERM);
        }
        current_task.fs.unshare_namespace();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::syscalls::sys_munmap;
    use crate::testing::*;
    use std::u64;

    #[::fuchsia::test]
    fn test_prctl_set_vma_anon_name() {
        let (_kernel, current_task) = create_kernel_and_task();

        let mapped_address = map_memory(&current_task, UserAddress::default(), *PAGE_SIZE);
        let name_addr = mapped_address + 128u64;
        let name = "test-name\0";
        current_task.mm.write_memory(name_addr, name.as_bytes()).expect("failed to write name");
        sys_prctl(
            &current_task,
            PR_SET_VMA,
            PR_SET_VMA_ANON_NAME as u64,
            mapped_address.ptr() as u64,
            32,
            name_addr.ptr() as u64,
        )
        .expect("failed to set name");
        assert_eq!(
            CString::new("test-name").unwrap(),
            current_task
                .mm
                .get_mapping_name(mapped_address + 24u64)
                .expect("failed to get address")
        );

        sys_munmap(&current_task, mapped_address, *PAGE_SIZE as usize)
            .expect("failed to unmap memory");
        assert_eq!(error!(EFAULT), current_task.mm.get_mapping_name(mapped_address + 24u64));
    }

    #[::fuchsia::test]
    fn test_prctl_get_set_dumpable() {
        let (_kernel, current_task) = create_kernel_and_task();

        sys_prctl(&current_task, PR_GET_DUMPABLE, 0, 0, 0, 0).expect("failed to get dumpable");

        sys_prctl(&current_task, PR_SET_DUMPABLE, 1, 0, 0, 0).expect("failed to set dumpable");
        sys_prctl(&current_task, PR_GET_DUMPABLE, 0, 0, 0, 0).expect("failed to get dumpable");

        // SUID_DUMP_ROOT not supported.
        sys_prctl(&current_task, PR_SET_DUMPABLE, 2, 0, 0, 0).expect("failed to set dumpable");
        sys_prctl(&current_task, PR_GET_DUMPABLE, 0, 0, 0, 0).expect("failed to get dumpable");
    }

    #[::fuchsia::test]
    fn test_sys_getsid() {
        let (kernel, current_task) = create_kernel_and_task();

        assert_eq!(
            current_task.get_tid(),
            sys_getsid(&current_task, 0).expect("failed to get sid")
        );

        let second_current = create_task(&kernel, "second task");

        assert_eq!(
            second_current.get_tid(),
            sys_getsid(&current_task, second_current.get_tid().into()).expect("failed to get sid")
        );
    }

    #[::fuchsia::test]
    fn test_get_affinity_size() {
        let (_kernel, current_task) = create_kernel_and_task();
        let mapped_address = map_memory(&current_task, UserAddress::default(), *PAGE_SIZE);
        assert_eq!(sys_sched_getaffinity(&current_task, 1, u32::MAX, mapped_address), Ok(()));
        assert_eq!(sys_sched_getaffinity(&current_task, 1, 1, mapped_address), Err(EINVAL));
    }

    #[::fuchsia::test]
    fn test_set_affinity_size() {
        let (_kernel, current_task) = create_kernel_and_task();
        let mapped_address = map_memory(&current_task, UserAddress::default(), *PAGE_SIZE);
        assert_eq!(sys_sched_setaffinity(&current_task, 1, u32::MAX, mapped_address), Ok(()));
        assert_eq!(sys_sched_setaffinity(&current_task, 1, 1, mapped_address), Err(EINVAL));
    }

    #[::fuchsia::test]
    fn test_task_name() {
        let (_kernel, current_task) = create_kernel_and_task();
        let mapped_address = map_memory(&current_task, UserAddress::default(), *PAGE_SIZE);
        let name = "my-task-name\0";
        current_task
            .mm
            .write_memory(mapped_address, name.as_bytes())
            .expect("failed to write name");

        let result =
            sys_prctl(&current_task, PR_SET_NAME, mapped_address.ptr() as u64, 0, 0, 0).unwrap();
        assert_eq!(SUCCESS, result);

        let mapped_address = map_memory(&current_task, UserAddress::default(), *PAGE_SIZE);
        let result =
            sys_prctl(&current_task, PR_GET_NAME, mapped_address.ptr() as u64, 0, 0, 0).unwrap();
        assert_eq!(SUCCESS, result);

        let name_length = name.len();
        let mut out_name = vec![0u8; name_length];

        current_task.mm.read_memory(mapped_address, &mut out_name).unwrap();
        assert_eq!(name.as_bytes(), &out_name);
    }

    #[::fuchsia::test]
    fn test_sched_getparam() {
        let (_kernel, current_task) = create_kernel_and_task();
        let mapped_address = map_memory(&current_task, UserAddress::default(), *PAGE_SIZE);
        sys_sched_getparam(&current_task, 0, mapped_address).expect("sched_getparam");
        let param_value: sched_param =
            current_task.mm.read_object(mapped_address.into()).expect("read_object");
        assert_eq!(param_value.sched_priority, 0);
    }

    #[::fuchsia::test]
    fn test_setuid() {
        let (_kernel, current_task) = create_kernel_and_task();
        // Test for root.
        current_task.write().creds = Credentials::root();
        sys_setuid(&current_task, 42).expect("setuid");
        let creds = current_task.read().creds.clone();
        assert_eq!(creds.euid, 42);
        assert_eq!(creds.uid, 42);
        assert_eq!(creds.saved_uid, 42);

        // Remove the CAP_SETUID capability to avoid overwriting permission checks.
        current_task.write().creds.cap_effective.remove(CAP_SETUID);
        // Test for non root, which task now is.
        assert_eq!(sys_setuid(&current_task, 0), Err(EPERM));
        assert_eq!(sys_setuid(&current_task, 43), Err(EPERM));

        sys_setuid(&current_task, 42).expect("setuid");
        let creds = current_task.read().creds.clone();
        assert_eq!(creds.euid, 42);
        assert_eq!(creds.uid, 42);
        assert_eq!(creds.saved_uid, 42);

        // Change uid and saved_uid, and check that one can set the euid to these.
        current_task.write().creds.uid = 41;
        current_task.write().creds.euid = 42;
        current_task.write().creds.saved_uid = 43;

        sys_setuid(&current_task, 41).expect("setuid");
        let creds = current_task.read().creds.clone();
        assert_eq!(creds.euid, 41);
        assert_eq!(creds.uid, 41);
        assert_eq!(creds.saved_uid, 43);

        current_task.write().creds.uid = 41;
        current_task.write().creds.euid = 42;
        current_task.write().creds.saved_uid = 43;

        sys_setuid(&current_task, 43).expect("setuid");
        let creds = current_task.read().creds.clone();
        assert_eq!(creds.euid, 43);
        assert_eq!(creds.uid, 41);
        assert_eq!(creds.saved_uid, 43);
    }
}
