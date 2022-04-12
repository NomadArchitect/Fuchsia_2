// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/test.transport/cpp/driver/wire.h>
#include <lib/async/cpp/task.h>
#include <lib/sync/cpp/completion.h>

#include <zxtest/zxtest.h>

#include "sdk/lib/fidl_driver/tests/transport/death_test_helper.h"
#include "sdk/lib/fidl_driver/tests/transport/scoped_fake_driver.h"

// Test creating a typed channel endpoint pair.
TEST(Endpoints, CreateFromProtocol) {
  // `std::move` pattern
  {
    auto endpoints = fdf::CreateEndpoints<test_transport::TwoWayTest>();
    ASSERT_OK(endpoints.status_value());
    ASSERT_EQ(ZX_OK, endpoints.status_value());
    fdf::ClientEnd<test_transport::TwoWayTest> client_end = std::move(endpoints->client);
    fdf::ServerEnd<test_transport::TwoWayTest> server_end = std::move(endpoints->server);

    ASSERT_TRUE(client_end.is_valid());
    ASSERT_TRUE(server_end.is_valid());
  }

  // Destructuring pattern
  {
    auto endpoints = fdf::CreateEndpoints<test_transport::TwoWayTest>();
    ASSERT_OK(endpoints.status_value());
    ASSERT_EQ(ZX_OK, endpoints.status_value());
    auto [client_end, server_end] = std::move(endpoints.value());

    ASSERT_TRUE(client_end.is_valid());
    ASSERT_TRUE(server_end.is_valid());
  }
}

// Test creating a typed channel endpoint pair using the out-parameter
// overloads.
TEST(Endpoints, CreateFromProtocolOutParameterStyleClientRetained) {
  fdf::ClientEnd<test_transport::TwoWayTest> client_end;
  auto server_end = fdf::CreateEndpoints(&client_end);
  ASSERT_OK(server_end.status_value());
  ASSERT_EQ(ZX_OK, server_end.status_value());

  ASSERT_TRUE(client_end.is_valid());
  ASSERT_TRUE(server_end->is_valid());
}

TEST(Endpoints, CreateFromProtocolOutParameterStyleServerRetained) {
  fdf::ServerEnd<test_transport::TwoWayTest> server_end;
  auto client_end = fdf::CreateEndpoints(&server_end);
  ASSERT_OK(client_end.status_value());
  ASSERT_EQ(ZX_OK, client_end.status_value());

  ASSERT_TRUE(server_end.is_valid());
  ASSERT_TRUE(client_end->is_valid());
}

// These checks are only performed in debug builds.
#ifndef NDEBUG

TEST(WireClient, CannotDestroyInDifferentDispatcherThanBound) {
  fidl_driver_testing::ScopedFakeDriver driver;

  libsync::Completion dispatcher1_shutdown;
  zx::status dispatcher1 = fdf::Dispatcher::Create(
      0, [&](fdf_dispatcher_t* dispatcher) { dispatcher1_shutdown.Signal(); });
  ASSERT_OK(dispatcher1.status_value());

  libsync::Completion dispatcher2_shutdown;
  zx::status dispatcher2 = fdf::Dispatcher::Create(
      0, [&](fdf_dispatcher_t* dispatcher) { dispatcher2_shutdown.Signal(); });
  ASSERT_OK(dispatcher2.status_value());

  zx::status endpoints = fdf::CreateEndpoints<test_transport::TwoWayTest>();
  ASSERT_OK(endpoints.status_value());

  std::unique_ptr<fdf::WireClient<test_transport::TwoWayTest>> client;

  // Create on one.
  libsync::Completion created;
  async::PostTask(dispatcher1->async_dispatcher(), [&] {
    client = std::make_unique<fdf::WireClient<test_transport::TwoWayTest>>();
    client->Bind(std::move(endpoints->client), dispatcher1->get());
    created.Signal();
  });
  ASSERT_OK(created.Wait());

  // Destroy on another.
  fidl_driver_testing::CurrentThreadExceptionHandler exception_handler;
  libsync::Completion destroyed;
  async::PostTask(dispatcher2->async_dispatcher(), [&] {
    exception_handler.Try([&] { client.reset(); });
    destroyed.Signal();
  });

  ASSERT_NO_FATAL_FAILURE(exception_handler.WaitForOneSwBreakpoint());
  ASSERT_OK(destroyed.Wait());

  dispatcher1->ShutdownAsync();
  dispatcher2->ShutdownAsync();

  ASSERT_OK(dispatcher1_shutdown.Wait());
  ASSERT_OK(dispatcher2_shutdown.Wait());
}

TEST(WireClient, CannotDestroyOnUnmanagedThread) {
  fidl_driver_testing::ScopedFakeDriver driver;

  libsync::Completion dispatcher1_shutdown;
  zx::status dispatcher1 = fdf::Dispatcher::Create(
      0, [&](fdf_dispatcher_t* dispatcher) { dispatcher1_shutdown.Signal(); });
  ASSERT_OK(dispatcher1.status_value());

  zx::status endpoints = fdf::CreateEndpoints<test_transport::TwoWayTest>();
  ASSERT_OK(endpoints.status_value());

  std::unique_ptr<fdf::WireClient<test_transport::TwoWayTest>> client;

  // Create on one.
  libsync::Completion created;
  async::PostTask(dispatcher1->async_dispatcher(), [&] {
    client = std::make_unique<fdf::WireClient<test_transport::TwoWayTest>>();
    client->Bind(std::move(endpoints->client), dispatcher1->get());
    created.Signal();
  });
  ASSERT_OK(created.Wait());

  // Destroy on another.
  fidl_driver_testing::CurrentThreadExceptionHandler exception_handler;
  libsync::Completion destroyed;
  std::thread thread([&] {
    exception_handler.Try([&] { client.reset(); });
    destroyed.Signal();
  });

  ASSERT_NO_FATAL_FAILURE(exception_handler.WaitForOneSwBreakpoint());
  ASSERT_OK(destroyed.Wait());
  thread.join();

  dispatcher1->ShutdownAsync();
  ASSERT_OK(dispatcher1_shutdown.Wait());
}

TEST(WireSharedClient, CanSendAcrossDispatcher) {
  fidl_driver_testing::ScopedFakeDriver driver;

  libsync::Completion dispatcher1_shutdown;
  zx::status dispatcher1 = fdf::Dispatcher::Create(
      0, [&](fdf_dispatcher_t* dispatcher) { dispatcher1_shutdown.Signal(); });
  ASSERT_OK(dispatcher1.status_value());

  libsync::Completion dispatcher2_shutdown;
  zx::status dispatcher2 = fdf::Dispatcher::Create(
      0, [&](fdf_dispatcher_t* dispatcher) { dispatcher2_shutdown.Signal(); });
  ASSERT_OK(dispatcher2.status_value());

  zx::status endpoints = fdf::CreateEndpoints<test_transport::TwoWayTest>();
  ASSERT_OK(endpoints.status_value());

  std::unique_ptr<fdf::WireSharedClient<test_transport::TwoWayTest>> client;

  // Create on one.
  libsync::Completion created;
  async::PostTask(dispatcher1->async_dispatcher(), [&] {
    client = std::make_unique<fdf::WireSharedClient<test_transport::TwoWayTest>>();
    client->Bind(std::move(endpoints->client), dispatcher1->get());
    created.Signal();
  });
  ASSERT_OK(created.Wait());

  // Destroy on another.
  libsync::Completion destroyed;
  async::PostTask(dispatcher2->async_dispatcher(), [&] {
    client.reset();
    destroyed.Signal();
  });
  ASSERT_OK(destroyed.Wait());

  dispatcher1->ShutdownAsync();
  dispatcher2->ShutdownAsync();
  ASSERT_OK(dispatcher1_shutdown.Wait());
  ASSERT_OK(dispatcher2_shutdown.Wait());
}

TEST(WireClient, CannotBindUnsynchronizedDispatcher) {
  fidl_driver_testing::ScopedFakeDriver driver;

  libsync::Completion dispatcher_shutdown;
  zx::status dispatcher =
      fdf::Dispatcher::Create(FDF_DISPATCHER_OPTION_UNSYNCHRONIZED,
                              [&](fdf_dispatcher_t* dispatcher) { dispatcher_shutdown.Signal(); });
  ASSERT_OK(dispatcher.status_value());

  zx::status endpoints = fdf::CreateEndpoints<test_transport::TwoWayTest>();
  ASSERT_OK(endpoints.status_value());

  fdf::WireClient<test_transport::TwoWayTest> client;
  libsync::Completion created;
  fidl_driver_testing::CurrentThreadExceptionHandler exception_handler;
  async::PostTask(dispatcher->async_dispatcher(), [&] {
    exception_handler.Try([&] { client.Bind(std::move(endpoints->client), dispatcher->get()); });
    client = {};
    created.Signal();
  });
  ASSERT_NO_FATAL_FAILURE(exception_handler.WaitForOneSwBreakpoint());
  ASSERT_OK(created.Wait());

  dispatcher->ShutdownAsync();
  ASSERT_OK(dispatcher_shutdown.Wait());
}

TEST(WireSharedClient, CanBindUnsynchronizedDispatcher) {
  fidl_driver_testing::ScopedFakeDriver driver;

  libsync::Completion dispatcher_shutdown;
  zx::status dispatcher =
      fdf::Dispatcher::Create(FDF_DISPATCHER_OPTION_UNSYNCHRONIZED,
                              [&](fdf_dispatcher_t* dispatcher) { dispatcher_shutdown.Signal(); });
  ASSERT_OK(dispatcher.status_value());

  zx::status endpoints = fdf::CreateEndpoints<test_transport::TwoWayTest>();
  ASSERT_OK(endpoints.status_value());

  fdf::WireSharedClient<test_transport::TwoWayTest> client;
  libsync::Completion created;
  async::PostTask(dispatcher->async_dispatcher(), [&] {
    client.Bind(std::move(endpoints->client), dispatcher->get());
    client = {};
    created.Signal();
  });
  ASSERT_OK(created.Wait());

  dispatcher->ShutdownAsync();
  ASSERT_OK(dispatcher_shutdown.Wait());
}

#endif  // NDEBUG
