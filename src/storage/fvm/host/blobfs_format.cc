// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/storage/fvm/host/blobfs_format.h"

#include <inttypes.h>
#include <lib/zx/status.h>

#include <limits>
#include <utility>

#include <safemath/checked_math.h>

#include "src/storage/blobfs/format.h"
#include "src/storage/fvm/host/fvm_reservation.h"

namespace {

template <class T>
uint32_t ToU32(T in) {
  if (in > std::numeric_limits<uint32_t>::max()) {
    fprintf(stderr, "%s:%d out of range %" PRIuMAX "\n", __FILE__, __LINE__,
            safemath::checked_cast<uintmax_t>(in));
    exit(-1);
  }
  return safemath::checked_cast<uint32_t>(in);
}

}  // namespace

BlobfsFormat::BlobfsFormat(fbl::unique_fd fd, const char* type) : Format(), fd_(std::move(fd)) {
  if (!strcmp(type, kBlobTypeName)) {
    memcpy(type_, kBlobType, sizeof(kBlobType));
  } else if (!strcmp(type, kDefaultTypeName)) {
    memcpy(type_, kDefaultType, sizeof(kDefaultType));
  } else {
    fprintf(stderr, "Unrecognized type for blobfs: %s\n", type);
    exit(-1);
  }

  if (blobfs::ReadBlock(fd_.get(), 0, reinterpret_cast<void*>(blk_)) < 0) {
    fprintf(stderr, "blobfs: could not read info block\n");
    exit(-1);
  }

  if (blobfs::GetBlockCount(fd_.get(), &blocks_) != ZX_OK) {
    fprintf(stderr, "blobfs: cannot find end of underlying device\n");
    exit(-1);
  } else if (blobfs::CheckSuperblock(&info_, blocks_, /*quiet=*/false) != ZX_OK) {
    fprintf(stderr, "blobfs: Info check failed\n");
    exit(-1);
  }
}

BlobfsFormat::~BlobfsFormat() = default;

zx_status_t BlobfsFormat::ComputeSlices(uint64_t inode_count, uint64_t data_blocks,
                                        uint64_t journal_block_count) {
  auto abm_blocks = blobfs::BlocksRequiredForBits(data_blocks);
  auto ino_blocks = blobfs::BlocksRequiredForInode(inode_count);

  fvm_info_.abm_slices = BlocksToSlices(abm_blocks);
  fvm_info_.ino_slices = BlocksToSlices(ino_blocks);
  fvm_info_.journal_slices = BlocksToSlices(ToU32(journal_block_count));
  fvm_info_.dat_slices = BlocksToSlices(safemath::checked_cast<uint32_t>(data_blocks));

  fvm_info_.inode_count = safemath::checked_cast<uint32_t>(
      fvm_info_.ino_slices * fvm_info_.slice_size / blobfs::kBlobfsInodeSize);
  fvm_info_.journal_block_count = SlicesToBlocks(fvm_info_.journal_slices);
  fvm_info_.data_block_count = SlicesToBlocks(fvm_info_.dat_slices);
  fvm_info_.flags |= blobfs::kBlobFlagFVM;

  xprintf("Blobfs: slice_size is %" PRIu64 "\n", fvm_info_.slice_size);
  xprintf("Blobfs: abm_blocks: %" PRIu64 ", abm_slices: %u\n", BlockMapBlocks(fvm_info_),
          fvm_info_.abm_slices);
  xprintf("Blobfs: ino_blocks: %" PRIu64 ", ino_slices: %u\n", NodeMapBlocks(fvm_info_),
          fvm_info_.ino_slices);
  xprintf("Blobfs: jnl_blocks: %" PRIu64 ", jnl_slices: %u\n", JournalBlocks(fvm_info_),
          fvm_info_.journal_slices);
  xprintf("Blobfs: dat_blocks: %" PRIu64 ", dat_slices: %u\n", DataBlocks(fvm_info_),
          fvm_info_.dat_slices);

  zx_status_t status;
  // Explicitly override the |max| number of blocks in CheckSuperblock. We already verified the
  // input image in BlobfsFormat::BlobfsFormat, so all we need to check is that the slice sizes we
  // computed above match up with the block sizes stored in the superblock. |blocks_| stores the
  // number of blocks in the input image, which is necessarily <= the number of blocks in the
  // resultant FVM image, so we can't use |blocks_| here.
  if ((status = CheckSuperblock(&fvm_info_, std::numeric_limits<uint64_t>::max(),
                                /*quiet=*/false)) != ZX_OK) {
    fprintf(stderr, "Check info failed\n");
    return status;
  }

  return ZX_OK;
}

zx_status_t BlobfsFormat::MakeFvmReady(size_t slice_size, uint32_t vpart_index,
                                       FvmReservation* reserve) {
  memcpy(&fvm_blk_, &blk_, BlockSize());
  xprintf("fvm_info has data block count %" PRIu64 "\n", fvm_info_.data_block_count);
  fvm_info_.slice_size = slice_size;

  if (fvm_info_.slice_size % BlockSize()) {
    fprintf(stderr, "MakeFvmReady: Slice size not multiple of minfs block\n");
    return ZX_ERR_INVALID_ARGS;
  }
  if (blobfs::kBlobfsBlockSize * 2 > fvm_info_.slice_size) {
    // Ensure that we have enough room in the first slice for the backup superblock, too.
    // We could, in theory, support a backup superblock which span past the first slice, but it
    // would be a lot of work given the tight coupling between FVM/blobfs, and the many places which
    // assume that the superblocks both fit within a slice.
    fprintf(stderr, "MakeFvmReady: Slice size not large enough for backup superblock\n");
    return ZX_ERR_INVALID_ARGS;
  }

  uint64_t minimum_data_blocks =
      fbl::round_up(reserve->data().request.value_or(0), BlockSize()) / BlockSize();
  if (minimum_data_blocks < fvm_info_.data_block_count) {
    minimum_data_blocks = fvm_info_.data_block_count;
  }

  uint64_t minimum_inode_count = reserve->inodes().request.value_or(0);
  if (minimum_inode_count < fvm_info_.inode_count) {
    minimum_inode_count = fvm_info_.inode_count;
  }

  zx_status_t status;
  if ((status = ComputeSlices(minimum_inode_count, minimum_data_blocks, JournalBlocks(info_))) !=
      ZX_OK) {
    return status;
  }

  // Lets see if we can increase journal size now
  uint64_t slice_limit = reserve->total_bytes().request.value_or(0) / slice_size;
  uint32_t vslice_count = blobfs::CalculateVsliceCount(fvm_info_);
  if (slice_limit > vslice_count) {
    // TODO(auradkar): This should use TransactionLimits
    uint64_t journal_block_count = blobfs::SuggestJournalBlocks(
        ToU32(JournalBlocks(fvm_info_)),
        ToU32((slice_limit - vslice_count) * slice_size / BlockSize()));
    // Above, we might have changed number of blocks allocated to the journal. This
    // might affect the number of allocated/reserved slices. Call ComputeSlices
    // again to adjust the count.
    if ((status = ComputeSlices(minimum_inode_count, minimum_data_blocks, journal_block_count)) !=
        ZX_OK) {
      return status;
    }
  }

  reserve->set_data_reserved(fvm_info_.data_block_count * BlockSize());
  reserve->set_inodes_reserved(fvm_info_.inode_count);
  reserve->set_total_bytes_reserved(SlicesToBlocks(vslice_count) * BlockSize());
  if (!reserve->Approved()) {
    return ZX_ERR_BUFFER_TOO_SMALL;
  }

  fvm_ready_ = true;
  vpart_index_ = vpart_index;
  return ZX_OK;
}

zx::status<ExtentInfo> BlobfsFormat::GetExtent(unsigned extent_index) const {
  CheckFvmReady();
  ExtentInfo info;
  switch (extent_index) {
    case 0: {
      info.vslice_start = 0;
      info.vslice_count = 1;
      info.block_offset = 0;
      // Kludge warning:
      // There is only one superblock stored in the non-FVM blobfs image, we need to expand that to
      // two in the FVM-contained blobfs image. |FillBlock| will read out |fvm_info_| for either
      // block while we fill them, but we have to say that there are two blocks to fill here.
      info.block_count = 2 * ToU32(SuperblockBlocks(info_));
      info.zero_fill = true;
      return zx::ok(info);
    }
    case 1: {
      info.vslice_start = blobfs::kFVMBlockMapStart / BlocksPerSlice();
      info.vslice_count = fvm_info_.abm_slices;
      info.block_offset = ToU32(BlockMapStartBlock(info_));
      info.block_count = ToU32(BlockMapBlocks(info_));
      info.zero_fill = true;
      return zx::ok(info);
    }
    case 2: {
      info.vslice_start = blobfs::kFVMNodeMapStart / BlocksPerSlice();
      info.vslice_count = fvm_info_.ino_slices;
      info.block_offset = ToU32(NodeMapStartBlock(info_));
      info.block_count = ToU32(NodeMapBlocks(info_));
      info.zero_fill = true;
      return zx::ok(info);
    }
    case 3: {
      info.vslice_start = blobfs::kFVMJournalStart / BlocksPerSlice();
      info.vslice_count = fvm_info_.journal_slices;
      info.block_offset = ToU32(JournalStartBlock(info_));
      info.block_count = ToU32(JournalBlocks(info_));
      info.zero_fill = false;
      return zx::ok(info);
    }
    case 4: {
      info.vslice_start = blobfs::kFVMDataStart / BlocksPerSlice();
      info.vslice_count = fvm_info_.dat_slices;
      info.block_offset = ToU32(DataStartBlock(info_));
      info.block_count = ToU32(DataBlocks(info_));
      info.zero_fill = false;
      return zx::ok(info);
    }
  }

  return zx::error(ZX_ERR_OUT_OF_RANGE);
}

zx_status_t BlobfsFormat::GetSliceCount(uint32_t* slices_out) const {
  CheckFvmReady();
  *slices_out = 1 + fvm_info_.abm_slices + fvm_info_.ino_slices + fvm_info_.journal_slices +
                fvm_info_.dat_slices;
  return ZX_OK;
}

zx_status_t BlobfsFormat::FillBlock(unsigned extent_index, size_t block_offset) {
  CheckFvmReady();
  // If we are reading the super block, make sure it is the fvm version and not the original
  if (extent_index == 0) {
    memcpy(datablk, fvm_blk_, BlockSize());
    return ZX_OK;
  }
  if (blobfs::ReadBlock(fd_.get(), block_offset, datablk) != ZX_OK) {
    fprintf(stderr, "blobfs: could not read block\n");
    return ZX_ERR_INTERNAL;
  }
  return ZX_OK;
}

zx_status_t BlobfsFormat::EmptyBlock() {
  CheckFvmReady();
  memset(datablk, 0, BlockSize());
  return ZX_OK;
}

void* BlobfsFormat::Data() { return datablk; }

const char* BlobfsFormat::Name() const { return kBlobfsName; }

uint32_t BlobfsFormat::BlockSize() const { return blobfs::kBlobfsBlockSize; }

uint32_t BlobfsFormat::BlocksPerSlice() const {
  CheckFvmReady();
  return ToU32(fvm_info_.slice_size / BlockSize());
}

uint32_t BlobfsFormat::BlocksToSlices(uint32_t block_count) const {
  return ToU32(fvm::BlocksToSlices(fvm_info_.slice_size, BlockSize(), block_count));
}

uint32_t BlobfsFormat::SlicesToBlocks(uint32_t slice_count) const {
  return ToU32(fvm::SlicesToBlocks(fvm_info_.slice_size, BlockSize(), slice_count));
}
