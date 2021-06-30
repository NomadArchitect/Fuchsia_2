// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/fidl/epitaph.h>
#include <lib/fidl/llcpp/client.h>
#include <lib/fidl/llcpp/client_base.h>
#include <lib/fidl/llcpp/coding.h>
#include <lib/fidl/llcpp/connect_service.h>
#include <lib/fidl/txn_header.h>
#include <lib/sync/completion.h>
#include <lib/zx/channel.h>
#include <lib/zx/time.h>

#include <mutex>
#include <thread>
#include <vector>

#include <sanitizer/lsan_interface.h>
#include <zxtest/zxtest.h>

#include "mock_client_impl.h"

namespace fidl {
namespace {

using ::fidl_testing::TestProtocol;
using ::fidl_testing::TestResponseContext;

TEST(ClientBindingTestCase, AsyncTxn) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  ASSERT_OK(loop.StartThread());

  auto endpoints = fidl::CreateEndpoints<TestProtocol>();
  ASSERT_OK(endpoints.status_value());
  auto [local, remote] = std::move(*endpoints);

  sync_completion_t unbound;
  Client<TestProtocol> client;

  class EventHandler : public fidl::WireAsyncEventHandler<TestProtocol> {
   public:
    EventHandler(sync_completion_t& unbound, Client<TestProtocol>& client)
        : unbound_(unbound), client_(client) {}

    void Unbound(::fidl::UnbindInfo info) override {
      EXPECT_EQ(fidl::Reason::kPeerClosed, info.reason());
      EXPECT_EQ(ZX_ERR_PEER_CLOSED, info.status());
      EXPECT_EQ("FIDL endpoint was unbound due to peer closed, status: ZX_ERR_PEER_CLOSED (-24)",
                info.FormatDescription());
      EXPECT_EQ(0, client_->GetTxidCount());
      sync_completion_signal(&unbound_);
    }

   private:
    sync_completion_t& unbound_;
    Client<TestProtocol>& client_;
  };

  client.Bind(std::move(local), loop.dispatcher(), std::make_shared<EventHandler>(unbound, client));

  // Generate a txid for a ResponseContext. Send a "response" message with the same txid from the
  // remote end of the channel.
  TestResponseContext context(client.operator->());
  client->PrepareAsyncTxn(&context);
  EXPECT_TRUE(client->IsPending(context.Txid()));
  fidl_message_header_t hdr;
  fidl_init_txn_header(&hdr, context.Txid(), 0);
  ASSERT_OK(remote.channel().write(0, &hdr, sizeof(fidl_message_header_t), nullptr, 0));

  // Trigger unbound handler.
  remote.reset();
  EXPECT_OK(sync_completion_wait(&unbound, ZX_TIME_INFINITE));
}

TEST(ClientBindingTestCase, ParallelAsyncTxns) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  ASSERT_OK(loop.StartThread());

  auto endpoints = fidl::CreateEndpoints<TestProtocol>();
  ASSERT_OK(endpoints.status_value());
  auto [local, remote] = std::move(*endpoints);

  sync_completion_t unbound;
  Client<TestProtocol> client;

  class EventHandler : public fidl::WireAsyncEventHandler<TestProtocol> {
   public:
    EventHandler(sync_completion_t& unbound, Client<TestProtocol>& client)
        : unbound_(unbound), client_(client) {}

    void Unbound(::fidl::UnbindInfo info) override {
      EXPECT_EQ(fidl::Reason::kPeerClosed, info.reason());
      EXPECT_EQ(ZX_ERR_PEER_CLOSED, info.status());
      EXPECT_EQ(0, client_->GetTxidCount());
      sync_completion_signal(&unbound_);
    }

   private:
    sync_completion_t& unbound_;
    Client<TestProtocol>& client_;
  };

  client.Bind(std::move(local), loop.dispatcher(), std::make_shared<EventHandler>(unbound, client));

  // In parallel, simulate 10 async transactions and send "response" messages from the remote end of
  // the channel.
  std::vector<std::unique_ptr<TestResponseContext>> contexts;
  std::thread threads[10];
  for (int i = 0; i < 10; ++i) {
    contexts.emplace_back(std::make_unique<TestResponseContext>(client.operator->()));
    threads[i] = std::thread([context = contexts[i].get(), remote = &remote.channel(), &client] {
      client->PrepareAsyncTxn(context);
      EXPECT_TRUE(client->IsPending(context->Txid()));
      fidl_message_header_t hdr;
      fidl_init_txn_header(&hdr, context->Txid(), 0);
      ASSERT_OK(remote->write(0, &hdr, sizeof(fidl_message_header_t), nullptr, 0));
    });
  }
  for (int i = 0; i < 10; ++i)
    threads[i].join();

  // Trigger unbound handler.
  remote.reset();
  EXPECT_OK(sync_completion_wait(&unbound, ZX_TIME_INFINITE));
}

TEST(ClientBindingTestCase, ForgetAsyncTxn) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  ASSERT_OK(loop.StartThread());

  auto endpoints = fidl::CreateEndpoints<TestProtocol>();
  ASSERT_OK(endpoints.status_value());
  auto [local, remote] = std::move(*endpoints);

  Client<TestProtocol> client(std::move(local), loop.dispatcher());

  // Generate a txid for a ResponseContext.
  TestResponseContext context(client.operator->());
  client->PrepareAsyncTxn(&context);
  EXPECT_TRUE(client->IsPending(context.Txid()));

  // Forget the transaction.
  client->ForgetAsyncTxn(&context);
  EXPECT_EQ(0, client->GetTxidCount());
}

TEST(ClientBindingTestCase, UnknownResponseTxid) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  ASSERT_OK(loop.StartThread());

  auto endpoints = fidl::CreateEndpoints<TestProtocol>();
  ASSERT_OK(endpoints.status_value());
  auto [local, remote] = std::move(*endpoints);

  sync_completion_t unbound;
  Client<TestProtocol> client;

  class EventHandler : public fidl::WireAsyncEventHandler<TestProtocol> {
   public:
    EventHandler(sync_completion_t& unbound, Client<TestProtocol>& client)
        : unbound_(unbound), client_(client) {}

    void Unbound(::fidl::UnbindInfo info) override {
      EXPECT_EQ(fidl::Reason::kUnexpectedMessage, info.reason());
      EXPECT_EQ(ZX_ERR_NOT_FOUND, info.status());
      EXPECT_EQ(
          "FIDL endpoint was unbound due to unexpected message, "
          "status: ZX_ERR_NOT_FOUND (-25), detail: unknown txid",
          info.FormatDescription());
      EXPECT_EQ(0, client_->GetTxidCount());
      sync_completion_signal(&unbound_);
    }

   private:
    sync_completion_t& unbound_;
    Client<TestProtocol>& client_;
  };

  client.Bind(std::move(local), loop.dispatcher(), std::make_shared<EventHandler>(unbound, client));

  // Send a "response" message for which there was no outgoing request.
  ASSERT_EQ(0, client->GetTxidCount());
  fidl_message_header_t hdr;
  fidl_init_txn_header(&hdr, 1, 0);
  ASSERT_OK(remote.channel().write(0, &hdr, sizeof(fidl_message_header_t), nullptr, 0));

  // on_unbound should be triggered by the erroneous response.
  EXPECT_OK(sync_completion_wait(&unbound, ZX_TIME_INFINITE));
}

TEST(ClientBindingTestCase, Events) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  ASSERT_OK(loop.StartThread());

  auto endpoints = fidl::CreateEndpoints<TestProtocol>();
  ASSERT_OK(endpoints.status_value());
  auto [local, remote] = std::move(*endpoints);

  sync_completion_t unbound;
  Client<TestProtocol> client;

  class EventHandler : public fidl::WireAsyncEventHandler<TestProtocol> {
   public:
    EventHandler(sync_completion_t& unbound, Client<TestProtocol>& client)
        : unbound_(unbound), client_(client) {}

    void Unbound(::fidl::UnbindInfo info) override {
      EXPECT_EQ(fidl::Reason::kPeerClosed, info.reason());
      EXPECT_EQ(ZX_ERR_PEER_CLOSED, info.status());
      EXPECT_EQ(10, client_->GetEventCount());  // Expect 10 events.
      sync_completion_signal(&unbound_);
    }

   private:
    sync_completion_t& unbound_;
    Client<TestProtocol>& client_;
  };

  client.Bind(std::move(local), loop.dispatcher(), std::make_shared<EventHandler>(unbound, client));

  // In parallel, send 10 event messages from the remote end of the channel.
  std::thread threads[10];
  for (int i = 0; i < 10; ++i) {
    threads[i] = std::thread([remote = &remote.channel()] {
      fidl_message_header_t hdr;
      fidl_init_txn_header(&hdr, 0, 0);
      ASSERT_OK(remote->write(0, &hdr, sizeof(fidl_message_header_t), nullptr, 0));
    });
  }
  for (int i = 0; i < 10; ++i)
    threads[i].join();

  // Trigger unbound handler.
  remote.reset();
  EXPECT_OK(sync_completion_wait(&unbound, ZX_TIME_INFINITE));
}

TEST(ClientBindingTestCase, UnbindOnInvalidClientShouldPanic) {
  Client<TestProtocol> client;
  ASSERT_DEATH([&] { client.Unbind(); });
}

TEST(ClientBindingTestCase, Unbind) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  ASSERT_OK(loop.StartThread());

  auto endpoints = fidl::CreateEndpoints<TestProtocol>();
  ASSERT_OK(endpoints.status_value());
  auto [local, remote] = std::move(*endpoints);

  sync_completion_t unbound;

  class EventHandler : public fidl::WireAsyncEventHandler<TestProtocol> {
   public:
    explicit EventHandler(sync_completion_t& unbound) : unbound_(unbound) {}

    void Unbound(::fidl::UnbindInfo info) override {
      EXPECT_EQ(fidl::Reason::kUnbind, info.reason());
      EXPECT_OK(info.status());
      sync_completion_signal(&unbound_);
    }

   private:
    sync_completion_t& unbound_;
  };

  Client<TestProtocol> client(std::move(local), loop.dispatcher(),
                              std::make_shared<EventHandler>(unbound));

  // Unbind the client and wait for on_unbound to run.
  client.Unbind();
  EXPECT_OK(sync_completion_wait(&unbound, ZX_TIME_INFINITE));
}

TEST(ClientBindingTestCase, UnbindOnDestroy) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  ASSERT_OK(loop.StartThread());

  auto endpoints = fidl::CreateEndpoints<TestProtocol>();
  ASSERT_OK(endpoints.status_value());
  auto [local, remote] = std::move(*endpoints);

  sync_completion_t unbound;

  class EventHandler : public fidl::WireAsyncEventHandler<TestProtocol> {
   public:
    explicit EventHandler(sync_completion_t& unbound) : unbound_(unbound) {}

    void Unbound(::fidl::UnbindInfo info) override {
      EXPECT_EQ(fidl::Reason::kUnbind, info.reason());
      EXPECT_OK(info.status());
      sync_completion_signal(&unbound_);
    }

   private:
    sync_completion_t& unbound_;
  };

  auto* client = new Client<TestProtocol>(std::move(local), loop.dispatcher(),
                                          std::make_shared<EventHandler>(unbound));

  // Delete the client and wait for on_unbound to run.
  delete client;
  EXPECT_OK(sync_completion_wait(&unbound, ZX_TIME_INFINITE));
}

TEST(ClientBindingTestCase, UnbindWhileActiveChannelRefs) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  ASSERT_OK(loop.StartThread());

  auto endpoints = fidl::CreateEndpoints<TestProtocol>();
  ASSERT_OK(endpoints.status_value());
  auto [local, remote] = std::move(*endpoints);

  sync_completion_t unbound;

  class EventHandler : public fidl::WireAsyncEventHandler<TestProtocol> {
   public:
    explicit EventHandler(sync_completion_t& unbound) : unbound_(unbound) {}

    void Unbound(::fidl::UnbindInfo info) override {
      EXPECT_EQ(fidl::Reason::kUnbind, info.reason());
      EXPECT_OK(info.status());
      sync_completion_signal(&unbound_);
    }

   private:
    sync_completion_t& unbound_;
  };

  Client<TestProtocol> client(std::move(local), loop.dispatcher(),
                              std::make_shared<EventHandler>(unbound));

  // Create a strong reference to the channel.
  auto channel = client->GetChannel();

  // Unbind() and the unbound handler should not be blocked by the channel reference.
  client.Unbind();
  EXPECT_OK(sync_completion_wait(&unbound, ZX_TIME_INFINITE));

  // Check that the channel handle is still valid.
  EXPECT_OK(
      zx_object_get_info(channel->handle(), ZX_INFO_HANDLE_VALID, nullptr, 0, nullptr, nullptr));
}

class OnCanceledTestResponseContext : public internal::ResponseContext {
 public:
  explicit OnCanceledTestResponseContext(sync_completion_t* done)
      : internal::ResponseContext(0), done_(done) {}
  cpp17::optional<fidl::UnbindInfo> OnRawResult(fidl::IncomingMessage&& msg) override {
    ADD_FAILURE("Should not be reached");
    delete this;
    return std::nullopt;
  }
  void OnCanceled() override {
    sync_completion_signal(done_);
    delete this;
  }
  sync_completion_t* done_;
};

TEST(ClientBindingTestCase, ReleaseOutstandingTxnsOnDestroy) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  ASSERT_OK(loop.StartThread());

  auto endpoints = fidl::CreateEndpoints<TestProtocol>();
  ASSERT_OK(endpoints.status_value());
  auto [local, remote] = std::move(*endpoints);

  auto* client = new Client<TestProtocol>(std::move(local), loop.dispatcher());

  // Create and register a response context which will signal when deleted.
  sync_completion_t done;
  (*client)->PrepareAsyncTxn(new OnCanceledTestResponseContext(&done));

  // Delete the client and ensure that the response context is deleted.
  delete client;
  EXPECT_OK(sync_completion_wait(&done, ZX_TIME_INFINITE));
}

class OnErrorTestResponseContext : public internal::ResponseContext {
 public:
  explicit OnErrorTestResponseContext(sync_completion_t* done, fidl::Reason expected_reason)
      : internal::ResponseContext(0), done_(done), expected_reason_(expected_reason) {}
  cpp17::optional<fidl::UnbindInfo> OnRawResult(fidl::IncomingMessage&& msg) override {
    EXPECT_TRUE(!msg.ok());
    EXPECT_EQ(expected_reason_, msg.error().reason());
    sync_completion_signal(done_);
    delete this;
    return std::nullopt;
  }
  void OnCanceled() override {
    ADD_FAILURE("Should not be reached");
    delete this;
  }
  sync_completion_t* done_;
  fidl::Reason expected_reason_;
};

TEST(ClientBindingTestCase, ReleaseOutstandingTxnsOnPeerClosed) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  ASSERT_OK(loop.StartThread());

  auto endpoints = fidl::CreateEndpoints<TestProtocol>();
  ASSERT_OK(endpoints.status_value());
  auto [local, remote] = std::move(*endpoints);

  Client<TestProtocol> client(std::move(local), loop.dispatcher());

  // Create and register a response context which will signal when deleted.
  sync_completion_t done;
  client->PrepareAsyncTxn(new OnErrorTestResponseContext(&done, fidl::Reason::kPeerClosed));

  // Close the server end and wait for the transaction context to be released.
  remote.reset();
  EXPECT_OK(sync_completion_wait(&done, ZX_TIME_INFINITE));
}

TEST(ClientBindingTestCase, Epitaph) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  ASSERT_OK(loop.StartThread());

  auto endpoints = fidl::CreateEndpoints<TestProtocol>();
  ASSERT_OK(endpoints.status_value());
  auto [local, remote] = std::move(*endpoints);

  sync_completion_t unbound;

  class EventHandler : public fidl::WireAsyncEventHandler<TestProtocol> {
   public:
    explicit EventHandler(sync_completion_t& unbound) : unbound_(unbound) {}

    void Unbound(::fidl::UnbindInfo info) override {
      EXPECT_EQ(fidl::Reason::kPeerClosed, info.reason());
      EXPECT_EQ(ZX_ERR_BAD_STATE, info.status());
      sync_completion_signal(&unbound_);
    }

   private:
    sync_completion_t& unbound_;
  };

  Client<TestProtocol> client(std::move(local), loop.dispatcher(),
                              std::make_shared<EventHandler>(unbound));

  // Send an epitaph and wait for on_unbound to run.
  ASSERT_OK(fidl_epitaph_write(remote.channel().get(), ZX_ERR_BAD_STATE));
  EXPECT_OK(sync_completion_wait(&unbound, ZX_TIME_INFINITE));
}

TEST(ClientBindingTestCase, PeerClosedNoEpitaph) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  ASSERT_OK(loop.StartThread());

  auto endpoints = fidl::CreateEndpoints<TestProtocol>();
  ASSERT_OK(endpoints.status_value());
  auto [local, remote] = std::move(*endpoints);

  sync_completion_t unbound;

  class EventHandler : public fidl::WireAsyncEventHandler<TestProtocol> {
   public:
    explicit EventHandler(sync_completion_t& unbound) : unbound_(unbound) {}

    void Unbound(::fidl::UnbindInfo info) override {
      EXPECT_EQ(fidl::Reason::kPeerClosed, info.reason());
      // No epitaph is equivalent to ZX_ERR_PEER_CLOSED epitaph.
      EXPECT_EQ(ZX_ERR_PEER_CLOSED, info.status());
      sync_completion_signal(&unbound_);
    }

   private:
    sync_completion_t& unbound_;
  };

  Client<TestProtocol> client(std::move(local), loop.dispatcher(),
                              std::make_shared<EventHandler>(unbound));

  // Close the server end and wait for on_unbound to run.
  remote.reset();
  EXPECT_OK(sync_completion_wait(&unbound, ZX_TIME_INFINITE));
}

TEST(ChannelRefTrackerTestCase, NoWaitNoHandleLeak) {
  zx::channel local, remote;
  ASSERT_OK(zx::channel::create(0, &local, &remote));

  // Pass ownership of local end of the channel to the ChannelRefTracker.
  auto channel_tracker = new internal::ChannelRefTracker();
  channel_tracker->Init(std::move(local));

  // Destroy the ChannelRefTracker. ZX_SIGNAL_PEER_CLOSED should be asserted on remote.
  delete channel_tracker;
  EXPECT_OK(remote.wait_one(ZX_CHANNEL_PEER_CLOSED, zx::time::infinite_past(), nullptr));
}

TEST(ChannelRefTrackerTestCase, WaitForChannelWithoutRefs) {
  zx::channel local, remote;
  ASSERT_OK(zx::channel::create(0, &local, &remote));
  auto local_handle = local.get();

  // Pass ownership of local end of the channel to the ChannelRefTracker.
  internal::ChannelRefTracker channel_tracker;
  channel_tracker.Init(std::move(local));

  // Retrieve the channel. Check the validity of the handle.
  local = channel_tracker.WaitForChannel();
  ASSERT_EQ(local_handle, local.get());
  ASSERT_OK(local.get_info(ZX_INFO_HANDLE_VALID, nullptr, 0, nullptr, nullptr));

  // Ensure that no new references can be created.
  EXPECT_FALSE(channel_tracker.Get());
}

TEST(ChannelRefTrackerTestCase, WaitForChannelWithRefs) {
  zx::channel local, remote;
  ASSERT_OK(zx::channel::create(0, &local, &remote));
  auto local_handle = local.get();

  // Pass ownership of local end of the channel to the ChannelRefTracker.
  internal::ChannelRefTracker channel_tracker;
  channel_tracker.Init(std::move(local));

  // Get a new reference.
  auto channel_ref = channel_tracker.Get();
  ASSERT_EQ(local_handle, channel_ref->handle());

  // Pass the reference to another thread, then wait for it to be released.
  // NOTE: This is inherently racy but should never fail regardless of the particular state.
  sync_completion_t running;
  std::thread([&running, channel_ref = std::move(channel_ref)]() mutable {
    sync_completion_signal(&running);  // Let the main thread continue.
    channel_ref = nullptr;             // Release this reference.
  }).detach();

  ASSERT_OK(sync_completion_wait(&running, ZX_TIME_INFINITE));

  // Retrieve the channel. Check the validity of the handle.
  local = channel_tracker.WaitForChannel();
  ASSERT_EQ(local_handle, local.get());
  ASSERT_OK(local.get_info(ZX_INFO_HANDLE_VALID, nullptr, 0, nullptr, nullptr));

  // Ensure that no new references can be created.
  EXPECT_FALSE(channel_tracker.Get());
}

TEST(WireClient, UseOnDispatcherThread) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  auto endpoints = fidl::CreateEndpoints<TestProtocol>();
  ASSERT_OK(endpoints.status_value());
  auto [local, remote] = std::move(*endpoints);

  std::optional<fidl::UnbindInfo> error;
  std::thread::id error_handling_thread;
  class EventHandler : public fidl::WireAsyncEventHandler<TestProtocol> {
   public:
    explicit EventHandler(std::optional<fidl::UnbindInfo>& error,
                          std::thread::id& error_handling_thread)
        : error_(error), error_handling_thread_(error_handling_thread) {}
    void on_fidl_error(fidl::UnbindInfo info) override {
      error_handling_thread_ = std::this_thread::get_id();
      error_ = info;
    }

   private:
    std::optional<fidl::UnbindInfo>& error_;
    std::thread::id& error_handling_thread_;
  };
  EventHandler handler(error, error_handling_thread);

  // Create the client on the current thread.
  fidl::WireClient client(std::move(local), loop.dispatcher(), &handler);

  // Dispatch messages on the current thread.
  ASSERT_OK(loop.RunUntilIdle());

  // Trigger an error; receive |on_fidl_error| on the same thread.
  ASSERT_FALSE(error.has_value());
  remote.reset();
  ASSERT_OK(loop.RunUntilIdle());
  ASSERT_TRUE(error.has_value());
  ASSERT_EQ(std::this_thread::get_id(), error_handling_thread);

  // Destroy the client on the same thread.
  client = {};
}

TEST(WireClient, CannotDestroyOnAnotherThread) {
  // Run our test in a thread with LSAN disabled.
  std::thread([&] {
#if __has_feature(address_sanitizer) || __has_feature(leak_sanitizer)
    // Disable LSAN for this thread. It is expected to leak by way of a crash.
    __lsan::ScopedDisabler _;
#endif
    async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
    auto endpoints = fidl::CreateEndpoints<TestProtocol>();
    ASSERT_OK(endpoints.status_value());
    auto [local, remote] = std::move(*endpoints);

    fidl::WireClient client(std::move(local), loop.dispatcher());
    remote.reset();

    // Panics when a foreign thread attempts to destroy the client.
#if ZX_DEBUG_ASSERT_IMPLEMENTED
    std::thread foreign_thread([&] { ASSERT_DEATH([&] { client = {}; }); });
    foreign_thread.join();
#endif
  }).join();
}

TEST(WireClient, CannotDispatchOnAnotherThread) {
  // Run our test in a thread with LSAN disabled.
  std::thread([&] {
#if __has_feature(address_sanitizer) || __has_feature(leak_sanitizer)
    // Disable LSAN for this thread. It is expected to leak by way of a crash.
    __lsan::ScopedDisabler _;
#endif
    async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
    auto endpoints = fidl::CreateEndpoints<TestProtocol>();
    ASSERT_OK(endpoints.status_value());
    auto [local, remote] = std::move(*endpoints);

    fidl::WireClient client(std::move(local), loop.dispatcher());
    remote.reset();

    // Panics when a different thread attempts to dispatch the error.
#if ZX_DEBUG_ASSERT_IMPLEMENTED
    std::thread foreign_thread([&] { ASSERT_DEATH([&] { loop.RunUntilIdle(); }); });
    foreign_thread.join();
#endif
  }).join();
}

}  // namespace
}  // namespace fidl
