// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <align.h>

#include <fbl/alloc_checker.h>
#include <hypervisor/guest_physical_address_space.h>
#include <kernel/range_check.h>
#include <ktl/move.h>
#include <vm/fault.h>
#include <vm/page_source.h>
#include <vm/vm_object_physical.h>

namespace {

constexpr uint kPfFlags = VMM_PF_FLAG_WRITE | VMM_PF_FLAG_SW_FAULT;
constexpr uint kInterruptMmuFlags = ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE;
constexpr uint kGuestMmuFlags =
    ARCH_MMU_FLAG_CACHED | ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE;

fbl::RefPtr<VmMapping> FindMapping(fbl::RefPtr<VmAddressRegion> region, zx_gpaddr_t guest_paddr) {
  for (fbl::RefPtr<VmAddressRegionOrMapping> next; (next = region->FindRegion(guest_paddr));
       region = next->as_vm_address_region()) {
    if (next->is_mapping()) {
      return next->as_vm_mapping();
    }
  }
  return nullptr;
}

}  // namespace

namespace hypervisor {

zx::status<GuestPhysicalAddressSpace> GuestPhysicalAddressSpace::Create(
#if ARCH_ARM64
    uint16_t vmid
#endif
) {
  auto guest_aspace = VmAspace::Create(VmAspace::Type::GuestPhys, "guest_aspace");
  if (!guest_aspace) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }
#if ARCH_ARM64
  guest_aspace->arch_aspace().arch_set_asid(vmid);
#endif
  GuestPhysicalAddressSpace gpas;
  gpas.guest_aspace_ = ktl::move(guest_aspace);
  return zx::ok(ktl::move(gpas));
}

GuestPhysicalAddressSpace::~GuestPhysicalAddressSpace() {
  // VmAspace maintains a circular reference with it's root VMAR. We need to
  // destroy the VmAspace in order to break that reference and allow the
  // VmAspace to be destructed.
  if (guest_aspace_) {
    guest_aspace_->Destroy();
  }
}

zx::status<> GuestPhysicalAddressSpace::MapInterruptController(zx_gpaddr_t guest_paddr,
                                                               zx_paddr_t host_paddr, size_t len) {
  fbl::RefPtr<VmObjectPhysical> vmo;
  zx_status_t status = VmObjectPhysical::Create(host_paddr, len, &vmo);
  if (status != ZX_OK) {
    return zx::error(status);
  }

  status = vmo->SetMappingCachePolicy(ARCH_MMU_FLAG_UNCACHED_DEVICE);
  if (status != ZX_OK) {
    return zx::error(status);
  }

  // The root VMAR will maintain a reference to the VmMapping internally so
  // we don't need to maintain a long-lived reference to the mapping here.
  fbl::RefPtr<VmMapping> mapping;
  status = RootVmar()->CreateVmMapping(guest_paddr, vmo->size(), /* align_pow2*/ 0,
                                       VMAR_FLAG_SPECIFIC, vmo, /* vmo_offset */ 0,
                                       kInterruptMmuFlags, "guest_interrupt_vmo", &mapping);
  if (status != ZX_OK) {
    return zx::error(status);
  }

  // Write mapping to page table.
  status = mapping->MapRange(0, vmo->size(), true);
  if (status != ZX_OK) {
    mapping->Destroy();
    return zx::error(status);
  }

  return zx::ok();
}

zx::status<> GuestPhysicalAddressSpace::UnmapRange(zx_gpaddr_t guest_paddr, size_t len) {
  zx_status_t status = RootVmar()->UnmapAllowPartial(guest_paddr, len);
  return zx::make_status(status);
}

zx::status<zx_paddr_t> GuestPhysicalAddressSpace::GetPage(zx_gpaddr_t guest_paddr) {
  fbl::RefPtr<VmMapping> mapping = FindMapping(RootVmar(), guest_paddr);
  if (!mapping) {
    return zx::error(ZX_ERR_NOT_FOUND);
  }

  zx_paddr_t host_paddr;
  zx_gpaddr_t offset;
  {
    Guard<Mutex> guard(mapping->lock());
    offset = guest_paddr - mapping->base() + mapping->object_offset_locked();
  }

  zx_status_t status =
      mapping->vmo()->GetPageBlocking(offset, kPfFlags, nullptr, nullptr, &host_paddr);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(host_paddr);
}

zx::status<> GuestPhysicalAddressSpace::PageFault(zx_gpaddr_t guest_paddr) {
  // TOOD(fxb/94078): Enforce no other locks are held here since we may wait on the page request.
  __UNINITIALIZED LazyPageRequest page_request;

  zx_status_t status = ZX_OK;
  do {
    fbl::RefPtr<VmMapping> mapping = FindMapping(RootVmar(), guest_paddr);
    if (!mapping) {
      return zx::error(ZX_ERR_NOT_FOUND);
    }

    // In order to avoid re-faulting if the guest changes how it accesses guest
    // physical memory, and to avoid the need for invalidation of the guest
    // physical address space on x86 (through the use of INVEPT), we fault the
    // page with the maximum allowable permissions of the mapping.
    {
      Guard<Mutex> guard{mapping->lock()};
      uint pf_flags = VMM_PF_FLAG_GUEST | VMM_PF_FLAG_HW_FAULT;
      uint mmu_flags = mapping->arch_mmu_flags_locked(guest_paddr);
      if (mmu_flags & ARCH_MMU_FLAG_PERM_WRITE) {
        pf_flags |= VMM_PF_FLAG_WRITE;
      }
      if (mmu_flags & ARCH_MMU_FLAG_PERM_EXECUTE) {
        pf_flags |= VMM_PF_FLAG_INSTRUCTION;
      }

      status = mapping->PageFault(guest_paddr, pf_flags, &page_request);
    }

    if (status == ZX_ERR_SHOULD_WAIT) {
      zx_status_t st = page_request->Wait();
      if (st != ZX_OK) {
        return zx::error(st);
      }
    }
  } while (status == ZX_ERR_SHOULD_WAIT);

  return zx::make_status(status);
}

zx::status<uint> GuestPhysicalAddressSpace::QueryFlags(zx_gpaddr_t guest_paddr) {
  fbl::RefPtr<VmMapping> mapping = FindMapping(RootVmar(), guest_paddr);
  if (!mapping) {
    return zx::error(ZX_ERR_NOT_FOUND);
  }

  uint mmu_flags;
  zx_gpaddr_t offset;
  {
    Guard<Mutex> guard(mapping->lock());
    offset = guest_paddr - mapping->base() + mapping->object_offset_locked();
  }

  zx_status_t status = mapping->aspace()->arch_aspace().Query(offset, nullptr, &mmu_flags);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(mmu_flags);
}

zx::status<GuestPtr> GuestPhysicalAddressSpace::CreateGuestPtr(zx_gpaddr_t guest_paddr, size_t len,
                                                               const char* name) {
  const zx_gpaddr_t begin = ROUNDDOWN(guest_paddr, PAGE_SIZE);
  const zx_gpaddr_t end = ROUNDUP(guest_paddr + len, PAGE_SIZE);
  const zx_gpaddr_t mapping_len = end - begin;
  if (begin > end || !InRange(begin, mapping_len, size())) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  fbl::RefPtr<VmAddressRegionOrMapping> region = RootVmar()->FindRegion(begin);
  if (!region) {
    return zx::error(ZX_ERR_NOT_FOUND);
  }
  fbl::RefPtr<VmMapping> guest_mapping = region->as_vm_mapping();
  if (!guest_mapping) {
    return zx::error(ZX_ERR_WRONG_TYPE);
  }
  const uint64_t intra_mapping_offset = begin - guest_mapping->base();
  if (!InRange(intra_mapping_offset, mapping_len, guest_mapping->size())) {
    // The address range is not contained within a single mapping.
    return zx::error(ZX_ERR_OUT_OF_RANGE);
  }

  uint64_t mapping_object_offset;
  {
    Guard<Mutex> guard{guest_mapping->lock()};
    mapping_object_offset = guest_mapping->object_offset_locked();
  }

  fbl::RefPtr<VmMapping> host_mapping;
  zx_status_t status = VmAspace::kernel_aspace()->RootVmar()->CreateVmMapping(
      /* mapping_offset */ 0, mapping_len,
      /* align_pow2 */ false,
      /* vmar_flags */ 0, guest_mapping->vmo(), mapping_object_offset + intra_mapping_offset,
      kGuestMmuFlags, name, &host_mapping);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  // Pre-populate the page tables so there's no need for kernel page faults.
  status = host_mapping->MapRange(0, mapping_len, true);
  if (status != ZX_OK) {
    return zx::error(status);
  }

  return zx::ok(GuestPtr(ktl::move(host_mapping), guest_paddr - begin));
}

}  // namespace hypervisor
