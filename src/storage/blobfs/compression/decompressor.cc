// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/storage/blobfs/compression/decompressor.h"

#include <zircon/errors.h>

#include <memory>

#include "src/storage/blobfs/compression/chunked.h"
#include "src/storage/blobfs/compression/lz4.h"
#include "src/storage/blobfs/compression/zstd_plain.h"
#include "src/storage/blobfs/compression/zstd_seekable.h"

namespace blobfs {

zx_status_t Decompressor::Create(CompressionAlgorithm algorithm,
                                 std::unique_ptr<Decompressor>* out) {
  switch (algorithm) {
    case CompressionAlgorithm::kLz4:
      *out = std::make_unique<LZ4Decompressor>();
      break;
    case CompressionAlgorithm::kZstd:
      *out = std::make_unique<ZSTDDecompressor>();
      break;
    case CompressionAlgorithm::kZstdSeekable:
      *out = std::make_unique<ZSTDSeekableDecompressor>();
      break;
    case CompressionAlgorithm::kChunked:
      *out = std::make_unique<ChunkedDecompressor>();
      break;
    case CompressionAlgorithm::kUncompressed:
      ZX_DEBUG_ASSERT(false);
      return ZX_ERR_NOT_SUPPORTED;
  }
  return ZX_OK;
}

}  // namespace blobfs
