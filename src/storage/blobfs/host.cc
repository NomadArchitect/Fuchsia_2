// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/storage/blobfs/host.h"

#include <fcntl.h>
#include <inttypes.h>
#include <lib/cksum.h>
#include <lib/syslog/cpp/macros.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/types.h>
#include <unistd.h>
#include <zircon/assert.h>
#include <zircon/status.h>

#include <cstddef>
#include <cstdint>
#include <memory>
#include <new>
#include <optional>
#include <string>
#include <utility>

#include <fbl/algorithm.h>
#include <fbl/array.h>
#include <fbl/macros.h>
#include <fs-host/common.h>
#include <safemath/checked_math.h>
#include <safemath/safe_conversions.h>

#include "src/lib/digest/digest.h"
#include "src/lib/digest/merkle-tree.h"
#include "src/lib/digest/node-digest.h"
#include "src/lib/storage/vfs/cpp/journal/initializer.h"
#include "src/lib/storage/vfs/cpp/transaction/transaction_handler.h"
#include "src/storage/blobfs/blob_layout.h"
#include "src/storage/blobfs/common.h"
#include "src/storage/blobfs/compression/chunked.h"
#include "src/storage/blobfs/compression/compressor.h"
#include "src/storage/blobfs/compression/decompressor.h"
#include "src/storage/blobfs/compression_settings.h"
#include "src/storage/blobfs/format.h"
#include "src/storage/blobfs/fsck_host.h"

using digest::Digest;
using digest::MerkleTreeCreator;
using digest::MerkleTreeVerifier;

constexpr uint32_t kExtentCount = 5;

namespace blobfs {
namespace {

// TODO(markdittmer): Abstract choice of host compressor, decompressor and metadata flag to support
// choosing from multiple strategies. This has already been done in non-host code but host tools do
// not use |BlobCompressor| the same way.
using HostCompressor = ChunkedCompressor;
using HostDecompressor = ChunkedDecompressor;

constexpr CompressionSettings kCompressionSettings = {
    .compression_algorithm = CompressionAlgorithm::kChunked,
};

zx_status_t ReadBlockOffset(int fd, uint64_t bno, off_t offset, void* data) {
  off_t off = offset + bno * kBlobfsBlockSize;
  if (pread(fd, data, kBlobfsBlockSize, off) != kBlobfsBlockSize) {
    FX_LOGS(ERROR) << "cannot read block " << bno;
    return ZX_ERR_IO;
  }
  return ZX_OK;
}

zx_status_t WriteBlockOffset(int fd, const void* data, uint64_t block_count, off_t offset,
                             uint64_t block_number) {
  off_t off = safemath::checked_cast<off_t>(
      safemath::CheckAdd(offset, safemath::CheckMul(block_number, kBlobfsBlockSize).ValueOrDie())
          .ValueOrDie());
  size_t size = safemath::CheckMul(block_count, kBlobfsBlockSize).ValueOrDie();
  ssize_t ret;
  auto udata = static_cast<const uint8_t*>(data);
  while (size > 0) {
    ret = pwrite(fd, udata, size, off);
    if (ret < 0) {
      perror("failed write");
      FX_LOGS(ERROR) << "cannot write block " << block_number << " size:" << size << " off:" << off;
      return ZX_ERR_IO;
    }
    size -= ret;
    off += ret;
    udata += ret;
  }
  return ZX_OK;
}

// From a buffer, create a merkle tree.
//
// Given a mapped blob at |blob_data| of length |length|, compute the Merkle digest and the output
// merkle tree as a uint8_t array.
zx_status_t buffer_create_merkle(const FileMapping& mapping, bool use_compact_format,
                                 MerkleInfo* out_info) {
  zx_status_t status;
  MerkleTreeCreator mtc;
  mtc.SetUseCompactFormat(use_compact_format);
  if ((status = mtc.SetDataLength(mapping.length())) != ZX_OK) {
    return status;
  }
  std::unique_ptr<uint8_t[]> merkle_tree;
  size_t merkle_length = mtc.GetTreeLength();
  if (merkle_length > 0) {
    merkle_tree.reset(new uint8_t[merkle_length]);
  }
  uint8_t root[digest::kSha256Length];
  if ((status = mtc.SetTree(merkle_tree.get(), merkle_length, root, digest::kSha256Length)) !=
      ZX_OK) {
    return status;
  }
  if ((status = mtc.Append(mapping.data(), mapping.length())) != ZX_OK) {
    return status;
  }
  out_info->digest = root;
  out_info->merkle = std::move(merkle_tree);
  out_info->merkle_length = merkle_length;
  out_info->length = mapping.length();
  return ZX_OK;
}

zx_status_t buffer_compress(const FileMapping& mapping, MerkleInfo* out_info) {
  size_t max = HostCompressor::BufferMax(mapping.length());
  out_info->compressed_data.reset(new uint8_t[max]);
  out_info->compressed = false;

  if (mapping.length() < kCompressionSizeThresholdBytes) {
    return ZX_OK;
  }

  zx_status_t status;
  std::unique_ptr<HostCompressor> compressor;
  size_t output_limit;
  if ((status = HostCompressor::Create(kCompressionSettings, mapping.length(), &output_limit,
                                       &compressor)) != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to initialize blobfs compressor: " << status;
    return status;
  }
  if ((status = compressor->SetOutput(out_info->compressed_data.get(), max)) != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to initialize blobfs compressor: " << status;
    return status;
  }

  if ((status = compressor->Update(mapping.data(), mapping.length())) != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to update blobfs compressor: " << status;
    return status;
  }

  if ((status = compressor->End()) != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to complete blobfs compressor: " << status;
    return status;
  }

  if (fbl::round_up(compressor->Size(), kBlobfsBlockSize) <
      fbl::round_up(mapping.length(), kBlobfsBlockSize)) {
    out_info->compressed_length = compressor->Size();
    out_info->compressed = true;
  }

  return ZX_OK;
}

// Given a buffer (and pre-computed merkle tree), add the buffer as a blob in Blobfs.
zx_status_t blobfs_add_mapped_blob_with_merkle(Blobfs* bs, JsonRecorder* json_recorder,
                                               const FileMapping& mapping, const MerkleInfo& info) {
  ZX_ASSERT(mapping.length() == info.length);
  const void* data;

  if (info.compressed) {
    data = info.compressed_data.get();
  } else {
    data = mapping.data();
  }

  auto blob_layout = BlobLayout::CreateFromSizes(GetBlobLayoutFormat(bs->Info()), info.length,
                                                 info.GetDataSize(), bs->GetBlockSize());
  if (blob_layout.is_error()) {
    FX_LOGS(ERROR) << "Failed to create blob layout: " << blob_layout.status_value();
    return blob_layout.status_value();
  }

  // After we've pre-calculated all necessary information, actually add the blob to the filesystem
  // itself.
  static std::mutex add_blob_mutex_;
  std::lock_guard lock(add_blob_mutex_);
  std::unique_ptr<InodeBlock> inode_block;
  zx_status_t status;
  if ((status = bs->NewBlob(info.digest, &inode_block)) != ZX_OK) {
    FX_LOGS(ERROR) << "error: Failed to allocate a new blob " << status;
    return status;
  }
  if (inode_block == nullptr) {
    FX_LOGS(ERROR) << "error: No nodes available on blobfs image";
    return ZX_ERR_NO_RESOURCES;
  }

  Inode* inode = inode_block->GetInode();
  inode->blob_size = mapping.length();
  inode->block_count = blob_layout->TotalBlockCount();
  inode->header.flags |=
      kBlobFlagAllocated | (info.compressed ? HostCompressor::InodeHeaderCompressionFlags() : 0);

  // TODO(fxbug.rev/74008) Currently, host-side tools can only generate single-extent blobs. This
  // should be fixed.
  if (inode->block_count > kBlockCountMax) {
    FX_LOGS(ERROR) << "error: Blobs larger than " << kBlockCountMax
                   << " blocks not yet implemented";
    return ZX_ERR_NOT_SUPPORTED;
  }

  size_t start_block = 0;
  if ((status = bs->AllocateBlocks(inode->block_count, &start_block)) != ZX_OK) {
    FX_LOGS(ERROR) << "error: No blocks available " << inode->block_count;
    return status;
  }

  // TODO(fxbug.rev/74008): This is hardcoded alongside the check against "kBlockCountMax" above.
  if (inode->block_count > 0) {
    inode->extents[0].SetStart(start_block);
    inode->extents[0].SetLength(static_cast<BlockCountType>(inode->block_count));
    inode->extent_count = 1;
  } else {
    inode->extent_count = 0;
  }

  if (json_recorder) {
    json_recorder->Append(info.path.c_str(), info.digest.ToString().c_str(), info.length,
                          kBlobfsBlockSize * inode->block_count);
  }

  if ((status = bs->WriteData(inode, info.merkle.get(), data, *blob_layout.value())) != ZX_OK) {
    FX_LOGS(ERROR) << "Blobfs WriteData failed " << status;
    return status;
  }

  if ((status = bs->WriteBitmap(inode->block_count, inode->extents[0].Start())) != ZX_OK) {
    FX_LOGS(ERROR) << "Blobfs WriteBitmap failed " << status;
    return status;
  }
  if ((status = bs->WriteNode(std::move(inode_block))) != ZX_OK) {
    FX_LOGS(ERROR) << "Blobfs WriteNode failed " << status;
    return status;
  }
  if ((status = bs->WriteInfo()) != ZX_OK) {
    FX_LOGS(ERROR) << "Blobfs WriteInfo failed " << status;
    return status;
  }

  return ZX_OK;
}

// Returns ZX_OK and copies blobfs info_block_t, which is a block worth of data containing
// superblock, into |out_info_block| if the block read from fd belongs to blobfs.
zx_status_t blobfs_load_info_block(const fbl::unique_fd& fd, info_block_t* out_info_block,
                                   off_t start = 0, std::optional<off_t> end = std::nullopt) {
  info_block_t info_block;

  if (ReadBlockOffset(fd.get(), 0, start, reinterpret_cast<void*>(info_block.block)) < 0) {
    return ZX_ERR_IO;
  }
  uint64_t blocks;
  zx_status_t status;
  if ((status = GetBlockCount(fd.get(), &blocks)) != ZX_OK) {
    FX_LOGS(ERROR) << "cannot find end of underlying device";
    return status;
  }

  if (end &&
      ((blocks * kBlobfsBlockSize) < safemath::checked_cast<uint64_t>(end.value() - start))) {
    FX_LOGS(ERROR) << "Invalid file size " << safemath::checked_cast<uint64_t>(end.value() - start);
    return ZX_ERR_BAD_STATE;
  }
  if ((status = CheckSuperblock(&info_block.info, blocks)) != ZX_OK) {
    FX_LOGS(ERROR) << "Info check failed " << status;
    return status;
  }

  memcpy(out_info_block, &info_block, sizeof(*out_info_block));

  return ZX_OK;
}

zx_status_t get_superblock(const fbl::unique_fd& fd, off_t start, std::optional<off_t> end,
                           Superblock* info) {
  info_block_t info_block;
  zx_status_t status;

  if ((status = blobfs_load_info_block(fd, &info_block, start, end)) != ZX_OK) {
    FX_LOGS(ERROR) << "Load of info block failed " << status;
    return status;
  }

  memcpy(info, &info_block.info, sizeof(info_block.info));
  return ZX_OK;
}

}  // namespace

zx_status_t ReadBlock(int fd, uint64_t bno, void* data) {
  off_t off = bno * kBlobfsBlockSize;
  if (pread(fd, data, kBlobfsBlockSize, off) != kBlobfsBlockSize) {
    FX_LOGS(ERROR) << "cannot read block " << bno;
    return ZX_ERR_IO;
  }
  return ZX_OK;
}

zx_status_t WriteBlocks(int fd, uint64_t block_offset, uint64_t block_count, const void* data) {
  if (WriteBlockOffset(fd, data, block_count, 0, block_offset) != ZX_OK) {
    FX_LOGS(ERROR) << "cannot write blocks: " << block_count
                   << " at block offset: " << block_offset;
    return ZX_ERR_IO;
  }
  return ZX_OK;
}

zx_status_t WriteBlock(int fd, uint64_t bno, const void* data) {
  return WriteBlocks(fd, bno, 1, data);
}

zx_status_t GetBlockCount(int fd, uint64_t* out) {
  struct stat s;
  if (fstat(fd, &s) < 0) {
    return ZX_ERR_BAD_STATE;
  }
  *out = s.st_size / kBlobfsBlockSize;
  return ZX_OK;
}

int Mkfs(int fd, uint64_t block_count, const FilesystemOptions& options) {
  Superblock info;
  InitializeSuperblock(block_count, options, &info);
  zx_status_t status = CheckSuperblock(&info, block_count);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to initialize superblock: " << status;
    return -1;
  }
  uint64_t block_bitmap_blocks = BlockMapBlocks(info);
  uint64_t node_map_blocks = NodeMapBlocks(info);

  RawBitmap block_bitmap;
  if (block_bitmap.Reset(block_bitmap_blocks * kBlobfsBlockBits)) {
    FX_LOGS(ERROR) << "Couldn't allocate blobfs block map";
    return -1;
  }
  if (block_bitmap.Shrink(info.data_block_count)) {
    FX_LOGS(ERROR) << "Couldn't shrink blobfs block map";
    return -1;
  }

  // Reserve first |kStartBlockMinimum| data blocks
  block_bitmap.Set(0, kStartBlockMinimum);

  // All in-memory structures have been created successfully. Dump everything to disk.
  // Initialize on-disk journal.
  fs::WriteBlocksFn write_blocks_fn = [fd, &info](fbl::Span<const uint8_t> buffer,
                                                  uint64_t block_offset, uint64_t block_count) {
    ZX_ASSERT((block_offset + block_count) <= JournalBlocks(info));
    ZX_ASSERT(buffer.size() >= (block_count * kBlobfsBlockSize));
    return WriteBlocks(fd, JournalStartBlock(info) + block_offset, block_count, buffer.data());
  };
  status = fs::MakeJournal(JournalBlocks(info), write_blocks_fn);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to write journal block";
    return -1;
  }

  // Write the root block to disk.
  static_assert(kBlobfsBlockSize == sizeof(info));
  if ((status = WriteBlock(fd, 0, &info)) != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to write Superblock";
    return -1;
  }

  // Write allocation bitmap to disk.
  if (WriteBlocks(fd, BlockMapStartBlock(info), block_bitmap_blocks,
                  block_bitmap.StorageUnsafe()->GetData()) != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to write blockmap block " << block_bitmap_blocks;
    return -1;
  }

  // Write node map to disk.
  size_t map_length = node_map_blocks * kBlobfsBlockSize;
  void* blocks = mmap(nullptr, map_length, PROT_READ, MAP_PRIVATE | MAP_ANON, -1, 0);
  if (blocks == MAP_FAILED) {
    FX_LOGS(ERROR) << "failed to map zeroes for inode map of size " << map_length;
    return -1;
  }
  if (WriteBlocks(fd, NodeMapStartBlock(info), node_map_blocks, blocks) != ZX_OK) {
    FX_LOGS(ERROR) << "failed writing inode map";
    munmap(blocks, map_length);
    return -1;
  }
  if (munmap(blocks, map_length) != 0) {
    FX_LOGS(ERROR) << "failed unmap inode map";
    return -1;
  }

  FX_LOGS(DEBUG) << "mkfs success";
  return 0;
}

zx_status_t UsedDataSize(const fbl::unique_fd& fd, uint64_t* out_size, off_t start,
                         std::optional<off_t> end) {
  Superblock info;
  zx_status_t status;

  if ((status = get_superblock(fd, start, end, &info)) != ZX_OK) {
    return status;
  }

  *out_size = info.alloc_block_count * info.block_size;
  return ZX_OK;
}

zx_status_t UsedInodes(const fbl::unique_fd& fd, uint64_t* out_inodes, off_t start,
                       std::optional<off_t> end) {
  Superblock info;
  zx_status_t status;

  if ((status = get_superblock(fd, start, end, &info)) != ZX_OK) {
    return status;
  }

  *out_inodes = info.alloc_inode_count;
  return ZX_OK;
}

zx_status_t UsedSize(const fbl::unique_fd& fd, uint64_t* out_size, off_t start,
                     std::optional<off_t> end) {
  Superblock info;
  zx_status_t status;

  if ((status = get_superblock(fd, start, end, &info)) != ZX_OK) {
    return status;
  }

  *out_size = (TotalNonDataBlocks(info) + info.alloc_block_count) * info.block_size;
  return ZX_OK;
}

zx_status_t blobfs_create(std::unique_ptr<Blobfs>* out, fbl::unique_fd fd) {
  info_block_t info_block;
  zx_status_t status;

  if ((status = blobfs_load_info_block(fd, &info_block)) != ZX_OK) {
    return status;
  }

  fbl::Array<size_t> extent_lengths(new size_t[kExtentCount], kExtentCount);

  if (info_block.info.flags & kBlobFlagFVM) {
    // The image is assumed to be a sparse file containing an FVM-formatted blobfs image with the
    // various metadata regions at their correct offsets. We just consider the "length" of each
    // extent to be the maximum possible length (i.e.  the number of blocks up to the offset of the
    // next region).
    extent_lengths[0] = BlockMapStartBlock(info_block.info) * kBlobfsBlockSize;
    extent_lengths[1] = (NodeMapStartBlock(info_block.info) - BlockMapStartBlock(info_block.info)) *
                        kBlobfsBlockSize;
    extent_lengths[2] = (JournalStartBlock(info_block.info) - NodeMapStartBlock(info_block.info)) *
                        kBlobfsBlockSize;
    extent_lengths[3] =
        (DataStartBlock(info_block.info) - JournalStartBlock(info_block.info)) * kBlobfsBlockSize;
    extent_lengths[4] = DataBlocks(info_block.info) * kBlobfsBlockSize;
  } else {
    extent_lengths[0] = BlockMapStartBlock(info_block.info) * kBlobfsBlockSize;
    extent_lengths[1] = BlockMapBlocks(info_block.info) * kBlobfsBlockSize;
    extent_lengths[2] = NodeMapBlocks(info_block.info) * kBlobfsBlockSize;
    extent_lengths[3] = JournalBlocks(info_block.info) * kBlobfsBlockSize;
    extent_lengths[4] = DataBlocks(info_block.info) * kBlobfsBlockSize;
  }

  if ((status = Blobfs::Create(std::move(fd), 0, info_block, extent_lengths, out)) != ZX_OK) {
    FX_LOGS(ERROR) << "mount failed; could not create blobfs";
    return status;
  }

  return ZX_OK;
}

zx_status_t blobfs_create_sparse(std::unique_ptr<Blobfs>* out, fbl::unique_fd fd, off_t start,
                                 off_t end, const fbl::Vector<size_t>& extent_vector) {
  if (start >= end) {
    FX_LOGS(ERROR) << "Insufficient space allocated";
    return ZX_ERR_INVALID_ARGS;
  }
  if (extent_vector.size() != kExtentCount) {
    FX_LOGS(ERROR) << "Incorrect number of extents";
    return ZX_ERR_INVALID_ARGS;
  }

  info_block_t info_block;
  zx_status_t status;

  if ((status = blobfs_load_info_block(fd, &info_block, start, end)) != ZX_OK) {
    return status;
  }

  fbl::Array<size_t> extent_lengths(new size_t[kExtentCount], kExtentCount);

  extent_lengths[0] = extent_vector[0];
  extent_lengths[1] = extent_vector[1];
  extent_lengths[2] = extent_vector[2];
  extent_lengths[3] = extent_vector[3];
  extent_lengths[4] = extent_vector[4];

  if ((status = Blobfs::Create(std::move(fd), start, info_block, extent_lengths, out)) != ZX_OK) {
    FX_LOGS(ERROR) << "mount failed; could not create blobfs";
    return status;
  }

  return ZX_OK;
}

zx_status_t blobfs_preprocess(int data_fd, bool compress, BlobLayoutFormat blob_layout_format,
                              MerkleInfo* out_info) {
  FileMapping mapping;
  zx_status_t status = mapping.Map(data_fd);
  if (status != ZX_OK) {
    return status;
  }

  if ((status = buffer_create_merkle(mapping, ShouldUseCompactMerkleTreeFormat(blob_layout_format),
                                     out_info)) != ZX_OK) {
    return status;
  }

  if (compress) {
    status = buffer_compress(mapping, out_info);
  }

  return status;
}

zx_status_t blobfs_add_blob(Blobfs* bs, JsonRecorder* json_recorder, int data_fd) {
  FileMapping mapping;
  zx_status_t status = mapping.Map(data_fd);
  if (status != ZX_OK) {
    return status;
  }

  // Calculate the actual Merkle tree.
  MerkleInfo info;
  status = buffer_create_merkle(
      mapping, ShouldUseCompactMerkleTreeFormat(GetBlobLayoutFormat(bs->Info())), &info);
  if (status != ZX_OK) {
    return status;
  }

  return blobfs_add_mapped_blob_with_merkle(bs, json_recorder, mapping, info);
}

zx_status_t blobfs_add_blob_with_merkle(Blobfs* bs, JsonRecorder* json_recorder, int data_fd,
                                        const MerkleInfo& info) {
  FileMapping mapping;
  zx_status_t status = mapping.Map(data_fd);
  if (status != ZX_OK) {
    return status;
  }

  return blobfs_add_mapped_blob_with_merkle(bs, json_recorder, mapping, info);
}

zx_status_t blobfs_fsck(fbl::unique_fd fd, off_t start, off_t end,
                        const fbl::Vector<size_t>& extent_lengths) {
  std::unique_ptr<Blobfs> blob;
  zx_status_t status;
  if ((status = blobfs_create_sparse(&blob, std::move(fd), start, end, extent_lengths)) != ZX_OK) {
    return status;
  }
  if ((status = Fsck(blob.get())) != ZX_OK) {
    return status;
  }
  return ZX_OK;
}

Blobfs::Blobfs(fbl::unique_fd fd, off_t offset, const info_block_t& info_block,
               const fbl::Array<size_t>& extent_lengths)
    : blockfd_(std::move(fd)), offset_(offset) {
  ZX_ASSERT(extent_lengths.size() == kExtentCount);
  memcpy(&info_block_, info_block.block, kBlobfsBlockSize);
  cache_.bno = 0;

  block_map_start_block_ = extent_lengths[0] / kBlobfsBlockSize;
  block_map_block_count_ = extent_lengths[1] / kBlobfsBlockSize;
  node_map_start_block_ = block_map_start_block_ + block_map_block_count_;
  node_map_block_count_ = extent_lengths[2] / kBlobfsBlockSize;
  journal_start_block_ = node_map_start_block_ + node_map_block_count_;
  journal_block_count_ = extent_lengths[3] / kBlobfsBlockSize;
  data_start_block_ = journal_start_block_ + journal_block_count_;
  data_block_count_ = extent_lengths[4] / kBlobfsBlockSize;
}

zx_status_t Blobfs::Create(fbl::unique_fd blockfd_, off_t offset, const info_block_t& info_block,
                           const fbl::Array<size_t>& extent_lengths, std::unique_ptr<Blobfs>* out) {
  zx_status_t status = CheckSuperblock(&info_block.info, TotalBlocks(info_block.info));
  if (status < 0) {
    FX_LOGS(ERROR) << "Check info failure";
    return status;
  }

  ZX_ASSERT(extent_lengths.size() == kExtentCount);

  for (unsigned i = 0; i < 3; i++) {
    if (extent_lengths[i] % kBlobfsBlockSize) {
      return ZX_ERR_INVALID_ARGS;
    }
  }

  auto fs =
      std::unique_ptr<Blobfs>(new Blobfs(std::move(blockfd_), offset, info_block, extent_lengths));

  if ((status = fs->LoadBitmap()) < 0) {
    FX_LOGS(ERROR) << "Failed to load bitmaps";
    return status;
  }

  *out = std::move(fs);
  return ZX_OK;
}

zx_status_t Blobfs::LoadBitmap() {
  zx_status_t status;
  if ((status = block_map_.Reset(block_map_block_count_ * kBlobfsBlockBits)) != ZX_OK) {
    return status;
  }
  if ((status = block_map_.Shrink(info_.data_block_count)) != ZX_OK) {
    return status;
  }
  const void* bmstart = block_map_.StorageUnsafe()->GetData();

  for (size_t n = 0; n < block_map_block_count_; n++) {
    void* bmdata = fs::GetBlock(kBlobfsBlockSize, bmstart, n);

    if (n >= node_map_start_block_) {
      memset(bmdata, 0, kBlobfsBlockSize);
    } else if ((status = ReadBlock(block_map_start_block_ + n)) != ZX_OK) {
      return status;
    } else {
      memcpy(bmdata, cache_.blk, kBlobfsBlockSize);
    }
  }
  return ZX_OK;
}

zx_status_t Blobfs::NewBlob(const Digest& digest, std::unique_ptr<InodeBlock>* out) {
  size_t ino = info_.inode_count;

  for (size_t i = 0; i < info_.inode_count; ++i) {
    size_t bno = (i / kBlobfsInodesPerBlock) + node_map_start_block_;

    zx_status_t status;
    if ((i % kBlobfsInodesPerBlock) == 0) {
      if ((status = ReadBlock(bno)) != ZX_OK) {
        FX_LOGS(ERROR) << "error: Failed to read block " << status;
        return status;
      }
    }

    auto iblk = reinterpret_cast<const Inode*>(cache_.blk);
    auto observed_inode = &iblk[i % kBlobfsInodesPerBlock];
    if (observed_inode->header.IsAllocated() && !observed_inode->header.IsExtentContainer()) {
      if (digest == observed_inode->merkle_root_hash) {
        FX_LOGS(ERROR) << "Blob already exists " << digest.ToString();
        return ZX_ERR_ALREADY_EXISTS;
      }
    } else if (ino >= info_.inode_count) {
      // If |ino| has not already been set to a valid value, set it to the first free value we find.
      // We still check all the remaining inodes to avoid adding a duplicate blob.
      ino = i;
    }
  }

  if (ino >= info_.inode_count) {
    FX_LOGS(ERROR) << "No inode resources left. requested inode number: " << ino
                   << " more than allowed inode count: " << info_.inode_count;
    return ZX_ERR_NO_RESOURCES;
  }

  size_t bno = (ino / kBlobfsInodesPerBlock) + NodeMapStartBlock(info_);
  zx_status_t status;
  if ((status = ReadBlock(bno)) != ZX_OK) {
    FX_LOGS(ERROR) << "ReadBlock failed " << status;
    return status;
  }

  Inode* inodes = reinterpret_cast<Inode*>(cache_.blk);

  std::unique_ptr<InodeBlock> ino_block(
      new InodeBlock(bno, &inodes[ino % kBlobfsInodesPerBlock], digest));

  dirty_ = true;
  info_.alloc_inode_count++;
  *out = std::move(ino_block);
  return ZX_OK;
}

zx_status_t Blobfs::AllocateBlocks(size_t nblocks, size_t* blkno_out) {
  zx_status_t status;
  if ((status = block_map_.Find(false, 0, block_map_.size(), nblocks, blkno_out)) != ZX_OK) {
    return status;
  }
  if ((status = block_map_.Set(*blkno_out, *blkno_out + nblocks)) != ZX_OK) {
    return status;
  }

  info_.alloc_block_count += nblocks;
  return ZX_OK;
}

zx_status_t Blobfs::WriteBitmap(size_t nblocks, size_t start_block) {
  uint64_t block_bitmap_start_block = start_block / kBlobfsBlockBits;
  uint64_t block_bitmap_end_block =
      fbl::round_up(start_block + nblocks, kBlobfsBlockBits) / kBlobfsBlockBits;
  const void* bmstart = block_map_.StorageUnsafe()->GetData();
  const void* data = fs::GetBlock(kBlobfsBlockSize, bmstart, block_bitmap_start_block);
  uint64_t absolute_block_number = block_map_start_block_ + block_bitmap_start_block;
  uint64_t block_count = block_bitmap_end_block - block_bitmap_start_block;
  return WriteBlocks(absolute_block_number, block_count, data);
}

zx_status_t Blobfs::WriteNode(std::unique_ptr<InodeBlock> ino_block) {
  if (ino_block->GetBno() != cache_.bno) {
    return ZX_ERR_ACCESS_DENIED;
  }

  dirty_ = false;
  return WriteBlock(cache_.bno, cache_.blk);
}

zx_status_t Blobfs::WriteData(Inode* inode, const void* merkle_data, const void* blob_data,
                              const BlobLayout& blob_layout) {
  if (blob_layout.TotalBlockCount() == 0) {
    // Nothing to write.
    return ZX_OK;
  }
  // Allocate a new buffer to hold both the data and Merkle tree together.  The data and Merkle tree
  // may not be block multiples in size which makes writing them separately in terms of blocks
  // difficult, also the data and Merkle tree may share a block.  Creating a new buffer to hold both
  // uses more memory but makes writing the blob significantly easier.
  uint64_t block_size = GetBlockSize();
  uint64_t buf_size = block_size * blob_layout.TotalBlockCount();
  auto buf = std::make_unique<uint8_t[]>(blob_layout.TotalBlockCount() * GetBlockSize());
  // Zero the entire buffer instead of trying to calculate where the data and Merkle tree won't be.
  memset(buf.get(), 0, buf_size);

  // Copy the data to the buffer.
  uint64_t data_offset = block_size * blob_layout.DataBlockOffset();
  memcpy(buf.get() + data_offset, blob_data, blob_layout.DataSizeUpperBound());

  // |merkle_data| will be null when the blob size is less than or equal to the Merkle tree node
  // size.
  if (merkle_data) {
    // Copy the Merkle tree to the buffer.
    uint64_t merkle_offset = block_size * blob_layout.MerkleTreeBlockOffset() +
                             blob_layout.MerkleTreeOffsetWithinBlockOffset();
    memcpy(buf.get() + merkle_offset, merkle_data, blob_layout.MerkleTreeSize());
  }

  zx_status_t status;
  uint32_t blob_start_block = data_start_block_ + inode->extents[0].Start();
  if ((status = WriteBlocks(blob_start_block, blob_layout.TotalBlockCount(), buf.get())) != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to write a blob: " << status;
    return status;
  }
  return ZX_OK;
}

zx_status_t Blobfs::WriteInfo() { return WriteBlock(0, info_block_); }

zx_status_t Blobfs::ReadBlock(size_t bno) {
  if (dirty_) {
    return ZX_ERR_ACCESS_DENIED;
  }

  zx_status_t status;
  if ((cache_.bno != bno) &&
      ((status = ReadBlockOffset(blockfd_.get(), bno, offset_, &cache_.blk)) != ZX_OK)) {
    FX_LOGS(ERROR) << "Failed to read a blob: " << status;
    return status;
  }

  cache_.bno = bno;
  return ZX_OK;
}

zx_status_t Blobfs::WriteBlocks(size_t block_number, uint64_t block_count, const void* data) {
  return WriteBlockOffset(blockfd_.get(), data, block_count, offset_, block_number);
}

zx_status_t Blobfs::WriteBlock(size_t bno, const void* data) {
  return WriteBlockOffset(blockfd_.get(), data, 1, offset_, bno);
}

zx_status_t Blobfs::ResetCache() {
  if (dirty_) {
    return ZX_ERR_ACCESS_DENIED;
  }

  if (cache_.bno != 0) {
    memset(cache_.blk, 0, kBlobfsBlockSize);
    cache_.bno = 0;
  }
  return ZX_OK;
}

zx::status<InodePtr> Blobfs::GetNode(uint32_t index) {
  if (index >= info_.inode_count) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  size_t bno = node_map_start_block_ + index / kBlobfsInodesPerBlock;
  if (zx_status_t status = ReadBlock(bno); status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to read block: " << status;
    return zx::error(status);
  }

  auto iblock = reinterpret_cast<Inode*>(cache_.blk);
  return zx::ok(InodePtr(&iblock[index % kBlobfsInodesPerBlock], InodePtrDeleter(this)));
}

fit::result<std::vector<uint8_t>, std::string> Blobfs::LoadAndVerifyBlob(Inode& inode) {
  size_t blob_start_block = data_start_block_ + inode.extents[0].Start();
  uint32_t block_size = GetBlockSize();
  zx_status_t status;
  auto make_error = [&](std::string error) {
    digest::Digest digest(inode.merkle_root_hash);
    auto digest_str = digest.ToString();
    return fit::error("Blob with merkle root hash of " +
                      std::string(digest_str.data(), digest_str.length()) +
                      " had errors. More specifically: " + error);
  };

  auto blob_layout =
      blobfs::BlobLayout::CreateFromInode(GetBlobLayoutFormat(Info()), inode, block_size);
  if (blob_layout.is_error()) {
    return make_error("Failed to create blob layout with status " +
                      std::to_string(blob_layout.status_value()));
  }

  // Read in the Merkle tree.
  uint32_t merkle_tree_block_count = blob_layout->MerkleTreeBlockCount();
  uint32_t merkle_tree_block_offset = blob_layout->MerkleTreeBlockOffset();
  std::vector<uint8_t> merkle_tree_blocks(blob_layout->MerkleTreeBlockAlignedSize(), 0);
  for (uint32_t block = 0; block < merkle_tree_block_count; ++block) {
    ReadBlock(blob_start_block + merkle_tree_block_offset + block);
    memcpy(&merkle_tree_blocks[block * block_size], cache_.blk, block_size);
  }

  // Read in the data.
  uint32_t data_block_count = blob_layout->DataBlockCount();
  uint32_t data_block_offset = blob_layout->DataBlockOffset();
  std::vector<uint8_t> data_blocks(blob_layout->DataBlockAlignedSize(), 0);
  for (uint32_t block = 0; block < data_block_count; ++block) {
    ReadBlock(blob_start_block + data_block_offset + block);
    memcpy(&data_blocks[block * block_size], cache_.blk, block_size);
  }

  // Decompress the data if necessary.
  if (inode.header.flags & HostCompressor::InodeHeaderCompressionFlags()) {
    size_t file_size = inode.blob_size;
    std::vector<uint8_t> uncompressed_data(file_size, 0);
    HostDecompressor decompressor;
    if ((status = decompressor.Decompress(uncompressed_data.data(), &file_size, data_blocks.data(),
                                          blob_layout->DataSizeUpperBound())) != ZX_OK) {
      return make_error("Failed to decompress with status " + std::to_string(status));
    }
    if (file_size != inode.blob_size) {
      return make_error("Decompressed blob size of " + std::to_string(file_size) +
                        " mismatch with blob inode expected size of " +
                        std::to_string(inode.blob_size));
    }
    // Replace the compressed data with the uncompressed data.
    data_blocks = std::move(uncompressed_data);
  }

  // Verify the contents of the blob.
  uint8_t* merkle_tree_ptr =
      merkle_tree_blocks.empty()
          ? nullptr
          : &merkle_tree_blocks[blob_layout->MerkleTreeOffsetWithinBlockOffset()];
  MerkleTreeVerifier mtv;
  mtv.SetUseCompactFormat(blobfs::ShouldUseCompactMerkleTreeFormat(blob_layout->Format()));
  if ((status = mtv.SetDataLength(inode.blob_size)) != ZX_OK ||
      (status = mtv.SetTree(merkle_tree_ptr, mtv.GetTreeLength(), inode.merkle_root_hash,
                            sizeof(inode.merkle_root_hash))) != ZX_OK ||
      (status = mtv.Verify(data_blocks.data(), inode.blob_size, 0)) != ZX_OK) {
    return make_error("Verification failed with status " + std::to_string(status));
  }

  // Remove trailing block alignment.
  data_blocks.resize(inode.blob_size, 0);

  return fit::ok(std::move(data_blocks));
}

zx_status_t Blobfs::LoadAndVerifyBlob(uint32_t node_index) {
  auto inode_ptr = GetNode(node_index);
  if (inode_ptr.is_error()) {
    return inode_ptr.status_value();
  }
  Inode inode = *inode_ptr.value();
  auto load_result = LoadAndVerifyBlob(inode);
  return load_result.is_ok() ? ZX_OK : ZX_ERR_INTERNAL;
}

uint32_t Blobfs::GetBlockSize() const { return Info().block_size; }

fit::result<void, std::string> Blobfs::VisitBlobs(BlobVisitor visitor) {
  for (uint64_t inode_index = 0, allocated_nodes = 0;
       inode_index < info_.inode_count && allocated_nodes < info_.alloc_inode_count;
       ++inode_index) {
    auto inode_ptr = GetNode(inode_index);
    if (inode_ptr.is_error()) {
      return fit::error("Failed to retrieve inode.");
    }
    if (!inode_ptr->header.IsAllocated()) {
      continue;
    }

    // Required copy to preven additional calls to ReadBlock or GetNode to replace the contents
    // of |cache_.blk| where inode_ptr comes from.
    Inode inode = *inode_ptr.value();
    allocated_nodes++;
    auto load_result = LoadAndVerifyBlob(inode);
    if (load_result.is_error()) {
      return load_result.take_error_result();
    }
    BlobView view = {
        .merkle_hash = fbl::Span<const uint8_t>(inode.merkle_root_hash),
        .blob_contents = load_result.value(),
    };

    auto visitor_result = visitor(view);
    if (visitor_result.is_error()) {
      return visitor_result.take_error_result();
    }
  }
  return fit::ok();
}

fit::result<void, std::string> ExportBlobs(int output_dir, Blobfs& fs) {
  return fs.VisitBlobs([output_dir](Blobfs::BlobView view) -> fit::result<void, std::string> {
    uint8_t hash[digest::kSha256Length];
    memcpy(hash, view.merkle_hash.data(), digest::kSha256Length);
    auto blob_name = digest::Digest(hash).ToString();
    fbl::unique_fd file(openat(output_dir, blob_name.c_str(), O_CREAT | O_RDWR, 0644));
    if (!file.is_valid()) {
      return fit::error(
          "Failed to create blob file" + std::string(blob_name.c_str()) +
          "(merkle root digest) in output dir. More specifically: " + strerror(errno));
    }

    size_t written_bytes = 0;
    int write_result = 0;
    while (written_bytes < view.blob_contents.size()) {
      write_result = write(file.get(), &view.blob_contents[written_bytes],
                           view.blob_contents.size() - written_bytes);
      if (write_result < 0) {
        return fit::error(
            "Failed to write blob " + std::string(blob_name.c_str()) +
            "(merkle root digest) contents in output file. More specifically: " + strerror(errno));
      }
      written_bytes += write_result;
    }

    return fit::ok();
  });
}

zx::status<std::unique_ptr<Superblock>> Blobfs::ReadBackupSuperblock() {
  if (zx_status_t status = ReadBlock(kFVMBackupSuperblockOffset); status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(std::make_unique<Superblock>(*reinterpret_cast<Superblock*>(cache_.blk)));
}

}  // namespace blobfs
