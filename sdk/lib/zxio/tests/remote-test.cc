// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.io/cpp/wire.h>
#include <fidl/fuchsia.io/cpp/wire_test_base.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/fidl-async/cpp/bind.h>
#include <lib/sync/completion.h>
#include <lib/zxio/zxio.h>

#include <atomic>
#include <memory>

#include <zxtest/zxtest.h>

#include "sdk/lib/zxio/private.h"

namespace {

namespace fio = fuchsia_io;

class TestServerBase : public fidl::testing::WireTestBase<fio::Node> {
 public:
  TestServerBase() = default;
  ~TestServerBase() override = default;

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) final {
    ADD_FAILURE("unexpected message received: %s", name.c_str());
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  // Exercised by |zxio_close|.
  void Close(CloseCompleter::Sync& completer) override {
    num_close_.fetch_add(1);
    completer.ReplySuccess();
    // After the reply, we should close the connection.
    completer.Close(ZX_OK);
  }

  void DescribeDeprecated(DescribeDeprecatedCompleter::Sync& completer) override {
    fio::wire::FileObject file_object;
    completer.Reply(fio::wire::NodeInfoDeprecated::WithFile(
        fidl::ObjectView<fio::wire::FileObject>::FromExternal(&file_object)));
  }

  uint32_t num_close() const { return num_close_.load(); }

 private:
  std::atomic<uint32_t> num_close_ = 0;
};

class Remote : public zxtest::Test {
 public:
  void SetUp() final {
    zx::status control_client_end = fidl::CreateEndpoints(&control_server_);
    ASSERT_OK(control_client_end.status_value());
    ASSERT_OK(zx::eventpair::create(0, &eventpair_to_client_, &eventpair_on_server_));
    ASSERT_OK(zxio_remote_init(&remote_, std::move(eventpair_to_client_),
                               std::move(control_client_end.value()), /*is_tty=*/false));
  }

  template <typename ServerImpl>
  ServerImpl* StartServer() {
    server_ = std::make_unique<ServerImpl>();
    loop_ = std::make_unique<async::Loop>(&kAsyncLoopConfigNoAttachToCurrentThread);
    zx_status_t status;
    EXPECT_OK(status = loop_->StartThread("fake-filesystem"));
    if (status != ZX_OK) {
      return nullptr;
    }

    EXPECT_OK(fidl::BindSingleInFlightOnly(loop_->dispatcher(), std::move(control_server_),
                                           server_.get()));
    if (status != ZX_OK) {
      return nullptr;
    }
    return static_cast<ServerImpl*>(server_.get());
  }

  void TearDown() final {
    ASSERT_EQ(0, server_->num_close());
    ASSERT_OK(zxio_close(&remote_.io));
    ASSERT_EQ(1, server_->num_close());
  }

 protected:
  zxio_storage_t remote_;
  fidl::ServerEnd<fio::Node> control_server_;

  zx::eventpair eventpair_on_server_;
  zx::eventpair eventpair_to_client_;
  std::unique_ptr<TestServerBase> server_;
  std::unique_ptr<async::Loop> loop_;
};

TEST_F(Remote, ServiceGetAttributes) {
  class TestServer : public TestServerBase {
   public:
    void GetAttr(GetAttrCompleter::Sync& completer) override {
      completer.Reply(ZX_OK,
                      fuchsia_io::wire::NodeAttributes{.mode = fuchsia_io::wire::kModeTypeService});
    }
  };
  ASSERT_NO_FAILURES(StartServer<TestServer>());

  zxio_node_attributes_t attr = {};
  ASSERT_OK(zxio_attr_get(&remote_.io, &attr));
  EXPECT_EQ(ZXIO_NODE_PROTOCOL_FILE, attr.protocols);
}

TEST_F(Remote, Borrow) {
  ASSERT_NO_FAILURES(StartServer<TestServerBase>());

  zx_handle_t handle = ZX_HANDLE_INVALID;
  EXPECT_OK(zxio_borrow(&remote_.io, &handle));
  EXPECT_NE(handle, ZX_HANDLE_INVALID);
}

class TestCloneServer : public TestServerBase {
 public:
  using CloneFunc = fit::function<void(CloneRequestView request, CloneCompleter::Sync& completer)>;

  void set_clone_func(CloneFunc clone_func) { clone_func_ = std::move(clone_func); }

  void Clone(CloneRequestView request, CloneCompleter::Sync& completer) override {
    clone_func_(request, completer);
  }

 private:
  CloneFunc clone_func_;
};

class CloneTest : public zxtest::Test {
 public:
  CloneTest() : server_loop_(&kAsyncLoopConfigNoAttachToCurrentThread) {}

  void SetUp() final {
    zx::status node_ends = fidl::CreateEndpoints<fio::Node>();
    ASSERT_OK(node_ends.status_value());
    node_client_end_ = std::move(node_ends->client);

    node_server_.set_clone_func(
        [this](TestCloneServer::CloneRequestView request,
               TestCloneServer::CloneCompleter::Sync& completer) { Clone(request, completer); });

    fidl::BindServer(server_loop_.dispatcher(), std::move(node_ends->server), &node_server_);

    ASSERT_OK(server_loop_.StartThread("fake-filesystem"));
  }

  void TearDown() final { server_loop_.Shutdown(); }

  fidl::ClientEnd<fio::Node> TakeClientEnd() { return std::move(node_client_end_); }

 private:
  void Clone(TestCloneServer::CloneRequestView request,
             TestCloneServer::CloneCompleter::Sync& completer) {
    auto server = std::make_unique<TestServerBase>();
    auto binding_ref =
        fidl::BindServer(server_loop_.dispatcher(), std::move(request->object), server.get());
    cloned_servers_.push_back(std::move(server));

    if (request->flags & fio::wire::OpenFlags::kDescribe) {
      fio::wire::FileObject file_object;
      const fidl::Status result =
          fidl::WireSendEvent(binding_ref)
              ->OnOpen(ZX_OK,
                       fio::wire::NodeInfoDeprecated::WithFile(
                           fidl::ObjectView<fio::wire::FileObject>::FromExternal(&file_object)));
      ASSERT_TRUE(result.ok(), "%s", result.FormatDescription().c_str());
    }
  }

  TestCloneServer node_server_;
  fidl::ClientEnd<fio::Node> node_client_end_;
  async::Loop server_loop_;
  std::vector<std::unique_ptr<TestServerBase>> cloned_servers_;
};

TEST_F(CloneTest, Clone) {
  zxio_storage_t node_storage;
  ASSERT_OK(zxio_create(TakeClientEnd().TakeChannel().release(), &node_storage));
  zxio_t* node = &node_storage.io;

  zx::channel clone;
  EXPECT_OK(zxio_clone(node, clone.reset_and_get_address()));

  fidl::ClientEnd<fio::Node> clone_client(std::move(clone));

  const fidl::WireResult describe_response = fidl::WireCall(clone_client)->DescribeDeprecated();
  ASSERT_OK(describe_response.status());

  EXPECT_TRUE(describe_response.value().info.is_file());
}

}  // namespace
