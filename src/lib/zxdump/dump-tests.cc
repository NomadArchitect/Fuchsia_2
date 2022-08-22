// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "dump-tests.h"

#include <lib/zxdump/dump.h>
#include <lib/zxdump/fd-writer.h>
#include <lib/zxdump/task.h>

#include <gtest/gtest.h>

#include "test-file.h"

// The dump format is complex enough that direct testing of output data would
// be tantamount to reimplementing the reader, and golden binary files aren't
// easy to match up with fresh data from a live system where all the KOID and
// statistics values will be different every time.  So the main method used to
// test the dumper is via end-to-end tests that dump into a file via the dumper
// API, read the dump back using the reader API, and then compare the data from
// the dump to the data from the original live tasks.

namespace zxdump::testing {

void TestProcessForPropertiesAndInfo::StartChild() {
  SpawnAction({
      .action = FDIO_SPAWN_ACTION_SET_NAME,
      .name = {kChildName},
  });
  ASSERT_NO_FATAL_FAILURE(TestProcess::StartChild());
}

void TestProcessForPropertiesAndInfo::CheckDump(zxdump::TaskHolder& holder, bool threads_dumped) {
  auto find_result = holder.root_job().find(koid());
  ASSERT_TRUE(find_result.is_ok()) << find_result.error_value();

  ASSERT_EQ(find_result->get().type(), ZX_OBJ_TYPE_PROCESS);
  zxdump::Process& read_process = static_cast<zxdump::Process&>(find_result->get());

  {
    auto name_result = read_process.get_property<ZX_PROP_NAME>();
    ASSERT_TRUE(name_result.is_ok()) << name_result.error_value();
    std::string_view name(name_result->data(), name_result->size());
    name = name.substr(0, name.find_first_of('\0'));
    EXPECT_EQ(name, std::string_view(kChildName));
  }

  {
    auto threads_result = read_process.get_info<ZX_INFO_PROCESS_THREADS>();
    ASSERT_TRUE(threads_result.is_ok()) << threads_result.error_value();
    EXPECT_EQ(threads_result->size(), size_t{1});
  }

  // Even though ZX_INFO_PROCESS_THREADS is present, threads() only
  // returns anything if the threads were actually dumped.
  {
    auto threads_result = read_process.threads();
    ASSERT_TRUE(threads_result.is_ok()) << threads_result.error_value();
    if (threads_dumped) {
      EXPECT_EQ(threads_result->get().size(), size_t{1});
    } else {
      EXPECT_EQ(threads_result->get().size(), size_t{0});
    }
  }

  {
    auto info_result = read_process.get_info<ZX_INFO_HANDLE_BASIC>();
    ASSERT_TRUE(info_result.is_ok()) << info_result.error_value();
    EXPECT_EQ(info_result->type, ZX_OBJ_TYPE_PROCESS);
    EXPECT_EQ(info_result->koid, koid());
  }
}

namespace {

TEST(ZxdumpTests, ProcessDumpBasic) {
  TestFile file;
  zxdump::FdWriter writer(file.RewoundFd());

  TestProcess process;
  ASSERT_NO_FATAL_FAILURE(process.StartChild());
  zxdump::ProcessDump<zx::unowned_process> dump(process.borrow());

  auto collect_result = dump.CollectProcess(TestProcess::PruneAllMemory);
  ASSERT_TRUE(collect_result.is_ok()) << collect_result.error_value();

  auto dump_result = dump.DumpHeaders(writer.AccumulateFragmentsCallback());
  ASSERT_TRUE(dump_result.is_ok()) << dump_result.error_value();

  auto write_result = writer.WriteFragments();
  ASSERT_TRUE(write_result.is_ok()) << write_result.error_value();
  const size_t bytes_written = write_result.value();

  auto memory_result = dump.DumpMemory(writer.WriteCallback());
  ASSERT_TRUE(memory_result.is_ok()) << memory_result.error_value();
  const size_t total_with_memory = memory_result.value();

  // We pruned all memory, so DumpMemory should not have added any output.
  EXPECT_EQ(bytes_written, total_with_memory);

  // Now read the file back in.
  zxdump::TaskHolder holder;
  auto read_result = holder.Insert(file.RewoundFd());
  ASSERT_TRUE(read_result.is_ok()) << read_result.error_value();

  // The dump has no jobs, so there should be a placeholder "super-root".
  EXPECT_EQ(ZX_KOID_INVALID, holder.root_job().koid());

  auto processes = holder.root_job().processes();
  ASSERT_TRUE(processes.is_ok()) << processes.error_value();

  // The fake job should have exactly one process.
  EXPECT_EQ(processes->get().size(), 1u);
  for (auto& [read_koid, read_process] : processes->get()) {
    EXPECT_NE(read_koid, ZX_KOID_INVALID);

    // Get the basic info from the real live process handle.
    zx_info_handle_basic_t basic;
    ASSERT_EQ(ZX_OK, process.borrow()->get_info(ZX_INFO_HANDLE_BASIC, &basic, sizeof(basic),
                                                nullptr, nullptr));
    EXPECT_EQ(read_koid, basic.koid);
    EXPECT_EQ(ZX_OBJ_TYPE_PROCESS, basic.type);

    // Get the same info from the dump and verify they match up.  Note that the
    // zx_info_handle_basic_t::rights in the dump is not usually particularly
    // meaningful about the dumped process, because it's just whatever rights
    // the dumper's own process handle had.  But in this case it does exactly
    // match the handle we just checked, since that's what we used to dump.
    auto read_basic = read_process.get_info<ZX_INFO_HANDLE_BASIC>();
    ASSERT_TRUE(read_basic.is_ok()) << read_basic.error_value();
    EXPECT_EQ(basic.koid, read_basic->koid);
    EXPECT_EQ(basic.rights, read_basic->rights);
    EXPECT_EQ(basic.type, read_basic->type);
    EXPECT_EQ(basic.related_koid, read_basic->related_koid);
  }
}

TEST(ZxdumpTests, ProcessDumpPropertiesAndInfo) {
  TestFile file;
  zxdump::FdWriter writer(file.RewoundFd());

  TestProcessForPropertiesAndInfo process;
  ASSERT_NO_FATAL_FAILURE(process.StartChild());

  zxdump::ProcessDump<zx::unowned_process> dump(process.borrow());

  auto collect_result = dump.CollectProcess(TestProcess::PruneAllMemory);
  ASSERT_TRUE(collect_result.is_ok()) << collect_result.error_value();

  auto dump_result = dump.DumpHeaders(writer.AccumulateFragmentsCallback());
  ASSERT_TRUE(dump_result.is_ok()) << dump_result.error_value();

  auto write_result = writer.WriteFragments();
  ASSERT_TRUE(write_result.is_ok()) << write_result.error_value();
  const size_t bytes_written = write_result.value();

  auto memory_result = dump.DumpMemory(writer.WriteCallback());
  ASSERT_TRUE(memory_result.is_ok()) << memory_result.error_value();
  const size_t total_with_memory = memory_result.value();

  // We pruned all memory, so DumpMemory should not have added any output.
  EXPECT_EQ(bytes_written, total_with_memory);

  zxdump::TaskHolder holder;
  auto read_result = holder.Insert(file.RewoundFd());
  ASSERT_TRUE(read_result.is_ok()) << read_result.error_value();
  ASSERT_NO_FATAL_FAILURE(process.CheckDump(holder, false));
}

}  // namespace
}  // namespace zxdump::testing
