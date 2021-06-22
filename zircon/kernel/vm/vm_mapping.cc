// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <align.h>
#include <assert.h>
#include <inttypes.h>
#include <lib/counters.h>
#include <lib/fit/defer.h>
#include <trace.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <fbl/alloc_checker.h>
#include <ktl/algorithm.h>
#include <ktl/iterator.h>
#include <ktl/move.h>
#include <vm/fault.h>
#include <vm/vm.h>
#include <vm/vm_aspace.h>
#include <vm/vm_object.h>
#include <vm/vm_object_paged.h>

#include "vm/vm_address_region.h"
#include "vm_priv.h"

#define LOCAL_TRACE VM_GLOBAL_TRACE(0)

namespace {

KCOUNTER(vm_mapping_attribution_queries, "vm.attributed_pages.mapping.queries")
KCOUNTER(vm_mapping_attribution_cache_hits, "vm.attributed_pages.mapping.cache_hits")
KCOUNTER(vm_mapping_attribution_cache_misses, "vm.attributed_pages.mapping.cache_misses")
KCOUNTER(vm_mappings_merged, "vm.aspace.mapping.merged_neighbors")

}  // namespace

VmMapping::VmMapping(VmAddressRegion& parent, vaddr_t base, size_t size, uint32_t vmar_flags,
                     fbl::RefPtr<VmObject> vmo, uint64_t vmo_offset, uint arch_mmu_flags,
                     Mergeable mergeable)
    : VmAddressRegionOrMapping(base, size, vmar_flags, parent.aspace_.get(), &parent, true),
      object_(ktl::move(vmo)),
      object_offset_(vmo_offset),
      arch_mmu_flags_(arch_mmu_flags),
      mergeable_(mergeable) {
  LTRACEF("%p aspace %p base %#" PRIxPTR " size %#zx offset %#" PRIx64 "\n", this, aspace_.get(),
          base_, size_, vmo_offset);
}

VmMapping::~VmMapping() {
  canary_.Assert();
  LTRACEF("%p aspace %p base %#" PRIxPTR " size %#zx\n", this, aspace_.get(), base_, size_);
}

fbl::RefPtr<VmObject> VmMapping::vmo() const {
  Guard<Mutex> guard{aspace_->lock()};
  return vmo_locked();
}

size_t VmMapping::AllocatedPagesLocked() const {
  canary_.Assert();

  if (state_ != LifeCycleState::ALIVE) {
    return 0;
  }

  vm_mapping_attribution_queries.Add(1);

  if (!object_->is_paged()) {
    return object_->AttributedPagesInRange(object_offset_locked(), size_);
  }

  // If |object_| is a VmObjectPaged, check if the previously cached value still holds.
  auto object_paged = static_cast<VmObjectPaged*>(object_.get());
  uint64_t vmo_gen_count = object_paged->GetHierarchyGenerationCount();
  uint64_t mapping_gen_count = GetMappingGenerationCountLocked();

  // Return the cached page count if the mapping's generation count and the vmo's generation count
  // have not changed.
  if (cached_page_attribution_.mapping_generation_count == mapping_gen_count &&
      cached_page_attribution_.vmo_generation_count == vmo_gen_count) {
    vm_mapping_attribution_cache_hits.Add(1);
    return cached_page_attribution_.page_count;
  }

  vm_mapping_attribution_cache_misses.Add(1);

  size_t page_count = object_paged->AttributedPagesInRange(object_offset_locked(), size_);

  DEBUG_ASSERT(cached_page_attribution_.mapping_generation_count != mapping_gen_count ||
               cached_page_attribution_.vmo_generation_count != vmo_gen_count);
  cached_page_attribution_.mapping_generation_count = mapping_gen_count;
  cached_page_attribution_.vmo_generation_count = vmo_gen_count;
  cached_page_attribution_.page_count = page_count;

  return page_count;
}

void VmMapping::DumpLocked(uint depth, bool verbose) const {
  canary_.Assert();
  for (uint i = 0; i < depth; ++i) {
    printf("  ");
  }
  char vmo_name[32];
  object_->get_name(vmo_name, sizeof(vmo_name));
  printf("map %p [%#" PRIxPTR " %#" PRIxPTR "] sz %#zx mmufl %#x\n", this, base_, base_ + size_ - 1,
         size_, arch_mmu_flags_locked());
  for (uint i = 0; i < depth + 1; ++i) {
    printf("  ");
  }
  printf("vmo %p/k%" PRIu64 " off %#" PRIx64 " pages %zu ref %d '%s'\n", object_.get(),
         object_->user_id(), object_offset_locked(),
         object_->AttributedPagesInRange(object_offset_locked(), size_), ref_count_debug(),
         vmo_name);
  if (verbose) {
    object_->Dump(depth + 1, false);
  }
}

zx_status_t VmMapping::Protect(vaddr_t base, size_t size, uint new_arch_mmu_flags) {
  canary_.Assert();
  LTRACEF("%p %#" PRIxPTR " %#x %#x\n", this, base_, flags_, new_arch_mmu_flags);

  if (!IS_PAGE_ALIGNED(base)) {
    return ZX_ERR_INVALID_ARGS;
  }

  size = ROUNDUP(size, PAGE_SIZE);

  Guard<Mutex> guard{aspace_->lock()};
  if (state_ != LifeCycleState::ALIVE) {
    return ZX_ERR_BAD_STATE;
  }

  if (size == 0 || !is_in_range(base, size)) {
    return ZX_ERR_INVALID_ARGS;
  }

  return ProtectLocked(base, size, new_arch_mmu_flags);
}

namespace {

// Implementation helper for ProtectLocked
zx_status_t ProtectOrUnmap(const fbl::RefPtr<VmAspace>& aspace, vaddr_t base, size_t size,
                           uint new_arch_mmu_flags) {
  if (new_arch_mmu_flags & ARCH_MMU_FLAG_PERM_RWX_MASK) {
    return aspace->arch_aspace().Protect(base, size / PAGE_SIZE, new_arch_mmu_flags);
  } else {
    return aspace->arch_aspace().Unmap(base, size / PAGE_SIZE, nullptr);
  }
}

}  // namespace

zx_status_t VmMapping::ProtectLocked(vaddr_t base, size_t size, uint new_arch_mmu_flags) {
  DEBUG_ASSERT(size != 0 && IS_PAGE_ALIGNED(base) && IS_PAGE_ALIGNED(size));

  // Do not allow changing caching
  if (new_arch_mmu_flags & ARCH_MMU_FLAG_CACHE_MASK) {
    return ZX_ERR_INVALID_ARGS;
  }

  if (!is_valid_mapping_flags(new_arch_mmu_flags)) {
    return ZX_ERR_ACCESS_DENIED;
  }

  DEBUG_ASSERT(object_);
  // grab the lock for the vmo
  Guard<Mutex> guard{object_->lock()};

  // Persist our current caching mode
  new_arch_mmu_flags |= (arch_mmu_flags_locked() & ARCH_MMU_FLAG_CACHE_MASK);

  // If we're not actually changing permissions, return fast.
  if (new_arch_mmu_flags == arch_mmu_flags_) {
    return ZX_OK;
  }

  // TODO(teisenbe): deal with error mapping on arch_mmu_protect fail

  // If we're changing the whole mapping, just make the change.
  if (base_ == base && size_ == size) {
    zx_status_t status = ProtectOrUnmap(aspace_, base, size, new_arch_mmu_flags);
    LTRACEF("arch_mmu_protect returns %d\n", status);
    arch_mmu_flags_ = new_arch_mmu_flags;
    return ZX_OK;
  }

  // Handle changing from the left
  if (base_ == base) {
    // Create a new mapping for the right half (has old perms)
    fbl::AllocChecker ac;
    fbl::RefPtr<VmMapping> mapping(fbl::AdoptRef(
        new (&ac) VmMapping(*parent_, base + size, size_ - size, flags_, object_,
                            object_offset_ + size, arch_mmu_flags_locked(), Mergeable::YES)));
    if (!ac.check()) {
      return ZX_ERR_NO_MEMORY;
    }

    zx_status_t status = ProtectOrUnmap(aspace_, base, size, new_arch_mmu_flags);
    LTRACEF("arch_mmu_protect returns %d\n", status);
    arch_mmu_flags_ = new_arch_mmu_flags;

    set_size_locked(size);
    AssertHeld(mapping->lock_ref());
    AssertHeld(*mapping->object_lock());
    mapping->ActivateLocked();
    return ZX_OK;
  }

  // Handle changing from the right
  if (base_ + size_ == base + size) {
    // Create a new mapping for the right half (has new perms)
    fbl::AllocChecker ac;

    fbl::RefPtr<VmMapping> mapping(fbl::AdoptRef(
        new (&ac) VmMapping(*parent_, base, size, flags_, object_, object_offset_ + base - base_,
                            new_arch_mmu_flags, Mergeable::YES)));
    if (!ac.check()) {
      return ZX_ERR_NO_MEMORY;
    }

    zx_status_t status = ProtectOrUnmap(aspace_, base, size, new_arch_mmu_flags);
    LTRACEF("arch_mmu_protect returns %d\n", status);

    set_size_locked(size_ - size);
    AssertHeld(mapping->lock_ref());
    AssertHeld(*mapping->object_lock());
    mapping->ActivateLocked();
    return ZX_OK;
  }

  // We're unmapping from the center, so we need to create two new mappings
  const size_t left_size = base - base_;
  const size_t right_size = (base_ + size_) - (base + size);
  const uint64_t center_vmo_offset = object_offset_ + base - base_;
  const uint64_t right_vmo_offset = center_vmo_offset + size;

  fbl::AllocChecker ac;
  fbl::RefPtr<VmMapping> center_mapping(
      fbl::AdoptRef(new (&ac) VmMapping(*parent_, base, size, flags_, object_, center_vmo_offset,
                                        new_arch_mmu_flags, Mergeable::YES)));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  fbl::RefPtr<VmMapping> right_mapping(
      fbl::AdoptRef(new (&ac) VmMapping(*parent_, base + size, right_size, flags_, object_,
                                        right_vmo_offset, arch_mmu_flags_, Mergeable::YES)));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }

  zx_status_t status = ProtectOrUnmap(aspace_, base, size, new_arch_mmu_flags);
  LTRACEF("arch_mmu_protect returns %d\n", status);

  // Turn us into the left half
  set_size_locked(left_size);

  AssertHeld(center_mapping->lock_ref());
  AssertHeld(*center_mapping->object_lock());
  center_mapping->ActivateLocked();
  AssertHeld(right_mapping->lock_ref());
  AssertHeld(*right_mapping->object_lock());
  right_mapping->ActivateLocked();
  return ZX_OK;
}

zx_status_t VmMapping::Unmap(vaddr_t base, size_t size) {
  LTRACEF("%p %#" PRIxPTR " %zu\n", this, base, size);

  if (!IS_PAGE_ALIGNED(base)) {
    return ZX_ERR_INVALID_ARGS;
  }

  size = ROUNDUP(size, PAGE_SIZE);

  fbl::RefPtr<VmAspace> aspace(aspace_);
  if (!aspace) {
    return ZX_ERR_BAD_STATE;
  }

  Guard<Mutex> guard{aspace_->lock()};
  if (state_ != LifeCycleState::ALIVE) {
    return ZX_ERR_BAD_STATE;
  }

  if (size == 0 || !is_in_range(base, size)) {
    return ZX_ERR_INVALID_ARGS;
  }

  // If we're unmapping everything, destroy this mapping
  if (base == base_ && size == size_) {
    return DestroyLocked();
  }

  return UnmapLocked(base, size);
}

zx_status_t VmMapping::UnmapLocked(vaddr_t base, size_t size) {
  canary_.Assert();
  DEBUG_ASSERT(size != 0 && IS_PAGE_ALIGNED(size) && IS_PAGE_ALIGNED(base));
  DEBUG_ASSERT(base >= base_ && base - base_ < size_);
  DEBUG_ASSERT(size_ - (base - base_) >= size);
  DEBUG_ASSERT(parent_);

  if (state_ != LifeCycleState::ALIVE) {
    return ZX_ERR_BAD_STATE;
  }

  // If our parent VMAR is DEAD, then we can only unmap everything.
  DEBUG_ASSERT(parent_->state_ != LifeCycleState::DEAD || (base == base_ && size == size_));

  LTRACEF("%p\n", this);

  // grab the lock for the vmo
  DEBUG_ASSERT(object_);
  Guard<Mutex> guard{object_->lock()};

  // Check if unmapping from one of the ends
  if (base_ == base || base + size == base_ + size_) {
    LTRACEF("unmapping base %#lx size %#zx\n", base, size);
    zx_status_t status = aspace_->arch_aspace().Unmap(base, size / PAGE_SIZE, nullptr);
    if (status != ZX_OK) {
      return status;
    }

    if (base_ == base && size_ != size) {
      // We need to remove ourselves from tree before updating base_,
      // since base_ is the tree key.
      AssertHeld(parent_->lock_ref());
      fbl::RefPtr<VmAddressRegionOrMapping> ref(parent_->subregions_.RemoveRegion(this));
      base_ += size;
      object_offset_ += size;
      parent_->subregions_.InsertRegion(ktl::move(ref));
    }
    set_size_locked(size_ - size);

    return ZX_OK;
  }

  // We're unmapping from the center, so we need to split the mapping
  DEBUG_ASSERT(parent_->state_ == LifeCycleState::ALIVE);

  const uint64_t vmo_offset = object_offset_ + (base + size) - base_;
  const vaddr_t new_base = base + size;
  const size_t new_size = (base_ + size_) - new_base;

  fbl::AllocChecker ac;
  fbl::RefPtr<VmMapping> mapping(
      fbl::AdoptRef(new (&ac) VmMapping(*parent_, new_base, new_size, flags_, object_, vmo_offset,
                                        arch_mmu_flags_locked(), Mergeable::YES)));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }

  // Unmap the middle segment
  LTRACEF("unmapping base %#lx size %#zx\n", base, size);
  zx_status_t status = aspace_->arch_aspace().Unmap(base, size / PAGE_SIZE, nullptr);
  if (status != ZX_OK) {
    return status;
  }

  // Turn us into the left half
  set_size_locked(base - base_);
  AssertHeld(mapping->lock_ref());
  AssertHeld(*mapping->object_lock());
  mapping->ActivateLocked();

  return ZX_OK;
}

bool VmMapping::ObjectRangeToVaddrRange(uint64_t offset, uint64_t len, vaddr_t* base,
                                        uint64_t* virtual_len) const {
  DEBUG_ASSERT(IS_PAGE_ALIGNED(offset));
  DEBUG_ASSERT(IS_PAGE_ALIGNED(len));
  DEBUG_ASSERT(base);
  DEBUG_ASSERT(virtual_len);

  // Zero sized ranges are considered to have no overlap.
  if (len == 0) {
    *base = 0;
    *virtual_len = 0;
    return false;
  }

  // compute the intersection of the passed in vmo range and our mapping
  uint64_t offset_new;
  if (!GetIntersect(object_offset_locked_object(), static_cast<uint64_t>(size_), offset, len,
                    &offset_new, virtual_len)) {
    return false;
  }

  DEBUG_ASSERT(*virtual_len > 0 && *virtual_len <= SIZE_MAX);
  DEBUG_ASSERT(offset_new >= object_offset_locked_object());

  LTRACEF("intersection offset %#" PRIx64 ", len %#" PRIx64 "\n", offset_new, *virtual_len);

  // make sure the base + offset is within our address space
  // should be, according to the range stored in base_ + size_
  bool overflowed = add_overflow(base_, offset_new - object_offset_locked_object(), base);
  ASSERT(!overflowed);

  // make sure we're only operating within our window
  ASSERT(*base >= base_);
  ASSERT((*base + *virtual_len - 1) <= (base_ + size_ - 1));

  return true;
}

zx_status_t VmMapping::AspaceUnmapVmoRangeLocked(uint64_t offset, uint64_t len) const {
  canary_.Assert();

  // NOTE: must be acquired with the vmo lock held, but doesn't need to take
  // the address space lock, since it will not manipulate its location in the
  // vmar tree. However, it must be held in the ALIVE state across this call.
  //
  // Avoids a race with DestroyLocked() since it removes ourself from the VMO's
  // mapping list with the VMO lock held before dropping this state to DEAD. The
  // VMO cant call back to us once we're out of their list.
  DEBUG_ASSERT(state_ == LifeCycleState::ALIVE);

  DEBUG_ASSERT(object_);

  LTRACEF("region %p obj_offset %#" PRIx64 " size %zu, offset %#" PRIx64 " len %#" PRIx64 "\n",
          this, object_offset_locked_object(), size_, offset, len);

  // If we're currently faulting and are responsible for the vmo code to be calling
  // back to us, detect the recursion and abort here.
  // The specific path we're avoiding is if the VMO calls back into us during vmo->GetPageLocked()
  // via AspaceUnmapVmoRangeLocked(). If we set this flag we're short circuiting the unmap operation
  // so that we don't do extra work.
  if (unlikely(currently_faulting_)) {
    LTRACEF("recursing to ourself, abort\n");
    return ZX_OK;
  }

  // See if there's an intersect.
  vaddr_t base;
  uint64_t new_len;
  if (!ObjectRangeToVaddrRange(offset, len, &base, &new_len)) {
    return ZX_OK;
  }

  return aspace_->arch_aspace().Unmap(base, new_len / PAGE_SIZE, nullptr);
}

zx_status_t VmMapping::HarvestAccessVmoRangeLocked(
    uint64_t offset, uint64_t len,
    const fbl::Function<bool(vm_page*, uint64_t)>& accessed_callback) const {
  canary_.Assert();

  // NOTE: must be acquired with the vmo lock held, but doesn't need to take
  // the address space lock, since it will not manipulate its location in the
  // vmar tree. However, it must be held in the ALIVE state across this call.
  //
  // Avoids a race with DestroyLocked() since it removes ourself from the VMO's
  // mapping list with the VMO lock held before dropping this state to DEAD. The
  // VMO cant call back to us once we're out of their list.
  DEBUG_ASSERT(state_ == LifeCycleState::ALIVE);

  DEBUG_ASSERT(object_);

  LTRACEF("region %p obj_offset %#" PRIx64 " size %zu, offset %#" PRIx64 " len %#" PRIx64 "\n",
          this, object_offset_locked_object(), size_, offset, len);

  // See if there's an intersect.
  vaddr_t base;
  uint64_t new_len;
  if (!ObjectRangeToVaddrRange(offset, len, &base, &new_len)) {
    return ZX_OK;
  }

  ArchVmAspace::HarvestCallback callback = [&accessed_callback, this](paddr_t paddr, vaddr_t vaddr,
                                                                      uint) {
    AssertHeld(object_->lock_ref());
    vm_page_t* page = paddr_to_vm_page(paddr);
    // It's possible page is invalid in the case of physical mappings that did not originate from
    // a vm_page_t. We just let the accessed_callback deal with this.

    // Turn the virtual address into an object offset. We know this will work as our virtual address
    // range we are operating on was already determined from the object earlier in
    // |ObjectRangeToVaddrRange|
    uint64_t offset;
    bool overflow = sub_overflow(vaddr, base_, &offset);
    DEBUG_ASSERT(!overflow);
    overflow = add_overflow(offset, object_offset_locked_object(), &offset);
    DEBUG_ASSERT(!overflow);
    return accessed_callback(page, offset);
  };

  return aspace_->arch_aspace().HarvestAccessed(base, new_len / PAGE_SIZE, callback);
}

zx_status_t VmMapping::AspaceRemoveWriteVmoRangeLocked(uint64_t offset, uint64_t len) const {
  LTRACEF("region %p obj_offset %#" PRIx64 " size %zu, offset %#" PRIx64 " len %#" PRIx64 "\n",
          this, object_offset_, size_, offset, len);

  canary_.Assert();

  // NOTE: must be acquired with the vmo lock held, but doesn't need to take
  // the address space lock, since it will not manipulate its location in the
  // vmar tree. However, it must be held in the ALIVE state across this call.
  //
  // Avoids a race with DestroyLocked() since it removes ourself from the VMO's
  // mapping list with the VMO lock held before dropping this state to DEAD. The
  // VMO cant call back to us once we're out of their list.
  DEBUG_ASSERT(state_ == LifeCycleState::ALIVE);

  DEBUG_ASSERT(object_);

  // If this doesn't support writing then nothing to be done, as we know we have no write mappings.
  if (!(flags_ & VMAR_FLAG_CAN_MAP_WRITE) ||
      !(arch_mmu_flags_locked_object() & ARCH_MMU_FLAG_PERM_WRITE)) {
    return ZX_OK;
  }

  // See if there's an intersect.
  vaddr_t base;
  uint64_t new_len;
  if (!ObjectRangeToVaddrRange(offset, len, &base, &new_len)) {
    return ZX_OK;
  }

  // Build new mmu flags without writing.
  uint mmu_flags = arch_mmu_flags_locked_object() & ~(ARCH_MMU_FLAG_PERM_WRITE);

  return ProtectOrUnmap(aspace_, base, new_len, mmu_flags);
}

namespace {

class VmMappingCoalescer {
 public:
  VmMappingCoalescer(VmMapping* mapping, vaddr_t base) TA_REQ(mapping->lock());
  ~VmMappingCoalescer();

  // Add a page to the mapping run.  If this fails, the VmMappingCoalescer is
  // no longer valid.
  zx_status_t Append(vaddr_t vaddr, paddr_t paddr) {
    AssertHeld(mapping_->lock_ref());
    DEBUG_ASSERT(!aborted_);
    // If this isn't the expected vaddr, flush the run we have first.
    if (count_ >= ktl::size(phys_) || vaddr != base_ + count_ * PAGE_SIZE) {
      zx_status_t status = Flush();
      if (status != ZX_OK) {
        return status;
      }
      base_ = vaddr;
    }
    phys_[count_] = paddr;
    ++count_;
    return ZX_OK;
  }

  // Submit any outstanding mappings to the MMU.  If this fails, the
  // VmMappingCoalescer is no longer valid.
  zx_status_t Flush();

  // Drop the current outstanding mappings without sending them to the MMU.
  // After this call, the VmMappingCoalescer is no longer valid.
  void Abort() { aborted_ = true; }

 private:
  DISALLOW_COPY_ASSIGN_AND_MOVE(VmMappingCoalescer);

  VmMapping* mapping_;
  vaddr_t base_;
  paddr_t phys_[16];
  size_t count_;
  bool aborted_;
};

VmMappingCoalescer::VmMappingCoalescer(VmMapping* mapping, vaddr_t base)
    : mapping_(mapping), base_(base), count_(0), aborted_(false) {}

VmMappingCoalescer::~VmMappingCoalescer() {
  // Make sure we've flushed or aborted
  DEBUG_ASSERT(count_ == 0 || aborted_);
}

zx_status_t VmMappingCoalescer::Flush() {
  AssertHeld(mapping_->lock_ref());

  if (count_ == 0) {
    return ZX_OK;
  }

  uint flags = mapping_->arch_mmu_flags_locked();
  if (flags & ARCH_MMU_FLAG_PERM_RWX_MASK) {
    size_t mapped;
    zx_status_t ret = mapping_->aspace()->arch_aspace().Map(
        base_, phys_, count_, flags, ArchVmAspace::ExistingEntryAction::Error, &mapped);
    if (ret != ZX_OK) {
      TRACEF("error %d mapping %zu pages starting at va %#" PRIxPTR "\n", ret, count_, base_);
      aborted_ = true;
      return ret;
    }
    DEBUG_ASSERT(mapped == count_);
  }
  base_ += count_ * PAGE_SIZE;
  count_ = 0;
  return ZX_OK;
}

}  // namespace

zx_status_t VmMapping::MapRange(size_t offset, size_t len, bool commit) {
  Guard<Mutex> aspace_guard{aspace_->lock()};
  return MapRangeLocked(offset, len, commit);
}

zx_status_t VmMapping::MapRangeLocked(size_t offset, size_t len, bool commit) {
  canary_.Assert();

  len = ROUNDUP(len, PAGE_SIZE);
  if (len == 0) {
    return ZX_ERR_INVALID_ARGS;
  }

  if (state_ != LifeCycleState::ALIVE) {
    return ZX_ERR_BAD_STATE;
  }

  LTRACEF("region %p, offset %#zx, size %#zx, commit %d\n", this, offset, len, commit);

  DEBUG_ASSERT(object_);
  if (!IS_PAGE_ALIGNED(offset) || !is_in_range(base_ + offset, len)) {
    return ZX_ERR_INVALID_ARGS;
  }

  // precompute the flags we'll pass GetPageLocked
  // if committing, then tell it to soft fault in a page
  uint pf_flags = VMM_PF_FLAG_WRITE;
  if (commit) {
    pf_flags |= VMM_PF_FLAG_SW_FAULT;
  }

  // grab the lock for the vmo
  Guard<Mutex> object_guard{object_->lock()};

  // set the currently faulting flag for any recursive calls the vmo may make back into us.
  DEBUG_ASSERT(!currently_faulting_);
  currently_faulting_ = true;
  auto cleanup = fit::defer([&]() {
    AssertHeld(object_->lock_ref());
    currently_faulting_ = false;
  });

  // iterate through the range, grabbing a page from the underlying object and
  // mapping it in
  size_t o;
  VmMappingCoalescer coalescer(this, base_ + offset);
  __UNINITIALIZED VmObject::LookupInfo pages;
  for (o = offset; o < offset + len;) {
    uint64_t vmo_offset = object_offset_ + o;

    zx_status_t status;
    status = object_->LookupPagesLocked(
        vmo_offset, pf_flags,
        ktl::min((offset + len - o) / PAGE_SIZE, VmObject::LookupInfo::kMaxPages), nullptr, nullptr,
        &pages);
    if (status != ZX_OK) {
      // no page to map
      if (commit) {
        // fail when we can't commit every requested page
        coalescer.Abort();
        return status;
      }

      // skip ahead
      o += PAGE_SIZE;
      continue;
    }
    DEBUG_ASSERT(pages.num_pages > 0);

    vaddr_t va = base_ + o;
    for (uint32_t i = 0; i < pages.num_pages; i++, va += PAGE_SIZE, o += PAGE_SIZE) {
      LTRACEF_LEVEL(2, "mapping pa %#" PRIxPTR " to va %#" PRIxPTR "\n", pages.paddrs[i], va);
      status = coalescer.Append(va, pages.paddrs[i]);
      if (status != ZX_OK) {
        return status;
      }
    }
  }
  return coalescer.Flush();
}

zx_status_t VmMapping::DecommitRange(size_t offset, size_t len) {
  canary_.Assert();
  LTRACEF("%p [%#zx+%#zx], offset %#zx, len %#zx\n", this, base_, size_, offset, len);

  Guard<Mutex> guard{aspace_->lock()};
  if (state_ != LifeCycleState::ALIVE) {
    return ZX_ERR_BAD_STATE;
  }
  if (offset + len < offset || offset + len > size_) {
    return ZX_ERR_OUT_OF_RANGE;
  }
  // VmObject::DecommitRange will typically call back into our instance's
  // VmMapping::AspaceUnmapVmoRangeLocked.
  return object_->DecommitRange(object_offset_locked() + offset, len);
}

zx_status_t VmMapping::DestroyLocked() {
  canary_.Assert();
  LTRACEF("%p\n", this);

  // Take a reference to ourself, so that we do not get destructed after
  // dropping our last reference in this method (e.g. when calling
  // subregions_.erase below).
  fbl::RefPtr<VmMapping> self(this);

  // If this is the last_fault_ then clear it before removing from the VMAR tree. Even if this
  // destroy fails, it's always safe to clear last_fault_, so we preference doing it upfront for
  // clarity.
  if (aspace_->last_fault_ == this) {
    aspace_->last_fault_ = nullptr;
  }

  // The vDSO code mapping can never be unmapped, not even
  // by VMAR destruction (except for process exit, of course).
  // TODO(mcgrathr): Turn this into a policy-driven process-fatal case
  // at some point.  teisenbe@ wants to eventually make zx_vmar_destroy
  // never fail.
  if (aspace_->vdso_code_mapping_ == self) {
    return ZX_ERR_ACCESS_DENIED;
  }

  // unmap our entire range
  zx_status_t status = UnmapLocked(base_, size_);
  if (status != ZX_OK) {
    return status;
  }
  // Unmap should have reset our size to 0
  DEBUG_ASSERT(size_ == 0);

  // grab the object lock and remove ourself from its list
  {
    Guard<Mutex> guard{object_->lock()};
    object_->RemoveMappingLocked(this);
  }

  // Clear the cached attribution count.
  // The generation count should already have been incremented by UnmapLocked above.
  cached_page_attribution_ = {};

  // detach from any object we have mapped. Note that we are holding the aspace_->lock() so we
  // will not race with other threads calling vmo()
  object_.reset();

  // Detach the now dead region from the parent
  if (parent_) {
    AssertHeld(parent_->lock_ref());
    DEBUG_ASSERT(this->in_subregion_tree());
    parent_->subregions_.RemoveRegion(this);
  }

  // mark ourself as dead
  parent_ = nullptr;
  state_ = LifeCycleState::DEAD;
  return ZX_OK;
}

zx_status_t VmMapping::PageFault(vaddr_t va, const uint pf_flags, LazyPageRequest* page_request) {
  canary_.Assert();

  DEBUG_ASSERT(is_in_range(va, 1));

  va = ROUNDDOWN(va, PAGE_SIZE);
  uint64_t vmo_offset = va - base_ + object_offset_locked();

  __UNUSED char pf_string[5];
  LTRACEF("%p va %#" PRIxPTR " vmo_offset %#" PRIx64 ", pf_flags %#x (%s)\n", this, va, vmo_offset,
          pf_flags, vmm_pf_flags_to_string(pf_flags, pf_string));

  // make sure we have permission to continue
  if ((pf_flags & VMM_PF_FLAG_USER) && !(arch_mmu_flags_locked() & ARCH_MMU_FLAG_PERM_USER)) {
    // user page fault on non user mapped region
    LTRACEF("permission failure: user fault on non user region\n");
    return ZX_ERR_ACCESS_DENIED;
  }
  if ((pf_flags & VMM_PF_FLAG_WRITE) && !(arch_mmu_flags_locked() & ARCH_MMU_FLAG_PERM_WRITE)) {
    // write to a non-writeable region
    LTRACEF("permission failure: write fault on non-writable region\n");
    return ZX_ERR_ACCESS_DENIED;
  }
  if (!(pf_flags & VMM_PF_FLAG_WRITE) && !(arch_mmu_flags_locked() & ARCH_MMU_FLAG_PERM_READ)) {
    // read to a non-readable region
    LTRACEF("permission failure: read fault on non-readable region\n");
    return ZX_ERR_ACCESS_DENIED;
  }
  if ((pf_flags & VMM_PF_FLAG_INSTRUCTION) &&
      !(arch_mmu_flags_locked() & ARCH_MMU_FLAG_PERM_EXECUTE)) {
    // instruction fetch from a no execute region
    LTRACEF("permission failure: execute fault on no execute region\n");
    return ZX_ERR_ACCESS_DENIED;
  }

  // grab the lock for the vmo
  Guard<Mutex> guard{object_->lock()};

  // set the currently faulting flag for any recursive calls the vmo may make back into us
  // The specific path we're avoiding is if the VMO calls back into us during vmo->GetPageLocked()
  // via AspaceUnmapVmoRangeLocked(). Since we're responsible for that page, signal to ourself to
  // skip the unmap operation.
  DEBUG_ASSERT(!currently_faulting_);
  currently_faulting_ = true;
  auto cleanup = fit::defer([&]() {
    AssertHeld(object_->lock_ref());
    currently_faulting_ = false;
  });

  // Determine how far to the end of the page table so we do not cause extra allocations.
  const uint64_t next_pt_base = ArchVmAspace::NextUserPageTableOffset(va);
  // Find the minimum between the size of this mapping and the end of the page table.
  const uint64_t max_map = ktl::min(next_pt_base, base_ + size_);
  // Convert this into a number of pages, limited by the max lookup window.
  const uint64_t max_pages = ktl::min((max_map - va) / PAGE_SIZE, VmObject::LookupInfo::kMaxPages);
  DEBUG_ASSERT(max_pages > 0);

  // fault in or grab existing pages.
  __UNINITIALIZED VmObject::LookupInfo lookup_info;
  zx_status_t status = object_->LookupPagesLocked(vmo_offset, pf_flags, max_pages, nullptr,
                                                  page_request, &lookup_info);
  if (status != ZX_OK) {
    // TODO(cpu): This trace was originally TRACEF() always on, but it fires if the
    // VMO was resized, rather than just when the system is running out of memory.
    LTRACEF("ERROR: failed to fault in or grab existing page: %d\n", (int)status);
    LTRACEF("%p vmo_offset %#" PRIx64 ", pf_flags %#x\n", this, vmo_offset, pf_flags);
    return status;
  }
  DEBUG_ASSERT(lookup_info.num_pages > 0);

  // if we read faulted, and lookup didn't say that this is always writable, then we map or modify
  // the page without any write permissions. This ensures we will fault again if a write is
  // attempted so we can potentially replace this page with a copy or a new one.
  uint mmu_flags = arch_mmu_flags_;
  if (!(pf_flags & VMM_PF_FLAG_WRITE) && !lookup_info.writable) {
    // we read faulted, so only map with read permissions
    mmu_flags &= ~ARCH_MMU_FLAG_PERM_WRITE;
  }

  // see if something is mapped here now
  // this may happen if we are one of multiple threads racing on a single address
  uint page_flags;
  paddr_t pa;
  zx_status_t err = aspace_->arch_aspace().Query(va, &pa, &page_flags);
  if (err >= 0) {
    LTRACEF("queried va, page at pa %#" PRIxPTR ", flags %#x is already there\n", pa, page_flags);
    if (pa == lookup_info.paddrs[0]) {
      // Faulting on a mapping that is the correct page could happen for a few reasons
      //  1. Permission are incorrect and this fault is a write fault for a read only mapping.
      //  2. Fault was caused by (1), but we were racing with another fault and the mapping is
      //     already fixed.
      //  3. Some other error, such as an access flag missing on arm, caused this fault
      // Of these three scenarios (1) is overwhelmingly the most common, and requires us to protect
      // the page with the new permissions. In the scenario of (2) we could fast return and not
      // perform the potentially expensive protect, but this scenario is quite rare and requires a
      // multi-thread race on causing and handling the fault. (3) should also be highly uncommon as
      // access faults would normally be handled by a separate fault handler, nevertheless we should
      // still resolve such faults here, which requires calling protect.
      // Given that (2) is rare and hard to distinguish from (3) we simply always call protect to
      // ensure the fault is resolved.

      // assert that we're not accidentally marking the zero page writable
      DEBUG_ASSERT((pa != vm_get_zero_page_paddr()) || !(mmu_flags & ARCH_MMU_FLAG_PERM_WRITE));

      // same page, different permission
      status = aspace_->arch_aspace().Protect(va, 1, mmu_flags);
      if (status != ZX_OK) {
        TRACEF("failed to modify permissions on existing mapping\n");
        return ZX_ERR_NO_MEMORY;
      }
    } else {
      // some other page is mapped there already
      LTRACEF("thread %s faulted on va %#" PRIxPTR ", different page was present\n",
              Thread::Current::Get()->name(), va);

      // assert that we're not accidentally mapping the zero page writable
      DEBUG_ASSERT(!(mmu_flags & ARCH_MMU_FLAG_PERM_WRITE) ||
                   ktl::all_of(lookup_info.paddrs, &lookup_info.paddrs[lookup_info.num_pages],
                               [](paddr_t p) { return p != vm_get_zero_page_paddr(); }));

      // unmap the old one and put the new one in place
      status = aspace_->arch_aspace().Unmap(va, 1, nullptr);
      if (status != ZX_OK) {
        TRACEF("failed to remove old mapping before replacing\n");
        return ZX_ERR_NO_MEMORY;
      }

      size_t mapped;
      status = aspace_->arch_aspace().Map(va, lookup_info.paddrs, lookup_info.num_pages, mmu_flags,
                                          ArchVmAspace::ExistingEntryAction::Skip, &mapped);
      if (status != ZX_OK) {
        TRACEF("failed to map replacement page\n");
        return ZX_ERR_NO_MEMORY;
      }
      DEBUG_ASSERT(mapped >= 1);

      return ZX_OK;
    }
  } else {
    // nothing was mapped there before, map it now

    // assert that we're not accidentally mapping the zero page writable
    DEBUG_ASSERT(!(mmu_flags & ARCH_MMU_FLAG_PERM_WRITE) ||
                 ktl::all_of(lookup_info.paddrs, &lookup_info.paddrs[lookup_info.num_pages],
                             [](paddr_t p) { return p != vm_get_zero_page_paddr(); }));

    size_t mapped;
    status = aspace_->arch_aspace().Map(va, lookup_info.paddrs, lookup_info.num_pages, mmu_flags,
                                        ArchVmAspace::ExistingEntryAction::Skip, &mapped);
    if (status != ZX_OK) {
      TRACEF("failed to map page\n");
      return ZX_ERR_NO_MEMORY;
    }
    DEBUG_ASSERT(mapped >= 1);
  }

  return ZX_OK;
}

void VmMapping::ActivateLocked() {
  DEBUG_ASSERT(state_ == LifeCycleState::NOT_READY);
  DEBUG_ASSERT(parent_);

  state_ = LifeCycleState::ALIVE;
  object_->AddMappingLocked(this);
  AssertHeld(parent_->lock_ref());
  parent_->subregions_.InsertRegion(fbl::RefPtr<VmAddressRegionOrMapping>(this));
}

void VmMapping::Activate() {
  Guard<Mutex> guard{object_->lock()};
  ActivateLocked();
}

void VmMapping::TryMergeRightNeighborLocked(VmMapping* right_candidate) {
  // This code is tolerant of many 'miss calls' if mappings aren't mergeable or are not neighbours
  // etc, but the caller should not be attempting to merge if these mappings are not actually from
  // the same vmar parent. Doing so indicates something structurally wrong with the hierarchy.
  DEBUG_ASSERT(parent_ == right_candidate->parent_);

  AssertHeld(right_candidate->lock_ref());

  // These tests are intended to be ordered such that we fail as fast as possible. As such testing
  // for mergeability, which we commonly expect to succeed and not fail, is done last.

  // Need to refer to the same object.
  if (object_.get() != right_candidate->object_.get()) {
    return;
  }
  // Aspace and VMO ranges need to be contiguous. Validate that the right candidate is actually to
  // the right in addition to checking that base+size lines up for single scenario where base_+size_
  // can overflow and becomes zero.
  if (base_ + size_ != right_candidate->base_ || right_candidate->base_ < base_) {
    return;
  }
  if (object_offset_locked() + size_ != right_candidate->object_offset_locked()) {
    return;
  }
  // All flags need to be consistent.
  if (flags_ != right_candidate->flags_) {
    return;
  }
  if (arch_mmu_flags_locked() != right_candidate->arch_mmu_flags_locked()) {
    return;
  }
  // Only merge live mappings.
  if (state_ != LifeCycleState::ALIVE || right_candidate->state_ != LifeCycleState::ALIVE) {
    return;
  }
  // Both need to be mergeable.
  if (mergeable_ == Mergeable::NO || right_candidate->mergeable_ == Mergeable::NO) {
    return;
  }

  // Destroy / DestroyLocked perform a lot more cleanup than we want, we just need to clear out a
  // few things from right_candidate and then mark it as dead, as we do not want to clear out any
  // arch page table mappings etc.
  {
    // Although it was safe to read size_ without holding the object lock, we need to acquire it to
    // perform changes.
    Guard<Mutex> guard{right_candidate->object_->lock()};
    AssertHeld(object_->lock_ref());

    set_size_locked(size_ + right_candidate->size_);
    right_candidate->set_size_locked(0);

    right_candidate->object_->RemoveMappingLocked(right_candidate);
  }

  // Detach from the VMO.
  right_candidate->object_.reset();

  // Detach the now dead region from the parent, ensuring our caller is correctly holding a refptr.
  DEBUG_ASSERT(right_candidate->in_subregion_tree());
  DEBUG_ASSERT(right_candidate->ref_count_debug() > 1);
  AssertHeld(parent_->lock_ref());
  parent_->subregions_.RemoveRegion(right_candidate);
  if (aspace_->last_fault_ == right_candidate) {
    aspace_->last_fault_ = nullptr;
  }

  // Mark it as dead.
  right_candidate->parent_ = nullptr;
  right_candidate->state_ = LifeCycleState::DEAD;

  vm_mappings_merged.Add(1);
}

void VmMapping::TryMergeNeighborsLocked() {
  canary_.Assert();

  // Check that this mapping is mergeable and is currently in the correct lifecycle state.
  if (mergeable_ == Mergeable::NO || state_ != LifeCycleState::ALIVE) {
    return;
  }
  // As a VmMapping if we we are alive we by definition have a parent.
  DEBUG_ASSERT(parent_);

  // We expect there to be a RefPtr to us held beyond the one for the wavl tree ensuring that we
  // cannot trigger our own destructor should we remove ourselves from the hierarchy.
  DEBUG_ASSERT(ref_count_debug() > 1);

  // First consider merging any mapping on our right, into |this|.
  AssertHeld(parent_->lock_ref());
  VmAddressRegionOrMapping* right_candidate = parent_->subregions_.FindRegion(base_ + size_);
  if (right_candidate) {
    // Request mapping as a refptr as we need to hold a refptr across the try merge.
    if (fbl::RefPtr<VmMapping> mapping = right_candidate->as_vm_mapping()) {
      TryMergeRightNeighborLocked(mapping.get());
    }
  }

  // Now attempt to merge |this| with any left neighbor.
  if (base_ == 0) {
    return;
  }
  AssertHeld(parent_->lock_ref());
  VmAddressRegionOrMapping* left_candidate = parent_->subregions_.FindRegion(base_ - 1);
  if (!left_candidate) {
    return;
  }
  if (auto mapping = left_candidate->as_vm_mapping()) {
    // Attempt actual merge. If this succeeds then |this| is in the dead state, but that's fine as
    // we are finished anyway.
    AssertHeld(mapping->lock_ref());
    mapping->TryMergeRightNeighborLocked(this);
  }
}

void VmMapping::MarkMergeable(fbl::RefPtr<VmMapping>&& mapping) {
  Guard<Mutex> guard{mapping->lock()};
  // Now that we have the lock check this mapping is still alive and we haven't raced with some
  // kind of destruction.
  if (mapping->state_ != LifeCycleState::ALIVE) {
    return;
  }
  // Skip marking any vdso segments mergeable. Although there is currently only one vdso segment and
  // so it would never actually get merged, marking it mergeable is technically incorrect.
  if (mapping->aspace_->vdso_code_mapping_ == mapping) {
    return;
  }
  mapping->mergeable_ = Mergeable::YES;
  mapping->TryMergeNeighborsLocked();
}
