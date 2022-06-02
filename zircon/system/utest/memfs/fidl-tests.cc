// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <fidl/fuchsia.io/cpp/wire.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/fdio/cpp/caller.h>
#include <lib/fdio/directory.h>
#include <lib/fdio/fd.h>
#include <lib/fdio/fdio.h>
#include <lib/fdio/vfs.h>
#include <lib/memfs/memfs.h>
#include <limits.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <threads.h>
#include <unistd.h>
#include <zircon/processargs.h>
#include <zircon/syscalls.h>

#include <utility>

#include <fbl/unique_fd.h>
#include <zxtest/zxtest.h>

namespace {

namespace fio = fuchsia_io;

TEST(FidlTests, TestFidlBasic) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  ASSERT_OK(loop.StartThread());

  memfs_filesystem_t* fs;
  ASSERT_OK(memfs_install_at(loop.dispatcher(), "/fidltmp", &fs));
  fbl::unique_fd fd(open("/fidltmp", O_DIRECTORY | O_RDONLY));
  ASSERT_GE(fd.get(), 0);

  // Create a file
  const char* filename = "file-a";
  fd.reset(openat(fd.get(), filename, O_CREAT | O_RDWR, S_IRUSR | S_IWUSR));
  ASSERT_GE(fd.get(), 0);
  const char* data = "hello";
  ssize_t datalen = strlen(data);
  ASSERT_EQ(write(fd.get(), data, datalen), datalen);
  fd.reset();

  zx::status endpoints = fidl::CreateEndpoints<fio::Node>();
  ASSERT_OK(endpoints.status_value());
  ASSERT_OK(fdio_service_connect("/fidltmp/file-a", endpoints->server.TakeChannel().release()));

  auto describe_result = fidl::WireCall(endpoints->client)->Describe();
  ASSERT_OK(describe_result.status());
  ASSERT_TRUE(describe_result->info.is_file());
  ASSERT_EQ(describe_result->info.file().event.get(), ZX_HANDLE_INVALID);
  endpoints->client.TakeChannel().reset();

  sync_completion_t unmounted;
  memfs_free_filesystem(fs, &unmounted);
  sync_completion_wait(&unmounted, zx::duration::infinite().get());

  loop.Shutdown();
}

TEST(FidlTests, TestFidlOpenReadOnly) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  ASSERT_OK(loop.StartThread());

  memfs_filesystem_t* fs;
  ASSERT_OK(memfs_install_at(loop.dispatcher(), "/fidltmp-ro", &fs));
  fbl::unique_fd fd(open("/fidltmp-ro", O_DIRECTORY | O_RDONLY));
  ASSERT_GE(fd.get(), 0);

  // Create a file
  const char* filename = "file-ro";
  fd.reset(openat(fd.get(), filename, O_CREAT | O_RDWR, S_IRUSR | S_IWUSR));
  ASSERT_GE(fd.get(), 0);
  fd.reset();

  zx::status endpoints = fidl::CreateEndpoints<fio::Node>();
  ASSERT_OK(endpoints.status_value());
  ASSERT_OK(fdio_open("/fidltmp-ro/file-ro",
                      static_cast<uint32_t>(fio::wire::OpenFlags::kRightReadable),
                      endpoints->server.TakeChannel().release()));

  auto result = fidl::WireCall(endpoints->client)->GetFlags();
  ASSERT_OK(result.status());
  ASSERT_OK(result->s);
  ASSERT_EQ(result->flags, fio::wire::OpenFlags::kRightReadable);
  endpoints->client.TakeChannel().reset();

  sync_completion_t unmounted;
  memfs_free_filesystem(fs, &unmounted);
  sync_completion_wait(&unmounted, zx::duration::infinite().get());

  loop.Shutdown();
}

void QueryInfo(const char* path, fuchsia_io::wire::FilesystemInfo* info) {
  fbl::unique_fd fd(open(path, O_RDONLY | O_DIRECTORY));
  ASSERT_TRUE(fd);
  fdio_cpp::FdioCaller caller(std::move(fd));
  auto result = fidl::WireCall(caller.node())->QueryFilesystem();
  ASSERT_OK(result.status());
  ASSERT_OK(result->s);
  ASSERT_NOT_NULL(result->info);
  *info = *(result->info);
  const char* kFsName = "memfs";
  const char* name = reinterpret_cast<const char*>(info->name.data());
  ASSERT_EQ(strncmp(name, kFsName, strlen(kFsName)), 0, "Unexpected filesystem mounted");
  ASSERT_EQ(info->block_size, ZX_PAGE_SIZE);
  ASSERT_EQ(info->max_filename_size, NAME_MAX);
  ASSERT_EQ(info->fs_type, VFS_TYPE_MEMFS);
  ASSERT_NE(info->fs_id, 0);
  ASSERT_EQ(info->used_bytes % info->block_size, 0);
}

TEST(FidlTests, TestFidlQueryFilesystem) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  ASSERT_OK(loop.StartThread());

  memfs_filesystem_t* fs;
  ASSERT_OK(memfs_install_at(loop.dispatcher(), "/fidltmp-basic", &fs));
  fbl::unique_fd fd(open("/fidltmp-basic", O_DIRECTORY | O_RDONLY));
  ASSERT_GE(fd.get(), 0);

  // Sanity checks
  fuchsia_io::wire::FilesystemInfo info;
  ASSERT_NO_FATAL_FAILURE(QueryInfo("/fidltmp-basic", &info));

  // These values are nonsense, but they're the nonsense we expect memfs to generate.
  ASSERT_EQ(info.total_bytes, UINT64_MAX);
  ASSERT_EQ(info.used_bytes, 0);

  sync_completion_t unmounted;
  memfs_free_filesystem(fs, &unmounted);
  sync_completion_wait(&unmounted, zx::duration::infinite().get());

  loop.Shutdown();
}

}  // namespace
