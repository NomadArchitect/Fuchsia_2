// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STORAGE_BLOBFS_BLOB_LOADER_H_
#define SRC_STORAGE_BLOBFS_BLOB_LOADER_H_

#include <lib/fzl/owned-vmo-mapper.h>
#include <lib/zx/status.h>
#include <lib/zx/vmo.h>
#include <zircon/status.h>

#include <memory>

#include <fbl/function.h>
#include <fbl/macros.h>
#include <storage/buffer/owned_vmoid.h>

#include "src/storage/blobfs/blob_layout.h"
#include "src/storage/blobfs/compression/external_decompressor.h"
#include "src/storage/blobfs/compression/seekable_decompressor.h"
#include "src/storage/blobfs/format.h"
#include "src/storage/blobfs/iterator/block_iterator_provider.h"
#include "src/storage/blobfs/metrics.h"
#include "src/storage/blobfs/node_finder.h"
#include "src/storage/blobfs/pager/page_watcher.h"
#include "src/storage/blobfs/transaction_manager.h"

namespace blobfs {

// BlobLoader is responsible for loading blobs from disk, decoding them and verifying their
// contents as needed.
class BlobLoader {
 public:
  struct PagedLoadResult {
    pager::UserPagerInfo pager_info;
    std::unique_ptr<BlobLayout> layout;
    fzl::OwnedVmoMapper merkle;
  };
  struct UnpagedLoadResult {
    zx::vmo data_vmo;
    fzl::VmoMapper data_mapper;

    fzl::OwnedVmoMapper merkle;
  };

  BlobLoader() = default;
  BlobLoader(BlobLoader&& o) = default;
  BlobLoader& operator=(BlobLoader&& o) = default;

  DISALLOW_COPY_AND_ASSIGN_ALLOW_MOVE(BlobLoader);

  // Creates a BlobLoader.
  static zx::status<BlobLoader> Create(TransactionManager* txn_manager,
                                       BlockIteratorProvider* block_iter_provider,
                                       NodeFinder* node_finder, BlobfsMetrics* metrics,
                                       bool sandbox_decompression);

  // Loads the merkle tree and data for the blob with index |node_index|.
  //
  // |data_out| will be a VMO containing all of the data of the blob, padded up to a block size.
  // |merkle_out| will be a VMO containing the merkle tree of the blob. For small blobs, there
  // may be no merkle tree (i.e. the entire 'tree' is just a single hash stored inline in the
  // inode), in which case no VMO is returned.
  //
  // This method verifies the following correctness properties:
  //  - The stored merkle tree is well-formed.
  //  - The blob's merkle root in |inode| matches the root of the merkle tree stored on-disk.
  //  - The blob's contents match the merkle tree.
  zx::status<UnpagedLoadResult> LoadBlob(uint32_t node_index,
                                         const BlobCorruptionNotifier* corruption_notifier);

  // Loads the merkle tree for the blob referenced |inode|, and prepare a pager-backed VMO for
  // data.
  //
  // This method verifies the following correctness properties:
  //  - The stored merkle tree is well-formed.
  //  - The blob's merkle root in |inode| matches the root of the merkle tree stored on-disk.
  //
  // This method does *NOT* immediately verify the integrity of the blob's data, this will be
  // lazily verified by the pager as chunks of the blob are loaded.
  zx::status<PagedLoadResult> LoadBlobPaged(uint32_t node_index,
                                            const BlobCorruptionNotifier* corruption_notifier);

 private:
  BlobLoader(TransactionManager* txn_manager, BlockIteratorProvider* block_iter_provider,
             NodeFinder* node_finder, BlobfsMetrics* metrics, fzl::OwnedVmoMapper read_mapper,
             zx::vmo sandbox_vmo, std::unique_ptr<ExternalDecompressorClient> decompressor_client);

  // Loads the merkle tree from disk and initializes a VMO mapping and BlobVerifier with the
  // contents. (Small blobs may have no stored tree, in which case |vmo_out| is not mapped but
  // |verifier_out| is still initialized.)
  zx_status_t InitMerkleVerifier(uint32_t node_index, const Inode& inode,
                                 const BlobLayout& blob_layout,
                                 const BlobCorruptionNotifier* corruption_notifier,
                                 fzl::OwnedVmoMapper* vmo_out,
                                 std::unique_ptr<BlobVerifier>* verifier_out);
  // Prepares |decompressor_out| to decompress the blob contents of |inode|.
  // If |inode| is not compressed, this is a NOP.
  // Depending on the format, some data may be read (e.g. for the CHUNKED format, the header
  // containing the seek table is read and used to initialize the decompressor).
  zx_status_t InitForDecompression(uint32_t node_index, const Inode& inode,
                                   const BlobLayout& blob_layout, const BlobVerifier& verifier,
                                   std::unique_ptr<SeekableDecompressor>* decompressor_out);
  zx_status_t LoadMerkle(uint32_t node_index, const BlobLayout& blob_layout,
                         const fzl::OwnedVmoMapper& mapper) const;
  zx_status_t LoadData(uint32_t node_index, const BlobLayout& blob_layout, zx::vmo& vmo,
                       fzl::VmoMapper& mapper) const;
  zx_status_t LoadAndDecompressData(uint32_t node_index, const Inode& inode,
                                    const BlobLayout& blob_layout, zx::vmo& vmo,
                                    void* mapped_data) const;

  // Verifies that |merkle_root| is the root hash of the null blob.
  zx_status_t VerifyNullBlob(Digest merkle_root, const BlobCorruptionNotifier* notifier);

  // Reads |block_count| blocks starting at |block_offset| from the blob specified by |node_index|
  // into |vmo|.
  //
  // The vmo will be written to (the const indicates the zx::vmo object won't change, but the
  // referenced data will be).
  zx::status<uint64_t> LoadBlocks(uint32_t node_index, uint32_t block_offset, uint32_t block_count,
                                  const zx::vmo& vmo) const;

  // If part of the Merkle tree is located within the data blocks then this function zeros out the
  // Merkle tree within those blocks.
  // |vmo| should contain the raw data stored which might be compressed or uncompressed.
  void ZeroMerkleTreeWithinDataVmo(void* mapped_data, size_t mapped_data_size,
                                   const BlobLayout& blob_layout) const;

  // Returns the block size used by blobfs.
  uint32_t GetBlockSize() const;

  TransactionManager* txn_manager_ = nullptr;
  BlockIteratorProvider* block_iter_provider_ = nullptr;
  NodeFinder* node_finder_ = nullptr;
  BlobfsMetrics* metrics_ = nullptr;
  fzl::OwnedVmoMapper read_mapper_;
  zx::vmo sandbox_vmo_;
  std::unique_ptr<ExternalDecompressorClient> decompressor_client_ = nullptr;
};

}  // namespace blobfs

#endif  // SRC_STORAGE_BLOBFS_BLOB_LOADER_H_
