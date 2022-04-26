// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::convert::TryFrom;
use std::sync::Arc;

use super::signalfd::*;
use crate::fs::*;
use crate::logging::not_implemented;
use crate::signals::*;
use crate::syscalls::*;
use crate::task::*;
use crate::types::*;
use zerocopy::FromBytes;

pub fn sys_rt_sigaction(
    current_task: &CurrentTask,
    signum: UncheckedSignal,
    user_action: UserRef<sigaction_t>,
    user_old_action: UserRef<sigaction_t>,
    sigset_size: usize,
) -> Result<(), Errno> {
    if sigset_size != std::mem::size_of::<sigset_t>() {
        return error!(EINVAL);
    }

    let signal = Signal::try_from(signum)?;

    let new_signal_action = if !user_action.is_null() {
        // Actions can't be set for SIGKILL and SIGSTOP, but the actions for these signals can
        // still be returned in `user_old_action`, so only return early if the intention is to
        // set an action (i.e., the user_action is non-null).
        if signal.is_unblockable() {
            return error!(EINVAL);
        }

        let mut signal_action = sigaction_t::default();
        current_task.mm.read_object(user_action, &mut signal_action)?;
        Some(signal_action)
    } else {
        None
    };

    let signal_actions = &current_task.thread_group.signal_actions;
    let old_action = if let Some(new_signal_action) = new_signal_action {
        signal_actions.set(signal, new_signal_action)
    } else {
        signal_actions.get(signal)
    };

    if !user_old_action.is_null() {
        current_task.mm.write_object(user_old_action, &old_action)?;
    }

    Ok(())
}

pub fn sys_rt_sigprocmask(
    current_task: &CurrentTask,
    how: u32,
    user_set: UserRef<sigset_t>,
    user_old_set: UserRef<sigset_t>,
    sigset_size: usize,
) -> Result<(), Errno> {
    if sigset_size != std::mem::size_of::<sigset_t>() {
        return error!(EINVAL);
    }
    match how {
        SIG_BLOCK | SIG_UNBLOCK | SIG_SETMASK => (),
        _ => return error!(EINVAL),
    };

    // Read the new mask. This must be done before the old maks is written to `user_old_set`
    // since it might point to the same location as `user_set`.
    let mut new_mask = sigset_t::default();
    if !user_set.is_null() {
        current_task.mm.read_object(user_set, &mut new_mask)?;
    }

    let mut state = current_task.write();
    let signal_state = &mut state.signals;
    let signal_mask = signal_state.mask;
    // If old_set is not null, store the previous value in old_set.
    if !user_old_set.is_null() {
        current_task.mm.write_object(user_old_set, &signal_mask)?;
    }

    // If set is null, how is ignored and the mask is not updated.
    if user_set.is_null() {
        return Ok(());
    }

    let signal_mask = match how {
        SIG_BLOCK => (signal_mask | new_mask),
        SIG_UNBLOCK => signal_mask & !new_mask,
        SIG_SETMASK => new_mask,
        // Arguments have already been verified, this should never match.
        _ => return error!(EINVAL),
    };
    signal_state.set_signal_mask(signal_mask);

    Ok(())
}

pub fn sys_sigaltstack(
    current_task: &CurrentTask,
    user_ss: UserRef<sigaltstack_t>,
    user_old_ss: UserRef<sigaltstack_t>,
) -> Result<(), Errno> {
    let mut state = current_task.write();
    let mut signal_state = &mut state.signals;
    let on_signal_stack = signal_state
        .alt_stack
        .map(|signal_stack| signal_stack.contains_pointer(current_task.registers.rsp))
        .unwrap_or(false);

    let mut ss = sigaltstack_t::default();
    if !user_ss.is_null() {
        if on_signal_stack {
            return error!(EPERM);
        }
        current_task.mm.read_object(user_ss, &mut ss)?;
        if (ss.ss_flags & !(SS_AUTODISARM | SS_DISABLE)) != 0 {
            return error!(EINVAL);
        }
    }

    if !user_old_ss.is_null() {
        let mut old_ss = match signal_state.alt_stack {
            Some(old_ss) => old_ss,
            None => sigaltstack_t { ss_flags: SS_DISABLE, ..sigaltstack_t::default() },
        };
        if on_signal_stack {
            old_ss.ss_flags = SS_ONSTACK;
        }
        current_task.mm.write_object(user_old_ss, &old_ss)?;
    }

    if !user_ss.is_null() {
        if ss.ss_flags & SS_DISABLE != 0 {
            signal_state.alt_stack = None;
        } else {
            signal_state.alt_stack = Some(ss);
        }
    }

    Ok(())
}

pub fn sys_rt_sigsuspend(
    current_task: &mut CurrentTask,
    user_mask: UserRef<sigset_t>,
    sigset_size: usize,
) -> Result<(), Errno> {
    if sigset_size != std::mem::size_of::<sigset_t>() {
        return error!(EINVAL);
    }

    let mut mask = sigset_t::default();
    current_task.mm.read_object(user_mask, &mut mask)?;

    let waiter = Waiter::new();
    current_task.wait_with_temporary_mask(mask, |current_task| waiter.wait(current_task))?;

    Ok(())
}

fn send_unchecked_signal(task: &Task, unchecked_signal: &UncheckedSignal) -> Result<(), Errno> {
    // 0 is a sentinel value used to do permission checks.
    let sentinel_signal = UncheckedSignal::from(0);
    if *unchecked_signal == sentinel_signal {
        return Ok(());
    }

    send_signal(
        task,
        SignalInfo {
            code: SI_USER as i32,
            ..SignalInfo::default(Signal::try_from(unchecked_signal)?)
        },
    );
    Ok(())
}

fn send_unchecked_signal_info(
    task: &Task,
    unchecked_signal: &UncheckedSignal,
    info: SignalInfo,
) -> Result<(), Errno> {
    // 0 is a sentinel value used to do permission checks.
    let sentinel_signal = UncheckedSignal::from(0);
    if *unchecked_signal == sentinel_signal {
        return Ok(());
    }

    send_signal(task, info);
    Ok(())
}

pub fn sys_kill(
    current_task: &CurrentTask,
    pid: pid_t,
    unchecked_signal: UncheckedSignal,
) -> Result<(), Errno> {
    let pids = current_task.thread_group.kernel.pids.read();
    match pid {
        pid if pid > 0 => {
            // "If pid is positive, then signal sig is sent to the process with
            // the ID specified by pid."
            let target_thread_group =
                &current_task.get_task(pid).ok_or(errno!(ESRCH))?.thread_group;
            let target = get_signal_target(target_thread_group, &unchecked_signal, &pids)
                .ok_or(errno!(ESRCH))?;
            if !current_task.can_signal(&target, &unchecked_signal) {
                return error!(EPERM);
            }
            send_unchecked_signal(&target, &unchecked_signal)?;
        }
        pid if pid == -1 => {
            // "If pid equals -1, then sig is sent to every process for which
            // the calling process has permission to send signals, except for
            // process 1 (init), but ... POSIX.1-2001 requires that kill(-1,sig)
            // send sig to all processes that the calling process may send
            // signals to, except possibly for some implementation-defined
            // system processes. Linux allows a process to signal itself, but on
            // Linux the call kill(-1,sig) does not signal the calling process."

            let thread_groups = pids.get_thread_groups();
            signal_thread_groups(
                &current_task,
                &unchecked_signal,
                thread_groups.into_iter().filter(|thread_group| {
                    if current_task.thread_group == *thread_group {
                        return false;
                    }
                    if thread_group.leader == 1 {
                        return false;
                    }
                    true
                }),
            )?;
        }
        _ => {
            // "If pid equals 0, then sig is sent to every process in the
            // process group of the calling process."
            //
            // "If pid is less than -1, then sig is sent to every process in the
            // process group whose ID is -pid."
            let process_group_id = match pid {
                0 => current_task.thread_group.read().process_group.leader,
                _ => -pid,
            };

            let thread_groups = {
                let process_group = pids.get_process_group(process_group_id);
                process_group
                    .iter()
                    .flat_map(|pg| {
                        pg.thread_groups
                            .read()
                            .iter()
                            .flat_map(|p| pids.get_thread_group(*p))
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>()
            };
            signal_thread_groups(&current_task, &unchecked_signal, thread_groups.into_iter())?;
        }
    };

    Ok(())
}

pub fn sys_tgkill(
    current_task: &CurrentTask,
    tgid: pid_t,
    tid: pid_t,
    unchecked_signal: UncheckedSignal,
) -> Result<(), Errno> {
    // Linux returns EINVAL when the tgid or tid <= 0.
    if tgid <= 0 || tid <= 0 {
        return error!(EINVAL);
    }

    let target = current_task.get_task(tid).ok_or(errno!(ESRCH))?;
    if target.get_pid() != tgid {
        return error!(EINVAL);
    }

    if !current_task.can_signal(&target, &unchecked_signal) {
        return error!(EPERM);
    }

    send_unchecked_signal(&target, &unchecked_signal)?;
    Ok(())
}

pub fn sys_rt_sigreturn(current_task: &mut CurrentTask) -> Result<SyscallResult, Errno> {
    restore_from_signal_handler(current_task)?;
    Ok(SyscallResult::keep_regs(current_task))
}

pub fn sys_rt_tgsigqueueinfo(
    current_task: &CurrentTask,
    tgid: pid_t,
    tid: pid_t,
    unchecked_signal: UncheckedSignal,
    siginfo_ref: UserAddress,
) -> Result<(), Errno> {
    let mut siginfo_mem = [0u8; SI_MAX_SIZE as usize];
    current_task.mm.read_memory(siginfo_ref, &mut siginfo_mem)?;

    let header = SignalInfoHeader::read_from(&siginfo_mem[..SI_HEADER_SIZE]).unwrap();

    let signal = Signal::try_from(unchecked_signal)?;

    let this_pid = current_task.get_pid();
    if this_pid == tgid {
        if header.code >= 0 || header.code == SI_TKILL {
            return error!(EINVAL);
        }
    }

    let mut bytes = [0u8; SI_MAX_SIZE as usize - SI_HEADER_SIZE];
    bytes.copy_from_slice(&siginfo_mem[SI_HEADER_SIZE..SI_MAX_SIZE as usize]);
    let details = SignalDetail::Raw { data: bytes };
    let signal_info = SignalInfo {
        signal,
        errno: header.errno,
        code: header.code,
        detail: details,
        force: false,
    };

    let target = current_task.get_task(tid).ok_or(errno!(ESRCH))?;
    if target.get_pid() != tgid {
        return error!(EINVAL);
    }
    if !current_task.can_signal(&target, &unchecked_signal) {
        return error!(EPERM);
    }

    send_unchecked_signal_info(&target, &unchecked_signal, signal_info)?;
    Ok(())
}

/// Sends a signal to all thread groups in `thread_groups`.
///
/// # Parameters
/// - `task`: The task that is sending the signal.
/// - `unchecked_signal`: The signal that is to be sent. Unchecked, since `0` is a sentinel value
/// where rights are to be checked but no signal is actually sent.
/// - `thread_groups`: The thread groups to signal.
///
/// # Returns
/// Returns Ok(()) if at least one signal was sent, otherwise the last error that was encountered.
fn signal_thread_groups<F>(
    task: &Task,
    unchecked_signal: &UncheckedSignal,
    thread_groups: F,
) -> Result<(), Errno>
where
    F: Iterator<Item = Arc<ThreadGroup>>,
{
    let pids = task.thread_group.kernel.pids.read();
    let mut last_error = errno!(ESRCH);
    let mut sent_signal = false;

    // This loop keeps track of whether a signal was sent, so that "on
    // success (at least one signal was sent), zero is returned."
    for thread_group in thread_groups {
        let target =
            get_signal_target(&thread_group, unchecked_signal, &pids).ok_or(errno!(ESRCH))?;
        if !task.can_signal(&target, &unchecked_signal) {
            last_error = errno!(EPERM);
            continue;
        }

        match send_unchecked_signal(&target, unchecked_signal) {
            Ok(_) => sent_signal = true,
            Err(errno) => last_error = errno,
        }
    }

    if sent_signal {
        return Ok(());
    } else {
        return Err(last_error);
    }
}

/// The generic options for both waitid and wait4.
pub struct WaitingOptions {
    /// Wait for a process that has exited.
    pub wait_for_exited: bool,
    /// Wait for a process in the stop state.
    pub wait_for_stopped: bool,
    /// Wait for a process that was continued.
    pub wait_for_continued: bool,
    /// Block the wait until a process matches.
    pub block: bool,
    /// Do not clear the waitable state.
    pub keep_waitable_state: bool,
}

impl WaitingOptions {
    fn new(options: u32) -> Self {
        assert!(WUNTRACED == WSTOPPED);
        Self {
            wait_for_exited: options & WEXITED > 0,
            wait_for_stopped: options & WSTOPPED > 0,
            wait_for_continued: options & WCONTINUED > 0,
            block: options & WNOHANG == 0,
            keep_waitable_state: options & WNOWAIT > 0,
        }
    }

    /// Build a `WaitingOptions` from the waiting flags of waitid.
    pub fn new_for_waitid(options: u32) -> Result<Self, Errno> {
        if options & !(WNOHANG | WNOWAIT | WSTOPPED | WEXITED | WCONTINUED) != 0 {
            not_implemented!("unsupported waitid options: {:#x}", options);
            return error!(EINVAL);
        }
        if options & (WEXITED | WSTOPPED | WCONTINUED) == 0 {
            return error!(EINVAL);
        }
        Ok(Self::new(options))
    }

    /// Build a `WaitingOptions` from the waiting flags of wait4.
    pub fn new_for_wait4(options: u32) -> Result<Self, Errno> {
        if options & !(WNOHANG | WUNTRACED | WCONTINUED) != 0 {
            not_implemented!("unsupported waitid options: {:#x}", options);
            return error!(EINVAL);
        }
        Ok(Self::new(options | WEXITED))
    }
}

/// Waits on the task with `pid` to exit.
///
/// - `current_task`: The current task.
/// - `pid`: The id of the task to wait on.
/// - `options`: The options passed to the wait syscall.
fn wait_on_pid(
    task: &Task,
    selector: ProcessSelector,
    options: &WaitingOptions,
) -> Result<Option<ZombieProcess>, Errno> {
    let waiter = Waiter::new_for_wait_on_child_thread();
    let (mut _guard, mut wait_result) = match waiter.register_waiter(task) {
        Ok(guard) => (Some(guard), Ok(())),
        Err(err) => (None, Err(err)),
    };
    loop {
        let child = task.thread_group.get_waitable_child(selector, options)?;
        match child {
            WaitableChild::Available(zombie) => {
                return Ok(Some(zombie));
            }
            WaitableChild::Pending => {}
            WaitableChild::NotFound => {
                return error!(ECHILD);
            }
        }

        if !options.block {
            return Ok(None);
        }
        // Return any error encountered during previous iteration's wait. This is done after the
        // zombie process has been dequeued to make sure that the zombie process is returned even
        // if the wait was interrupted.
        wait_result?;
        wait_result = waiter.wait_kernel();
    }
}

pub fn sys_waitid(
    current_task: &CurrentTask,
    id_type: u32,
    id: i32,
    user_info: UserAddress,
    options: u32,
) -> Result<(), Errno> {
    let waiting_options = WaitingOptions::new_for_waitid(options)?;

    let task_selector = match id_type {
        P_PID => ProcessSelector::Pid(id),
        P_ALL => ProcessSelector::Any,
        P_PGID => ProcessSelector::Pgid(if id == 0 {
            current_task.thread_group.read().process_group.leader
        } else {
            id
        }),
        P_PIDFD => {
            not_implemented!("unsupported waitpid id_type {:?}", id_type);
            return error!(ENOSYS);
        }
        _ => return error!(EINVAL),
    };

    // wait_on_pid returns None if the task was not waited on. In that case, we don't write out a
    // siginfo. This seems weird but is the correct behavior according to the waitid(2) man page.
    if let Some(waitable_process) = wait_on_pid(current_task, task_selector, &waiting_options)? {
        let siginfo = waitable_process.as_signal_info();
        current_task.mm.write_memory(user_info, &siginfo.as_siginfo_bytes())?;
    }

    Ok(())
}

pub fn sys_wait4(
    current_task: &CurrentTask,
    pid: pid_t,
    user_wstatus: UserRef<i32>,
    options: u32,
    user_rusage: UserRef<rusage>,
) -> Result<pid_t, Errno> {
    let waiting_options = WaitingOptions::new_for_wait4(options)?;

    let selector = if pid == 0 {
        ProcessSelector::Pgid(current_task.thread_group.read().process_group.leader)
    } else if pid == -1 {
        ProcessSelector::Any
    } else if pid > 0 {
        ProcessSelector::Pid(pid)
    } else if pid < -1 {
        ProcessSelector::Pgid(-pid)
    } else {
        not_implemented!("unimplemented wait4 pid selector {}", pid);
        return error!(ENOSYS);
    };

    if let Some(waitable_process) = wait_on_pid(current_task, selector, &waiting_options)? {
        let status = waitable_process.exit_status.wait_status();

        if !user_rusage.is_null() {
            let usage = rusage::default();
            // TODO(fxb/76976): Return proper usage information.
            current_task.mm.write_object(user_rusage, &usage)?;
            not_implemented!("wait4 does not set rusage info");
        }

        if !user_wstatus.is_null() {
            current_task.mm.write_object(user_wstatus, &status)?;
        }

        Ok(waitable_process.pid)
    } else {
        Ok(0)
    }
}

pub fn sys_signalfd4(
    current_task: &CurrentTask,
    fd: FdNumber,
    mask_addr: UserRef<sigset_t>,
    mask_size: usize,
    flags: u32,
) -> Result<FdNumber, Errno> {
    if fd.raw() != -1 {
        not_implemented!("changing mask of a signalfd");
        return error!(EINVAL);
    }
    if flags & !(SFD_CLOEXEC | SFD_NONBLOCK) != 0 {
        return error!(EINVAL);
    }
    if mask_size != std::mem::size_of::<sigset_t>() {
        return error!(EINVAL);
    }

    let mut mask: sigset_t = 0;
    current_task.mm.read_object(mask_addr, &mut mask)?;
    let signalfd = SignalFd::new(current_task.kernel(), mask, flags);
    let flags = if flags & SFD_CLOEXEC != 0 { FdFlags::CLOEXEC } else { FdFlags::empty() };
    let fd = current_task.files.add_with_flags(signalfd, flags)?;
    Ok(fd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Credentials;
    use crate::mm::PAGE_SIZE;
    use crate::testing::*;
    use std::convert::TryInto;
    use zerocopy::AsBytes;

    #[::fuchsia::test]
    fn test_sigaltstack() {
        let (_kernel, current_task) = create_kernel_and_task();
        let addr = map_memory(&current_task, UserAddress::default(), *PAGE_SIZE);

        let user_ss = UserRef::<sigaltstack_t>::new(addr);
        let nullptr = UserRef::<sigaltstack_t>::default();

        // Check that the initial state is disabled.
        sys_sigaltstack(&current_task, nullptr, user_ss).expect("failed to call sigaltstack");
        let mut ss = sigaltstack_t::default();
        current_task.mm.read_object(user_ss, &mut ss).expect("failed to read struct");
        assert!(ss.ss_flags & SS_DISABLE != 0);

        // Install a sigaltstack and read it back out.
        ss.ss_sp = UserAddress::from(0x7FFFF);
        ss.ss_size = 0x1000;
        ss.ss_flags = SS_AUTODISARM;
        current_task.mm.write_object(user_ss, &ss).expect("failed to write struct");
        sys_sigaltstack(&current_task, user_ss, nullptr).expect("failed to call sigaltstack");
        current_task
            .mm
            .write_memory(addr, &[0u8; std::mem::size_of::<sigaltstack_t>()])
            .expect("failed to clear struct");
        sys_sigaltstack(&current_task, nullptr, user_ss).expect("failed to call sigaltstack");
        let mut another_ss = sigaltstack_t::default();
        current_task.mm.read_object(user_ss, &mut another_ss).expect("failed to read struct");
        assert_eq!(ss, another_ss);

        // Disable the sigaltstack and read it back out.
        ss.ss_flags = SS_DISABLE;
        current_task.mm.write_object(user_ss, &ss).expect("failed to write struct");
        sys_sigaltstack(&current_task, user_ss, nullptr).expect("failed to call sigaltstack");
        current_task
            .mm
            .write_memory(addr, &[0u8; std::mem::size_of::<sigaltstack_t>()])
            .expect("failed to clear struct");
        sys_sigaltstack(&current_task, nullptr, user_ss).expect("failed to call sigaltstack");
        current_task.mm.read_object(user_ss, &mut ss).expect("failed to read struct");
        assert!(ss.ss_flags & SS_DISABLE != 0);
    }

    /// It is invalid to call rt_sigprocmask with a sigsetsize that does not match the size of
    /// sigset_t.
    #[::fuchsia::test]
    fn test_sigprocmask_invalid_size() {
        let (_kernel, current_task) = create_kernel_and_task();

        let set = UserRef::<sigset_t>::default();
        let old_set = UserRef::<sigset_t>::default();
        let how = 0;

        assert_eq!(
            sys_rt_sigprocmask(
                &current_task,
                how,
                set,
                old_set,
                std::mem::size_of::<sigset_t>() * 2
            ),
            error!(EINVAL)
        );
        assert_eq!(
            sys_rt_sigprocmask(
                &current_task,
                how,
                set,
                old_set,
                std::mem::size_of::<sigset_t>() / 2
            ),
            error!(EINVAL)
        );
    }

    /// It is invalid to call rt_sigprocmask with a bad `how`.
    #[::fuchsia::test]
    fn test_sigprocmask_invalid_how() {
        let (_kernel, current_task) = create_kernel_and_task();
        let addr = map_memory(&current_task, UserAddress::default(), *PAGE_SIZE);

        let set = UserRef::<sigset_t>::new(addr);
        let old_set = UserRef::<sigset_t>::default();
        let how = SIG_SETMASK | SIG_UNBLOCK | SIG_BLOCK;

        assert_eq!(
            sys_rt_sigprocmask(&current_task, how, set, old_set, std::mem::size_of::<sigset_t>()),
            error!(EINVAL)
        );
    }

    /// It is valid to call rt_sigprocmask with a null value for set. In that case, old_set should
    /// contain the current signal mask.
    #[::fuchsia::test]
    fn test_sigprocmask_null_set() {
        let (_kernel, current_task) = create_kernel_and_task();
        let addr = map_memory(&current_task, UserAddress::default(), *PAGE_SIZE);
        let original_mask = SIGTRAP.mask();
        {
            current_task.write().signals.mask = original_mask;
        }

        let set = UserRef::<sigset_t>::default();
        let old_set = UserRef::<sigset_t>::new(addr);
        let how = SIG_SETMASK;

        current_task
            .mm
            .write_memory(addr, &[0u8; std::mem::size_of::<sigset_t>()])
            .expect("failed to clear struct");

        assert_eq!(
            sys_rt_sigprocmask(&current_task, how, set, old_set, std::mem::size_of::<sigset_t>()),
            Ok(())
        );

        let mut old_mask = sigset_t::default();
        current_task.mm.read_object(old_set, &mut old_mask).expect("failed to read mask");
        assert_eq!(old_mask, original_mask);
    }

    /// It is valid to call rt_sigprocmask with null values for both set and old_set.
    /// In this case, how should be ignored and the set remains the same.
    #[::fuchsia::test]
    fn test_sigprocmask_null_set_and_old_set() {
        let (_kernel, current_task) = create_kernel_and_task();
        let original_mask = SIGTRAP.mask();
        {
            current_task.write().signals.mask = original_mask;
        }

        let set = UserRef::<sigset_t>::default();
        let old_set = UserRef::<sigset_t>::default();
        let how = SIG_SETMASK;

        assert_eq!(
            sys_rt_sigprocmask(&current_task, how, set, old_set, std::mem::size_of::<sigset_t>()),
            Ok(())
        );
        assert_eq!(current_task.read().signals.mask, original_mask);
    }

    /// Calling rt_sigprocmask with SIG_SETMASK should set the mask to the provided set.
    #[::fuchsia::test]
    fn test_sigprocmask_setmask() {
        let (_kernel, current_task) = create_kernel_and_task();
        let addr = map_memory(&current_task, UserAddress::default(), *PAGE_SIZE);
        current_task
            .mm
            .write_memory(addr, &[0u8; std::mem::size_of::<sigset_t>() * 2])
            .expect("failed to clear struct");

        let original_mask = SIGTRAP.mask();
        {
            current_task.write().signals.mask = original_mask;
        }

        let new_mask: sigset_t = SIGIO.mask();
        let set = UserRef::<sigset_t>::new(addr);
        current_task.mm.write_object(set, &new_mask).expect("failed to set mask");

        let old_set = UserRef::<sigset_t>::new(addr + std::mem::size_of::<sigset_t>());
        let how = SIG_SETMASK;

        assert_eq!(
            sys_rt_sigprocmask(&current_task, how, set, old_set, std::mem::size_of::<sigset_t>()),
            Ok(())
        );

        let mut old_mask = sigset_t::default();
        current_task.mm.read_object(old_set, &mut old_mask).expect("failed to read mask");
        assert_eq!(old_mask, original_mask);
        assert_eq!(current_task.read().signals.mask, new_mask);
    }

    /// Calling st_sigprocmask with a how of SIG_BLOCK should add to the existing set.
    #[::fuchsia::test]
    fn test_sigprocmask_block() {
        let (_kernel, current_task) = create_kernel_and_task();
        let addr = map_memory(&current_task, UserAddress::default(), *PAGE_SIZE);
        current_task
            .mm
            .write_memory(addr, &[0u8; std::mem::size_of::<sigset_t>() * 2])
            .expect("failed to clear struct");

        let original_mask = SIGTRAP.mask();
        {
            current_task.write().signals.mask = original_mask;
        }

        let new_mask: sigset_t = SIGIO.mask();
        let set = UserRef::<sigset_t>::new(addr);
        current_task.mm.write_object(set, &new_mask).expect("failed to set mask");

        let old_set = UserRef::<sigset_t>::new(addr + std::mem::size_of::<sigset_t>());
        let how = SIG_BLOCK;

        assert_eq!(
            sys_rt_sigprocmask(&current_task, how, set, old_set, std::mem::size_of::<sigset_t>()),
            Ok(())
        );

        let mut old_mask = sigset_t::default();
        current_task.mm.read_object(old_set, &mut old_mask).expect("failed to read mask");
        assert_eq!(old_mask, original_mask);
        assert_eq!(current_task.read().signals.mask, new_mask | original_mask);
    }

    /// Calling st_sigprocmask with a how of SIG_UNBLOCK should remove from the existing set.
    #[::fuchsia::test]
    fn test_sigprocmask_unblock() {
        let (_kernel, current_task) = create_kernel_and_task();
        let addr = map_memory(&current_task, UserAddress::default(), *PAGE_SIZE);
        current_task
            .mm
            .write_memory(addr, &[0u8; std::mem::size_of::<sigset_t>() * 2])
            .expect("failed to clear struct");

        let original_mask = SIGTRAP.mask() | SIGIO.mask();
        {
            current_task.write().signals.mask = original_mask;
        }

        let new_mask: sigset_t = SIGTRAP.mask();
        let set = UserRef::<sigset_t>::new(addr);
        current_task.mm.write_object(set, &new_mask).expect("failed to set mask");

        let old_set = UserRef::<sigset_t>::new(addr + std::mem::size_of::<sigset_t>());
        let how = SIG_UNBLOCK;

        assert_eq!(
            sys_rt_sigprocmask(&current_task, how, set, old_set, std::mem::size_of::<sigset_t>()),
            Ok(())
        );

        let mut old_mask = sigset_t::default();
        current_task.mm.read_object(old_set, &mut old_mask).expect("failed to read mask");
        assert_eq!(old_mask, original_mask);
        assert_eq!(current_task.read().signals.mask, SIGIO.mask());
    }

    /// It's ok to call sigprocmask to unblock a signal that is not set.
    #[::fuchsia::test]
    fn test_sigprocmask_unblock_not_set() {
        let (_kernel, current_task) = create_kernel_and_task();
        let addr = map_memory(&current_task, UserAddress::default(), *PAGE_SIZE);
        current_task
            .mm
            .write_memory(addr, &[0u8; std::mem::size_of::<sigset_t>() * 2])
            .expect("failed to clear struct");

        let original_mask = SIGIO.mask();
        {
            current_task.write().signals.mask = original_mask;
        }

        let new_mask: sigset_t = SIGTRAP.mask();
        let set = UserRef::<sigset_t>::new(addr);
        current_task.mm.write_object(set, &new_mask).expect("failed to set mask");

        let old_set = UserRef::<sigset_t>::new(addr + std::mem::size_of::<sigset_t>());
        let how = SIG_UNBLOCK;

        assert_eq!(
            sys_rt_sigprocmask(&current_task, how, set, old_set, std::mem::size_of::<sigset_t>()),
            Ok(())
        );

        let mut old_mask = sigset_t::default();
        current_task.mm.read_object(old_set, &mut old_mask).expect("failed to read mask");
        assert_eq!(old_mask, original_mask);
        assert_eq!(current_task.read().signals.mask, original_mask);
    }

    /// It's not possible to block SIGKILL or SIGSTOP.
    #[::fuchsia::test]
    fn test_sigprocmask_kill_stop() {
        let (_kernel, current_task) = create_kernel_and_task();
        let addr = map_memory(&current_task, UserAddress::default(), *PAGE_SIZE);
        current_task
            .mm
            .write_memory(addr, &[0u8; std::mem::size_of::<sigset_t>() * 2])
            .expect("failed to clear struct");

        let original_mask = SIGIO.mask();
        {
            current_task.write().signals.mask = original_mask;
        }

        let new_mask: sigset_t = SIGSTOP.mask() | SIGKILL.mask();
        let set = UserRef::<sigset_t>::new(addr);
        current_task.mm.write_object(set, &new_mask).expect("failed to set mask");

        let old_set = UserRef::<sigset_t>::new(addr + std::mem::size_of::<sigset_t>());
        let how = SIG_BLOCK;

        assert_eq!(
            sys_rt_sigprocmask(&current_task, how, set, old_set, std::mem::size_of::<sigset_t>()),
            Ok(())
        );

        let mut old_mask = sigset_t::default();
        current_task.mm.read_object(old_set, &mut old_mask).expect("failed to read mask");
        assert_eq!(old_mask, original_mask);
        assert_eq!(current_task.read().signals.mask, original_mask);
    }

    #[::fuchsia::test]
    fn test_sigaction_invalid_signal() {
        let (_kernel, current_task) = create_kernel_and_task();
        assert_eq!(
            sys_rt_sigaction(
                &current_task,
                UncheckedSignal::from(SIGKILL),
                // The signal is only checked when the action is set (i.e., action is non-null).
                UserRef::<sigaction_t>::new(UserAddress::from(10)),
                UserRef::<sigaction_t>::default(),
                std::mem::size_of::<sigset_t>(),
            ),
            error!(EINVAL)
        );
        assert_eq!(
            sys_rt_sigaction(
                &current_task,
                UncheckedSignal::from(SIGSTOP),
                // The signal is only checked when the action is set (i.e., action is non-null).
                UserRef::<sigaction_t>::new(UserAddress::from(10)),
                UserRef::<sigaction_t>::default(),
                std::mem::size_of::<sigset_t>(),
            ),
            error!(EINVAL)
        );
        assert_eq!(
            sys_rt_sigaction(
                &current_task,
                UncheckedSignal::from(Signal::NUM_SIGNALS + 1),
                // The signal is only checked when the action is set (i.e., action is non-null).
                UserRef::<sigaction_t>::new(UserAddress::from(10)),
                UserRef::<sigaction_t>::default(),
                std::mem::size_of::<sigset_t>(),
            ),
            error!(EINVAL)
        );
    }

    #[::fuchsia::test]
    fn test_sigaction_old_value_set() {
        let (_kernel, current_task) = create_kernel_and_task();
        let addr = map_memory(&current_task, UserAddress::default(), *PAGE_SIZE);
        current_task
            .mm
            .write_memory(addr, &[0u8; std::mem::size_of::<sigaction_t>()])
            .expect("failed to clear struct");

        let mut original_action = sigaction_t::default();
        original_action.sa_mask = 3;

        {
            current_task.thread_group.signal_actions.set(SIGHUP, original_action.clone());
        }

        let old_action_ref = UserRef::<sigaction_t>::new(addr);
        assert_eq!(
            sys_rt_sigaction(
                &current_task,
                UncheckedSignal::from(SIGHUP),
                UserRef::<sigaction_t>::default(),
                old_action_ref,
                std::mem::size_of::<sigset_t>()
            ),
            Ok(())
        );

        let mut old_action = sigaction_t::default();
        current_task
            .mm
            .read_object(old_action_ref, &mut old_action)
            .expect("failed to read action");
        assert_eq!(old_action, original_action);
    }

    #[::fuchsia::test]
    fn test_sigaction_new_value_set() {
        let (_kernel, current_task) = create_kernel_and_task();
        let addr = map_memory(&current_task, UserAddress::default(), *PAGE_SIZE);
        current_task
            .mm
            .write_memory(addr, &[0u8; std::mem::size_of::<sigaction_t>()])
            .expect("failed to clear struct");

        let mut original_action = sigaction_t::default();
        original_action.sa_mask = 3;
        let set_action_ref = UserRef::<sigaction_t>::new(addr);
        current_task
            .mm
            .write_object(set_action_ref, &original_action)
            .expect("failed to set action");

        assert_eq!(
            sys_rt_sigaction(
                &current_task,
                UncheckedSignal::from(SIGINT),
                set_action_ref,
                UserRef::<sigaction_t>::default(),
                std::mem::size_of::<sigset_t>(),
            ),
            Ok(())
        );

        assert_eq!(current_task.thread_group.signal_actions.get(SIGINT), original_action,);
    }

    /// A task should be able to signal itself.
    #[::fuchsia::test]
    fn test_kill_same_task() {
        let (_kernel, current_task) = create_kernel_and_task();

        assert_eq!(sys_kill(&current_task, current_task.id, SIGINT.into()), Ok(()));
    }

    /// A task should be able to signal its own thread group.
    #[::fuchsia::test]
    fn test_kill_own_thread_group() {
        let (_kernel, init_task) = create_kernel_and_task();
        let task1 = init_task.clone_task_for_test(0);
        task1.thread_group.setsid().expect("setsid");
        let task2 = task1.clone_task_for_test(0);

        assert_eq!(sys_kill(&task1, 0, SIGINT.into()), Ok(()));
        assert_eq!(task1.read().signals.queued_count(SIGINT), 1);
        assert_eq!(task2.read().signals.queued_count(SIGINT), 1);
        assert_eq!(init_task.read().signals.queued_count(SIGINT), 0);
    }

    /// A task should be able to signal a thread group.
    #[::fuchsia::test]
    fn test_kill_thread_group() {
        let (_kernel, init_task) = create_kernel_and_task();
        let task1 = init_task.clone_task_for_test(0);
        task1.thread_group.setsid().expect("setsid");
        let task2 = task1.clone_task_for_test(0);

        assert_eq!(sys_kill(&task1, -task1.id, SIGINT.into()), Ok(()));
        assert_eq!(task1.read().signals.queued_count(SIGINT), 1);
        assert_eq!(task2.read().signals.queued_count(SIGINT), 1);
        assert_eq!(init_task.read().signals.queued_count(SIGINT), 0);
    }

    /// A task should be able to signal everything but init and itself.
    #[::fuchsia::test]
    fn test_kill_all() {
        let (_kernel, init_task) = create_kernel_and_task();
        let task1 = init_task.clone_task_for_test(0);
        task1.thread_group.setsid().expect("setsid");
        let task2 = task1.clone_task_for_test(0);

        assert_eq!(sys_kill(&task1, -1, SIGINT.into()), Ok(()));
        assert_eq!(task1.read().signals.queued_count(SIGINT), 0);
        assert_eq!(task2.read().signals.queued_count(SIGINT), 1);
        assert_eq!(init_task.read().signals.queued_count(SIGINT), 0);
    }

    /// A task should not be able to signal a nonexistent task.
    #[::fuchsia::test]
    fn test_kill_inexistant_task() {
        let (_kernel, current_task) = create_kernel_and_task();

        assert_eq!(sys_kill(&current_task, 9, SIGINT.into()), error!(ESRCH));
    }

    /// A task should not be able to signal a task owned by another uid.
    #[::fuchsia::test]
    fn test_kill_invalid_task() {
        let (_kernel, task1) = create_kernel_and_task();
        // Task must not have the kill capability.
        task1.write().creds =
            Credentials::from_passwd("foo:x:1:1").expect("Credentials::from_passwd");
        let task2 = task1.clone_task_for_test(0);
        task2.write().creds = Credentials::from_passwd("bin:x:2:2:bin:/bin:/usr/sbin/nologin")
            .expect("build credentials");

        assert_eq!(task1.can_signal(&task2, &SIGINT.into()), false);
        assert_eq!(sys_kill(&task2, task1.id, SIGINT.into()), error!(EPERM));
        assert_eq!(task1.read().signals.queued_count(SIGINT), 0);
    }

    /// A task should not be able to signal a task owned by another uid in a thead group.
    #[::fuchsia::test]
    fn test_kill_invalid_task_in_thread_group() {
        let (_kernel, init_task) = create_kernel_and_task();
        let task1 = init_task.clone_task_for_test(0);
        task1.thread_group.setsid().expect("setsid");
        let task2 = task1.clone_task_for_test(0);
        task2.thread_group.setsid().expect("setsid");
        task2.write().creds = Credentials::from_passwd("bin:x:2:2:bin:/bin:/usr/sbin/nologin")
            .expect("build credentials");

        assert_eq!(task2.can_signal(&task1, &SIGINT.into()), false);
        assert_eq!(sys_kill(&task2, -task1.id, SIGINT.into()), error!(EPERM));
        assert_eq!(task1.read().signals.queued_count(SIGINT), 0);
    }

    /// A task should not be able to send an invalid signal.
    #[::fuchsia::test]
    fn test_kill_invalid_signal() {
        let (_kernel, current_task) = create_kernel_and_task();

        assert_eq!(
            sys_kill(&current_task, current_task.id, UncheckedSignal::from(75)),
            error!(EINVAL)
        );
    }

    /// Sending a blocked signal should result in a pending signal.
    #[::fuchsia::test]
    fn test_blocked_signal_pending() {
        let (_kernel, current_task) = create_kernel_and_task();
        let addr = map_memory(&current_task, UserAddress::default(), *PAGE_SIZE);
        current_task
            .mm
            .write_memory(addr, &[0u8; std::mem::size_of::<sigset_t>() * 2])
            .expect("failed to clear struct");

        let new_mask: sigset_t = SIGIO.mask();
        let set = UserRef::<sigset_t>::new(addr);
        current_task.mm.write_object(set, &new_mask).expect("failed to set mask");

        assert_eq!(
            sys_rt_sigprocmask(
                &current_task,
                SIG_BLOCK,
                set,
                UserRef::default(),
                std::mem::size_of::<sigset_t>()
            ),
            Ok(())
        );
        assert_eq!(sys_kill(&current_task, current_task.id, SIGIO.into()), Ok(()));
        assert_eq!(current_task.read().signals.queued_count(SIGIO), 1);

        // A second signal should not increment the number of pending signals.
        assert_eq!(sys_kill(&current_task, current_task.id, SIGIO.into()), Ok(()));
        assert_eq!(current_task.read().signals.queued_count(SIGIO), 1);
    }

    /// More than one instance of a real-time signal can be blocked.
    #[::fuchsia::test]
    fn test_blocked_real_time_signal_pending() {
        let (_kernel, current_task) = create_kernel_and_task();
        let addr = map_memory(&current_task, UserAddress::default(), *PAGE_SIZE);
        current_task
            .mm
            .write_memory(addr, &[0u8; std::mem::size_of::<sigset_t>() * 2])
            .expect("failed to clear struct");

        let new_mask: sigset_t = SIGRTMIN.mask();
        let set = UserRef::<sigset_t>::new(addr);
        current_task.mm.write_object(set, &new_mask).expect("failed to set mask");

        assert_eq!(
            sys_rt_sigprocmask(
                &current_task,
                SIG_BLOCK,
                set,
                UserRef::default(),
                std::mem::size_of::<sigset_t>()
            ),
            Ok(())
        );
        assert_eq!(sys_kill(&current_task, current_task.id, SIGRTMIN.into()), Ok(()));
        assert_eq!(current_task.read().signals.queued_count(SIGRTMIN), 1);

        // A second signal should increment the number of pending signals.
        assert_eq!(sys_kill(&current_task, current_task.id, SIGRTMIN.into()), Ok(()));
        assert_eq!(current_task.read().signals.queued_count(SIGRTMIN), 2);
    }

    #[::fuchsia::test]
    fn test_suspend() {
        let (kernel, first_current) = create_kernel_and_task();
        let first_task_clone = first_current.task_arc_clone();
        let first_task_id = first_task_clone.id;

        let thread = std::thread::spawn(move || {
            let mut current_task = first_current;
            let addr = map_memory(&current_task, UserAddress::default(), *PAGE_SIZE);
            let user_ref = UserRef::<sigset_t>::new(addr);

            let sigset: sigset_t = !SIGHUP.mask();
            current_task.mm.write_object(user_ref, &sigset).expect("failed to set action");

            assert_eq!(
                sys_rt_sigsuspend(&mut current_task, user_ref, std::mem::size_of::<sigset_t>()),
                error!(EINTR)
            );
        });

        let current_task = create_task(&kernel, "test-task-2");

        // Wait for the first task to be suspended.
        let mut suspended = false;
        while !suspended {
            suspended = first_task_clone.read().signals.waiter.is_some();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // Signal the suspended task with a signal that is not blocked (only SIGHUP in this test).
        let _ = sys_kill(&current_task, first_task_id, UncheckedSignal::from(SIGHUP));

        // Wait for the sigsuspend to complete.
        let _ = thread.join();

        assert!(first_task_clone.read().signals.waiter.is_none());
    }

    #[::fuchsia::test]
    fn test_stop_cont() {
        let (_kernel, task) = create_kernel_and_task();
        let mut child = task.clone_task_for_test(0);

        assert_eq!(sys_kill(&task, child.id, UncheckedSignal::from(SIGSTOP)), Ok(()));
        // Child should be stopped immediately.
        assert_eq!(child.thread_group.read().stopped, true);
        dequeue_signal(&mut child);

        // Child is now waitable using WUNTRACED.
        assert_eq!(
            sys_wait4(&task, child.id, UserRef::default(), WNOHANG | WUNTRACED, UserRef::default()),
            Ok(child.id)
        );
        // The same wait does not happen twice.
        assert_eq!(
            sys_wait4(&task, child.id, UserRef::default(), WNOHANG | WUNTRACED, UserRef::default()),
            Ok(0)
        );

        assert_eq!(sys_kill(&task, child.id, UncheckedSignal::from(SIGCONT)), Ok(()));
        // Child should be restarted immediately.
        assert_eq!(child.thread_group.read().stopped, false);
        dequeue_signal(&mut child);

        // Child is now waitable using WUNTRACED.
        assert_eq!(
            sys_wait4(
                &task,
                child.id,
                UserRef::default(),
                WNOHANG | WCONTINUED,
                UserRef::default()
            ),
            Ok(child.id)
        );
        // The same wait does not happen twice.
        assert_eq!(
            sys_wait4(
                &task,
                child.id,
                UserRef::default(),
                WNOHANG | WCONTINUED,
                UserRef::default()
            ),
            Ok(0)
        );
    }

    /// Waitid does not support all options.
    #[::fuchsia::test]
    fn test_waitid_options() {
        let (_kernel, current_task) = create_kernel_and_task();
        let id = 1;
        assert_eq!(sys_waitid(&current_task, P_PID, id, UserAddress::default(), 0), Err(EINVAL));
        assert_eq!(
            sys_waitid(&current_task, P_PID, id, UserAddress::default(), 0xffff),
            Err(EINVAL)
        );
    }

    /// Wait4 does not support all options.
    #[::fuchsia::test]
    fn test_wait4_options() {
        let (_kernel, current_task) = create_kernel_and_task();
        let id = 1;
        assert_eq!(
            sys_wait4(&current_task, id, UserRef::default(), WEXITED, UserRef::default()),
            Err(EINVAL)
        );
        assert_eq!(
            sys_wait4(&current_task, id, UserRef::default(), WNOWAIT, UserRef::default()),
            Err(EINVAL)
        );
        assert_eq!(
            sys_wait4(&current_task, id, UserRef::default(), 0xffff, UserRef::default()),
            Err(EINVAL)
        );
    }

    #[::fuchsia::test]
    fn test_echild_when_no_zombie() {
        let (_kernel, current_task) = create_kernel_and_task();
        // Send the signal to the task.
        assert!(
            sys_kill(&current_task, current_task.get_pid(), UncheckedSignal::from(SIGCHLD)).is_ok()
        );
        // Verify that ECHILD is returned because there is no zombie process and no children to
        // block waiting for.
        assert_eq!(
            wait_on_pid(
                &current_task,
                ProcessSelector::Any,
                &WaitingOptions::new_for_wait4(0).expect("WaitingOptions")
            ),
            Err(ECHILD)
        );
    }

    #[::fuchsia::test]
    fn test_no_error_when_zombie() {
        let (_kernel, current_task) = create_kernel_and_task();
        let child = current_task.clone_task_for_test(0);
        let expected_zombie = ZombieProcess {
            pid: child.id,
            pgid: child.thread_group.read().process_group.leader,
            uid: 0,
            exit_status: ExitStatus::Exit(1),
        };
        child.thread_group.exit(ExitStatus::Exit(1));
        std::mem::drop(child);

        assert_eq!(
            wait_on_pid(
                &current_task,
                ProcessSelector::Any,
                &WaitingOptions::new_for_wait4(0).expect("WaitingOptions")
            ),
            Ok(Some(expected_zombie))
        );
    }

    #[::fuchsia::test]
    fn test_waiting_for_child() {
        let (_kernel, task) = create_kernel_and_task();
        let child = task.clone_task_for_test(0);

        // No child is currently terminated.
        assert_eq!(
            wait_on_pid(
                &task,
                ProcessSelector::Any,
                &WaitingOptions::new_for_wait4(WNOHANG).expect("WaitingOptions")
            ),
            Ok(None)
        );

        let task_clone = task.task_arc_clone();
        let child_id = child.id;
        let thread = std::thread::spawn(move || {
            // Block until child is terminated.
            let waited_child = wait_on_pid(
                &task_clone,
                ProcessSelector::Any,
                &WaitingOptions::new_for_wait4(0).expect("WaitingOptions"),
            )
            .expect("wait_on_pid")
            .unwrap();
            assert_eq!(waited_child.pid, child_id);
        });

        // Wait for the thread to be blocked on waiting for a child.
        while task.read().signals.waiter.is_none() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        child.thread_group.exit(ExitStatus::Exit(0));
        std::mem::drop(child);

        // Child is deleted, the thread must be able to terminate.
        thread.join().expect("join");
    }

    #[::fuchsia::test]
    fn test_waiting_for_child_with_signal_pending() {
        let (_kernel, task) = create_kernel_and_task();

        // Register a signal action to ensure that the `SIGUSR1` signal interrupts the task.
        task.thread_group.signal_actions.set(
            SIGUSR1,
            sigaction_t {
                sa_handler: UserAddress::from(0xDEADBEEF),
                sa_restorer: UserAddress::from(0xDEADBEEF),
                ..sigaction_t::default()
            },
        );

        // Start a child task. This will ensure that `wait_on_pid` tries to wait for the child.
        let _child = task.clone_task_for_test(0);

        // Send a signal to the task. `wait_on_pid` should realize there is a signal pending when
        // entering a wait and return with `EINTR`.
        send_signal(&task, SignalInfo::default(SIGUSR1));

        let errno = wait_on_pid(
            &task,
            ProcessSelector::Any,
            &WaitingOptions::new_for_wait4(0).expect("WaitingOptions"),
        )
        .expect_err("wait_on_pid");
        assert_eq!(errno, EINTR);
    }

    #[::fuchsia::test]
    fn test_sigkill() {
        let (_kernel, current_task) = create_kernel_and_task();
        let child = current_task.clone_task_for_test(0);

        // Send SigKill to the child. As kill is handled immediately, no need to dequeue signals.
        send_signal(&child, SignalInfo::default(SIGKILL));
        std::mem::drop(child);

        // Retrieve the exit status.
        let address =
            map_memory(&current_task, UserAddress::default(), std::mem::size_of::<i32>() as u64);
        let address_ref = UserRef::<i32>::new(address);
        sys_wait4(&current_task, -1, address_ref, 0, UserRef::default()).expect("wait4");
        let mut wstatus: i32 = 0;
        current_task.mm.read_object(address_ref, &mut wstatus).expect("read memory");
        assert_eq!(wstatus, SIGKILL.number() as i32);
    }

    fn test_exit_status_for_signal(sig: Signal, wait_status: i32) {
        let (_kernel, current_task) = create_kernel_and_task();
        let mut child = current_task.clone_task_for_test(0);

        // Send the signal to the child.
        send_signal(&child, SignalInfo::default(sig));
        dequeue_signal(&mut child);
        std::mem::drop(child);

        // Retrieve the exit status.
        let address =
            map_memory(&current_task, UserAddress::default(), std::mem::size_of::<i32>() as u64);
        let address_ref = UserRef::<i32>::new(address);
        sys_wait4(&current_task, -1, address_ref, 0, UserRef::default()).expect("wait4");
        let mut wstatus: i32 = 0;
        current_task.mm.read_object(address_ref, &mut wstatus).expect("read memory");
        assert_eq!(wstatus, wait_status);
    }

    #[::fuchsia::test]
    fn test_exit_status() {
        // Default action is Terminate
        test_exit_status_for_signal(SIGTERM, SIGTERM.number() as i32);
        // Default action is CoreDump
        test_exit_status_for_signal(SIGSEGV, (SIGSEGV.number() as i32) | 0x80);
    }

    #[::fuchsia::test]
    fn test_wait4_by_pgid() {
        let (_kernel, current_task) = create_kernel_and_task();
        let child1 = current_task.clone_task_for_test(0);
        let child1_pid = child1.id;
        child1.thread_group.exit(ExitStatus::Exit(42));
        std::mem::drop(child1);
        let child2 = current_task.clone_task_for_test(0);
        child2.thread_group.setsid().expect("setsid");
        let child2_pid = child2.id;
        child2.thread_group.exit(ExitStatus::Exit(42));
        std::mem::drop(child2);

        assert_eq!(
            sys_wait4(&current_task, -child2_pid, UserRef::default(), 0, UserRef::default()),
            Ok(child2_pid)
        );
        assert_eq!(
            sys_wait4(&current_task, 0, UserRef::default(), 0, UserRef::default()),
            Ok(child1_pid)
        );
    }

    #[::fuchsia::test]
    fn test_waitid_by_pgid() {
        let (_kernel, current_task) = create_kernel_and_task();
        let child1 = current_task.clone_task_for_test(0);
        let child1_pid = child1.id;
        child1.thread_group.exit(ExitStatus::Exit(42));
        std::mem::drop(child1);
        let child2 = current_task.clone_task_for_test(0);
        child2.thread_group.setsid().expect("setsid");
        let child2_pid = child2.id;
        child2.thread_group.exit(ExitStatus::Exit(42));
        std::mem::drop(child2);

        let address = map_memory(&current_task, UserAddress::default(), *PAGE_SIZE as u64);
        assert_eq!(sys_waitid(&current_task, P_PGID, child2_pid, address, WEXITED), Ok(()));
        // The previous wait matched child2, only child1 should be in the available zombies.
        assert_eq!(current_task.thread_group.read().zombie_children[0].pid, child1_pid);

        assert_eq!(sys_waitid(&current_task, P_PGID, 0, address, WEXITED), Ok(()));
    }

    #[::fuchsia::test]
    fn test_sigqueue() {
        let (kernel, current_task) = create_kernel_and_task();
        let current_uid = current_task.read().creds.uid;
        let current_pid = current_task.get_pid();

        const TEST_VALUE: u64 = 101;

        // Taken from gVisor of SignalInfo in  //pkg/abi/linux/signal.go
        const PID_DATA_OFFSET: usize = SI_HEADER_SIZE;
        const UID_DATA_OFFSET: usize = SI_HEADER_SIZE + 4;
        const VALUE_DATA_OFFSET: usize = SI_HEADER_SIZE + 8;

        let mut data = vec![0u8; SI_MAX_SIZE as usize];
        let mut header = SignalInfoHeader::default();
        header.code = SI_QUEUE;
        header.write_to(&mut data[..SI_HEADER_SIZE]);
        data[PID_DATA_OFFSET..PID_DATA_OFFSET + 4].copy_from_slice(&current_pid.to_ne_bytes());
        data[UID_DATA_OFFSET..UID_DATA_OFFSET + 4].copy_from_slice(&current_uid.to_ne_bytes());
        data[VALUE_DATA_OFFSET..VALUE_DATA_OFFSET + 8].copy_from_slice(&TEST_VALUE.to_ne_bytes());

        let addr = map_memory(&current_task, UserAddress::default(), *PAGE_SIZE);
        current_task.mm.write_memory(addr, &data).unwrap();
        let second_current = create_task(&kernel, "second task");
        let second_pid = second_current.get_pid();
        let second_tid = second_current.get_tid();
        assert_eq!(second_current.read().signals.queued_count(SIGIO), 0);

        assert_eq!(
            sys_rt_tgsigqueueinfo(
                &current_task,
                second_pid,
                second_tid,
                UncheckedSignal::from(SIGIO),
                addr
            ),
            Ok(())
        );
        assert_eq!(second_current.read().signals.queued_count(SIGIO), 1);

        let queued_signal = second_current
            .write()
            .signals
            .take_next_where(|sig| sig.signal.is_in_set(SIGIO.mask()));
        if let Some(sig) = queued_signal {
            assert_eq!(sig.signal, SIGIO);
            assert_eq!(sig.errno, 0);
            assert_eq!(sig.code, SI_QUEUE);
            if let SignalDetail::Raw { data } = sig.detail {
                // offsets into the raw portion of the signal info
                let offset_pid = PID_DATA_OFFSET - SI_HEADER_SIZE;
                let offset_uid = UID_DATA_OFFSET - SI_HEADER_SIZE;
                let offset_value = VALUE_DATA_OFFSET - SI_HEADER_SIZE;
                let pid =
                    pid_t::from_ne_bytes(data[offset_pid..offset_pid + 4].try_into().unwrap());
                let uid =
                    uid_t::from_ne_bytes(data[offset_uid..offset_uid + 4].try_into().unwrap());
                let value =
                    u64::from_ne_bytes(data[offset_value..offset_value + 8].try_into().unwrap());
                assert_eq!(pid, current_pid);
                assert_eq!(uid, current_uid);
                assert_eq!(value, TEST_VALUE);
            } else {
                panic!("incorrect signal detail");
            }
        } else {
            panic!("expected a queued signal");
        }
    }
}
