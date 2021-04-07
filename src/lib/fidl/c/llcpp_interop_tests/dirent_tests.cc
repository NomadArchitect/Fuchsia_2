// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async-loop/loop.h>
#include <lib/async/cpp/task.h>
#include <lib/fidl-async/cpp/bind.h>
#include <lib/fidl-utils/bind.h>
#include <lib/fidl/llcpp/coding.h>
#include <lib/fidl/txn_header.h>
#include <lib/zx/channel.h>
#include <lib/zx/eventpair.h>
#include <lib/zx/time.h>
#include <zircon/fidl.h>
#include <zircon/syscalls.h>

#include <atomic>
#include <cstdlib>
#include <cstring>
#include <memory>
#include <utility>

#include <fidl/test/llcpp/dirent/c/fidl.h>
#include <zxtest/zxtest.h>

// Interface under test.
#include <fidl/test/llcpp/dirent/llcpp/fidl.h>

// Namespace shorthand for bindings generated code
namespace gen = ::fidl_test_llcpp_dirent;

// Toy test data
namespace {

static_assert(gen::wire::SMALL_DIR_VECTOR_SIZE == 3);

gen::wire::DirEnt golden_dirents_array[gen::wire::SMALL_DIR_VECTOR_SIZE] = {
    gen::wire::DirEnt{
        .is_dir = false,
        .name = fidl::StringView{"ab"},
        .some_flags = 0,
    },
    gen::wire::DirEnt{
        .is_dir = true,
        .name = fidl::StringView{"cde"},
        .some_flags = 1,
    },
    gen::wire::DirEnt{
        .is_dir = false,
        .name = fidl::StringView{"fghi"},
        .some_flags = 2,
    },
};

auto golden_dirents() {
  return fidl::VectorView<gen::wire::DirEnt>::FromExternal(golden_dirents_array);
}

}  // namespace

// Manual server implementation, since the C binding does not support
// types with more than one level of indirection.
// The server is an async loop that reads messages from the channel.
// It uses the llcpp raw API to decode the message, then calls one of the handlers.
namespace manual_server {

class Server {
 public:
  Server(zx::channel chan)
      : chan_(std::move(chan)), loop_(&kAsyncLoopConfigNoAttachToCurrentThread) {}

  zx_status_t Start() {
    zx_status_t status = loop_.StartThread("llcpp_manual_server");
    if (status != ZX_OK) {
      return status;
    }
    return fidl_bind(loop_.dispatcher(), chan_.get(), Server::FidlDispatch, this, nullptr);
  }

  uint64_t CountNumDirectoriesNumCalls() const { return count_num_directories_num_calls_.load(); }

  uint64_t ReadDirNumCalls() const { return read_dir_num_calls_.load(); }

  uint64_t ConsumeDirectoriesNumCalls() const { return consume_directories_num_calls_.load(); }

  uint64_t OneWayDirentsNumCalls() const { return one_way_dirents_num_calls_.load(); }

 private:
  template <typename FidlType>
  zx_status_t Reply(fidl_txn_t* txn, FidlType* value) {
    fidl::OwnedEncodedMessage<FidlType> encoded(value);
    zx_status_t status = txn->reply(txn, encoded.GetOutgoingMessage().message());
    encoded.GetOutgoingMessage().ReleaseHandles();
    return status;
  }

  zx_status_t DoCountNumDirectories(
      fidl_txn_t* txn,
      fidl::DecodedMessage<gen::DirEntTestInterface::CountNumDirectoriesRequest>& decoded) {
    count_num_directories_num_calls_.fetch_add(1);
    const auto& request = *decoded.PrimaryObject();
    int64_t count = 0;
    for (const auto& dirent : request.dirents) {
      if (dirent.is_dir) {
        count++;
      }
    }
    gen::DirEntTestInterface::CountNumDirectoriesResponse response(count);
    response._hdr.txid = request._hdr.txid;
    return Reply(txn, &response);
  }

  zx_status_t DoReadDir(fidl_txn_t* txn,
                        fidl::DecodedMessage<gen::DirEntTestInterface::ReadDirRequest>& decoded) {
    read_dir_num_calls_.fetch_add(1);
    auto golden = golden_dirents();
    gen::DirEntTestInterface::ReadDirResponse response(golden);
    response._hdr.txid = decoded.PrimaryObject()->_hdr.txid;
    return Reply(txn, &response);
  }

  zx_status_t DoConsumeDirectories(
      fidl_txn_t* txn,
      fidl::DecodedMessage<gen::DirEntTestInterface::ConsumeDirectoriesRequest>& decoded) {
    consume_directories_num_calls_.fetch_add(1);
    EXPECT_EQ(decoded.PrimaryObject()->dirents.count(), 3);
    gen::DirEntTestInterface::ConsumeDirectoriesResponse response;
    fidl_init_txn_header(&response._hdr, 0, decoded.PrimaryObject()->_hdr.ordinal);
    return Reply(txn, &response);
  }

  zx_status_t DoOneWayDirents(
      fidl_txn_t* txn,
      fidl::DecodedMessage<gen::DirEntTestInterface::OneWayDirentsRequest>& decoded) {
    one_way_dirents_num_calls_.fetch_add(1);
    EXPECT_EQ(decoded.PrimaryObject()->dirents.count(), 3);
    EXPECT_OK(decoded.PrimaryObject()->ep.signal_peer(0, ZX_EVENTPAIR_SIGNALED));
    // No response required for one-way calls.
    return ZX_OK;
  }

  template <typename FidlType>
  static fidl::DecodedMessage<FidlType> DecodeAs(fidl_incoming_msg_t* msg) {
    return fidl::DecodedMessage<FidlType>(msg);
  }

  static zx_status_t FidlDispatch(void* ctx, fidl_txn_t* txn, fidl_incoming_msg_t* msg,
                                  const void* ops) {
    if (msg->num_bytes < sizeof(fidl_message_header_t)) {
      FidlHandleInfoCloseMany(msg->handles, msg->num_handles);
      return ZX_ERR_INVALID_ARGS;
    }
    fidl_message_header_t* hdr = reinterpret_cast<fidl_message_header_t*>(msg->bytes);
    Server* server = reinterpret_cast<Server*>(ctx);
    switch (hdr->ordinal) {
      case fidl_test_llcpp_dirent_DirEntTestInterfaceCountNumDirectoriesOrdinal: {
        auto result = DecodeAs<gen::DirEntTestInterface::CountNumDirectoriesRequest>(msg);
        if (!result.ok()) {
          return result.status();
        }
        return server->DoCountNumDirectories(txn, result);
      }
      case fidl_test_llcpp_dirent_DirEntTestInterfaceReadDirOrdinal: {
        auto result = DecodeAs<gen::DirEntTestInterface::ReadDirRequest>(msg);
        if (!result.ok()) {
          return result.status();
        }
        return server->DoReadDir(txn, result);
      }
      case fidl_test_llcpp_dirent_DirEntTestInterfaceConsumeDirectoriesOrdinal: {
        auto result = DecodeAs<gen::DirEntTestInterface::ConsumeDirectoriesRequest>(msg);
        if (!result.ok()) {
          return result.status();
        }
        return server->DoConsumeDirectories(txn, result);
      }
      case fidl_test_llcpp_dirent_DirEntTestInterfaceOneWayDirentsOrdinal: {
        auto result = DecodeAs<gen::DirEntTestInterface::OneWayDirentsRequest>(msg);
        if (!result.ok()) {
          return result.status();
        }
        return server->DoOneWayDirents(txn, result);
      }
      default:
        return ZX_ERR_NOT_SUPPORTED;
    }
  }

  zx::channel chan_;
  async::Loop loop_;

  std::atomic<uint64_t> count_num_directories_num_calls_ = 0;
  std::atomic<uint64_t> read_dir_num_calls_ = 0;
  std::atomic<uint64_t> consume_directories_num_calls_ = 0;
  std::atomic<uint64_t> one_way_dirents_num_calls_ = 0;
};

}  // namespace manual_server

// Server implemented with low-level C++ FIDL bindings
namespace llcpp_server {

class ServerBase : public fidl::WireInterface<gen::DirEntTestInterface> {
 public:
  ServerBase(zx::channel chan)
      : chan_(std::move(chan)), loop_(&kAsyncLoopConfigNoAttachToCurrentThread) {}

  zx_status_t Start() {
    zx_status_t status = loop_.StartThread("llcpp_bindings_server");
    if (status != ZX_OK) {
      return status;
    }
    return fidl::BindSingleInFlightOnly(loop_.dispatcher(), std::move(chan_), this);
  }

  uint64_t CountNumDirectoriesNumCalls() const { return count_num_directories_num_calls_.load(); }

  uint64_t ReadDirNumCalls() const { return read_dir_num_calls_.load(); }

  uint64_t ConsumeDirectoriesNumCalls() const { return consume_directories_num_calls_.load(); }

  uint64_t OneWayDirentsNumCalls() const { return one_way_dirents_num_calls_.load(); }

 protected:
  async_dispatcher_t* dispatcher() const { return loop_.dispatcher(); }

  std::atomic<uint64_t> count_num_directories_num_calls_ = 0;
  std::atomic<uint64_t> read_dir_num_calls_ = 0;
  std::atomic<uint64_t> consume_directories_num_calls_ = 0;
  std::atomic<uint64_t> one_way_dirents_num_calls_ = 0;

 private:
  zx::channel chan_;
  async::Loop loop_;
};

// There are three implementations each exercising a different flavor of the reply API:
// C-style, caller-allocating, in-place, and async.

class CFlavorServer : public ServerBase {
 public:
  CFlavorServer(zx::channel chan) : ServerBase(std::move(chan)) {}

  void CountNumDirectories(fidl::VectorView<gen::wire::DirEnt> dirents,
                           CountNumDirectoriesCompleter::Sync& txn) override {
    count_num_directories_num_calls_.fetch_add(1);
    int64_t count = 0;
    for (const auto& dirent : dirents) {
      if (dirent.is_dir) {
        count++;
      }
    }
    txn.Reply(count);
  }

  void ReadDir(ReadDirCompleter::Sync& txn) override {
    read_dir_num_calls_.fetch_add(1);
    txn.Reply(golden_dirents());
  }

  // |ConsumeDirectories| has zero number of arguments in its return value, hence only the
  // C-flavor reply API is generated.
  void ConsumeDirectories(fidl::VectorView<gen::wire::DirEnt> dirents,
                          ConsumeDirectoriesCompleter::Sync& txn) override {
    consume_directories_num_calls_.fetch_add(1);
    EXPECT_EQ(dirents.count(), 3);
    txn.Reply();
  }

  // |OneWayDirents| has no return value, hence there is no reply API generated
  void OneWayDirents(fidl::VectorView<gen::wire::DirEnt> dirents, zx::eventpair ep,
                     OneWayDirentsCompleter::Sync& txn) override {
    one_way_dirents_num_calls_.fetch_add(1);
    EXPECT_EQ(dirents.count(), 3);
    EXPECT_OK(ep.signal_peer(0, ZX_EVENTPAIR_SIGNALED));
    // No response required for one-way calls.
  }
};

class CallerAllocateServer : public ServerBase {
 public:
  CallerAllocateServer(zx::channel chan) : ServerBase(std::move(chan)) {}

  void CountNumDirectories(fidl::VectorView<gen::wire::DirEnt> dirents,
                           CountNumDirectoriesCompleter::Sync& txn) override {
    count_num_directories_num_calls_.fetch_add(1);
    int64_t count = 0;
    for (const auto& dirent : dirents) {
      if (dirent.is_dir) {
        count++;
      }
    }
    fidl::Buffer<gen::DirEntTestInterface::CountNumDirectoriesResponse> buffer;
    txn.Reply(buffer.view(), count);
  }

  void ReadDir(ReadDirCompleter::Sync& txn) override {
    read_dir_num_calls_.fetch_add(1);
    fidl::Buffer<gen::DirEntTestInterface::ReadDirResponse> buffer;
    txn.Reply(buffer.view(), golden_dirents());
  }

  // |ConsumeDirectories| has zero number of arguments in its return value, hence only the
  // C-flavor reply API is applicable.
  void ConsumeDirectories(fidl::VectorView<gen::wire::DirEnt> dirents,
                          ConsumeDirectoriesCompleter::Sync& txn) override {
    ZX_ASSERT_MSG(false, "Never used by unit tests");
  }

  // |OneWayDirents| has no return value, hence there is no reply API generated
  void OneWayDirents(fidl::VectorView<gen::wire::DirEnt> dirents, zx::eventpair ep,
                     OneWayDirentsCompleter::Sync&) override {
    ZX_ASSERT_MSG(false, "Never used by unit tests");
  }
};

// Every reply is delayed using async::PostTask
class AsyncReplyServer : public ServerBase {
 public:
  AsyncReplyServer(zx::channel chan) : ServerBase(std::move(chan)) {}

  void CountNumDirectories(fidl::VectorView<gen::wire::DirEnt> dirents,
                           CountNumDirectoriesCompleter::Sync& txn) override {
    count_num_directories_num_calls_.fetch_add(1);
    int64_t count = 0;
    for (const auto& dirent : dirents) {
      if (dirent.is_dir) {
        count++;
      }
    }
    async::PostTask(dispatcher(), [txn = txn.ToAsync(), count]() mutable { txn.Reply(count); });
  }

  void ReadDir(ReadDirCompleter::Sync& txn) override {
    read_dir_num_calls_.fetch_add(1);
    async::PostTask(dispatcher(), [txn = txn.ToAsync()]() mutable { txn.Reply(golden_dirents()); });
  }

  void ConsumeDirectories(fidl::VectorView<gen::wire::DirEnt> dirents,
                          ConsumeDirectoriesCompleter::Sync& txn) override {
    consume_directories_num_calls_.fetch_add(1);
    EXPECT_EQ(dirents.count(), 3);
    async::PostTask(dispatcher(), [txn = txn.ToAsync()]() mutable { txn.Reply(); });
  }

  // |OneWayDirents| has no return value, hence there is no reply API generated
  void OneWayDirents(fidl::VectorView<gen::wire::DirEnt> dirents, zx::eventpair ep,
                     OneWayDirentsCompleter::Sync&) override {
    ZX_ASSERT_MSG(false, "Never used by unit tests");
  }
};

}  // namespace llcpp_server

// Parametric tests allowing choosing a custom server implementation
namespace {

class Random {
 public:
  Random(
      unsigned int seed = static_cast<unsigned int>(zxtest::Runner::GetInstance()->random_seed()))
      : seed_(seed) {}

  unsigned int seed() const { return seed_; }

  unsigned int UpTo(unsigned int limit) {
    unsigned int next = rand_r(&seed_);
    return next % limit;
  }

 private:
  unsigned int seed_;
};

template <size_t kNumDirents>
std::array<gen::wire::DirEnt, kNumDirents> RandomlyFillDirEnt(char* name) {
  Random random;
  std::array<gen::wire::DirEnt, kNumDirents> dirents;
  for (size_t i = 0; i < kNumDirents; i++) {
    int str_len = random.UpTo(gen::wire::TEST_MAX_PATH) + 1;
    bool is_dir = random.UpTo(2) == 0;
    int32_t flags = static_cast<int32_t>(random.UpTo(1000));
    dirents[i] = gen::wire::DirEnt{
        .is_dir = is_dir,
        .name = fidl::StringView::FromExternal(name, static_cast<uint64_t>(str_len)),
        .some_flags = flags};
  }
  return dirents;
}

template <typename Server>
void SimpleCountNumDirectories() {
  zx::channel client_chan, server_chan;
  ASSERT_OK(zx::channel::create(0, &client_chan, &server_chan));
  Server server(std::move(server_chan));
  ASSERT_OK(server.Start());
  fidl::WireSyncClient<gen::DirEntTestInterface> client(std::move(client_chan));

  constexpr size_t kNumDirents = 80;
  std::unique_ptr<char[]> name(new char[gen::wire::TEST_MAX_PATH]);
  for (uint32_t i = 0; i < gen::wire::TEST_MAX_PATH; i++) {
    name[i] = 'A';
  }
  ASSERT_EQ(server.CountNumDirectoriesNumCalls(), 0);
  constexpr uint64_t kNumIterations = 100;
  // Stress test linearizing dirents
  for (uint64_t iter = 0; iter < kNumIterations; iter++) {
    auto dirents = RandomlyFillDirEnt<kNumDirents>(name.get());
    auto result =
        client.CountNumDirectories(fidl::VectorView<gen::wire::DirEnt>::FromExternal(dirents));
    int64_t expected_num_dir = 0;
    for (const auto& dirent : dirents) {
      if (dirent.is_dir) {
        expected_num_dir++;
      }
    }
    ASSERT_OK(result.status());
    ASSERT_EQ(expected_num_dir, result.Unwrap()->num_dir);
  }
  ASSERT_EQ(server.CountNumDirectoriesNumCalls(), kNumIterations);
}

template <typename Server>
void CallerAllocateCountNumDirectories() {
  zx::channel client_chan, server_chan;
  ASSERT_OK(zx::channel::create(0, &client_chan, &server_chan));
  Server server(std::move(server_chan));
  ASSERT_OK(server.Start());
  fidl::WireSyncClient<gen::DirEntTestInterface> client(std::move(client_chan));

  Random random;
  constexpr size_t kNumDirents = 80;
  std::unique_ptr<char[]> name(new char[gen::wire::TEST_MAX_PATH]);
  for (uint32_t i = 0; i < gen::wire::TEST_MAX_PATH; i++) {
    name[i] = 'B';
  }
  ASSERT_EQ(server.CountNumDirectoriesNumCalls(), 0);
  constexpr uint64_t kNumIterations = 100;
  // Stress test linearizing dirents
  for (uint64_t iter = 0; iter < kNumIterations; iter++) {
    auto dirents = RandomlyFillDirEnt<kNumDirents>(name.get());
    fidl::Buffer<gen::DirEntTestInterface::CountNumDirectoriesRequest> request_buffer;
    fidl::Buffer<gen::DirEntTestInterface::CountNumDirectoriesResponse> response_buffer;
    auto result = client.CountNumDirectories(
        request_buffer.view(), fidl::VectorView<gen::wire::DirEnt>::FromExternal(dirents),
        response_buffer.view());
    int64_t expected_num_dir = 0;
    for (const auto& dirent : dirents) {
      if (dirent.is_dir) {
        expected_num_dir++;
      }
    }
    ASSERT_OK(result.status());
    ASSERT_NULL(result.error());
    ASSERT_EQ(expected_num_dir, result.Unwrap()->num_dir);
  }
  ASSERT_EQ(server.CountNumDirectoriesNumCalls(), kNumIterations);
}

template <typename Server>
void CallerAllocateReadDir() {
  zx::channel client_chan, server_chan;
  ASSERT_OK(zx::channel::create(0, &client_chan, &server_chan));
  Server server(std::move(server_chan));
  ASSERT_OK(server.Start());
  fidl::WireSyncClient<gen::DirEntTestInterface> client(std::move(client_chan));

  ASSERT_EQ(server.ReadDirNumCalls(), 0);
  constexpr uint64_t kNumIterations = 100;
  // Stress test server-linearizing dirents
  for (uint64_t iter = 0; iter < kNumIterations; iter++) {
    fidl::Buffer<gen::DirEntTestInterface::ReadDirResponse> buffer;
    auto result = client.ReadDir(buffer.view());
    ASSERT_OK(result.status());
    ASSERT_NULL(result.error(), "%s", result.error());
    const auto& dirents = result.Unwrap()->dirents;
    ASSERT_EQ(dirents.count(), golden_dirents().count());
    for (uint64_t i = 0; i < dirents.count(); i++) {
      auto& actual = dirents[i];
      auto& expected = golden_dirents()[i];
      EXPECT_EQ(actual.is_dir, expected.is_dir);
      EXPECT_EQ(actual.some_flags, expected.some_flags);
      ASSERT_EQ(actual.name.size(), expected.name.size());
      EXPECT_BYTES_EQ(reinterpret_cast<const uint8_t*>(actual.name.data()),
                      reinterpret_cast<const uint8_t*>(expected.name.data()), actual.name.size(),
                      "dirent name mismatch");
    }
  }
  ASSERT_EQ(server.ReadDirNumCalls(), kNumIterations);
}

template <typename Server>
void SimpleConsumeDirectories() {
  zx::channel client_chan, server_chan;
  ASSERT_OK(zx::channel::create(0, &client_chan, &server_chan));
  Server server(std::move(server_chan));
  ASSERT_OK(server.Start());
  fidl::WireSyncClient<gen::DirEntTestInterface> client(std::move(client_chan));

  ASSERT_EQ(server.ConsumeDirectoriesNumCalls(), 0);
  ASSERT_OK(client.ConsumeDirectories(golden_dirents()).status());
  ASSERT_EQ(server.ConsumeDirectoriesNumCalls(), 1);
}

template <typename Server>
void CallerAllocateConsumeDirectories() {
  zx::channel client_chan, server_chan;
  ASSERT_OK(zx::channel::create(0, &client_chan, &server_chan));
  Server server(std::move(server_chan));
  ASSERT_OK(server.Start());
  fidl::WireSyncClient<gen::DirEntTestInterface> client(std::move(client_chan));

  ASSERT_EQ(server.ConsumeDirectoriesNumCalls(), 0);
  fidl::Buffer<gen::DirEntTestInterface::ConsumeDirectoriesRequest> request_buffer;
  fidl::Buffer<gen::DirEntTestInterface::ConsumeDirectoriesResponse> response_buffer;
  auto result =
      client.ConsumeDirectories(request_buffer.view(), golden_dirents(), response_buffer.view());
  ASSERT_OK(result.status());
  ASSERT_NULL(result.error(), "%s", result.error());
  ASSERT_EQ(server.ConsumeDirectoriesNumCalls(), 1);
}

template <typename Server>
void SimpleOneWayDirents() {
  zx::channel client_chan, server_chan;
  ASSERT_OK(zx::channel::create(0, &client_chan, &server_chan));
  Server server(std::move(server_chan));
  ASSERT_OK(server.Start());
  fidl::WireSyncClient<gen::DirEntTestInterface> client(std::move(client_chan));

  zx::eventpair client_ep, server_ep;
  ASSERT_OK(zx::eventpair::create(0, &client_ep, &server_ep));
  ASSERT_EQ(server.OneWayDirentsNumCalls(), 0);
  ASSERT_OK(client.OneWayDirents(golden_dirents(), std::move(server_ep)).status());
  zx_signals_t signals = 0;
  client_ep.wait_one(ZX_EVENTPAIR_SIGNALED, zx::time::infinite(), &signals);
  ASSERT_EQ(signals & ZX_EVENTPAIR_SIGNALED, ZX_EVENTPAIR_SIGNALED);
  ASSERT_EQ(server.OneWayDirentsNumCalls(), 1);
}

template <typename Server>
void CallerAllocateOneWayDirents() {
  zx::channel client_chan, server_chan;
  ASSERT_OK(zx::channel::create(0, &client_chan, &server_chan));
  Server server(std::move(server_chan));
  ASSERT_OK(server.Start());
  fidl::WireSyncClient<gen::DirEntTestInterface> client(std::move(client_chan));

  zx::eventpair client_ep, server_ep;
  ASSERT_OK(zx::eventpair::create(0, &client_ep, &server_ep));
  ASSERT_EQ(server.OneWayDirentsNumCalls(), 0);
  fidl::Buffer<gen::DirEntTestInterface::OneWayDirentsRequest> buffer;
  ASSERT_OK(client.OneWayDirents(buffer.view(), golden_dirents(), std::move(server_ep)).status());
  zx_signals_t signals = 0;
  client_ep.wait_one(ZX_EVENTPAIR_SIGNALED, zx::time::infinite(), &signals);
  ASSERT_EQ(signals & ZX_EVENTPAIR_SIGNALED, ZX_EVENTPAIR_SIGNALED);
  ASSERT_EQ(server.OneWayDirentsNumCalls(), 1);
}

template <typename DirentArray>
void AssertReadOnDirentsEvent(zx::channel chan, const DirentArray& expected_dirents) {
  class EventHandler : public fidl::WireSyncEventHandler<gen::DirEntTestInterface> {
   public:
    explicit EventHandler(const DirentArray& expected_dirents)
        : expected_dirents_(expected_dirents) {}

    zx_status_t status() const { return status_; }

    void OnDirents(gen::DirEntTestInterface::OnDirentsResponse* event) override {
      EXPECT_EQ(event->dirents.count(), expected_dirents_.size());
      if (event->dirents.count() != expected_dirents_.size()) {
        status_ = ZX_ERR_INVALID_ARGS;
      } else {
        for (uint64_t i = 0; i < event->dirents.count(); i++) {
          EXPECT_EQ(event->dirents[i].is_dir, expected_dirents_[i].is_dir);
          EXPECT_EQ(event->dirents[i].some_flags, expected_dirents_[i].some_flags);
          EXPECT_EQ(event->dirents[i].name.size(), expected_dirents_[i].name.size());
          EXPECT_BYTES_EQ(reinterpret_cast<const uint8_t*>(event->dirents[i].name.data()),
                          reinterpret_cast<const uint8_t*>(expected_dirents_[i].name.data()),
                          event->dirents[i].name.size(), "dirent name mismatch");
        }
      }
    }

    zx_status_t Unknown() override {
      ADD_FAILURE("unknown event received; expected OnDirents");
      return ZX_ERR_INVALID_ARGS;
    }

   private:
    const DirentArray& expected_dirents_;
    zx_status_t status_ = ZX_OK;
  };

  EventHandler event_handler(expected_dirents);
  ::fidl::Result result = event_handler.HandleOneEvent(zx::unowned_channel(chan));
  ASSERT_OK(result.status());
  ASSERT_OK(event_handler.status());
}

}  // namespace

TEST(DirentServerTest, CFlavorSendOnDirents) {
  zx::channel client_chan, server_chan;
  ASSERT_OK(zx::channel::create(0, &client_chan, &server_chan));

  constexpr size_t kNumDirents = 80;
  std::unique_ptr<char[]> name(new char[gen::wire::TEST_MAX_PATH]);
  for (uint32_t i = 0; i < gen::wire::TEST_MAX_PATH; i++) {
    name[i] = 'A';
  }
  auto dirents = RandomlyFillDirEnt<kNumDirents>(name.get());
  fidl::WireEventSender<gen::DirEntTestInterface> event_sender(std::move(server_chan));
  auto status = event_sender.OnDirents(fidl::VectorView<gen::wire::DirEnt>::FromExternal(dirents));
  ASSERT_OK(status);
  ASSERT_NO_FATAL_FAILURES(AssertReadOnDirentsEvent(std::move(client_chan), dirents));
}

TEST(DirentServerTest, CallerAllocateSendOnDirents) {
  zx::channel client_chan, server_chan;
  ASSERT_OK(zx::channel::create(0, &client_chan, &server_chan));

  constexpr size_t kNumDirents = 80;
  std::unique_ptr<char[]> name(new char[gen::wire::TEST_MAX_PATH]);
  for (uint32_t i = 0; i < gen::wire::TEST_MAX_PATH; i++) {
    name[i] = 'B';
  }
  auto dirents = RandomlyFillDirEnt<kNumDirents>(name.get());
  auto buffer = std::make_unique<fidl::Buffer<gen::DirEntTestInterface::OnDirentsResponse>>();
  fidl::WireEventSender<gen::DirEntTestInterface> event_sender(std::move(server_chan));
  auto status = event_sender.OnDirents(buffer->view(),
                                       fidl::VectorView<gen::wire::DirEnt>::FromExternal(dirents));
  ASSERT_OK(status);
  ASSERT_NO_FATAL_FAILURES(AssertReadOnDirentsEvent(std::move(client_chan), dirents));
}

// Parameterized tests
TEST(DirentClientTest, SimpleCountNumDirectories) {
  SimpleCountNumDirectories<manual_server::Server>();
}

TEST(DirentClientTest, CallerAllocateCountNumDirectories) {
  CallerAllocateCountNumDirectories<manual_server::Server>();
}

TEST(DirentClientTest, CallerAllocateReadDir) { CallerAllocateReadDir<manual_server::Server>(); }

TEST(DirentClientTest, SimpleConsumeDirectories) {
  SimpleConsumeDirectories<manual_server::Server>();
}

TEST(DirentClientTest, CallerAllocateConsumeDirectories) {
  CallerAllocateConsumeDirectories<manual_server::Server>();
}

TEST(DirentClientTest, SimpleOneWayDirents) { SimpleOneWayDirents<manual_server::Server>(); }

TEST(DirentClientTest, CallerAllocateOneWayDirents) {
  CallerAllocateOneWayDirents<manual_server::Server>();
}

TEST(DirentServerTest, SimpleCountNumDirectoriesWithCFlavorServer) {
  SimpleCountNumDirectories<llcpp_server::CFlavorServer>();
}

TEST(DirentServerTest, SimpleCountNumDirectoriesWithCallerAllocateServer) {
  SimpleCountNumDirectories<llcpp_server::CallerAllocateServer>();
}

TEST(DirentServerTest, SimpleCountNumDirectoriesWithAsyncReplyServer) {
  SimpleCountNumDirectories<llcpp_server::AsyncReplyServer>();
}

TEST(DirentServerTest, SimpleConsumeDirectoriesWithCFlavorServer) {
  SimpleConsumeDirectories<llcpp_server::CFlavorServer>();
}

TEST(DirentServerTest, SimpleConsumeDirectoriesWithAsyncReplyServer) {
  SimpleConsumeDirectories<llcpp_server::AsyncReplyServer>();
}

TEST(DirentServerTest, SimpleOneWayDirentsWithCFlavorServer) {
  SimpleOneWayDirents<llcpp_server::CFlavorServer>();
}
