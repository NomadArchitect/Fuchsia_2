// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <align.h>
#include <zircon/syscalls/hypervisor.h>

#include <arch/hypervisor.h>
#include <dev/interrupt/arm_gic_hw_interface.h>
#include <hypervisor/guest_physical_address_space.h>
#include <vm/pmm.h>

#include "el2_cpu_state_priv.h"

static constexpr zx_gpaddr_t kGicvAddress = 0x800001000;
static constexpr size_t kGicvSize = 0x2000;

// static
zx_status_t Guest::Create(ktl::unique_ptr<Guest>* out) {
  if (arm64_get_boot_el() < 2) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  auto vmid = alloc_vmid();
  if (vmid.is_error()) {
    return vmid.status_value();
  }

  fbl::AllocChecker ac;
  ktl::unique_ptr<Guest> guest(new (&ac) Guest(*vmid));
  if (!ac.check()) {
    auto result = free_vmid(*vmid);
    ZX_ASSERT(result.is_ok());
    return ZX_ERR_NO_MEMORY;
  }

  auto gpas = hypervisor::GuestPhysicalAddressSpace::Create(*vmid);
  if (gpas.is_error()) {
    return gpas.status_value();
  }
  guest->gpas_ = ktl::move(*gpas);

  zx_paddr_t gicv_paddr;
  zx_status_t status = gic_get_gicv(&gicv_paddr);

  // If `status` is ZX_OK, we are running GICv2. We then need to map GICV.
  // If `status is ZX_ERR_NOT_FOUND, we are running GICv3.
  // Otherwise, return `status`.
  if (status == ZX_OK) {
    if (auto result = guest->gpas_.MapInterruptController(kGicvAddress, gicv_paddr, kGicvSize);
        result.is_error()) {
      return result.status_value();
    }
  } else if (status != ZX_ERR_NOT_FOUND) {
    return status;
  }

  *out = ktl::move(guest);
  return ZX_OK;
}

Guest::Guest(uint16_t vmid) : vmid_(vmid) {}

Guest::~Guest() {
  auto result = free_vmid(vmid_);
  ZX_ASSERT(result.is_ok());
}

zx_status_t Guest::SetTrap(uint32_t kind, zx_gpaddr_t addr, size_t len,
                           fbl::RefPtr<PortDispatcher> port, uint64_t key) {
  switch (kind) {
    case ZX_GUEST_TRAP_MEM:
      if (port) {
        return ZX_ERR_INVALID_ARGS;
      }
      break;
    case ZX_GUEST_TRAP_BELL:
      if (!port) {
        return ZX_ERR_INVALID_ARGS;
      }
      break;
    case ZX_GUEST_TRAP_IO:
      return ZX_ERR_NOT_SUPPORTED;
    default:
      return ZX_ERR_INVALID_ARGS;
  }

  if (!IS_PAGE_ALIGNED(addr) || !IS_PAGE_ALIGNED(len)) {
    return ZX_ERR_INVALID_ARGS;
  }
  if (auto result = gpas_.UnmapRange(addr, len); result.is_error()) {
    return result.status_value();
  }
  return traps_.InsertTrap(kind, addr, len, ktl::move(port), key).status_value();
}
