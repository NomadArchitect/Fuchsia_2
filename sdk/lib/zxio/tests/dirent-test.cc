// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fuchsia/io/llcpp/fidl.h>
#include <fuchsia/io2/llcpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/fidl-async/cpp/bind.h>
#include <lib/zxio/inception.h>
#include <lib/zxio/ops.h>
#include <string.h>

#include <algorithm>
#include <atomic>
#include <memory>

#include <zxtest/zxtest.h>

namespace {

namespace fio = fuchsia_io;
namespace fio2 = fuchsia_io2;

class TestServer final : public fidl::WireServer<fio::Directory> {
 public:
  TestServer() = default;

  constexpr static int kEntryCount = 1000;

  // Exercised by |zxio_close|.
  void Close(CloseRequestView request, CloseCompleter::Sync& completer) override {
    num_close_.fetch_add(1);
    completer.Reply(ZX_OK);
  }

  void Clone(CloneRequestView request, CloneCompleter::Sync& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  void Describe(DescribeRequestView request, DescribeCompleter::Sync& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  void Sync(SyncRequestView request, SyncCompleter::Sync& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  void GetAttr(GetAttrRequestView request, GetAttrCompleter::Sync& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  void SetAttr(SetAttrRequestView request, SetAttrCompleter::Sync& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  void Open(OpenRequestView request, OpenCompleter::Sync& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  void AddInotifyFilter(AddInotifyFilterRequestView request,
                        AddInotifyFilterCompleter::Sync& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  void Unlink(UnlinkRequestView request, UnlinkCompleter::Sync& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  void Unlink2(Unlink2RequestView request, Unlink2Completer::Sync& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  void ReadDirents(ReadDirentsRequestView request, ReadDirentsCompleter::Sync& completer) override {
    auto buffer_start = reinterpret_cast<uint8_t*>(buffer_);
    size_t actual = 0;

    for (; index_ < kEntryCount; index_++) {
      const size_t name_length = std::min(static_cast<size_t>(index_) + 1, fio::wire::kMaxFilename);
      auto buffer_position = buffer_start + actual;

      struct dirent {
        uint64_t inode;
        uint8_t size;
        uint8_t type;
        char name[0];
      } __PACKED;

      auto entry = reinterpret_cast<dirent*>(buffer_position);
      size_t entry_size = sizeof(dirent) + name_length;

      if (actual + entry_size > request->max_bytes) {
        completer.Reply(ZX_OK, fidl::VectorView<uint8_t>::FromExternal(buffer_start, actual));
        return;
      }

      auto name = new char[name_length + 1];
      snprintf(name, name_length + 1, "%0*d", static_cast<int>(name_length), index_);
      // No null termination
      memcpy(entry->name, name, name_length);
      delete[] name;

      if (name_length > UINT8_MAX) {
        return completer.Close(ZX_ERR_BAD_STATE);
      }
      entry->size = static_cast<uint8_t>(name_length);
      entry->inode = index_;

      actual += entry_size;
    }
    completer.Reply(ZX_OK, fidl::VectorView<uint8_t>::FromExternal(buffer_start, actual));
  }

  void Rewind(RewindRequestView request, RewindCompleter::Sync& completer) override {
    memset(buffer_, 0, sizeof(buffer_));
    index_ = 0;
    completer.Reply(ZX_OK);
  }

  void GetToken(GetTokenRequestView request, GetTokenCompleter::Sync& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  void Rename2(Rename2RequestView request, Rename2Completer::Sync& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  void Link(LinkRequestView request, LinkCompleter::Sync& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  void Watch(WatchRequestView request, WatchCompleter::Sync& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  uint32_t num_close() const { return num_close_.load(); }

 private:
  std::atomic<uint32_t> num_close_ = 0;
  char buffer_[fio::wire::kMaxBuf] = {};
  int index_ = 0;
};

class DirentTest : public zxtest::Test {
 public:
  void SetUp() final {
    ASSERT_OK(zx::channel::create(0, &control_client_end_, &control_server_end_));
    ASSERT_OK(zxio_dir_init(&dir_, control_client_end_.release()));
    server_ = std::make_unique<TestServer>();
    loop_ = std::make_unique<async::Loop>(&kAsyncLoopConfigNoAttachToCurrentThread);
    ASSERT_OK(loop_->StartThread("fake-filesystem"));
    ASSERT_OK(fidl::BindSingleInFlightOnly(loop_->dispatcher(), std::move(control_server_end_),
                                           server_.get()));
  }

  void TearDown() final {
    ASSERT_EQ(0, server_->num_close());
    ASSERT_OK(zxio_close(&dir_.io));
    ASSERT_EQ(1, server_->num_close());
  }

 protected:
  zxio_storage_t dir_;
  zx::channel control_client_end_;
  zx::channel control_server_end_;
  std::unique_ptr<TestServer> server_;
  std::unique_ptr<async::Loop> loop_;
};

TEST_F(DirentTest, StandardBufferSize) {
  zxio_dirent_iterator_t iterator;
  ASSERT_OK(zxio_dirent_iterator_init(&iterator, &dir_.io));

  for (int count = 0; count < TestServer::kEntryCount; count++) {
    zxio_dirent_t* entry;
    EXPECT_OK(zxio_dirent_iterator_next(&iterator, &entry));
    EXPECT_TRUE(entry->has.id);
    EXPECT_EQ(entry->id, count);
    const size_t name_length = std::min(static_cast<size_t>(count) + 1, fio::wire::kMaxFilename);
    EXPECT_EQ(entry->name_length, name_length);
  }

  zxio_dirent_iterator_destroy(&iterator);
}

}  // namespace
