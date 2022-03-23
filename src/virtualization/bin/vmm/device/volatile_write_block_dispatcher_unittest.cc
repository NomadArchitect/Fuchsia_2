// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/fpromise/promise.h>
#include <lib/fpromise/single_threaded_executor.h>

#include <gtest/gtest.h>

#include "src/virtualization/bin/vmm/device/block.h"
#include "src/virtualization/bin/vmm/device/block_dispatcher.h"

namespace {

static constexpr size_t kDispatcherSize = 8 * 1024 * 1024;

using BufVector = std::vector<uint8_t>;

// Read only dispatcher that returns blocks containing a single byte.
class StaticDispatcher : public BlockDispatcher {
 public:
  fpromise::promise<void, zx_status_t> Sync() override {
    return fpromise::make_result_promise<void, zx_status_t>(fpromise::ok());
  }

  fpromise::promise<void, zx_status_t> ReadAt(void* data, uint64_t size, uint64_t off) override {
    memset(data, value_, size);
    return fpromise::make_result_promise<void, zx_status_t>(fpromise::ok());
  }

  fpromise::promise<void, zx_status_t> WriteAt(const void* data, uint64_t size,
                                               uint64_t off) override {
    return fpromise::make_error_promise(ZX_ERR_NOT_SUPPORTED);
  }

 private:
  uint8_t value_ = 0xab;
};

#define ASSERT_BLOCK_VALUE(ptr, size, val) \
  do {                                     \
    for (size_t i = 0; i < (size); ++i) {  \
      ASSERT_EQ((val), (ptr)[i]);          \
    }                                      \
  } while (false)

std::unique_ptr<BlockDispatcher> CreateDispatcher() {
  std::unique_ptr<BlockDispatcher> disp;
  CreateVolatileWriteBlockDispatcher(
      kDispatcherSize, kBlockSectorSize, std::make_unique<StaticDispatcher>(),
      [&disp](uint64_t capacity, uint32_t block_size, std::unique_ptr<BlockDispatcher> in) {
        disp = std::move(in);
      });
  return disp;
}

TEST(VolatileWriteBlockDispatcherTest, WriteBlock) {
  auto disp = CreateDispatcher();

  fpromise::result<void, zx_status_t> result;
  fidl::VectorPtr<uint8_t> buf(kBlockSectorSize);
  result = fpromise::run_single_threaded(disp->ReadAt(buf->data(), buf->size(), 0));
  ASSERT_TRUE(result.is_ok());
  ASSERT_BLOCK_VALUE(buf->data(), buf->size(), 0xab);

  fidl::VectorPtr<uint8_t> write_buf(BufVector(kBlockSectorSize, 0xbe));
  result = fpromise::run_single_threaded(disp->WriteAt(write_buf->data(), write_buf->size(), 0));
  ASSERT_TRUE(result.is_ok());

  result = fpromise::run_single_threaded(disp->ReadAt(buf->data(), buf->size(), 0));
  ASSERT_TRUE(result.is_ok());
  ASSERT_BLOCK_VALUE(buf->data(), buf->size(), 0xbe);
}

TEST(VolatileWriteBlockDispatcherTest, WriteBlockComplex) {
  auto disp = CreateDispatcher();

  // Write blocks 0 & 2, blocks 1 & 3 will hit the static dispatcher.
  fidl::VectorPtr<uint8_t> write_buf(BufVector(kBlockSectorSize, 0xbe));
  fpromise::result<void, zx_status_t> result;
  result = fpromise::run_single_threaded(disp->WriteAt(write_buf->data(), write_buf->size(), 0));
  ASSERT_TRUE(result.is_ok());
  result = fpromise::run_single_threaded(
      disp->WriteAt(write_buf->data(), write_buf->size(), kBlockSectorSize * 2));
  ASSERT_TRUE(result.is_ok());

  fidl::VectorPtr<uint8_t> buf(kBlockSectorSize * 4);
  result = fpromise::run_single_threaded(disp->ReadAt(buf->data(), buf->size(), 0));
  ASSERT_TRUE(result.is_ok());
  ASSERT_BLOCK_VALUE(buf->data(), kBlockSectorSize, 0xbe);
  ASSERT_BLOCK_VALUE(buf->data() + kBlockSectorSize, kBlockSectorSize, 0xab);
  ASSERT_BLOCK_VALUE(buf->data() + kBlockSectorSize * 2, kBlockSectorSize, 0xbe);
  ASSERT_BLOCK_VALUE(buf->data() + kBlockSectorSize * 3, kBlockSectorSize, 0xab);
}

TEST(VolatileWriteBlockDispatcherTest, BadRequest) {
  auto disp = CreateDispatcher();

  fpromise::result<void, zx_status_t> result;
  result = fpromise::run_single_threaded(disp->ReadAt(nullptr, kBlockSectorSize, 1));
  ASSERT_TRUE(result.is_error());
  EXPECT_EQ(ZX_ERR_INVALID_ARGS, result.error());

  result = fpromise::run_single_threaded(disp->ReadAt(nullptr, kBlockSectorSize - 1, 0));
  ASSERT_TRUE(result.is_error());
  EXPECT_EQ(ZX_ERR_INVALID_ARGS, result.error());

  result = fpromise::run_single_threaded(disp->WriteAt(nullptr, kBlockSectorSize, 1));
  ASSERT_TRUE(result.is_error());
  EXPECT_EQ(ZX_ERR_INVALID_ARGS, result.error());

  result = fpromise::run_single_threaded(disp->WriteAt(nullptr, kBlockSectorSize - 1, 0));
  ASSERT_TRUE(result.is_error());
  EXPECT_EQ(ZX_ERR_INVALID_ARGS, result.error());
}

}  // namespace
