// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STORAGE_F2FS_FILE_CACHE_H_
#define SRC_STORAGE_F2FS_FILE_CACHE_H_

#include <safemath/checked_math.h>
#include <storage/buffer/block_buffer.h>

namespace f2fs {

class F2fs;
class VnodeF2fs;
class FileCache;

enum class PageFlag {
  kPageUptodate = 0,  // It is uptodate. No need to read blocks from disk.
  kPageDirty,         // It needs to be written out.
  kPageWriteback,     // It is under writeback.
  kPageLocked,        // It is locked. Wait for it to be unlocked.
  kPageVmoLocked,     // Its vmo is locked to prevent mm from reclaiming it.
  kPageMapped,        // It has a valid mapping to the address space.
  kPageActive,        // It is being referenced.
  // TODO: Clear |kPageMmapped| when all mmaped areas are unmapped.
  kPageMmapped,  // It is mmapped. Once set, it remains regardless of munmap.
  kPageFlagSize,
};

constexpr pgoff_t kPgOffMax = std::numeric_limits<pgoff_t>::max();
// TODO: Once f2fs can get hints about memory pressure, remove it.
// Now, the maximum allowable memory for dirty data pages is 200MiB
constexpr int kMaxDirtyDataPages = 51200;

// It defines a writeback operation.
struct WritebackOperation {
  pgoff_t start = 0;  // All dirty Pages within the range of [start, end) are subject to writeback.
  pgoff_t end = kPgOffMax;
  pgoff_t to_write = kPgOffMax;  // The number of dirty Pages to be written.
  bool bSync = false;            // If true, FileCache::Writeback() waits for writeback Pages to be
                                 // written to disk.
  bool bReleasePages =
      true;  // If true, it releases clean Pages while traversing FileCache::page_tree_.
  VnodeCallback if_vnode = nullptr;  // If set, it determines which vnodes are subject to writeback.
  PageCallback if_page = nullptr;    // If set, it determines which Pages are subject to writeback.
  PageCallback node_page_cb = nullptr;  // If set, the callback is executed. This callback is for
                                        // node page only and is executed before writeback.
};

template <typename T, bool EnableAdoptionValidator = ZX_DEBUG_ASSERT_IMPLEMENTED>
class PageRefCounted : public fs::VnodeRefCounted<T> {
 public:
  PageRefCounted(const Page &) = delete;
  PageRefCounted &operator=(const PageRefCounted &) = delete;
  PageRefCounted(const PageRefCounted &&) = delete;
  PageRefCounted &operator=(const PageRefCounted &&) = delete;
  using ::fbl::internal::RefCountedBase<EnableAdoptionValidator>::IsLastReference;

 protected:
  constexpr PageRefCounted() = default;
  ~PageRefCounted() = default;
};

class Page : public PageRefCounted<Page>,
             public fbl::Recyclable<Page>,
             public fbl::WAVLTreeContainable<Page *> {
 public:
  Page() = delete;
  Page(FileCache *file_cache, pgoff_t index);
  Page(const Page &) = delete;
  Page &operator=(const Page &) = delete;
  Page(const Page &&) = delete;
  Page &operator=(const Page &&) = delete;
  virtual ~Page();

  void fbl_recycle() { RecyclePage(); }

  pgoff_t GetKey() const { return index_; }
  pgoff_t GetIndex() const { return GetKey(); }
  VnodeF2fs &GetVnode() const;
  FileCache &GetFileCache() const;
  // A caller is allowed to access |this| via address_ after GetPage().
  // Calling it ensures that VmoManager creates and maintains a vmo called VmoNode that
  // |this| will use. When VmoManager does not have the corresponding VmoNode, it creates
  // a discardable vmo and tracks a reference count to the vmo.
  // The vmo keeps VMO_OP_LOCK as long as any corresponding RefPtr<Page> exists. The mapping
  // also keeps with its vmo.
  zx_status_t GetPage();
  zx_status_t VmoOpUnlock(bool evict = false);
  zx::status<bool> VmoOpLock();
  template <typename T = void>
  T *GetAddress() const {
    ZX_DEBUG_ASSERT(IsMapped());
    return reinterpret_cast<T *>(address_);
  }

  bool IsUptodate() const { return TestFlag(PageFlag::kPageUptodate); }
  bool IsDirty() const { return TestFlag(PageFlag::kPageDirty); }
  bool IsWriteback() const { return TestFlag(PageFlag::kPageWriteback); }
  bool IsLocked() const { return TestFlag(PageFlag::kPageLocked); }
  bool IsVmoLocked() const { return TestFlag(PageFlag::kPageVmoLocked); }
  bool IsMapped() const { return TestFlag(PageFlag::kPageMapped); }
  bool IsActive() const { return TestFlag(PageFlag::kPageActive); }
  bool IsMmapped() const { return TestFlag(PageFlag::kPageMmapped); }

  void ClearMapped() { ClearFlag(PageFlag::kPageMapped); }

  // Each Setxxx() method atomically sets a flag and returns the previous value.
  // It is called when the first reference is made.
  bool SetActive() { return SetFlag(PageFlag::kPageActive); }
  // It is called after the last reference is destroyed in FileCache::Downgrade().
  void ClearActive() { ClearFlag(PageFlag::kPageActive); }

  void Lock() {
    while (flags_[static_cast<uint8_t>(PageFlag::kPageLocked)].test_and_set(
        std::memory_order_acquire)) {
      flags_[static_cast<uint8_t>(PageFlag::kPageLocked)].wait(true, std::memory_order_relaxed);
    }
  }
  bool TryLock() {
    if (!flags_[static_cast<uint8_t>(PageFlag::kPageLocked)].test_and_set(
            std::memory_order_acquire)) {
      return false;
    }
    return true;
  }
  void Unlock() {
    if (IsLocked()) {
      ClearFlag(PageFlag::kPageLocked);
      WakeupFlag(PageFlag::kPageLocked);
    }
  }

  // It ensures that |this| is written to disk if IsDirty() is true.
  void WaitOnWriteback();
  bool SetWriteback();
  void ClearWriteback();

  bool SetUptodate();
  void ClearUptodate();

  bool SetDirty();
  bool ClearDirtyForIo();

  // It ensures that the contents of |this| is synchronized with the corresponding pager backed vmo.
  void SetMmapped();
  bool ClearMmapped();

  // It invalidates |this| for truncate and punch-a-hole operations.
  // It clears PageFlag::kPageUptodate and PageFlag::kPageDirty. If a caller invalidates
  // |this| that is under writeback, writeback keeps going. So, it is recommended to invalidate
  // its block address in a dnode or nat entry first.
  void Invalidate();

  void ZeroUserSegment(uint64_t start, uint64_t end) {
    if (start < end && end <= BlockSize()) {
      std::memset(GetAddress<uint8_t>() + start, 0, end - start);
    }
  }

  uint32_t BlockSize() const { return kPageSize; }

 protected:
  // It notifies VmoManager that there is no reference to |this|.
  void RecyclePage();

 private:
  zx_status_t Map();
  void WaitOnFlag(PageFlag flag) {
    while (flags_[static_cast<uint8_t>(flag)].test(std::memory_order_acquire)) {
      flags_[static_cast<uint8_t>(flag)].wait(true, std::memory_order_relaxed);
    }
  }
  bool TestFlag(PageFlag flag) const {
    return flags_[static_cast<uint8_t>(flag)].test(std::memory_order_acquire);
  }
  void ClearFlag(PageFlag flag) {
    flags_[static_cast<uint8_t>(flag)].clear(std::memory_order_relaxed);
  }
  void WakeupFlag(PageFlag flag) { flags_[static_cast<uint8_t>(flag)].notify_all(); }
  bool SetFlag(PageFlag flag) {
    return flags_[static_cast<uint8_t>(flag)].test_and_set(std::memory_order_acquire);
  }

  // After a successful call to GetPage(), it has a valid mapping and virtual address
  // through which a user can access to the vmo. It is valid only when IsMapped() returns true.
  zx_vaddr_t address_ = 0;
  // It is used to track the status of a page by using PageFlag
  std::array<std::atomic_flag, static_cast<uint8_t>(PageFlag::kPageFlagSize)> flags_ = {
      ATOMIC_FLAG_INIT};
#ifndef __Fuchsia__
  FsBlock blk_;
#endif  // __Fuchsia__
  // It indicates FileCache to which |this| belongs.
  FileCache *file_cache_ = nullptr;
  // It is used as the key of |this| in a lookup table (i.e., FileCache::page_tree_).
  // It indicates different information according to the type of FileCache::vnode_ such as file,
  // node, and meta vnodes. For file vnodes, it has file offset. For node vnodes, it indicates the
  // node id. For meta vnode, it points to the block address to which the metadata is written.
  const pgoff_t index_;

 protected:
  F2fs *fs_ = nullptr;
};

// LockedPage is a wrapper class for f2fs::Page lock management.
// When LockedPage holds "fbl::RefPtr<Page> page" and the page is not nullptr, it guarantees that
// the page is locked.
//
// The syntax looks something like...
// fbl::RefPtr<Page> unlocked_page;
// {
//   LockedPage locked_page(unlocked_page);
//   do something requiring page lock...
// }
//
// When Page is used as a function parameter, you should use `Page&` type for unlocked page, and use
// `LockedPage&` type for locked page.
class LockedPage final {
 public:
  LockedPage() : page_(nullptr) {}

  LockedPage(const LockedPage &) = delete;
  LockedPage &operator=(const LockedPage &) = delete;

  LockedPage(LockedPage &&p) {
    page_ = std::move(p.page_);
    p.page_ = nullptr;
  }
  LockedPage &operator=(LockedPage &&p) {
    reset();
    page_ = std::move(p.page_);
    p.page_ = nullptr;
    return *this;
  }

  LockedPage(fbl::RefPtr<Page> page) {
    page_ = page;
    page_->Lock();
  }

  ~LockedPage() { reset(); }

  void reset() {
    if (page_ != nullptr) {
      ZX_DEBUG_ASSERT(page_->IsLocked());
      page_->Unlock();
      page_.reset();
    }
  }

  // release() returns the unlocked page without changing its ref_count.
  // After release() is called, the LockedPage instance no longer has the ownership of the Page.
  // Therefore, the LockedPage instance should no longer be referenced.
  fbl::RefPtr<Page> release() {
    if (page_ != nullptr) {
      page_->Unlock();
    }
    return fbl::RefPtr<Page>(std::move(page_));
  }

  // CopyRefPtr() returns copied RefPtr, so that increases ref_count of page.
  // The page remains locked, and still managed by the LockedPage instance.
  fbl::RefPtr<Page> CopyRefPtr() { return fbl::RefPtr<Page>(page_); }

  template <typename T = Page>
  T &GetPage() {
    return static_cast<T &>(*page_);
  }

  Page *get() { return page_.get(); }
  Page &operator*() { return *page_; }
  Page *operator->() { return page_.get(); }
  explicit operator bool() const { return bool(page_); }

  // Comparison against nullptr operators (of the form, myptr == nullptr).
  bool operator==(decltype(nullptr)) const { return (page_ == nullptr); }
  bool operator!=(decltype(nullptr)) const { return (page_ != nullptr); }

 private:
  fbl::RefPtr<Page> page_ = nullptr;
};

class FileCache {
 public:
#ifdef __Fuchsia__
  FileCache(VnodeF2fs *vnode, VmoManager *vmo_manager);
#else   // __Fuchsia__
  FileCache(VnodeF2fs *vnode);
#endif  // __Fuchsia__
  FileCache() = delete;
  FileCache(const FileCache &) = delete;
  FileCache &operator=(const FileCache &) = delete;
  FileCache(const FileCache &&) = delete;
  FileCache &operator=(const FileCache &&) = delete;
  ~FileCache();

  // It returns a locked Page corresponding to |index| from |page_tree_|.
  // If there is no Page, it creates and returns a locked Page.
  zx_status_t GetPage(const pgoff_t index, LockedPage *out) __TA_EXCLUDES(tree_lock_);
  // It returns an unlocked Page corresponding to |index| from |page_tree|.
  // If it fails to find the Page in |page_tree_|, it returns ZX_ERR_NOT_FOUND.
  zx_status_t FindPage(const pgoff_t index, fbl::RefPtr<Page> *out) __TA_EXCLUDES(tree_lock_);
  // It tries to write out dirty Pages that meets |operation| in |page_tree_|.
  pgoff_t Writeback(WritebackOperation &operation) __TA_EXCLUDES(tree_lock_);
  // It invalidates Pages within the range of |start| to |end| in |page_tree_|.
  std::vector<LockedPage> InvalidatePages(pgoff_t start, pgoff_t end) __TA_EXCLUDES(tree_lock_);
  // It removes all Pages from |page_tree_|. It should be called when no one can get access to
  // |vnode_|. (e.g., fbl_recycle()) It assumes that all active Pages are under writeback.
  void Reset() __TA_EXCLUDES(tree_lock_);
  VnodeF2fs &GetVnode() const { return *vnode_; }
  // Only Page::RecyclePage() is allowed to call it.
  void Downgrade(Page *raw_page) __TA_EXCLUDES(tree_lock_);
#ifdef __Fuchsia__
  VmoManager &GetVmoManager() { return *vmo_manager_; }
#endif  // __Fuchsia__

 private:
  // It returns a set of locked dirty Pages that meet |operation|.
  std::vector<LockedPage> GetLockedDirtyPagesUnsafe(const WritebackOperation &operation)
      __TA_REQUIRES(tree_lock_);
  zx::status<bool> GetPageUnsafe(const pgoff_t index, fbl::RefPtr<Page> *out)
      __TA_REQUIRES(tree_lock_);
  zx_status_t AddPageUnsafe(const fbl::RefPtr<Page> &page) __TA_REQUIRES(tree_lock_);
  zx_status_t EvictUnsafe(Page *page) __TA_REQUIRES(tree_lock_);
  std::vector<LockedPage> GetLockedPagesUnsafe(pgoff_t start = 0, pgoff_t end = kPgOffMax)
      __TA_REQUIRES(tree_lock_);
  // It evicts all Pages within the range of |start| to |end| and returns them locked.
  // When a caller resets returned Pages after doing some necessary work, they will be deleted.
  std::vector<LockedPage> CleanupPagesUnsafe(pgoff_t start = 0, pgoff_t end = kPgOffMax)
      __TA_REQUIRES(tree_lock_);

  using PageTreeTraits = fbl::DefaultKeyedObjectTraits<pgoff_t, Page>;
  using PageTree = fbl::WAVLTree<pgoff_t, Page *, PageTreeTraits>;

  fs::SharedMutex tree_lock_;
  std::condition_variable_any recycle_cvar_;
  PageTree page_tree_ __TA_GUARDED(tree_lock_);
  VnodeF2fs *vnode_;
#ifdef __Fuchsia__
  VmoManager *vmo_manager_;
#endif  // __Fuchsia__
};

}  // namespace f2fs

#endif  // SRC_STORAGE_F2FS_FILE_CACHE_H_
