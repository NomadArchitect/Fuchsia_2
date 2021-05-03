// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/storage/blobfs/blob_verifier.h"

#include <memory>
#include <random>

#include <gtest/gtest.h>

#include "src/lib/digest/merkle-tree.h"
#include "src/storage/blobfs/blob_layout.h"
#include "src/storage/blobfs/test/blob_utils.h"
#include "src/storage/blobfs/test/unit/utils.h"

namespace blobfs {
namespace {

class TestCorruptionNotifier : public BlobCorruptionNotifier {
 public:
  void NotifyCorruptBlob(const digest::Digest& digest) const override { last_corruption_ = digest; }

  std::optional<digest::Digest>& last_corruption() { return last_corruption_; }
  void ResetLastCorruption() { last_corruption_ = std::optional<digest::Digest>(); }

 private:
  mutable std::optional<digest::Digest> last_corruption_;
};

class BlobVerifierTest : public testing::TestWithParam<BlobLayoutFormat> {
 public:
  std::shared_ptr<BlobfsMetrics> GetMetrics() { return metrics_; }

  void SetUp() override { srand(testing::UnitTest::GetInstance()->random_seed()); }

  static std::unique_ptr<MerkleTreeInfo> GenerateTree(const uint8_t* data, size_t len) {
    return CreateMerkleTree(data, len, ShouldUseCompactMerkleTreeFormat(GetParam()));
  }

 private:
  std::shared_ptr<BlobfsMetrics> metrics_ = std::make_shared<BlobfsMetrics>(false);
};

void FillWithRandom(uint8_t* buf, size_t len) {
  for (unsigned i = 0; i < len; ++i) {
    buf[i] = static_cast<uint8_t>(rand());
  }
}

TEST_P(BlobVerifierTest, CreateAndVerify_NullBlob) {
  auto merkle_tree = GenerateTree(nullptr, 0);

  std::unique_ptr<BlobVerifier> verifier;
  ASSERT_EQ(
      BlobVerifier::CreateWithoutTree(merkle_tree->root, GetMetrics(), 0ul, nullptr, &verifier),
      ZX_OK);
  EXPECT_EQ(verifier->Verify(nullptr, 0ul, 0ul), ZX_OK);
  EXPECT_EQ(verifier->VerifyPartial(nullptr, 0ul, 0ul, 0ul), ZX_OK);
}

TEST_P(BlobVerifierTest, CreateAndVerify_SmallBlob) {
  uint8_t buf[8192];
  FillWithRandom(buf, sizeof(buf));

  auto merkle_tree = GenerateTree(buf, sizeof(buf));

  std::unique_ptr<BlobVerifier> verifier;
  ASSERT_EQ(BlobVerifier::CreateWithoutTree(merkle_tree->root, GetMetrics(), sizeof(buf), nullptr,
                                            &verifier),
            ZX_OK);

  EXPECT_EQ(verifier->Verify(buf, sizeof(buf), sizeof(buf)), ZX_OK);

  EXPECT_EQ(verifier->VerifyPartial(buf, 8192, 0, 8192), ZX_OK);

  // Partial ranges
  EXPECT_EQ(verifier->VerifyPartial(buf, 8191, 0, 8191), ZX_ERR_INVALID_ARGS);

  // Verify past the end
  EXPECT_EQ(verifier->VerifyPartial(buf, 2 * 8192, 0, 2 * 8192), ZX_ERR_INVALID_ARGS);
}

TEST_P(BlobVerifierTest, CreateAndVerify_SmallBlob_DataCorrupted) {
  TestCorruptionNotifier notifier;

  uint8_t buf[8192];
  FillWithRandom(buf, sizeof(buf));

  auto merkle_tree = GenerateTree(buf, sizeof(buf));

  // Invert one character
  buf[42] = ~(buf[42]);

  std::unique_ptr<BlobVerifier> verifier;
  ASSERT_EQ(BlobVerifier::CreateWithoutTree(merkle_tree->root, GetMetrics(), sizeof(buf), &notifier,
                                            &verifier),
            ZX_OK);

  EXPECT_EQ(verifier->Verify(buf, sizeof(buf), sizeof(buf)), ZX_ERR_IO_DATA_INTEGRITY);
  EXPECT_EQ(verifier->VerifyPartial(buf, 8192, 0, 8192), ZX_ERR_IO_DATA_INTEGRITY);

  ASSERT_TRUE(notifier.last_corruption());
  EXPECT_EQ(notifier.last_corruption(), merkle_tree->root);
}

TEST_P(BlobVerifierTest, CreateAndVerify_BigBlob) {
  TestCorruptionNotifier notifier;

  size_t sz = 1 << 16;
  fbl::Array<uint8_t> buf(new uint8_t[sz], sz);
  FillWithRandom(buf.get(), sz);

  auto merkle_tree = GenerateTree(buf.get(), sz);

  std::unique_ptr<BlobVerifier> verifier;
  ASSERT_EQ(
      BlobVerifier::Create(merkle_tree->root, GetMetrics(), merkle_tree->merkle_tree.get(),
                           merkle_tree->merkle_tree_size, GetParam(), sz, &notifier, &verifier),
      ZX_OK);

  EXPECT_EQ(verifier->Verify(buf.get(), sz, sz), ZX_OK);

  EXPECT_EQ(verifier->VerifyPartial(buf.get(), sz, 0, sz), ZX_OK);

  // Block-by-block
  for (size_t i = 0; i < sz; i += 8192) {
    EXPECT_EQ(verifier->VerifyPartial(buf.get() + i, 8192, i, 8192), ZX_OK);
  }

  // Partial ranges
  EXPECT_EQ(verifier->VerifyPartial(buf.data(), 8191, 0, 8191), ZX_ERR_INVALID_ARGS);

  // Verify past the end
  EXPECT_EQ(verifier->VerifyPartial(buf.data() + (sz - 8192), 2 * 8192, sz - 8192, 2 * 8192),
            ZX_ERR_INVALID_ARGS);

  // Should be no corruptions.
  EXPECT_FALSE(notifier.last_corruption());
}

TEST_P(BlobVerifierTest, CreateAndVerify_BigBlob_DataCorrupted) {
  TestCorruptionNotifier notifier;

  size_t sz = 1 << 16;
  fbl::Array<uint8_t> buf(new uint8_t[sz], sz);
  FillWithRandom(buf.get(), sz);

  auto merkle_tree = GenerateTree(buf.get(), sz);

  // Invert a char in the first block. All other blocks are still valid.
  buf.get()[42] = ~(buf.get()[42]);

  std::unique_ptr<BlobVerifier> verifier;
  ASSERT_EQ(
      BlobVerifier::Create(merkle_tree->root, GetMetrics(), merkle_tree->merkle_tree.get(),
                           merkle_tree->merkle_tree_size, GetParam(), sz, &notifier, &verifier),
      ZX_OK);

  EXPECT_EQ(verifier->Verify(buf.get(), sz, sz), ZX_ERR_IO_DATA_INTEGRITY);

  EXPECT_EQ(verifier->VerifyPartial(buf.get(), sz, 0, sz), ZX_ERR_IO_DATA_INTEGRITY);

  // Block-by-block -- first block fails, rest succeed
  for (size_t i = 0; i < sz; i += 8192) {
    zx_status_t status = verifier->VerifyPartial(buf.get() + i, 8192, i, 8192);
    if (i == 0) {
      EXPECT_EQ(status, ZX_ERR_IO_DATA_INTEGRITY);
      ASSERT_TRUE(notifier.last_corruption());
      EXPECT_EQ(notifier.last_corruption(), merkle_tree->root);

      // Reset so we can tell it's not called again.
      notifier.ResetLastCorruption();
    } else {
      EXPECT_EQ(status, ZX_OK);
      EXPECT_FALSE(notifier.last_corruption());
    }
  }
}

TEST_P(BlobVerifierTest, CreateAndVerify_BigBlob_MerkleCorrupted) {
  TestCorruptionNotifier notifier;

  size_t sz = 1 << 16;
  fbl::Array<uint8_t> buf(new uint8_t[sz], sz);
  FillWithRandom(buf.get(), sz);

  auto merkle_tree = GenerateTree(buf.get(), sz);

  // Invert a char in the tree.
  merkle_tree->merkle_tree.get()[0] = ~(merkle_tree->merkle_tree.get()[0]);

  std::unique_ptr<BlobVerifier> verifier;
  ASSERT_EQ(
      BlobVerifier::Create(merkle_tree->root, GetMetrics(), merkle_tree->merkle_tree.get(),
                           merkle_tree->merkle_tree_size, GetParam(), sz, &notifier, &verifier),
      ZX_OK);

  EXPECT_EQ(verifier->Verify(buf.get(), sz, sz), ZX_ERR_IO_DATA_INTEGRITY);

  EXPECT_EQ(verifier->VerifyPartial(buf.get(), sz, 0, sz), ZX_ERR_IO_DATA_INTEGRITY);

  // Block-by-block -- everything fails
  for (size_t i = 0; i < sz; i += 8192) {
    EXPECT_EQ(verifier->VerifyPartial(buf.get() + i, 8192, i, 8192), ZX_ERR_IO_DATA_INTEGRITY);

    ASSERT_TRUE(notifier.last_corruption());
    EXPECT_EQ(*notifier.last_corruption(), merkle_tree->root);
    notifier.ResetLastCorruption();
  }
}

TEST_P(BlobVerifierTest, NonZeroTailCausesVerifyToFail) {
  constexpr int kBlobSize = 8000;
  uint8_t buf[kBlobfsBlockSize];
  FillWithRandom(buf, kBlobSize);
  // Zero the tail.
  memset(&buf[kBlobSize], 0, kBlobfsBlockSize - kBlobSize);

  auto merkle_tree = GenerateTree(buf, kBlobSize);

  std::unique_ptr<BlobVerifier> verifier;
  EXPECT_EQ(BlobVerifier::CreateWithoutTree(merkle_tree->root, GetMetrics(), kBlobSize, nullptr,
                                            &verifier),
            ZX_OK);

  EXPECT_EQ(verifier->Verify(buf, kBlobSize, sizeof(buf)), ZX_OK);

  buf[kBlobSize] = 1;
  EXPECT_EQ(verifier->Verify(buf, kBlobSize, sizeof(buf)), ZX_ERR_IO_DATA_INTEGRITY);
}

TEST_P(BlobVerifierTest, NonZeroTailCausesVerifyPartialToFail) {
  constexpr unsigned kBlobSize = (1 << 16) - 100;
  std::vector<uint8_t> buf(fbl::round_up(kBlobSize, kBlobfsBlockSize));
  FillWithRandom(buf.data(), kBlobSize);

  auto merkle_tree = GenerateTree(buf.data(), kBlobSize);

  std::unique_ptr<BlobVerifier> verifier;
  ASSERT_EQ(BlobVerifier::Create(merkle_tree->root, GetMetrics(), merkle_tree->merkle_tree.get(),
                                 merkle_tree->merkle_tree_size, GetParam(), kBlobSize, nullptr,
                                 &verifier),
            ZX_OK);

  constexpr int kVerifyOffset = kBlobSize - kBlobSize % kBlobfsBlockSize;
  EXPECT_EQ(verifier->VerifyPartial(&buf[kVerifyOffset], kBlobSize - kVerifyOffset, kVerifyOffset,
                                    buf.size() - kVerifyOffset),
            ZX_OK);

  buf[kBlobSize] = 1;
  EXPECT_EQ(verifier->VerifyPartial(&buf[kVerifyOffset], kBlobSize - kVerifyOffset, kVerifyOffset,
                                    buf.size() - kVerifyOffset),
            ZX_ERR_IO_DATA_INTEGRITY);
}

std::string GetTestName(const testing::TestParamInfo<BlobLayoutFormat>& param) {
  return GetBlobLayoutFormatNameForTests(param.param);
}

INSTANTIATE_TEST_SUITE_P(/*no prefix*/, BlobVerifierTest,
                         ::testing::Values(BlobLayoutFormat::kPaddedMerkleTreeAtStart,
                                           BlobLayoutFormat::kCompactMerkleTreeAtEnd),
                         GetTestName);

}  // namespace
}  // namespace blobfs
