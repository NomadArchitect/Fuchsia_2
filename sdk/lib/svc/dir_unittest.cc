// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "lib/svc/dir.h"

#include <fcntl.h>
#include <fuchsia/io/cpp/fidl.h>
#include <lib/fdio/directory.h>
#include <lib/fdio/fd.h>
#include <lib/fdio/fdio.h>
#include <lib/gtest/real_loop_fixture.h>
#include <lib/vfs/cpp/pseudo_dir.h>
#include <lib/vfs/cpp/pseudo_file.h>
#include <lib/zx/channel.h>
#include <zircon/errors.h>

#include <cstdint>
#include <memory>
#include <thread>

#include <fbl/unique_fd.h>

namespace svc {
namespace {

static void connect(void* context, const char* service_name, zx_handle_t service_request) {
  EXPECT_EQ(std::string("foobar"), service_name);
  zx::channel binding(service_request);
  zx_signals_t observed;
  EXPECT_EQ(ZX_OK, binding.wait_one(ZX_CHANNEL_READABLE, zx::time::infinite(), &observed));
  EXPECT_EQ(ZX_ERR_BUFFER_TOO_SMALL,
            binding.read(ZX_CHANNEL_READ_MAY_DISCARD, nullptr, nullptr, 0, 0, nullptr, nullptr));
  EXPECT_EQ(ZX_OK, binding.write(0, "ok", 2, 0, 0));
}

using ServiceTest = ::gtest::RealLoopFixture;

TEST_F(ServiceTest, Control) {
  zx::channel dir, dir_request;
  EXPECT_EQ(ZX_OK, zx::channel::create(0, &dir, &dir_request));

  std::thread child([this, dir_request = std::move(dir_request)]() mutable {
    svc_dir_t* dir = nullptr;
    EXPECT_EQ(ZX_OK, svc_dir_create(dispatcher(), dir_request.release(), &dir));
    EXPECT_EQ(ZX_OK, svc_dir_add_service(dir, "svc", "foobar", nullptr, connect));
    EXPECT_EQ(ZX_OK, svc_dir_add_service(dir, "svc", "baz", nullptr, nullptr));
    EXPECT_EQ(ZX_ERR_ALREADY_EXISTS, svc_dir_add_service(dir, "svc", "baz", nullptr, nullptr));
    EXPECT_EQ(ZX_OK, svc_dir_remove_service(dir, "svc", "baz"));
    EXPECT_EQ(ZX_OK, svc_dir_add_service(dir, "another", "qux", nullptr, nullptr));

    RunLoop();

    svc_dir_destroy(dir);
  });

  // Verify that we can connect to a foobar service and get a response.
  zx::channel svc, request;
  EXPECT_EQ(ZX_OK, zx::channel::create(0, &svc, &request));
  fdio_service_connect_at(dir.get(), "svc/foobar", request.release());
  EXPECT_EQ(ZX_OK, svc.write(0, "hello", 5, 0, 0));
  zx_signals_t observed;
  EXPECT_EQ(ZX_OK, svc.wait_one(ZX_CHANNEL_READABLE, zx::time::infinite(), &observed));
  EXPECT_EQ(ZX_ERR_BUFFER_TOO_SMALL,
            svc.read(ZX_CHANNEL_READ_MAY_DISCARD, nullptr, nullptr, 0, 0, nullptr, nullptr));

  // Verify that connection to a removed service fails.
  EXPECT_EQ(ZX_OK, zx::channel::create(0, &svc, &request));
  fdio_service_connect_at(dir.get(), "svc/baz", request.release());
  EXPECT_EQ(ZX_OK, svc.wait_one(ZX_CHANNEL_PEER_CLOSED, zx::time::infinite(), &observed));

  // Shutdown the service thread.
  QuitLoop();
  child.join();

  // Verify that connection fails after svc_dir_destroy().
  EXPECT_EQ(ZX_OK, zx::channel::create(0, &svc, &request));
  fdio_service_connect_at(dir.get(), "svc/foobar", request.release());
  EXPECT_EQ(ZX_OK, svc.wait_one(ZX_CHANNEL_PEER_CLOSED, zx::time::infinite(), &observed));
}

TEST_F(ServiceTest, PublishLegacyService) {
  zx::channel dir, dir_request;
  EXPECT_EQ(ZX_OK, zx::channel::create(0, &dir, &dir_request));

  std::thread child([this, dir_request = std::move(dir_request)]() mutable {
    svc_dir_t* dir = nullptr;
    EXPECT_EQ(ZX_OK, svc_dir_create(dispatcher(), dir_request.release(), &dir));
    EXPECT_EQ(ZX_OK, svc_dir_add_service(dir, nullptr, "foobar", nullptr, connect));
    EXPECT_EQ(ZX_OK, svc_dir_add_service(dir, nullptr, "baz", nullptr, connect));
    EXPECT_EQ(ZX_OK, svc_dir_remove_service(dir, nullptr, "baz"));

    RunLoop();

    svc_dir_destroy(dir);
  });

  // Verify that we can connect to a foobar service and get a response.
  zx::channel svc, request;
  EXPECT_EQ(ZX_OK, zx::channel::create(0, &svc, &request));
  fdio_service_connect_at(dir.get(), "foobar", request.release());
  EXPECT_EQ(ZX_OK, svc.write(0, "hello", 5, 0, 0));
  zx_signals_t observed;
  EXPECT_EQ(ZX_OK, svc.wait_one(ZX_CHANNEL_READABLE, zx::time::infinite(), &observed));
  EXPECT_EQ(ZX_ERR_BUFFER_TOO_SMALL,
            svc.read(ZX_CHANNEL_READ_MAY_DISCARD, nullptr, nullptr, 0, 0, nullptr, nullptr));

  // Verify that connection to a removed service fails.
  EXPECT_EQ(ZX_OK, zx::channel::create(0, &svc, &request));
  fdio_service_connect_at(dir.get(), "baz", request.release());
  EXPECT_EQ(ZX_OK, svc.wait_one(ZX_CHANNEL_PEER_CLOSED, zx::time::infinite(), &observed));

  // Shutdown the service thread.
  QuitLoop();
  child.join();

  // Verify that connection fails after svc_dir_destroy().
  EXPECT_EQ(ZX_OK, zx::channel::create(0, &svc, &request));
  fdio_service_connect_at(dir.get(), "foobar", request.release());
  EXPECT_EQ(ZX_OK, svc.wait_one(ZX_CHANNEL_PEER_CLOSED, zx::time::infinite(), &observed));
}

TEST_F(ServiceTest, ConnectsByPath) {
  zx::channel dir, dir_request;
  ASSERT_EQ(ZX_OK, zx::channel::create(0, &dir, &dir_request));

  std::thread child([this, dir_request = std::move(dir_request)]() mutable {
    svc_dir_t* dir = nullptr;
    ASSERT_EQ(ZX_OK, svc_dir_create(dispatcher(), dir_request.release(), &dir));
    ASSERT_EQ(ZX_OK, svc_dir_add_service_by_path(dir, "svc/fuchsia.logger.LogSink/default",
                                                 "foobar", nullptr, connect));

    RunLoop();

    ASSERT_EQ(svc_dir_destroy(dir), ZX_OK);
  });

  // Verify that we can connect to svc/fuchsia.logger.LogSink/default/foobar
  // and get a response.
  zx::channel svc, request;
  ASSERT_EQ(ZX_OK, zx::channel::create(0, &svc, &request));
  fdio_service_connect_at(dir.get(), "svc/fuchsia.logger.LogSink/default/foobar",
                          request.release());
  EXPECT_EQ(ZX_OK, svc.write(0, "hello", 5, 0, 0));
  zx_signals_t observed;
  EXPECT_EQ(ZX_OK, svc.wait_one(ZX_CHANNEL_READABLE, zx::time::infinite(), &observed));
  EXPECT_EQ(ZX_ERR_BUFFER_TOO_SMALL,
            svc.read(ZX_CHANNEL_READ_MAY_DISCARD, nullptr, nullptr, 0, 0, nullptr, nullptr));

  // Shutdown the service thread.
  QuitLoop();
  child.join();
}

TEST_F(ServiceTest, RejectsMalformedPaths) {
  zx::channel _directory, dir_request;
  ASSERT_EQ(ZX_OK, zx::channel::create(0, &_directory, &dir_request));

  svc_dir_t* dir = nullptr;
  ASSERT_EQ(ZX_OK, svc_dir_create(dispatcher(), dir_request.release(), &dir));

  // The following paths should all fail.
  EXPECT_EQ(svc_dir_add_service_by_path(dir, "/", "foobar", nullptr, connect), ZX_ERR_INVALID_ARGS);
  EXPECT_EQ(svc_dir_add_service_by_path(dir, "/svc", "foobar", nullptr, connect),
            ZX_ERR_INVALID_ARGS);
  EXPECT_EQ(svc_dir_add_service_by_path(dir, "/svc//foo", "foobar", nullptr, connect),
            ZX_ERR_INVALID_ARGS);
  EXPECT_EQ(svc_dir_add_service_by_path(dir, "svc/", "foobar", nullptr, connect),
            ZX_ERR_INVALID_ARGS);
  EXPECT_EQ(svc_dir_add_service_by_path(dir, ".", "foobar", nullptr, connect), ZX_ERR_INVALID_ARGS);
  EXPECT_EQ(svc_dir_add_service_by_path(dir, "..", "foobar", nullptr, connect),
            ZX_ERR_INVALID_ARGS);
  EXPECT_EQ(svc_dir_add_service_by_path(dir, "...", "foobar", nullptr, connect),
            ZX_ERR_INVALID_ARGS);
  EXPECT_EQ(svc_dir_add_service_by_path(dir, "svc/..", "foobar", nullptr, connect),
            ZX_ERR_INVALID_ARGS);

  // Cleanup resources.
  ASSERT_EQ(svc_dir_destroy(dir), ZX_OK);
}

TEST_F(ServiceTest, AddSubdDir) {
  static constexpr char kTestDirectory[] = "foobar";
  static constexpr char kTestFile[] = "sample.txt";
  static constexpr char kTestContent[] = "Hello World!";
  static constexpr size_t kMaxFileSize = 1024;

  zx::channel dir, dir_request;
  ASSERT_EQ(ZX_OK, zx::channel::create(0, &dir, &dir_request));

  std::thread child([this, dir_request = std::move(dir_request)]() mutable {
    svc_dir_t* dir = nullptr;
    ASSERT_EQ(ZX_OK, svc_dir_create(dispatcher(), dir_request.release(), &dir));

    auto subdir = std::make_unique<vfs::PseudoDir>();
    subdir->AddEntry(
        kTestFile,
        std::make_unique<vfs::PseudoFile>(
            kMaxFileSize,
            /*read_handler=*/[](std::vector<uint8_t>* output, size_t max_bytes) -> zx_status_t {
              for (const char& c : kTestContent) {
                output->push_back(c);
              }
              return ZX_OK;
            }));
    zx::channel server_end, client_end;
    ASSERT_EQ(ZX_OK, zx::channel::create(0, &server_end, &client_end));
    subdir->Serve(fuchsia::io::OPEN_RIGHT_READABLE | fuchsia::io::OPEN_RIGHT_WRITABLE |
                      fuchsia::io::OPEN_FLAG_DIRECTORY,
                  std::move(server_end), dispatcher());

    ASSERT_EQ(ZX_OK, svc_dir_add_directory(dir, kTestDirectory, client_end.release()));

    RunLoop();

    EXPECT_EQ(svc_dir_remove_directory(dir, kTestDirectory), ZX_OK);
    ASSERT_EQ(svc_dir_destroy(dir), ZX_OK);
  });

  fbl::unique_fd root_fd;
  ASSERT_EQ(fdio_fd_create(dir.release(), root_fd.reset_and_get_address()), ZX_OK);
  ZX_ASSERT_MSG(root_fd.is_valid(), "Failed to open root ns as a file descriptor: %s",
                strerror(errno));

  fbl::unique_fd dir_fd(openat(root_fd.get(), kTestDirectory, O_DIRECTORY));
  ZX_ASSERT_MSG(dir_fd.is_valid(), "Failed to open directory \"%s\": %s", kTestDirectory,
                strerror(errno));

  fbl::unique_fd filefd(openat(dir_fd.get(), kTestFile, O_RDONLY));
  ZX_ASSERT_MSG(filefd.is_valid(), "Failed to open file \"%s\": %s", kTestFile, strerror(errno));
  static constexpr size_t kMaxBufferSize = 1024;
  static char kReadBuffer[kMaxBufferSize];
  size_t bytes_read = read(filefd.get(), reinterpret_cast<void*>(kReadBuffer), kMaxBufferSize);
  ZX_ASSERT_MSG(bytes_read > 0, "Read 0 bytes from file at \"%s\": %s", kTestFile, strerror(errno));

  std::string actual_content(kReadBuffer, bytes_read - 1 /* Minus NULL terminator */);
  EXPECT_EQ(actual_content, kTestContent);

  QuitLoop();
  child.join();
}

TEST_F(ServiceTest, AddDirFailsOnBadInput) {
  // |dir| is nullptr
  {
    zx::channel _server_end, client_end;
    ASSERT_EQ(ZX_OK, zx::channel::create(0, &_server_end, &client_end));
    EXPECT_EQ(svc_dir_add_directory(/*dir=*/nullptr, "AValidEntry", client_end.release()),
              ZX_ERR_INVALID_ARGS);
  }

  // |name| is nullptr
  {
    zx::channel _directory, dir_request;
    ASSERT_EQ(ZX_OK, zx::channel::create(0, &_directory, &dir_request));

    svc_dir_t* dir = nullptr;
    ASSERT_EQ(ZX_OK, svc_dir_create(dispatcher(), dir_request.release(), &dir));

    zx::channel _subdir, subdir_client;
    ASSERT_EQ(ZX_OK, zx::channel::create(0, &_subdir, &subdir_client));

    EXPECT_EQ(svc_dir_add_directory(dir, /*name=*/nullptr, subdir_client.release()),
              ZX_ERR_INVALID_ARGS);

    svc_dir_destroy(dir);
  }

  // |subdir| is an invalid handle
  {
    zx::channel _directory, dir_request;
    ASSERT_EQ(ZX_OK, zx::channel::create(0, &_directory, &dir_request));

    svc_dir_t* dir = nullptr;
    ASSERT_EQ(ZX_OK, svc_dir_create(dispatcher(), dir_request.release(), &dir));

    EXPECT_EQ(svc_dir_add_directory(dir, "AValidEntry", /*subdir=*/ZX_HANDLE_INVALID),
              ZX_ERR_INVALID_ARGS);

    svc_dir_destroy(dir);
  }
}

}  // namespace
}  // namespace svc
