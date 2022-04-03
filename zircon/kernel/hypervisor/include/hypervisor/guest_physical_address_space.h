// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_HYPERVISOR_INCLUDE_HYPERVISOR_GUEST_PHYSICAL_ADDRESS_SPACE_H_
#define ZIRCON_KERNEL_HYPERVISOR_INCLUDE_HYPERVISOR_GUEST_PHYSICAL_ADDRESS_SPACE_H_

#include <lib/zx/status.h>

#include <ktl/move.h>
#include <ktl/unique_ptr.h>
#include <vm/vm_address_region.h>
#include <vm/vm_aspace.h>
#include <vm/vm_object.h>

namespace hypervisor {

// RAII object that holds a mapping of guest physical address space to the host
// kernel virtual address space. Can be used to map a frequently accessed
// portion of guest physical memory for faster access.
class GuestPtr {
 public:
  GuestPtr(fbl::RefPtr<VmMapping> mapping, zx_vaddr_t offset)
      : mapping_(ktl::move(mapping)), offset_(offset) {}
  ~GuestPtr() { reset(); }

  GuestPtr() = default;
  GuestPtr(GuestPtr&&) = default;
  GuestPtr& operator=(GuestPtr&&) = default;

  GuestPtr(const GuestPtr&) = delete;
  GuestPtr& operator=(const GuestPtr&) = delete;

  void reset() {
    if (mapping_) {
      mapping_->Destroy();
      mapping_.reset();
    }
  }

  template <typename T>
  T* as() const {
    if (offset_ + sizeof(T) > mapping_->size()) {
      return nullptr;
    }
    return reinterpret_cast<T*>(mapping_->base() + offset_);
  }

 private:
  fbl::RefPtr<VmMapping> mapping_;
  zx_vaddr_t offset_;
};

class GuestPhysicalAddressSpace {
 public:
  static zx::status<GuestPhysicalAddressSpace> Create(
#ifdef ARCH_ARM64
      uint16_t vmid
#endif
  );
  ~GuestPhysicalAddressSpace();

  GuestPhysicalAddressSpace() = default;
  GuestPhysicalAddressSpace(GuestPhysicalAddressSpace&&) = default;
  GuestPhysicalAddressSpace& operator=(GuestPhysicalAddressSpace&&) = default;

  GuestPhysicalAddressSpace(const GuestPhysicalAddressSpace&) = delete;
  GuestPhysicalAddressSpace& operator=(const GuestPhysicalAddressSpace&) = delete;

  size_t size() const { return guest_aspace_->size(); }
  zx_paddr_t arch_table_phys() const { return guest_aspace_->arch_aspace().arch_table_phys(); }
#if ARCH_ARM64
  uint16_t arch_asid() const { return guest_aspace_->arch_aspace().arch_asid(); }
#endif
  fbl::RefPtr<VmAddressRegion> RootVmar() const { return guest_aspace_->RootVmar(); }

  bool IsMapped(zx_gpaddr_t guest_paddr) const;
  zx::status<> MapInterruptController(zx_gpaddr_t guest_paddr, zx_paddr_t host_paddr, size_t len);
  zx::status<> UnmapRange(zx_gpaddr_t guest_paddr, size_t len);
  zx::status<> PageFault(zx_gpaddr_t guest_paddr);
  zx::status<uint> QueryFlags(zx_gpaddr_t guest_paddr);
  zx::status<GuestPtr> CreateGuestPtr(zx_gpaddr_t guest_paddr, size_t len, const char* name);

 private:
  fbl::RefPtr<VmAspace> guest_aspace_;
};

}  // namespace hypervisor

#endif  // ZIRCON_KERNEL_HYPERVISOR_INCLUDE_HYPERVISOR_GUEST_PHYSICAL_ADDRESS_SPACE_H_
