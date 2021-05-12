// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use paste::paste;
use zerocopy::{AsBytes, FromBytes};

use crate::fs::syscalls::*;
use crate::fs::FdNumber;
use crate::mm::syscalls::*;
use crate::signals::syscalls::*;
use crate::signals::types::*;
use crate::syscalls::system::*;
use crate::syscalls::{SyscallContext, SyscallResult};
use crate::task::syscalls::*;
use crate::types::*;

trait FromSyscallArg {
    fn from_arg(arg: u64) -> Self;
}
impl FromSyscallArg for i32 {
    fn from_arg(arg: u64) -> i32 {
        arg as i32
    }
}
impl FromSyscallArg for u32 {
    fn from_arg(arg: u64) -> u32 {
        arg as u32
    }
}
impl FromSyscallArg for usize {
    fn from_arg(arg: u64) -> usize {
        arg as usize
    }
}
impl FromSyscallArg for u64 {
    fn from_arg(arg: u64) -> u64 {
        arg
    }
}
impl FromSyscallArg for UserAddress {
    fn from_arg(arg: u64) -> UserAddress {
        UserAddress::from(arg)
    }
}

impl<T: AsBytes + FromBytes> FromSyscallArg for UserRef<T> {
    fn from_arg(arg: u64) -> UserRef<T> {
        UserRef::<T>::new(UserAddress::from(arg))
    }
}

impl FromSyscallArg for UserCString {
    fn from_arg(arg: u64) -> UserCString {
        UserCString::new(UserAddress::from(arg))
    }
}

impl FromSyscallArg for FdNumber {
    fn from_arg(arg: u64) -> FdNumber {
        FdNumber::from_raw(arg as i32)
    }
}

impl FromSyscallArg for UncheckedSignal {
    fn from_arg(arg: u64) -> UncheckedSignal {
        UncheckedSignal::new(arg)
    }
}
trait IntoSyscallArg<T> {
    fn into_arg(self) -> T;
}
impl<T> IntoSyscallArg<T> for u64
where
    T: FromSyscallArg,
{
    fn into_arg(self) -> T {
        T::from_arg(self)
    }
}

macro_rules! syscall_match {
    {
        $ctx:ident; $syscall_number:ident; $args:ident;
        $($call:ident [$num_args:tt],)*
    } => {
        paste! {
            match $syscall_number as u32 {
                $(crate::types::[<__NR_ $call>] => syscall_match!(@call $ctx; $args; [<sys_ $call>][$num_args]),)*
                _ => sys_unknown($ctx, $syscall_number),
            }
        }
    };

    (@call $ctx:ident; $args:ident; $func:ident [0]) => ($func($ctx));
    (@call $ctx:ident; $args:ident; $func:ident [1]) => ($func($ctx, $args.0.into_arg()));
    (@call $ctx:ident; $args:ident; $func:ident [2]) => ($func($ctx, $args.0.into_arg(), $args.1.into_arg()));
    (@call $ctx:ident; $args:ident; $func:ident [3]) => ($func($ctx, $args.0.into_arg(), $args.1.into_arg(), $args.2.into_arg()));
    (@call $ctx:ident; $args:ident; $func:ident [4]) => ($func($ctx, $args.0.into_arg(), $args.1.into_arg(), $args.2.into_arg(), $args.3.into_arg()));
    (@call $ctx:ident; $args:ident; $func:ident [5]) => ($func($ctx, $args.0.into_arg(), $args.1.into_arg(), $args.2.into_arg(), $args.3.into_arg(), $args.4.into_arg()));
    (@call $ctx:ident; $args:ident; $func:ident [6]) => ($func($ctx, $args.0.into_arg(), $args.1.into_arg(), $args.2.into_arg(), $args.3.into_arg(), $args.4.into_arg(), $args.5.into_arg()));
}

pub fn dispatch_syscall(
    ctx: &mut SyscallContext<'_>,
    syscall_number: u64,
    args: (u64, u64, u64, u64, u64, u64),
) -> Result<SyscallResult, Errno> {
    syscall_match! {
        ctx; syscall_number; args;
        access[2],
        arch_prctl[2],
        brk[1],
        clock_gettime[2],
        close[1],
        exit[1],
        exit_group[1],
        faccessat[3],
        fcntl[3],
        fstat[2],
        fstatfs[2],
        getcwd[2],
        getegid[0],
        geteuid[0],
        getgid[0],
        getpgid[1],
        getpgrp[0],
        getpid[0],
        getppid[0],
        getrandom[3],
        gettid[0],
        gettimeofday[2],
        getuid[0],
        ioctl[4],
        mmap[6],
        mprotect[3],
        munmap[2],
        newfstatat[4],
        openat[4],
        prctl[5],
        pread64[4],
        read[3],
        readlink[3],
        readlinkat[4],
        rt_sigaction[3],
        rt_sigprocmask[4],
        sched_getaffinity[3],
        sched_getscheduler[1],
        sched_setaffinity[3],
        set_tid_address[1],
        sigaltstack[2],
        uname[1],
        write[3],
        writev[3],
    }
}
