// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
#include "vm/vm_aspace.h"

#include <align.h>
#include <assert.h>
#include <inttypes.h>
#include <lib/boot-options/boot-options.h>
#include <lib/crypto/global_prng.h>
#include <lib/crypto/prng.h>
#include <lib/ktrace.h>
#include <lib/lazy_init/lazy_init.h>
#include <lib/userabi/vdso.h>
#include <lib/zircon-internal/macros.h>
#include <stdlib.h>
#include <string.h>
#include <trace.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <arch/kernel_aspace.h>
#include <fbl/alloc_checker.h>
#include <fbl/intrusive_double_list.h>
#include <kernel/mutex.h>
#include <kernel/thread.h>
#include <kernel/thread_lock.h>
#include <ktl/algorithm.h>
#include <vm/fault.h>
#include <vm/vm.h>
#include <vm/vm_address_region.h>
#include <vm/vm_object.h>
#include <vm/vm_object_paged.h>
#include <vm/vm_object_physical.h>

#include "vm_priv.h"

#define LOCAL_TRACE VM_GLOBAL_TRACE(0)

#define GUEST_PHYSICAL_ASPACE_BASE 0UL
#define GUEST_PHYSICAL_ASPACE_SIZE (1UL << MMU_GUEST_SIZE_SHIFT)

// pointer to a singleton kernel address space
VmAspace* VmAspace::kernel_aspace_ = nullptr;

// list of all address spaces
struct VmAspaceListGlobal {};
static DECLARE_MUTEX(VmAspaceListGlobal) aspace_list_lock;
static fbl::DoublyLinkedList<VmAspace*> aspaces TA_GUARDED(aspace_list_lock);

namespace {
// the singleton kernel address space
lazy_init::LazyInit<VmAspace, lazy_init::CheckType::None, lazy_init::Destructor::Disabled>
    g_kernel_aspace;
lazy_init::LazyInit<VmAddressRegion, lazy_init::CheckType::None, lazy_init::Destructor::Disabled>
    g_kernel_root_vmar;
}  // namespace

// Called once at boot to initialize the singleton kernel address
// space. Thread safety analysis is disabled since we don't need to
// lock yet.
void VmAspace::KernelAspaceInitPreHeap() TA_NO_THREAD_SAFETY_ANALYSIS {

  g_kernel_aspace.Initialize(KERNEL_ASPACE_BASE, KERNEL_ASPACE_SIZE, VmAspace::TYPE_KERNEL, "kernel");

#if LK_DEBUGLEVEL > 1
  g_kernel_aspace->Adopt();
#endif

  g_kernel_root_vmar.Initialize(g_kernel_aspace.Get());
  g_kernel_aspace->root_vmar_ = fbl::AdoptRef(&g_kernel_root_vmar.Get());

  zx_status_t status = g_kernel_aspace->Init();
  ASSERT(status == ZX_OK);

  // save a pointer to the singleton kernel address space
  VmAspace::kernel_aspace_ = &g_kernel_aspace.Get();
  aspaces.push_front(kernel_aspace_);
}

// simple test routines
static inline bool is_inside(VmAspace& aspace, vaddr_t vaddr) {
  return (vaddr >= aspace.base() && vaddr <= aspace.base() + aspace.size() - 1);
}

static inline size_t trim_to_aspace(VmAspace& aspace, vaddr_t vaddr, size_t size) {
  DEBUG_ASSERT(is_inside(aspace, vaddr));

  if (size == 0) {
    return size;
  }

  size_t offset = vaddr - aspace.base();

  // LTRACEF("vaddr 0x%lx size 0x%zx offset 0x%zx aspace base 0x%lx aspace size 0x%zx\n",
  //        vaddr, size, offset, aspace.base(), aspace.size());

  if (offset + size < offset) {
    size = ULONG_MAX - offset - 1;
  }

  // LTRACEF("size now 0x%zx\n", size);

  if (offset + size >= aspace.size() - 1) {
    size = aspace.size() - offset;
  }

  // LTRACEF("size now 0x%zx\n", size);

  return size;
}

VmAspace::VmAspace(vaddr_t base, size_t size, uint32_t flags, const char* name)
    : base_(base),
      size_(size),
      flags_(flags),
      root_vmar_(nullptr),
      aslr_prng_(nullptr, 0),
      arch_aspace_(base, size, arch_aspace_flags_from_flags(flags)) {
  DEBUG_ASSERT(size != 0);
  DEBUG_ASSERT(base + size - 1 >= base);

  Rename(name);

  LTRACEF("%p '%s'\n", this, name_);
}

zx_status_t VmAspace::Init() {
  canary_.Assert();

  LTRACEF("%p '%s'\n", this, name_);

  // initialize the architecturally specific part
  zx_status_t status = arch_aspace_.Init();
  if (status != ZX_OK) {
    return status;
  }

  InitializeAslr();

  if (likely(!root_vmar_)) {
    return VmAddressRegion::CreateRoot(*this, VMAR_FLAG_CAN_MAP_SPECIFIC, &root_vmar_);
  }
  return ZX_OK;
}

fbl::RefPtr<VmAspace> VmAspace::Create(uint32_t flags, const char* name) {
  LTRACEF("flags 0x%x, name '%s'\n", flags, name);

  vaddr_t base;
  size_t size;
  switch (flags & TYPE_MASK) {
    case TYPE_USER:
      base = USER_ASPACE_BASE;
      size = USER_ASPACE_SIZE;
      break;
    case TYPE_KERNEL:
      base = KERNEL_ASPACE_BASE;
      size = KERNEL_ASPACE_SIZE;
      break;
    case TYPE_LOW_KERNEL:
      base = 0;
      size = USER_ASPACE_BASE + USER_ASPACE_SIZE;
      break;
    case TYPE_GUEST_PHYS:
      base = GUEST_PHYSICAL_ASPACE_BASE;
      size = GUEST_PHYSICAL_ASPACE_SIZE;
      break;
    default:
      panic("Invalid aspace type");
  }

  fbl::AllocChecker ac;
  auto aspace = fbl::AdoptRef(new (&ac) VmAspace(base, size, flags, name));
  if (!ac.check()) {
    return nullptr;
  }

  // initialize the arch specific component to our address space
  zx_status_t status = aspace->Init();
  if (status != ZX_OK) {
    status = aspace->Destroy();
    DEBUG_ASSERT(status == ZX_OK);
    return nullptr;
  }

  // add it to the global list
  {
    Guard<Mutex> guard{&aspace_list_lock};
    aspaces.push_back(aspace.get());
  }

  // return a ref pointer to the aspace
  return aspace;
}

void VmAspace::Rename(const char* name) {
  canary_.Assert();
  strlcpy(name_, name ? name : "unnamed", sizeof(name_));
}

VmAspace::~VmAspace() {
  canary_.Assert();
  LTRACEF("%p '%s'\n", this, name_);

  // we have to have already been destroyed before freeing
  DEBUG_ASSERT(aspace_destroyed_);

  // pop it out of the global aspace list
  {
    Guard<Mutex> guard{&aspace_list_lock};
    if (this->InContainer()) {
      aspaces.erase(*this);
    }
  }

  // destroy the arch portion of the aspace
  // TODO(teisenbe): Move this to Destroy().  Currently can't move since
  // ProcessDispatcher calls Destroy() from the context of a thread in the
  // aspace and HarvestAllUserPageTables assumes the arch_aspace is valid if
  // the aspace is in the global list.
  zx_status_t status = arch_aspace_.Destroy();
  DEBUG_ASSERT(status == ZX_OK);
}

fbl::RefPtr<VmAddressRegion> VmAspace::RootVmar() {
  Guard<Mutex> guard{&lock_};
  if (root_vmar_) {
    return fbl::RefPtr<VmAddressRegion>(root_vmar_);
  }
  return nullptr;
}

zx_status_t VmAspace::Destroy() {
  canary_.Assert();
  LTRACEF("%p '%s'\n", this, name_);

  Guard<Mutex> guard{&lock_};

  // Don't let a vDSO mapping prevent destroying a VMAR
  // when the whole process is being destroyed.
  vdso_code_mapping_.reset();

  // tear down and free all of the regions in our address space
  if (root_vmar_) {
    AssertHeld(root_vmar_->lock_ref());
    zx_status_t status = root_vmar_->DestroyLocked();
    if (status != ZX_OK && status != ZX_ERR_BAD_STATE) {
      return status;
    }
  }
  aspace_destroyed_ = true;

  root_vmar_.reset();

  return ZX_OK;
}

bool VmAspace::is_destroyed() const {
  Guard<Mutex> guard{&lock_};
  return aspace_destroyed_;
}

zx_status_t VmAspace::MapObjectInternal(fbl::RefPtr<VmObject> vmo, const char* name,
                                        uint64_t offset, size_t size, void** ptr,
                                        uint8_t align_pow2, uint vmm_flags, uint arch_mmu_flags) {
  canary_.Assert();
  LTRACEF("aspace %p name '%s' vmo %p, offset %#" PRIx64
          " size %#zx "
          "ptr %p align %hhu vmm_flags %#x arch_mmu_flags %#x\n",
          this, name, vmo.get(), offset, size, ptr ? *ptr : 0, align_pow2, vmm_flags,
          arch_mmu_flags);

  DEBUG_ASSERT(!is_user());

  size = ROUNDUP(size, PAGE_SIZE);
  if (size == 0) {
    return ZX_ERR_INVALID_ARGS;
  }
  if (!vmo) {
    return ZX_ERR_INVALID_ARGS;
  }
  if (!IS_PAGE_ALIGNED(offset)) {
    return ZX_ERR_INVALID_ARGS;
  }

  vaddr_t vmar_offset = 0;
  // if they're asking for a specific spot or starting address, copy the address
  if (vmm_flags & VMM_FLAG_VALLOC_SPECIFIC) {
    // can't ask for a specific spot and then not provide one
    if (!ptr) {
      return ZX_ERR_INVALID_ARGS;
    }
    vmar_offset = reinterpret_cast<vaddr_t>(*ptr);

    // check that it's page aligned
    if (!IS_PAGE_ALIGNED(vmar_offset) || vmar_offset < base_) {
      return ZX_ERR_INVALID_ARGS;
    }

    vmar_offset -= base_;
  }

  uint32_t vmar_flags = 0;
  if (vmm_flags & VMM_FLAG_VALLOC_SPECIFIC) {
    vmar_flags |= VMAR_FLAG_SPECIFIC;
  }

  // Create the mappings with all of the CAN_* RWX flags, so that
  // Protect() can transition them arbitrarily.  This is not desirable for the
  // long-term.
  vmar_flags |= VMAR_CAN_RWX_FLAGS;

  // allocate a region and put it in the aspace list
  fbl::RefPtr<VmMapping> r(nullptr);
  zx_status_t status = RootVmar()->CreateVmMapping(vmar_offset, size, align_pow2, vmar_flags, vmo,
                                                   offset, arch_mmu_flags, name, &r);
  if (status != ZX_OK) {
    return status;
  }

  // if we're committing it, map the region now
  if (vmm_flags & VMM_FLAG_COMMIT) {
    status = r->MapRange(0, size, true);
    if (status != ZX_OK) {
      return status;
    }
  }

  // return the vaddr if requested
  if (ptr) {
    *ptr = (void*)r->base();
  }

  return ZX_OK;
}

zx_status_t VmAspace::ReserveSpace(const char* name, size_t size, vaddr_t vaddr) {
  canary_.Assert();
  LTRACEF("aspace %p name '%s' size %#zx vaddr %#" PRIxPTR "\n", this, name, size, vaddr);

  DEBUG_ASSERT(IS_PAGE_ALIGNED(vaddr));
  DEBUG_ASSERT(IS_PAGE_ALIGNED(size));

  size = ROUNDUP_PAGE_SIZE(size);
  if (size == 0) {
    return ZX_OK;
  }
  if (!IS_PAGE_ALIGNED(vaddr)) {
    return ZX_ERR_INVALID_ARGS;
  }
  if (!is_inside(*this, vaddr)) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  // trim the size
  size = trim_to_aspace(*this, vaddr, size);

  // allocate a zero length vm object to back it
  // TODO: decide if a null vmo object is worth it
  fbl::RefPtr<VmObjectPaged> vmo;
  zx_status_t status = VmObjectPaged::Create(PMM_ALLOC_FLAG_ANY, 0u, 0, &vmo);
  if (status != ZX_OK) {
    return status;
  }
  vmo->set_name(name, strlen(name));

  // lookup how it's already mapped
  uint arch_mmu_flags = 0;
  auto err = arch_aspace_.Query(vaddr, nullptr, &arch_mmu_flags);
  if (err) {
    // if it wasn't already mapped, use some sort of strict default
    arch_mmu_flags = ARCH_MMU_FLAG_CACHED | ARCH_MMU_FLAG_PERM_READ;
  }
  if ((arch_mmu_flags & ARCH_MMU_FLAG_CACHE_MASK) != 0) {
    status = vmo->SetMappingCachePolicy(arch_mmu_flags & ARCH_MMU_FLAG_CACHE_MASK);
    if (status != ZX_OK) {
      return status;
    }
  }

  // map it, creating a new region
  void* ptr = reinterpret_cast<void*>(vaddr);
  return MapObjectInternal(ktl::move(vmo), name, 0, size, &ptr, 0, VMM_FLAG_VALLOC_SPECIFIC,
                           arch_mmu_flags);
}

zx_status_t VmAspace::AllocPhysical(const char* name, size_t size, void** ptr, uint8_t align_pow2,
                                    paddr_t paddr, uint vmm_flags, uint arch_mmu_flags) {
  canary_.Assert();
  LTRACEF("aspace %p name '%s' size %#zx ptr %p paddr %#" PRIxPTR
          " vmm_flags 0x%x arch_mmu_flags 0x%x\n",
          this, name, size, ptr ? *ptr : 0, paddr, vmm_flags, arch_mmu_flags);

  DEBUG_ASSERT(IS_PAGE_ALIGNED(paddr));

  if (size == 0) {
    return ZX_OK;
  }
  if (!IS_PAGE_ALIGNED(paddr)) {
    return ZX_ERR_INVALID_ARGS;
  }

  size = ROUNDUP_PAGE_SIZE(size);

  // create a vm object to back it
  fbl::RefPtr<VmObjectPhysical> vmo;
  zx_status_t status = VmObjectPhysical::Create(paddr, size, &vmo);
  if (status != ZX_OK) {
    return status;
  }
  vmo->set_name(name, strlen(name));

  // force it to be mapped up front
  // TODO: add new flag to precisely mean pre-map
  vmm_flags |= VMM_FLAG_COMMIT;

  // Apply the cache policy
  if (vmo->SetMappingCachePolicy(arch_mmu_flags & ARCH_MMU_FLAG_CACHE_MASK) != ZX_OK) {
    return ZX_ERR_INVALID_ARGS;
  }

  arch_mmu_flags &= ~ARCH_MMU_FLAG_CACHE_MASK;
  return MapObjectInternal(ktl::move(vmo), name, 0, size, ptr, align_pow2, vmm_flags,
                           arch_mmu_flags);
}

zx_status_t VmAspace::AllocContiguous(const char* name, size_t size, void** ptr, uint8_t align_pow2,
                                      uint vmm_flags, uint arch_mmu_flags) {
  canary_.Assert();
  LTRACEF("aspace %p name '%s' size 0x%zx ptr %p align %hhu vmm_flags 0x%x arch_mmu_flags 0x%x\n",
          this, name, size, ptr ? *ptr : 0, align_pow2, vmm_flags, arch_mmu_flags);

  size = ROUNDUP(size, PAGE_SIZE);
  if (size == 0) {
    return ZX_ERR_INVALID_ARGS;
  }

  // test for invalid flags
  if (!(vmm_flags & VMM_FLAG_COMMIT)) {
    return ZX_ERR_INVALID_ARGS;
  }

  // create a vm object to back it
  fbl::RefPtr<VmObjectPaged> vmo;
  zx_status_t status = VmObjectPaged::CreateContiguous(PMM_ALLOC_FLAG_ANY, size, align_pow2, &vmo);
  if (status != ZX_OK) {
    return status;
  }
  vmo->set_name(name, strlen(name));

  return MapObjectInternal(ktl::move(vmo), name, 0, size, ptr, align_pow2, vmm_flags,
                           arch_mmu_flags);
}

zx_status_t VmAspace::Alloc(const char* name, size_t size, void** ptr, uint8_t align_pow2,
                            uint vmm_flags, uint arch_mmu_flags) {
  canary_.Assert();
  LTRACEF("aspace %p name '%s' size 0x%zx ptr %p align %hhu vmm_flags 0x%x arch_mmu_flags 0x%x\n",
          this, name, size, ptr ? *ptr : 0, align_pow2, vmm_flags, arch_mmu_flags);

  size = ROUNDUP(size, PAGE_SIZE);
  if (size == 0) {
    return ZX_ERR_INVALID_ARGS;
  }

  // allocate a vm object to back it
  fbl::RefPtr<VmObjectPaged> vmo;
  zx_status_t status = VmObjectPaged::Create(PMM_ALLOC_FLAG_ANY, 0u, size, &vmo);
  if (status != ZX_OK) {
    return status;
  }
  vmo->set_name(name, strlen(name));

  // commit memory up front if requested
  if (vmm_flags & VMM_FLAG_COMMIT) {
    // commit memory to the object
    status = vmo->CommitRange(0, size);
    if (status != ZX_OK) {
      return status;
    }
  }

  // map it, creating a new region
  return MapObjectInternal(ktl::move(vmo), name, 0, size, ptr, align_pow2, vmm_flags,
                           arch_mmu_flags);
}

zx_status_t VmAspace::FreeRegion(vaddr_t va) {
  DEBUG_ASSERT(!is_user());

  fbl::RefPtr<VmAddressRegionOrMapping> root_vmar = RootVmar();
  if (!root_vmar) {
    return ZX_ERR_NOT_FOUND;
  }
  fbl::RefPtr<VmAddressRegionOrMapping> r = RootVmar()->FindRegion(va);
  if (!r) {
    return ZX_ERR_NOT_FOUND;
  }

  return r->Destroy();
}

fbl::RefPtr<VmAddressRegionOrMapping> VmAspace::FindRegion(vaddr_t va) {
  fbl::RefPtr<VmAddressRegion> vmar(RootVmar());
  if (!vmar) {
    return nullptr;
  }
  while (1) {
    fbl::RefPtr<VmAddressRegionOrMapping> next(vmar->FindRegion(va));
    if (!next) {
      return vmar;
    }

    if (next->is_mapping()) {
      return next;
    }

    vmar = next->as_vm_address_region();
  }
}

void VmAspace::AttachToThread(Thread* t) {
  canary_.Assert();
  DEBUG_ASSERT(t);

  // point the lk thread at our object
  Guard<MonitoredSpinLock, IrqSave> thread_lock_guard{ThreadLock::Get(), SOURCE_TAG};

  // not prepared to handle setting a new address space or one on a running thread
  DEBUG_ASSERT(!t->aspace());
  DEBUG_ASSERT(t->state() != THREAD_RUNNING);

  t->switch_aspace(this);
}

zx_status_t VmAspace::PageFault(vaddr_t va, uint flags) {
  VM_KTRACE_DURATION(2, "VmAspace::PageFault", va, flags);
  canary_.Assert();
  DEBUG_ASSERT(!aspace_destroyed_);
  LTRACEF("va %#" PRIxPTR ", flags %#x\n", va, flags);

  if ((flags_ & TYPE_MASK) == TYPE_GUEST_PHYS) {
    flags &= ~VMM_PF_FLAG_USER;
    flags |= VMM_PF_FLAG_GUEST;
  }

  zx_status_t status = ZX_OK;
  __UNINITIALIZED LazyPageRequest page_request;
  do {
    {
      // for now, hold the aspace lock across the page fault operation,
      // which stops any other operations on the address space from moving
      // the region out from underneath it
      Guard<Mutex> guard{&lock_};
      // First check if we're faulting on the same mapping as last time to short-circuit the vmar
      // walk.
      if (likely(last_fault_ && last_fault_->is_in_range(va, 1))) {
        AssertHeld(last_fault_->lock_ref());
        status = last_fault_->PageFault(va, flags, &page_request);
      } else {
        AssertHeld(root_vmar_->lock_ref());
        status = root_vmar_->PageFault(va, flags, &page_request);
      }
    }

    if (status == ZX_ERR_SHOULD_WAIT) {
      zx_status_t st = page_request->Wait();
      if (st != ZX_OK) {
        if (st == ZX_ERR_TIMED_OUT) {
          Guard<Mutex> guard{&lock_};
          AssertHeld(root_vmar_->lock_ref());
          root_vmar_->DumpLocked(0, false);
        }
        return st;
      }
    }
  } while (status == ZX_ERR_SHOULD_WAIT);

  return status;
}

zx_status_t VmAspace::SoftFault(vaddr_t va, uint flags) {
  // With the current implementation we can just reuse the internal PageFault mechanism.
  return PageFault(va, flags | VMM_PF_FLAG_SW_FAULT);
}

zx_status_t VmAspace::AccessedFault(vaddr_t va) {
  VM_KTRACE_DURATION(2, "VmAspace::AccessedFault", va, 0);
  // There are no permissions etc associated with accessed bits so we can skip any vmar walking and
  // just let the hardware aspace walk for the virtual address.
  // Similar to a page fault, multiple additional pages in the page table will be marked active to
  // amortize the cost of accessed faults. This reduces the accuracy of page age information, at the
  // gain of performance due to reduced number of faults. Given this accessed fault path is meant to
  // just be a fastpath of the page fault path, using the same count and strategy as a page fault at
  // least provides consistency of the trade off of page age accuracy and fault frequency.
  va = ROUNDDOWN(va, PAGE_SIZE);
  const uint64_t next_pt_base = ArchVmAspace::NextUserPageTableOffset(va);
  // Find the minimum between the size of this mapping and the end of the page table.
  const uint64_t max_mark = ktl::min(next_pt_base, base_ + size_);
  // Convert this into a number of pages, limiting to the max lookup pages for consistency with the
  // page fault path.
  const uint64_t max_pages = ktl::min((max_mark - va) / PAGE_SIZE, VmObject::LookupInfo::kMaxPages);
  return arch_aspace_.MarkAccessed(va, max_pages);
}

void VmAspace::Dump(bool verbose) const {
  Guard<Mutex> guard{&lock_};
  DumpLocked(verbose);
}

void VmAspace::DumpLocked(bool verbose) const {
  canary_.Assert();
  printf("as %p [%#" PRIxPTR " %#" PRIxPTR "] sz %#zx fl %#x ref %d '%s' destroyed %d\n", this,
         base_, base_ + size_ - 1, size_, flags_, ref_count_debug(), name_, aspace_destroyed_);

  if (verbose && root_vmar_) {
    AssertHeld(root_vmar_->lock_ref());
    root_vmar_->DumpLocked(1, verbose);
  }
}

bool VmAspace::EnumerateChildren(VmEnumerator* ve) {
  canary_.Assert();
  DEBUG_ASSERT(ve != nullptr);
  Guard<Mutex> guard{&lock_};
  if (root_vmar_ == nullptr || aspace_destroyed_) {
    // Aspace hasn't been initialized or has already been destroyed.
    return true;
  }
  DEBUG_ASSERT(root_vmar_->IsAliveLocked());
  AssertHeld(root_vmar_->lock_ref());
  if (!ve->OnVmAddressRegion(root_vmar_.get(), 0)) {
    return false;
  }
  return root_vmar_->EnumerateChildrenLocked(ve);
}

void DumpAllAspaces(bool verbose) {
  Guard<Mutex> guard{&aspace_list_lock};

  for (const auto& a : aspaces) {
    a.Dump(verbose);
  }
}

VmAspace* VmAspace::vaddr_to_aspace(uintptr_t address) {
  if (is_kernel_address(address)) {
    return kernel_aspace();
  } else if (is_user_address(address)) {
    return Thread::Current::Get()->aspace();
  } else {
    return nullptr;
  }
}

// TODO(dbort): Use GetMemoryUsage()
size_t VmAspace::AllocatedPages() const {
  canary_.Assert();

  Guard<Mutex> guard{&lock_};
  if (!root_vmar_) {
    return 0;
  }
  AssertHeld(root_vmar_->lock_ref());
  return root_vmar_->AllocatedPagesLocked();
}

void VmAspace::InitializeAslr() {
  // As documented in //docs/gen/boot-options.md.
  static constexpr uint8_t kMaxAslrEntropy = 36;

  aslr_enabled_ = is_user() && !gBootOptions->aslr_disabled;
  if (aslr_enabled_) {
    aslr_entropy_bits_ = ktl::min(gBootOptions->aslr_entropy_bits, kMaxAslrEntropy);
    aslr_compact_entropy_bits_ = 8;
  }

  crypto::GlobalPRNG::GetInstance()->Draw(aslr_seed_, sizeof(aslr_seed_));
  aslr_prng_.AddEntropy(aslr_seed_, sizeof(aslr_seed_));
}

uintptr_t VmAspace::vdso_base_address() const {
  Guard<Mutex> guard{&lock_};
  return VDso::base_address(vdso_code_mapping_);
}

uintptr_t VmAspace::vdso_code_address() const {
  Guard<Mutex> guard{&lock_};
  return vdso_code_mapping_ ? vdso_code_mapping_->base() : 0;
}

void VmAspace::DropAllUserPageTables() {
  Guard<Mutex> guard{&aspace_list_lock};

  for (auto& a : aspaces) {
    a.DropUserPageTables();
  }
}

void VmAspace::DropUserPageTables() {
  if (!is_user())
    return;
  Guard<Mutex> guard{&lock_};
  arch_aspace().Unmap(base(), size() / PAGE_SIZE, nullptr);
}

bool VmAspace::IntersectsVdsoCode(vaddr_t base, size_t size) const {
  return vdso_code_mapping_ &&
         Intersects(vdso_code_mapping_->base(), vdso_code_mapping_->size(), base, size);
}

void VmAspace::HarvestAllUserAccessedBits(NonTerminalAction action) {
  VM_KTRACE_DURATION(2, "VmAspace::HarvestAllUserAccessedBits");
  Guard<Mutex> guard{&aspace_list_lock};

  for (auto& a : aspaces) {
    if (a.is_user()) {
      // The arch_aspace is only destroyed in the VmAspace destructor *after* the aspace is removed
      // from the aspaces list. As we presently hold the aspace_list_lock we know that this
      // destructor has not completed, and so the arch_aspace has not been destroyed. Even if the
      // actual VmAspace has been destroyed, it is still completely safe to walk to the hardware
      // page tables, there just will not be anything there.
      zx_status_t __UNUSED result =
          a.arch_aspace().HarvestAccessed(a.base(), a.size() / PAGE_SIZE, action);
      DEBUG_ASSERT(result == ZX_OK);
    }
  }
}
