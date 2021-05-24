// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::signals::Signal;
use crate::syscalls::SyscallContext;
use crate::types::sigaction_t;
use crate::types::UserAddress;
use fuchsia_zircon::sys::zx_thread_state_general_regs_t;

/// The size of the red zone.
///
/// From the AMD64 ABI:
///   > The 128-byte area beyond the location pointed to
///   > by %rsp is considered to be reserved and shall not be modified by signal or
///   > interrupt handlers. Therefore, functions may use this area for temporary
///   > data that is not needed across function calls. In particular, leaf functions
///   > may use this area for their entire stack frame, rather than adjusting the
///   > stack pointer in the prologue and epilogue. This area is known as the red
///   > zone.
#[cfg(target_arch = "x86_64")]
const RED_ZONE_SIZE: u64 = 128;

/// A `SignalStackFrame` contains all the state that is stored on the stack prior
/// to executing a signal handler.
///
/// Note that the ordering of the fields is significant. In particular, restorer_address
/// must be the first field, since that is where the signal handler will return after
/// it finishes executing.
#[repr(C)]
#[derive(Default, Debug)]
struct SignalStackFrame {
    /// The address of the signal handler function.
    restorer_address: u64,

    // The register state at the time the signal was handled.
    registers: zx_thread_state_general_regs_t,
}

const SIG_STACK_SIZE: usize = std::mem::size_of::<SignalStackFrame>();

impl SignalStackFrame {
    fn new(registers: zx_thread_state_general_regs_t, restorer_address: u64) -> SignalStackFrame {
        SignalStackFrame { registers, restorer_address }
    }

    fn as_bytes(self) -> [u8; SIG_STACK_SIZE] {
        unsafe { std::mem::transmute(self) }
    }

    fn from_bytes(bytes: [u8; SIG_STACK_SIZE]) -> SignalStackFrame {
        unsafe { std::mem::transmute(bytes) }
    }
}

/// Aligns the stack pointer to be 16 byte aligned, and then misaligns it by 8 bytes.
///
/// This is done because x86-64 functions expect the stack to be misaligned by 8 bytes,
/// as if the stack was 16 byte aligned and then someone used a call instruction. This
/// is due to alignment-requiring SSE instructions.
fn misalign_stack_pointer(pointer: u64) -> u64 {
    pointer - (pointer % 16 + 8)
}

/// Prepares `ctx` state to execute the signal handler stored in `action`.
///
/// This function stores the state required to restore after the signal handler on the stack.
// TODO(lindkvist): Honor the flags in `sa_flags`.
// TODO(lindkvist): Apply `sa_mask` to block signals during the execution of the signal handler.
pub fn dispatch_signal_handler(ctx: &mut SyscallContext<'_>, signal: Signal, action: sigaction_t) {
    let signal_stack_frame = SignalStackFrame::new(ctx.registers, action.sa_restorer.ptr() as u64);

    // Adjust the stack pointer to allow space for the red zone.
    // https://en.wikipedia.org/wiki/Red_zone_%28computing%29
    let mut stack_pointer = ctx.registers.rsp - RED_ZONE_SIZE;
    stack_pointer -= SIG_STACK_SIZE as u64;
    stack_pointer = misalign_stack_pointer(stack_pointer);

    // Write the signal stack frame at the updated stack pointer.
    ctx.task
        .mm
        .write_memory(UserAddress::from(stack_pointer), &signal_stack_frame.as_bytes())
        .unwrap();

    ctx.registers.rsp = stack_pointer;
    ctx.registers.rdi = signal.number as u64;
    ctx.registers.rip = action.sa_handler.ptr() as u64;
}

pub fn restore_from_signal_handler(ctx: &mut SyscallContext<'_>) {
    // The stack pointer was intentionally misaligned, so this must be done
    // again to get the correct address for the stack frame.
    let signal_frame_address = misalign_stack_pointer(ctx.registers.rsp);
    let mut signal_stack_bytes = [0; SIG_STACK_SIZE];
    ctx.task
        .mm
        .read_memory(UserAddress::from(signal_frame_address), &mut signal_stack_bytes)
        .unwrap();

    let signal_stack_frame = SignalStackFrame::from_bytes(signal_stack_bytes);
    // Restore the register state from before executing the signal handler.
    ctx.registers = signal_stack_frame.registers;

    // If the task is suspended, wake it.
    ctx.task.wake();
}
