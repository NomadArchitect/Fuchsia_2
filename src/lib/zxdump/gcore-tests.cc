// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/elfldltl/layout.h>
#include <lib/stdcompat/string_view.h>

#include <cstdlib>

#include "dump-tests.h"
#include "job-archive.h"
#include "test-tool-process.h"

namespace {

constexpr const char* kOutputSwitch = "-o";
constexpr const char* kExcludeMemorySwitch = "--exclude-memory";
constexpr const char* kNoThreadsSwitch = "--no-threads";
constexpr const char* kNoChildrenSwitch = "--no-children";
constexpr const char* kNoProcessesSwitch = "--no-processes";
constexpr const char* kJobsSwitch = "--jobs";
constexpr const char* kJobArchiveSwitch = "--job-archive";
constexpr const char* kZstdSwitch = "--zstd";

constexpr std::string_view kArchiveSuffix = ".a";

struct OutputFile {
  zxdump::testing::TestToolProcess::File& file;
  std::string prefix;
  std::string pid_string;
};

OutputFile GetOutputFile(zxdump::testing::TestToolProcess& child, std::string_view name,
                         zx_koid_t koid, bool archive = false, std::string_view final_suffix = {}) {
  std::string pid_string = std::to_string(koid);
  std::string suffix = "." + pid_string;
  suffix += final_suffix;
  if (archive) {
    suffix += kArchiveSuffix;
  }
  auto& file = child.MakeFile(name, std::move(suffix));
  std::string prefix = file.name();
  prefix.resize(prefix.size() - pid_string.size() - (archive ? kArchiveSuffix.size() : 0) -
                final_suffix.size());
  return {file, prefix, pid_string};
}

void UsageTest(int expected_status, const std::vector<std::string>& args = {}) {
  zxdump::testing::TestToolProcess child;
  ASSERT_NO_FATAL_FAILURE(child.Init());
  ASSERT_NO_FATAL_FAILURE(child.Start("gcore", args));
  ASSERT_NO_FATAL_FAILURE(child.CollectStdout());
  ASSERT_NO_FATAL_FAILURE(child.CollectStderr());
  int status;
  ASSERT_NO_FATAL_FAILURE(child.Finish(status));
  EXPECT_EQ(status, expected_status);
  EXPECT_EQ(child.collected_stdout(), "");
  std::string text = child.collected_stderr();
  EXPECT_TRUE(cpp20::starts_with(std::string_view(text), "Usage: "));
  EXPECT_TRUE(cpp20::ends_with(std::string_view(text), '\n'));
}

TEST(ZxdumpTests, GcoreHelp) { UsageTest(EXIT_SUCCESS, {"--help"}); }

TEST(ZxdumpTests, GcoreUsage) { UsageTest(EXIT_FAILURE); }

TEST(ZxdumpTests, GcoreProcessDumpIsElfCore) {
  zxdump::testing::TestProcess process;
  ASSERT_NO_FATAL_FAILURE(process.StartChild());

  zxdump::testing::TestToolProcess child;
  ASSERT_NO_FATAL_FAILURE(child.Init());
  const auto& [dump_file, prefix, pid_string] =
      GetOutputFile(child, "process-dump", process.koid());
  std::vector<std::string> args({
      // Don't dump memory since we don't need it and it is large.
      kExcludeMemorySwitch,
      // Don't bother dumping threads since this test doesn't check for them.
      kNoThreadsSwitch,
      kOutputSwitch,
      prefix,
      pid_string,
  });
  ASSERT_NO_FATAL_FAILURE(child.Start("gcore", args));
  ASSERT_NO_FATAL_FAILURE(child.CollectStdout());
  ASSERT_NO_FATAL_FAILURE(child.CollectStderr());
  int status;
  ASSERT_NO_FATAL_FAILURE(child.Finish(status));
  EXPECT_EQ(status, EXIT_SUCCESS);
  EXPECT_EQ(child.collected_stdout(), "");
  EXPECT_EQ(child.collected_stderr(), "");

  fbl::unique_fd fd = dump_file.OpenOutput();
  ASSERT_TRUE(fd);
  elfldltl::Elf<>::Ehdr ehdr;
  ASSERT_EQ(read(fd.get(), &ehdr, sizeof(ehdr)), static_cast<ssize_t>(sizeof(ehdr)))
      << strerror(errno);
  EXPECT_TRUE(ehdr.Valid());
  EXPECT_EQ(ehdr.type, elfldltl::ElfType::kCore);
}

// Without --jobs, `gcore JOB_KOID` is an error.
TEST(ZxdumpTests, GcoreJobRequiresSwitch) {
  zxdump::testing::TestProcess process;

  // We don't even need to spawn a process for this test.
  // Just create an empty job and (fail to) dump it.
  ASSERT_NO_FATAL_FAILURE(process.HermeticJob());

  zxdump::testing::TestToolProcess child;
  ASSERT_NO_FATAL_FAILURE(child.Init());
  const auto& [dump_file, prefix, pid_string] =
      GetOutputFile(child, "job-dump", process.job_koid(), true);
  dump_file.NoFile();
  std::vector<std::string> args({
      kNoChildrenSwitch,
      kNoProcessesSwitch,
      kOutputSwitch,
      prefix,
      pid_string,
  });
  ASSERT_NO_FATAL_FAILURE(child.Start("gcore", args));
  ASSERT_NO_FATAL_FAILURE(child.CollectStdout());
  ASSERT_NO_FATAL_FAILURE(child.CollectStderr());
  int status;
  ASSERT_NO_FATAL_FAILURE(child.Finish(status));
  EXPECT_EQ(status, EXIT_FAILURE);
  EXPECT_EQ(child.collected_stdout(), "");
  std::string error_text = child.collected_stderr();
  EXPECT_TRUE(cpp20::ends_with(std::string_view(error_text), ": KOID is not a process\n"));
}

// With --jobs, you still just get an ET_CORE file (for each process).
TEST(ZxdumpTests, GcoreProcessDumpViaJob) {
  zxdump::testing::TestProcess process;
  ASSERT_NO_FATAL_FAILURE(process.HermeticJob());
  ASSERT_NO_FATAL_FAILURE(process.StartChild());

  zxdump::testing::TestToolProcess child;
  ASSERT_NO_FATAL_FAILURE(child.Init());
  const auto& [dump_file, prefix, pid_string] =
      GetOutputFile(child, "process-dump-via-job", process.koid());
  std::vector<std::string> args({
      kJobsSwitch,
      // Don't dump memory since we don't need it and it is large.
      kExcludeMemorySwitch,
      // Don't bother dumping threads since this test doesn't check for them.
      kNoThreadsSwitch,
      kOutputSwitch,
      prefix,
      std::to_string(process.job_koid()),
  });
  ASSERT_NO_FATAL_FAILURE(child.Start("gcore", args));
  ASSERT_NO_FATAL_FAILURE(child.CollectStdout());
  ASSERT_NO_FATAL_FAILURE(child.CollectStderr());
  int status;
  ASSERT_NO_FATAL_FAILURE(child.Finish(status));
  EXPECT_EQ(status, EXIT_SUCCESS);
  EXPECT_EQ(child.collected_stdout(), "");
  EXPECT_EQ(child.collected_stderr(), "");

  fbl::unique_fd fd = dump_file.OpenOutput();
  ASSERT_TRUE(fd);
  elfldltl::Elf<>::Ehdr ehdr;
  ASSERT_EQ(read(fd.get(), &ehdr, sizeof(ehdr)), static_cast<ssize_t>(sizeof(ehdr)))
      << strerror(errno);
  EXPECT_TRUE(ehdr.Valid());
  EXPECT_EQ(ehdr.type, elfldltl::ElfType::kCore);
}

TEST(ZxdumpTests, GcoreJobDumpIsArchive) {
  zxdump::testing::TestProcess process;

  // We don't even need to spawn a process for this test.
  // Just create an empty job and dump it.
  ASSERT_NO_FATAL_FAILURE(process.HermeticJob());

  zxdump::testing::TestToolProcess child;
  ASSERT_NO_FATAL_FAILURE(child.Init());
  const auto& [dump_file, prefix, pid_string] =
      GetOutputFile(child, "job-dump", process.job_koid(), true);
  std::vector<std::string> args({
      kJobArchiveSwitch,
      kNoChildrenSwitch,
      kNoProcessesSwitch,
      kOutputSwitch,
      prefix,
      pid_string,
  });
  ASSERT_NO_FATAL_FAILURE(child.Start("gcore", args));
  ASSERT_NO_FATAL_FAILURE(child.CollectStdout());
  ASSERT_NO_FATAL_FAILURE(child.CollectStderr());
  int status;
  ASSERT_NO_FATAL_FAILURE(child.Finish(status));
  EXPECT_EQ(status, EXIT_SUCCESS);
  EXPECT_EQ(child.collected_stdout(), "");
  EXPECT_EQ(child.collected_stderr(), "");

  fbl::unique_fd fd = dump_file.OpenOutput();
  ASSERT_TRUE(fd) << dump_file.name() << ": " << strerror(errno);

  char buffer[zxdump::kMinimumArchive];
  ASSERT_EQ(read(fd.get(), buffer, sizeof(buffer)), static_cast<ssize_t>(sizeof(buffer)))
      << strerror(errno);
  EXPECT_TRUE(cpp20::starts_with(std::string_view(buffer, sizeof(buffer)), zxdump::kArchiveMagic));
}

TEST(ZxdumpTests, GcoreProcessDumpPropertiesAndInfo) {
  zxdump::testing::TestProcessForPropertiesAndInfo process;
  ASSERT_NO_FATAL_FAILURE(process.StartChild());

  zxdump::testing::TestToolProcess child;
  ASSERT_NO_FATAL_FAILURE(child.Init());
  const auto& [dump_file, prefix, pid_string] =
      GetOutputFile(child, "process-dump-no-threads", process.koid());
  std::vector<std::string> args({
      // Don't include threads.
      kNoThreadsSwitch,
      // Don't dump memory since we don't need it and it is large.
      kExcludeMemorySwitch,
      kOutputSwitch,
      prefix,
      pid_string,
  });
  ASSERT_NO_FATAL_FAILURE(child.Start("gcore", args));
  ASSERT_NO_FATAL_FAILURE(child.CollectStdout());
  ASSERT_NO_FATAL_FAILURE(child.CollectStderr());
  int status;
  ASSERT_NO_FATAL_FAILURE(child.Finish(status));
  EXPECT_EQ(status, EXIT_SUCCESS);
  EXPECT_EQ(child.collected_stdout(), "");
  EXPECT_EQ(child.collected_stderr(), "");

  fbl::unique_fd fd = dump_file.OpenOutput();
  ASSERT_TRUE(fd) << dump_file.name() << ": " << strerror(errno);

  zxdump::TaskHolder holder;
  auto read_result = holder.Insert(std::move(fd));
  ASSERT_TRUE(read_result.is_ok()) << read_result.error_value();
  ASSERT_NO_FATAL_FAILURE(process.CheckDump(holder, false));
}

TEST(ZxdumpTests, GcoreProcessDumpZstd) {
  zxdump::testing::TestProcessForPropertiesAndInfo process;
  ASSERT_NO_FATAL_FAILURE(process.StartChild());

  zxdump::testing::TestToolProcess child;
  ASSERT_NO_FATAL_FAILURE(child.Init());
  const auto& [dump_file, prefix, pid_string] =
      GetOutputFile(child, "gcore-process-zstd", process.koid(), false,
                    zxdump::testing::TestToolProcess::File::kZstdSuffix);
  std::vector<std::string> args({
      // Compress the output.
      kZstdSwitch,
      // Don't include threads.
      kNoThreadsSwitch,
      // Don't dump memory since we don't need it and it is large.
      kExcludeMemorySwitch,
      kOutputSwitch,
      prefix,
      pid_string,
  });
  ASSERT_NO_FATAL_FAILURE(child.Start("gcore", args));
  ASSERT_NO_FATAL_FAILURE(child.CollectStdout());
  ASSERT_NO_FATAL_FAILURE(child.CollectStderr());
  int status;
  ASSERT_NO_FATAL_FAILURE(child.Finish(status));
  EXPECT_EQ(status, EXIT_SUCCESS);
  EXPECT_EQ(child.collected_stdout(), "");
  EXPECT_EQ(child.collected_stderr(), "");

  // Decompress the file using the zstd tool.
  auto& decompressed_file = dump_file.ZstdDecompress();
  fbl::unique_fd fd = decompressed_file.OpenOutput();
  ASSERT_TRUE(fd) << decompressed_file.name() << ": " << strerror(errno);

  zxdump::TaskHolder holder;
  auto read_result = holder.Insert(std::move(fd));
  ASSERT_TRUE(read_result.is_ok()) << read_result.error_value();
  ASSERT_NO_FATAL_FAILURE(process.CheckDump(holder, false));
}

}  // namespace
