// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_zircon::{self as zx, AsHandleRef};
use std::ffi::CStr;

use crate::fs::*;
use crate::logging::*;
use crate::mm::*;
use crate::syscalls::*;
use crate::types::*;

fn mmap_prot_to_vm_opt(prot: u32) -> zx::VmarFlags {
    let mut flags = zx::VmarFlags::empty();
    if prot & PROT_READ != 0 {
        flags |= zx::VmarFlags::PERM_READ;
    }
    if prot & PROT_WRITE != 0 {
        flags |= zx::VmarFlags::PERM_WRITE;
    }
    if prot & PROT_EXEC != 0 {
        flags |= zx::VmarFlags::PERM_EXECUTE;
    }
    flags
}

pub fn sys_mmap(
    ctx: &SyscallContext<'_>,
    addr: UserAddress,
    length: usize,
    prot: u32,
    flags: u32,
    fd: FdNumber,
    offset: usize,
) -> Result<SyscallResult, Errno> {
    // These are the flags that are currently supported.
    if prot & !(PROT_READ | PROT_WRITE | PROT_EXEC) != 0 {
        return Err(EINVAL);
    }
    if flags & !(MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED | MAP_NORESERVE) != 0 {
        return Err(EINVAL);
    }

    if flags & (MAP_PRIVATE | MAP_SHARED) == 0
        || flags & (MAP_PRIVATE | MAP_SHARED) == MAP_PRIVATE | MAP_SHARED
    {
        return Err(EINVAL);
    }
    if length == 0 {
        return Err(EINVAL);
    }
    if offset as u64 % *PAGE_SIZE != 0 {
        return Err(EINVAL);
    }

    // TODO(tbodt): should we consider MAP_NORESERVE?

    if flags & MAP_ANONYMOUS != 0 && fd.raw() != -1 {
        return Err(EINVAL);
    }

    let mut zx_flags = mmap_prot_to_vm_opt(prot) | zx::VmarFlags::ALLOW_FAULTS;
    if addr.ptr() != 0 {
        // TODO(tbodt): if no MAP_FIXED, retry on EINVAL
        zx_flags |= zx::VmarFlags::SPECIFIC;
    }
    if flags & MAP_FIXED != 0 {
        // SAFETY: We are operating on another process, so it's safe to use SPECIFIC_OVERWRITE
        zx_flags |= unsafe {
            zx::VmarFlags::from_bits_unchecked(zx::VmarFlagsExtended::SPECIFIC_OVERWRITE.bits())
        };
    }

    let vmo = if flags & MAP_ANONYMOUS != 0 {
        let vmo = zx::Vmo::create(length as u64).map_err(|s| match s {
            zx::Status::NO_MEMORY => ENOMEM,
            _ => impossible_error(s),
        })?;
        vmo.set_name(CStr::from_bytes_with_nul(b"starnix-anon\0").unwrap())
            .map_err(impossible_error)?;
        vmo
    } else {
        // TODO(tbodt): maximize protection flags so that mprotect works
        let file = ctx.task.files.get(fd)?;
        let zx_prot = mmap_prot_to_vm_opt(prot);
        if flags & MAP_PRIVATE != 0 {
            // TODO(tbodt): Use VMO_FLAG_PRIVATE to have the filesystem server do the clone for us.
            let vmo =
                file.ops().get_vmo(&file, &ctx.task, zx_prot - zx::VmarFlags::PERM_WRITE, flags)?;
            let mut clone_flags = zx::VmoChildOptions::COPY_ON_WRITE;
            if !zx_prot.contains(zx::VmarFlags::PERM_WRITE) {
                clone_flags |= zx::VmoChildOptions::NO_WRITE;
            }
            vmo.create_child(clone_flags, 0, vmo.get_size().map_err(impossible_error)?)
                .map_err(impossible_error)?
        } else {
            file.ops().get_vmo(&file, &ctx.task, zx_prot, flags)?
        }
    };
    let vmo_offset = if flags & MAP_ANONYMOUS != 0 { 0 } else { offset };

    let addr = ctx.task.mm.map(addr, vmo, vmo_offset as u64, length, zx_flags)?;
    Ok(addr.into())
}

pub fn sys_mprotect(
    ctx: &SyscallContext<'_>,
    addr: UserAddress,
    length: usize,
    prot: u32,
) -> Result<SyscallResult, Errno> {
    ctx.task.mm.protect(addr, length, mmap_prot_to_vm_opt(prot))?;
    Ok(SUCCESS)
}

pub fn sys_munmap(
    ctx: &SyscallContext<'_>,
    addr: UserAddress,
    length: usize,
) -> Result<SyscallResult, Errno> {
    ctx.task.mm.unmap(addr, length)?;
    Ok(SUCCESS)
}

pub fn sys_brk(ctx: &SyscallContext<'_>, addr: UserAddress) -> Result<SyscallResult, Errno> {
    Ok(ctx.task.mm.set_brk(addr)?.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuchsia_async as fasync;

    use crate::testing::*;

    /// It is ok to call munmap with an address that is a multiple of the page size, and
    /// a non-zero length.
    #[fasync::run_singlethreaded(test)]
    async fn test_munmap() {
        let (_kernel, task_owner) = create_kernel_and_task();
        let ctx = SyscallContext::new(&task_owner.task);

        let mapped_address = map_memory(&ctx, UserAddress::default(), *PAGE_SIZE);
        assert_eq!(sys_munmap(&ctx, mapped_address, *PAGE_SIZE as usize), Ok(SUCCESS));

        // Verify that the memory is no longer readable.
        let mut data: [u8; 5] = [0; 5];
        assert_eq!(ctx.task.mm.read_memory(mapped_address, &mut data), Err(EFAULT));
    }

    /// It is ok to call munmap on an unmapped range.
    #[fasync::run_singlethreaded(test)]
    async fn test_munmap_not_mapped() {
        let (_kernel, task_owner) = create_kernel_and_task();
        let ctx = SyscallContext::new(&task_owner.task);

        let mapped_address = map_memory(&ctx, UserAddress::default(), *PAGE_SIZE);
        assert_eq!(sys_munmap(&ctx, mapped_address, *PAGE_SIZE as usize), Ok(SUCCESS));
        assert_eq!(sys_munmap(&ctx, mapped_address, *PAGE_SIZE as usize), Ok(SUCCESS));
    }

    /// It is an error to call munmap with a length of 0.
    #[fasync::run_singlethreaded(test)]
    async fn test_munmap_0_length() {
        let (_kernel, task_owner) = create_kernel_and_task();
        let ctx = SyscallContext::new(&task_owner.task);

        let mapped_address = map_memory(&ctx, UserAddress::default(), *PAGE_SIZE);
        assert_eq!(sys_munmap(&ctx, mapped_address, 0), Err(EINVAL));
    }

    /// It is an error to call munmap with an address that is not a multiple of the page size.
    #[fasync::run_singlethreaded(test)]
    async fn test_munmap_not_aligned() {
        let (_kernel, task_owner) = create_kernel_and_task();
        let ctx = SyscallContext::new(&task_owner.task);

        let mapped_address = map_memory(&ctx, UserAddress::default(), *PAGE_SIZE);
        assert_eq!(sys_munmap(&ctx, mapped_address + 1u64, *PAGE_SIZE as usize), Err(EINVAL));

        // Verify that the memory is still readable.
        let mut data: [u8; 5] = [0; 5];
        assert_eq!(ctx.task.mm.read_memory(mapped_address, &mut data), Ok(()));
    }

    /// The entire page should be unmapped, not just the range [address, address + length).
    #[fasync::run_singlethreaded(test)]
    async fn test_munmap_unmap_partial() {
        let (_kernel, task_owner) = create_kernel_and_task();
        let ctx = SyscallContext::new(&task_owner.task);

        let mapped_address = map_memory(&ctx, UserAddress::default(), *PAGE_SIZE);
        assert_eq!(sys_munmap(&ctx, mapped_address, (*PAGE_SIZE as usize) / 2), Ok(SUCCESS));

        // Verify that memory can't be read in either half of the page.
        let mut data: [u8; 5] = [0; 5];
        assert_eq!(ctx.task.mm.read_memory(mapped_address, &mut data), Err(EFAULT));
        assert_eq!(
            ctx.task.mm.read_memory(mapped_address + (*PAGE_SIZE - 2), &mut data),
            Err(EFAULT)
        );
    }

    /// All pages that intersect the munmap range should be unmapped.
    #[fasync::run_singlethreaded(test)]
    async fn test_munmap_multiple_pages() {
        let (_kernel, task_owner) = create_kernel_and_task();
        let ctx = SyscallContext::new(&task_owner.task);

        let mapped_address = map_memory(&ctx, UserAddress::default(), *PAGE_SIZE * 2);
        assert_eq!(sys_munmap(&ctx, mapped_address, (*PAGE_SIZE as usize) + 1), Ok(SUCCESS));

        // Verify that neither page is readable.
        let mut data: [u8; 5] = [0; 5];
        assert_eq!(ctx.task.mm.read_memory(mapped_address, &mut data), Err(EFAULT));
        assert_eq!(
            ctx.task.mm.read_memory(mapped_address + *PAGE_SIZE + 1u64, &mut data),
            Err(EFAULT)
        );
    }

    /// Only the pages that intersect the munmap range should be unmapped.
    #[fasync::run_singlethreaded(test)]
    async fn test_munmap_one_of_many_pages() {
        let (_kernel, task_owner) = create_kernel_and_task();
        let ctx = SyscallContext::new(&task_owner.task);

        let mapped_address = map_memory(&ctx, UserAddress::default(), *PAGE_SIZE * 2);
        assert_eq!(sys_munmap(&ctx, mapped_address, (*PAGE_SIZE as usize) - 1), Ok(SUCCESS));

        // Verify that the second page is still readable.
        let mut data: [u8; 5] = [0; 5];
        assert_eq!(ctx.task.mm.read_memory(mapped_address, &mut data), Err(EFAULT));
        assert_eq!(ctx.task.mm.read_memory(mapped_address + *PAGE_SIZE + 1u64, &mut data), Ok(()));
    }
}
