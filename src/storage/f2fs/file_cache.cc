// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/storage/f2fs/f2fs.h"

namespace f2fs {

Page::Page(FileCache *file_cache, pgoff_t index)
    : file_cache_(file_cache), index_(index), fs_(file_cache_->GetVnode().fs()) {}

VnodeF2fs &Page::GetVnode() const { return file_cache_->GetVnode(); }

FileCache &Page::GetFileCache() const { return *file_cache_; }

Page::~Page() {
  ZX_DEBUG_ASSERT(IsWriteback() == false);
  ZX_DEBUG_ASSERT(InContainer() == false);
  ZX_DEBUG_ASSERT(IsDirty() == false);
  ZX_DEBUG_ASSERT(IsLocked() == false);
  ZX_DEBUG_ASSERT(IsMmapped() == false);
}

void Page::RecyclePage() {
  // Since active Pages are evicted only when having strong references,
  // it is safe to call InContainer().
  if (InContainer()) {
    ZX_ASSERT(VmoOpUnlock() == ZX_OK);
    file_cache_->Downgrade(this);
  } else {
    delete this;
  }
}

bool Page::SetDirty() {
  SetUptodate();
  // No need to make dirty Pages for orphan files.
  if (!file_cache_->IsOrphan() &&
      !flags_[static_cast<uint8_t>(PageFlag::kPageDirty)].test_and_set(std::memory_order_acquire)) {
    VnodeF2fs &vnode = GetVnode();
    SuperblockInfo &superblock_info = fs_->GetSuperblockInfo();
    vnode.MarkInodeDirty();
    vnode.IncreaseDirtyPageCount();
    if (vnode.IsNode()) {
      superblock_info.IncreasePageCount(CountType::kDirtyNodes);
    } else if (vnode.IsDir()) {
      superblock_info.IncreasePageCount(CountType::kDirtyDents);
      superblock_info.IncreaseDirtyDir();
    } else if (vnode.IsMeta()) {
      superblock_info.IncreasePageCount(CountType::kDirtyMeta);
      superblock_info.SetDirty();
    } else {
      superblock_info.IncreasePageCount(CountType::kDirtyData);
    }
    return false;
  }
  return true;
}

bool Page::ClearDirtyForIo() {
  ZX_DEBUG_ASSERT(IsLocked());
  VnodeF2fs &vnode = GetVnode();
  SuperblockInfo &superblock_info = fs_->GetSuperblockInfo();
  if (IsDirty()) {
    ClearFlag(PageFlag::kPageDirty);
    vnode.DecreaseDirtyPageCount();
    if (vnode.IsNode()) {
      superblock_info.DecreasePageCount(CountType::kDirtyNodes);
    } else if (vnode.IsDir()) {
      superblock_info.DecreasePageCount(CountType::kDirtyDents);
      superblock_info.DecreaseDirtyDir();
    } else if (vnode.IsMeta()) {
      superblock_info.DecreasePageCount(CountType::kDirtyMeta);
    } else {
      superblock_info.DecreasePageCount(CountType::kDirtyData);
    }
    return true;
  }
  return false;
}

zx_status_t Page::GetPage() {
  ZX_DEBUG_ASSERT(IsLocked());
  auto committed_or = VmoOpLock();
  if (committed_or.is_ok()) {
    if (!committed_or.value()) {
      ZX_DEBUG_ASSERT(!IsDirty());
      ZX_DEBUG_ASSERT(!IsWriteback());
      ClearFlag(PageFlag::kPageUptodate);
      ClearMapped();
    }
    ZX_ASSERT(Map() == ZX_OK);
  }
  return committed_or.status_value();
}

zx_status_t Page::Map() {
  if (!SetFlag(PageFlag::kPageMapped)) {
#ifdef __Fuchsia__
    auto address_or = file_cache_->GetVmoManager().GetAddress(index_);
    if (address_or.is_ok()) {
      address_ = address_or.value();
    }
    return address_or.status_value();
#else   // __Fuchsia__
    address_ = reinterpret_cast<zx_vaddr_t>(blk_.GetData());
#endif  // __Fuchsia__
  }
  return ZX_OK;
}

void Page::Invalidate() {
  ZX_DEBUG_ASSERT(IsLocked());
  ClearDirtyForIo();
  ClearColdData();
  if (ClearMmapped()) {
    ZX_ASSERT(GetVnode().InvalidatePagedVmo(GetIndex() * kBlockSize, kBlockSize) == ZX_OK);
  }
  ClearUptodate();
}

bool Page::SetUptodate() {
  ZX_DEBUG_ASSERT(IsLocked());
  return SetFlag(PageFlag::kPageUptodate);
}

void Page::ClearUptodate() { ClearFlag(PageFlag::kPageUptodate); }

void Page::WaitOnWriteback() {
  if (IsWriteback()) {
    fs_->ScheduleWriterSubmitPages(nullptr, GetVnode().GetPageType());
  }
  WaitOnFlag(PageFlag::kPageWriteback);
}

bool Page::SetWriteback() {
  bool ret = SetFlag(PageFlag::kPageWriteback);
  if (!ret) {
    fs_->GetSuperblockInfo().IncreasePageCount(CountType::kWriteback);
  }
  return ret;
}

void Page::ClearWriteback() {
  if (IsWriteback()) {
    fs_->GetSuperblockInfo().DecreasePageCount(CountType::kWriteback);
    ClearFlag(PageFlag::kPageWriteback);
    WakeupFlag(PageFlag::kPageWriteback);
  }
}

void Page::SetMmapped() {
  ZX_DEBUG_ASSERT(IsLocked());
  if (IsUptodate()) {
    if (!SetFlag(PageFlag::kPageMmapped)) {
      fs_->GetSuperblockInfo().IncreasePageCount(CountType::kMmapedData);
    }
  }
}

bool Page::ClearMmapped() {
  ZX_DEBUG_ASSERT(IsLocked());
  if (IsMmapped()) {
    fs_->GetSuperblockInfo().DecreasePageCount(CountType::kMmapedData);
    ClearFlag(PageFlag::kPageMmapped);
    return true;
  }
  return false;
}

void Page::SetColdData() {
  ZX_DEBUG_ASSERT(IsLocked());
  ZX_DEBUG_ASSERT(!IsWriteback());
  SetFlag(PageFlag::kPageColdData);
}

bool Page::ClearColdData() {
  if (IsColdData()) {
    ClearFlag(PageFlag::kPageColdData);
    return true;
  }
  return false;
}

#ifdef __Fuchsia__
zx_status_t Page::VmoOpUnlock(bool evict) {
  ZX_DEBUG_ASSERT(InContainer());
  // |evict| can be true only when the Page is clean or subject to invalidation.
  if ((!IsDirty() || evict) && IsVmoLocked()) {
    ClearFlag(PageFlag::kPageVmoLocked);
    return file_cache_->GetVmoManager().UnlockVmo(index_, evict);
  }
  return ZX_OK;
}

zx::status<bool> Page::VmoOpLock() {
  ZX_DEBUG_ASSERT(InContainer());
  ZX_DEBUG_ASSERT(IsLocked());
  if (!SetFlag(PageFlag::kPageVmoLocked)) {
    return file_cache_->GetVmoManager().CreateAndLockVmo(index_);
  }
  return zx::ok(true);
}
#else   // __Fuchsia__
// Do nothing on Linux.
zx_status_t Page::VmoOpUnlock(bool evict) { return ZX_OK; }

zx::status<bool> Page::VmoOpLock() { return zx::ok(true); }
#endif  // __Fuchsia__

#ifdef __Fuchsia__
FileCache::FileCache(VnodeF2fs *vnode, VmoManager *vmo_manager)
    : vnode_(vnode), vmo_manager_(vmo_manager) {}
#else   // __Fuchsia__
FileCache::FileCache(VnodeF2fs *vnode) : vnode_(vnode) {}
#endif  // __Fuchsia__

FileCache::~FileCache() {
  Reset();
  {
    std::lock_guard tree_lock(tree_lock_);
    ZX_DEBUG_ASSERT(page_tree_.is_empty());
  }
}

void FileCache::Downgrade(Page *raw_page) {
  // We can downgrade multiple Pages simultaneously.
  fs::SharedLock tree_lock(tree_lock_);
  // Resurrect |this|.
  raw_page->ResurrectRef();
  fbl::RefPtr<Page> page = fbl::ImportFromRawPtr(raw_page);
  // Leak it to keep alive in FileCache.
  __UNUSED auto leak = fbl::ExportToRawPtr(&page);
  raw_page->ClearActive();
  recycle_cvar_.notify_all();
}

zx_status_t FileCache::AddPageUnsafe(const fbl::RefPtr<Page> &page) {
  if (page->InContainer()) {
    return ZX_ERR_ALREADY_EXISTS;
  }
  page_tree_.insert(page.get());
  return ZX_OK;
}

zx::status<std::vector<LockedPage>> FileCache::GetPages(const pgoff_t start, const pgoff_t end) {
  std::lock_guard tree_lock(tree_lock_);
  std::vector<LockedPage> locked_pages(end - start);
  auto exist_pages = GetLockedPagesUnsafe(start, end);
  uint32_t exist_pages_index = 0, count = 0;
  for (pgoff_t index = start; index < end; ++index, ++count) {
    if (exist_pages_index < exist_pages.size() &&
        exist_pages[exist_pages_index]->GetKey() == index) {
      locked_pages[count] = std::move(exist_pages[exist_pages_index]);
      ++exist_pages_index;
    } else {
      locked_pages[count] = GetNewPage(index);
    }

    if (auto ret = locked_pages[count]->GetPage(); ret != ZX_OK) {
      return zx::error(ret);
    }
  }

  return zx::ok(std::move(locked_pages));
}

LockedPage FileCache::GetNewPage(const pgoff_t index) {
  fbl::RefPtr<Page> page;
  if (GetVnode().IsNode()) {
    page = fbl::MakeRefCounted<NodePage>(this, index);
  } else {
    page = fbl::MakeRefCounted<Page>(this, index);
  }
  ZX_ASSERT(AddPageUnsafe(page) == ZX_OK);
  auto locked_page = LockedPage(std::move(page));
  locked_page->SetActive();
  return locked_page;
}

zx_status_t FileCache::GetPage(const pgoff_t index, LockedPage *out) {
  LockedPage locked_page;
  std::lock_guard tree_lock(tree_lock_);
  auto locked_page_or = GetPageUnsafe(index);
  if (locked_page_or.is_error()) {
    locked_page = GetNewPage(index);
  } else {
    locked_page = std::move(*locked_page_or);
  }
  if (auto ret = locked_page->GetPage(); ret != ZX_OK) {
    return ret;
  }
  *out = std::move(locked_page);
  return ZX_OK;
}

zx_status_t FileCache::FindPage(const pgoff_t index, fbl::RefPtr<Page> *out) {
  std::lock_guard tree_lock(tree_lock_);
  auto locked_page_or = GetPageUnsafe(index);
  if (locked_page_or.is_error()) {
    return locked_page_or.error_value();
  }
  if (auto ret = (*locked_page_or)->GetPage(); ret != ZX_OK) {
    return ret;
  }
  *out = (*locked_page_or).release();
  return ZX_OK;
}

zx::status<LockedPage> FileCache::GetPageUnsafe(const pgoff_t index) {
  while (true) {
    auto raw_ptr = page_tree_.find(index).CopyPointer();
    fbl::RefPtr<Page> page;
    if (raw_ptr != nullptr) {
      if (raw_ptr->IsActive()) {
        page = fbl::MakeRefPtrUpgradeFromRaw(raw_ptr, tree_lock_);
        // We wait for it to be resurrected in fbl_recycle().
        if (page == nullptr) {
          recycle_cvar_.wait(tree_lock_);
          continue;
        }
        auto locked_page_or = GetLockedPage(std::move(page));
        if (locked_page_or.is_error()) {
          continue;
        }
        // Here, Page::ref_count should not be less than one.
        return zx::ok(std::move(locked_page_or.value()));
      }
      page = fbl::ImportFromRawPtr(raw_ptr);
      LockedPage locked_page(std::move(page));
      locked_page->SetActive();
      ZX_DEBUG_ASSERT(locked_page->IsLastReference());
      return zx::ok(std::move(locked_page));
    }
    break;
  }
  return zx::error(ZX_ERR_NOT_FOUND);
}

zx::status<LockedPage> FileCache::GetLockedPage(fbl::RefPtr<Page> page) {
  if (page->TryLock()) {
    tree_lock_.unlock();
    {
      // If |page| is already locked, wait for it to be unlocked.
      // Ensure that the references to |page| drop before |tree_lock_|.
      // If |page| is the last reference, it enters Page::RecyclePage() and
      // possibly acquires |tree_lock_|.
      LockedPage locked_page(std::move(page));
    }
    // It is not allowed to acquire |tree_lock_| with locked Pages.
    tree_lock_.lock();
    return zx::error(ZX_ERR_SHOULD_WAIT);
  }
  LockedPage locked_page(std::move(page), false);
  return zx::ok(std::move(locked_page));
}

zx_status_t FileCache::EvictUnsafe(Page *page) {
  if (!page->InContainer()) {
    return ZX_ERR_NOT_FOUND;
  }
  // Before eviction, check if it requires VMO_OP_UNLOCK
  // since Page::RecyclePage() tries VMO_OP_UNLOCK only when |page| keeps in FileCache.
  ZX_ASSERT(page->VmoOpUnlock(true) == ZX_OK);
  page_tree_.erase(*page);
  return ZX_OK;
}

std::vector<LockedPage> FileCache::GetLockedPagesUnsafe(pgoff_t start, pgoff_t end) {
  std::vector<LockedPage> pages;
  auto current = page_tree_.lower_bound(start);
  while (current != page_tree_.end() && current->GetKey() < end) {
    if (!current->IsActive()) {
      LockedPage locked_page(fbl::ImportFromRawPtr(&(*current)));
      locked_page->SetActive();
      pages.push_back(std::move(locked_page));
    } else {
      auto page = fbl::MakeRefPtrUpgradeFromRaw(&(*current), tree_lock_);
      // When it is being recycled, wait and try it again.
      auto prev_key = current->GetKey();
      if (page == nullptr) {
        recycle_cvar_.wait(tree_lock_);
        // |current| might have been deleted during recycling.
        current = page_tree_.lower_bound(prev_key);
        continue;
      }
      auto locked_page_or = GetLockedPage(std::move(page));
      if (locked_page_or.is_error()) {
        current = page_tree_.lower_bound(prev_key);
        continue;
      }
      pages.push_back(std::move(*locked_page_or));
    }
    ++current;
  }
  return pages;
}

std::vector<LockedPage> FileCache::CleanupPagesUnsafe(pgoff_t start, pgoff_t end) {
  std::vector<LockedPage> pages = GetLockedPagesUnsafe(start, end);
  for (auto &page : pages) {
    EvictUnsafe(page.get());
  }
  return pages;
}

std::vector<LockedPage> FileCache::InvalidatePages(pgoff_t start, pgoff_t end) {
  std::vector<LockedPage> pages;
  {
    std::lock_guard tree_lock(tree_lock_);
    pages = GetLockedPagesUnsafe(start, end);
  }
  for (auto &page : pages) {
    page->Invalidate();
  }
  return pages;
}

void FileCache::ClearDirtyPages(pgoff_t start, pgoff_t end) {
  std::vector<LockedPage> pages;
  {
    std::lock_guard tree_lock(tree_lock_);
    pages = GetLockedPagesUnsafe(start, end);
  }
  // Clear the dirty flag of all Pages.
  for (auto &page : pages) {
    page->ClearDirtyForIo();
  }
}

void FileCache::Reset() {
  std::vector<LockedPage> pages;
  {
    std::lock_guard tree_lock(tree_lock_);
    pages = CleanupPagesUnsafe();
  }
  for (auto &page : pages) {
    page->WaitOnWriteback();
    if (page->IsDirty()) {
      FX_LOGS(WARNING) << "[f2fs] An unexpected dirty page found.";
      page->Invalidate();
    }
    page->ClearMmapped();
  }
}

std::vector<LockedPage> FileCache::GetLockedDirtyPagesUnsafe(const WritebackOperation &operation) {
  std::vector<LockedPage> pages;
  pgoff_t nwritten = 0;

  auto current = page_tree_.lower_bound(operation.start);
  // Acquire Pages from the the lower bound of |operation.start| to |operation.end|.
  while (nwritten <= operation.to_write && current != page_tree_.end() &&
         current->GetKey() < operation.end) {
    auto raw_page = current.CopyPointer();
    ++current;
    // Do not touch active Pages.
    if (raw_page->IsActive()) {
      continue;
    }
    ZX_ASSERT(!raw_page->IsLocked());
    LockedPage page(fbl::ImportFromRawPtr(raw_page));

    if (page->IsDirty()) {
      ZX_DEBUG_ASSERT(page->IsLastReference());
      auto page_ref = page.CopyRefPtr();
      if (!operation.if_page || operation.if_page(page_ref) == ZX_OK) {
        page->SetActive();
        ZX_DEBUG_ASSERT(page->IsUptodate());
        ZX_DEBUG_ASSERT(page->IsVmoLocked());
        pages.push_back(std::move(page));
        ++nwritten;
      }
    } else if (!page->IsMmapped() && (operation.bReleasePages || !vnode_->IsActive())) {
      // There is no other reference. It is safe to release it.
      page->SetActive();
      EvictUnsafe(page.get());
    }
    if (page && !page->IsActive()) {
      auto page_ref = page.release();
      // It prevents |page| from entering RecyclePage() and
      // keeps |page| alive in FileCache.
      __UNUSED auto leak = fbl::ExportToRawPtr(&page_ref);
    }
  }
  return pages;
}

// TODO: Consider using a global lock as below
// if (!IsDir())
//   mutex_lock(&superblock_info->writepages);
// Writeback()
// if (!IsDir())
//   mutex_unlock(&superblock_info->writepages);
// fs()->RemoveDirtyDirInode(this);
pgoff_t FileCache::Writeback(WritebackOperation &operation) {
  std::vector<LockedPage> pages;
  {
    std::lock_guard tree_lock(tree_lock_);
    pages = GetLockedDirtyPagesUnsafe(operation);
  }

  pgoff_t nwritten = 0;
  // The Pages of a vnode should belong to a PageType.
  PageType type = vnode_->GetPageType();
  for (size_t i = 0; i < pages.size(); ++i) {
    // Writeback for memory reclaim is not allowed for some reason such as gc and checkpoint.
    if (operation.bReclaim && !vnode_->fs()->CanReclaim()) {
      // Release remaining Pages in |pages| for waiters.
      pages.clear();
      pages.shrink_to_fit();
      break;
    }
    LockedPage page = std::move(pages[i]);

    ZX_DEBUG_ASSERT(page->IsUptodate());
    ZX_DEBUG_ASSERT(page->IsLocked());
    if (vnode_->IsNode() && operation.node_page_cb) {
      operation.node_page_cb(page.CopyRefPtr());
    }
    zx_status_t ret = vnode_->WriteDirtyPage(page, operation.bReclaim);
    if (ret != ZX_OK) {
      if (ret != ZX_ERR_NOT_FOUND && ret != ZX_ERR_OUT_OF_RANGE) {
        if (page->IsUptodate()) {
          // In case of failure, we just redirty it.
          page->SetDirty();
          FX_LOGS(WARNING) << "[f2fs] Writeback is not available for now." << ret;
        }
      }
      page->ClearWriteback();
    } else {
      ++nwritten;
    }
  }
  if (operation.bSync) {
    sync_completion_t completion;
    vnode_->fs()->ScheduleWriterSubmitPages(&completion, type);
    sync_completion_wait(&completion, ZX_TIME_INFINITE);
  }
  return nwritten;
}

}  // namespace f2fs
