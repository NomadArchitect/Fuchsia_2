// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STORAGE_F2FS_VNODE_H_
#define SRC_STORAGE_F2FS_VNODE_H_

namespace f2fs {
constexpr uint32_t kNullIno = std::numeric_limits<uint32_t>::max();

class F2fs;
// for in-memory extent cache entry
struct ExtentInfo {
  fs::SharedMutex ext_lock;  // rwlock for consistency
  uint64_t fofs = 0;         // start offset in a file
  uint32_t blk_addr = 0;     // start block address of the extent
  uint32_t len = 0;          // lenth of the extent
};

// i_advise uses Fadvise:xxx bit. We can add additional hints later.
enum class FAdvise {
  kCold = 1,
};

struct InodeInfo {
  uint32_t i_flags = 0;          // keep an inode flags for ioctl
  uint8_t i_advise = 0;          // use to give file attribute hints
  uint8_t i_dir_level = 0;       // use for dentry level for large dir
  uint16_t i_extra_isize = 0;    // size of extra space located in i_addr
  uint64_t i_current_depth = 0;  // use only in directory structure
  umode_t i_acl_mode = 0;        // keep file acl mode temporarily

  uint32_t flags = 0;         // use to pass per-file flags
  uint64_t data_version = 0;  // lastest version of data for fsync
  atomic_t dirty_dents = 0;   // # of dirty dentry pages
  f2fs_hash_t chash;          // hash value of given file name
  uint64_t clevel = 0;        // maximum level of given file name
  nid_t i_xattr_nid = 0;      // node id that contains xattrs
  ExtentInfo ext;             // in-memory extent cache entry
};

class VnodeF2fs : public fs::Vnode,
                  public fbl::Recyclable<VnodeF2fs>,
                  public fbl::WAVLTreeContainable<VnodeF2fs *>,
                  public fbl::DoublyLinkedListable<fbl::RefPtr<VnodeF2fs>> {
 public:
  explicit VnodeF2fs(F2fs *fs, ino_t ino);

  uint32_t InlineDataOffset() const {
    return kPageCacheSize - sizeof(NodeFooter) -
           sizeof(uint32_t) * (kAddrsPerInode + kNidsPerInode - 1) + GetExtraISize();
  }
  uint32_t MaxInlineData() const {
    return sizeof(uint32_t) *
           (kAddrsPerInode - GetExtraISize() / sizeof(uint32_t) - kInlineXattrAddrs - 1);
  }

  static void Allocate(F2fs *fs, ino_t ino, uint32_t mode, fbl::RefPtr<VnodeF2fs> *out);
  static zx_status_t Create(F2fs *fs, ino_t ino, fbl::RefPtr<VnodeF2fs> *out);
  void Init();

  ino_t GetKey() const { return ino_; }

#ifdef __Fuchsia__
  void Sync(SyncCallback closure) final;
#endif  // __Fuchsia__
  zx_status_t SyncFile(loff_t start, loff_t end, int datasync);
  int NeedToSyncDir();

#ifdef __Fuchsia__
  zx_status_t QueryFilesystem(fuchsia_io_admin::wire::FilesystemInfo *info) final;
#endif  // __Fuchsia__

  void fbl_recycle() { RecycleNode(); };

  F2fs *Vfs() __TA_EXCLUDES(mutex_) {
    fs::SharedLock lock(mutex_);
    return reinterpret_cast<F2fs *>(vfs());
  }
  ino_t Ino() const { return ino_; }

  zx_status_t GetAttributes(fs::VnodeAttributes *a) final __TA_EXCLUDES(mutex_);
  zx_status_t SetAttributes(fs::VnodeAttributesUpdate attr) final __TA_EXCLUDES(mutex_);

#ifdef __Fuchsia__
  zx_status_t GetNodeInfoForProtocol([[maybe_unused]] fs::VnodeProtocol protocol,
                                     [[maybe_unused]] fs::Rights rights,
                                     fs::VnodeRepresentation *info) final;
#endif  // __Fuchsia__

  fs::VnodeProtocolSet GetProtocols() const final;

#if 0  // porting needed
  // void F2fsSetInodeFlags();
  // int F2fsIgetTest(void *data);
  // VnodeF2fs *F2fsIgetNowait(uint64_t ino);
  // static int CheckExtentCache(inode *inode, pgoff_t pgofs,
  //        buffer_head *bh_result);
  // static int GetDataBlockRo(inode *inode, sector_t iblock,
  //      buffer_head *bh_result, int create);
  // static int F2fsReadDataPage(file *file, page *page);
  // static int F2fsReadDataPages(file *file,
  //       address_space *mapping,
  //       list_head *pages, unsigned nr_pages);
  // int F2fsWriteDataPages(/*address_space *mapping,*/
  //                        WritebackControl *wbc);
  // ssize_t F2fsDirectIO(/*int rw, kiocb *iocb,
  //   const iovec *iov, */
  //                      loff_t offset, uint64_t nr_segs);
  //   [[maybe_unused]] static void F2fsInvalidateDataPage(Page *page, uint64_t offset);
  //   [[maybe_unused]] static int F2fsReleaseDataPage(Page *page, gfp_t wait);
  // int F2fsSetDataPageDirty(Page *page);
#endif

  static zx_status_t Vget(F2fs *fs, ino_t ino, fbl::RefPtr<VnodeF2fs> *out);
  void UpdateInode(Page *node_page);
  zx_status_t WriteInode(WritebackControl *wbc);
  zx_status_t DoTruncate(size_t len);
  int TruncateDataBlocksRange(DnodeOfData *dn, int count);
  void TruncateDataBlocks(DnodeOfData *dn);
  void TruncatePartialDataPage(uint64_t from);
  zx_status_t TruncateBlocks(uint64_t from);
  zx_status_t TruncateHole(pgoff_t pg_start, pgoff_t pg_end);
  void TruncateToSize();
  void EvictVnode();

  void SetDataBlkaddr(DnodeOfData *dn, block_t new_addr);
  zx_status_t ReserveNewBlock(DnodeOfData *dn);

  void UpdateExtentCache(block_t blk_addr, DnodeOfData *dn);
  zx_status_t FindDataPage(pgoff_t index, Page **out);
  zx_status_t GetLockDataPage(pgoff_t index, Page **out);
  zx_status_t GetNewDataPage(pgoff_t index, bool new_i_size, Page **out);

  static zx_status_t Readpage(F2fs *fs, Page *page, block_t blk_addr, int type);
  zx_status_t DoWriteDataPage(Page *page);
  zx_status_t WriteDataPageReq(Page *page, WritebackControl *wbc);
  zx_status_t WriteBegin(size_t pos, size_t len, Page **page);

#ifdef __Fuchsia__
  void Notify(std::string_view name, unsigned event) final;
  zx_status_t WatchDir(fs::Vfs *vfs, uint32_t mask, uint32_t options, zx::channel watcher) final;
#endif  // __Fuchsia__

  void MarkInodeDirty() __TA_EXCLUDES(mutex_);

  void GetExtentInfo(const Extent &i_ext);
  void SetRawExtent(Extent &i_ext);

#ifdef __Fuchsia__
  void IncNlink() __TA_EXCLUDES(mutex_) {
    std::lock_guard lock(mutex_);
    ++nlink_;
  }

  void DropNlink() __TA_EXCLUDES(mutex_) {
    std::lock_guard lock(mutex_);
    --nlink_;
  }

  void ClearNlink() __TA_EXCLUDES(mutex_) {
    std::lock_guard lock(mutex_);
    nlink_ = 0;
  }

  void SetNlink(const uint32_t &nlink) __TA_EXCLUDES(mutex_) {
    std::lock_guard lock(mutex_);
    nlink_ = nlink;
  }

  uint32_t GetNlink() const __TA_EXCLUDES(mutex_) {
    fs::SharedLock lock(mutex_);
    return nlink_;
  }
#else   // __Fuchsia__
  void IncNlink() { ++nlink_; }

  void DropNlink() { --nlink_; }

  void ClearNlink() { nlink_ = 0; }

  void SetNlink(const uint32_t &nlink) { nlink_ = nlink; }

  uint32_t GetNlink() const { return nlink_; }
#endif  // __Fuchsia__

  void SetMode(const umode_t &mode);
  umode_t GetMode() const;
  bool IsDir() const;
  bool IsReg() const;
  bool IsLink() const;
  bool IsChr() const;
  bool IsBlk() const;
  bool IsSock() const;
  bool IsFifo() const;
  bool HasGid() const;

  void SetName(const std::string_view &name) { name_ = name; }
  bool IsSameName(const std::string_view &name) const {
    return (name_.GetStringView().compare(name) == 0);
  }
  std::string_view GetNameView() const { return name_.GetStringView(); }
  uint32_t GetNameLen() const { return name_.GetLen(); }
  const char *GetName() { return name_.GetData(); }

  // stat_lock
  uint64_t GetBlockCount() const { return (size_ + kBlockSize - 1) / kBlockSize; }
  void IncBlocks(const block_t &nblocks) { blocks_ += nblocks; }
  void DecBlocks(const block_t &nblocks) {
    ZX_ASSERT(blocks_ >= nblocks);
    blocks_ -= nblocks;
  }
  void InitBlocks() { blocks_ = 0; }
  uint64_t GetBlocks() const { return blocks_; }
  void SetBlocks(const uint64_t &blocks) { blocks_ = blocks; }
  bool HasBlocks() const {
    // TODO: Need to consider i_xattr_nid
    return (GetBlocks() > kDefaultAllocatedBlocks);
  }

#ifdef __Fuchsia__
  void SetSize(const uint64_t &nbytes) __TA_EXCLUDES(mutex_) {
    std::lock_guard lock(mutex_);
    size_ = nbytes;
  }

  void InitSize() __TA_EXCLUDES(mutex_) {
    std::lock_guard lock(mutex_);
    size_ = 0;
  }

  uint64_t GetSize() const __TA_EXCLUDES(mutex_) {
    fs::SharedLock lock(mutex_);
    return size_;
  }
#else   // __Fuchsia__
  void SetSize(const uint64_t &nbytes) { size_ = nbytes; }

  void InitSize() { size_ = 0; }

  uint64_t GetSize() const { return size_; }
#endif  // __Fuchsia__

  void SetParentNid(const ino_t &pino) { parent_ino_ = pino; };
  ino_t GetParentNid() const { return parent_ino_; };

  void SetGeneration(const uint32_t &gen) { generation_ = gen; }
  uint32_t GetGeneration() const { return generation_; }

  void SetUid(const uid_t &uid) { uid_ = uid; }
  uid_t GetUid() const { return uid_; }

  void SetGid(const gid_t &gid) { gid_ = gid; }
  gid_t GetGid() const { return gid_; }

  timespec GetATime() const { return atime_; }
  void SetATime(const timespec &time) { atime_ = time; }
  void SetATime(const uint64_t &sec, const uint32_t &nsec) {
    atime_.tv_sec = sec;
    atime_.tv_nsec = nsec;
  }

  timespec GetMTime() const { return mtime_; }
  void SetMTime(const timespec &time) { mtime_ = time; }
  void SetMTime(const uint64_t &sec, const uint32_t &nsec) {
    mtime_.tv_sec = sec;
    mtime_.tv_nsec = nsec;
  }

  timespec GetCTime() const { return ctime_; }
  void SetCTime(const timespec &time) { ctime_ = time; }
  void SetCTime(const uint64_t &sec, const uint32_t &nsec) {
    ctime_.tv_sec = sec;
    ctime_.tv_nsec = nsec;
  }

  void SetInodeFlags(const uint32_t &flags) { fi_.i_flags = flags; }
  uint32_t GetInodeFlags() const { return fi_.i_flags; }

#ifdef __Fuchsia__
  bool SetFlag(const InodeInfoFlag &flag) __TA_EXCLUDES(mutex_) {
    std::lock_guard lock(mutex_);
    return TestAndSetBit(static_cast<int>(flag), &fi_.flags);
  }
  bool ClearFlag(const InodeInfoFlag &flag) __TA_EXCLUDES(mutex_) {
    std::lock_guard lock(mutex_);
    return TestAndClearBit(static_cast<int>(flag), &fi_.flags);
  }
  bool TestFlag(const InodeInfoFlag &flag) __TA_EXCLUDES(mutex_) {
    fs::SharedLock lock(mutex_);
    return TestBit(static_cast<int>(flag), &fi_.flags);
  }
#else   // __Fuchsia__
  bool SetFlag(const InodeInfoFlag &flag) {
    return TestAndSetBit(static_cast<int>(flag), &fi_.flags);
  }
  bool ClearFlag(const InodeInfoFlag &flag) {
    return TestAndClearBit(static_cast<int>(flag), &fi_.flags);
  }
  bool TestFlag(const InodeInfoFlag &flag) { return TestBit(static_cast<int>(flag), &fi_.flags); }
#endif  // __Fuchsia__

  void ClearAdvise(const FAdvise &bit) { ClearBit(static_cast<int>(bit), &fi_.i_advise); }
  void SetAdvise(const FAdvise &bit) { SetBit(static_cast<int>(bit), &fi_.i_advise); }
  uint8_t GetAdvise() const { return fi_.i_advise; }
  void SetAdvise(const uint8_t &bits) { fi_.i_advise = bits; }
  int IsAdviseSet(const FAdvise &bit) { return TestBit(static_cast<int>(bit), &fi_.i_advise); }

  uint64_t GetDirHashLevel() const { return fi_.clevel; }
  bool IsSameDirHash(const f2fs_hash_t &hash) const { return (fi_.chash == hash); }
  void ClearDirHash() { fi_.chash = 0; }
  void SetDirHash(const f2fs_hash_t &hash, const uint64_t &level) {
    fi_.chash = hash;
    fi_.clevel = level;
  }

  void AddDirtyDentry() {
    // TODO: enable it when impl page cache
    // atomic_fetch_add_explicit(&fi_.dirty_dents, 1, std::memory_order_relaxed);
  }

  void RemoveDirtyDentry() {
    // TODO: enable it when impl page cache
    // atomic_fetch_sub_explicit(&fi_.dirty_dents, 1, std::memory_order_relaxed);
  }

  uint8_t GetDirLevel() const { return fi_.i_dir_level; }
  void SetDirLevel(const uint8_t level) { fi_.i_dir_level = level; }

  uint64_t GetCurDirDepth() const { return fi_.i_current_depth; }
  void SetCurDirDepth(const uint64_t depth) { fi_.i_current_depth = depth; }

  nid_t GetXattrNid() const { return fi_.i_xattr_nid; }
  void SetXattrNid(const nid_t nid) { fi_.i_xattr_nid = nid; }
  void ClearXattrNid() { fi_.i_xattr_nid = 0; }

  uint16_t GetExtraISize() const { return fi_.i_extra_isize; }
  void SetExtraISize(const uint16_t size) { fi_.i_extra_isize = size; }

  bool IsBad() { return TestFlag(InodeInfoFlag::kBad); }

  void Activate() __TA_EXCLUDES(mutex_) { SetFlag(InodeInfoFlag::kActive); }

  void Deactivate() __TA_EXCLUDES(mutex_) {
    ClearFlag(InodeInfoFlag::kActive);
    flag_cvar_.notify_all();
  }

  bool IsActive() __TA_EXCLUDES(mutex_) { return TestFlag(InodeInfoFlag::kActive); }

  bool WaitForDeactive(fs::SharedMutex &mutex) __TA_REQUIRES_SHARED(mutex) {
    if (IsActive()) {
      flag_cvar_.wait(mutex, [this]() {
        return (TestBit(static_cast<int>(InodeInfoFlag::kActive), &fi_.flags) == 0);
      });
      return true;
    }
    return false;
  }

  bool ClearDirty() __TA_EXCLUDES(mutex_) { return ClearFlag(InodeInfoFlag::kDirty); }

  bool IsDirty() __TA_EXCLUDES(mutex_) { return TestFlag(InodeInfoFlag::kDirty); }

  bool ShouldFlush() __TA_EXCLUDES(mutex_) {
    if (!GetNlink() || !IsDirty() || IsBad()) {
      return false;
    }
    return true;
  }

  void WaitForInit() __TA_EXCLUDES(mutex_) {
    fs::SharedLock lock(mutex_);
    if (TestBit(static_cast<int>(InodeInfoFlag::kInit), &fi_.flags)) {
      flag_cvar_.wait(
          mutex_, [this]() __TA_EXCLUDES(mutex_) { return (TestFlag(InodeInfoFlag::kInit) == 0); });
    }
  }

  void UnlockNewInode() __TA_EXCLUDES(mutex_) {
    ClearFlag(InodeInfoFlag::kInit);
    flag_cvar_.notify_all();
  }

 protected:
  void RecycleNode() override;
  std::condition_variable_any flag_cvar_{};
  fs::SharedMutex io_lock_;

 private:
  zx_status_t OpenNode(ValidatedOptions options, fbl::RefPtr<Vnode> *out_redirect) final
      __TA_EXCLUDES(mutex_);
  zx_status_t CloseNode() final;

  InodeInfo fi_;
  uid_t uid_ = 0;
  gid_t gid_ = 0;
  uint64_t size_ = 0;
  uint64_t blocks_ = 0;
  uint32_t nlink_ = 0;
  uint32_t generation_ = 0;
  umode_t mode_ = 0;
  NameString name_;
  ino_t parent_ino_{kNullIno};
  timespec atime_ = {0, 0};
  timespec mtime_ = {0, 0};
  timespec ctime_ = {0, 0};
  ino_t ino_ = 0;
#ifdef __Fuchsia__
  fs::WatcherContainer watcher_{};
#endif  // __Fuchsia__
};

}  // namespace f2fs

#endif  // SRC_STORAGE_F2FS_VNODE_H_
