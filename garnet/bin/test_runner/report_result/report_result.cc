// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fuchsia/testing/runner/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/fdio/io.h>
#include <lib/fdio/spawn.h>
#include <lib/sys/cpp/component_context.h>
#include <lib/zx/clock.h>
#include <unistd.h>
#include <zircon/processargs.h>
#include <zircon/syscalls/object.h>

#include <iostream>
#include <iterator>
#include <sstream>

using fuchsia::testing::runner::TestResult;
using fuchsia::testing::runner::TestRunner;

static zx_status_t AddPipe(int target_fd, int* local_fd, fdio_spawn_action_t* action) {
  zx_status_t status = fdio_pipe_half(local_fd, &action->h.handle);
  if (status != ZX_OK)
    return status;
  action->action = FDIO_SPAWN_ACTION_ADD_HANDLE;
  action->h.id = PA_HND(PA_HND_TYPE(PA_FD), target_fd);
  return ZX_OK;
}

class Reporter {
 public:
  Reporter(async::Loop* loop, const std::string& name, TestRunner* test_runner)
      : loop_(loop), name_(name), test_runner_(test_runner) {}

  ~Reporter() {}

  void Start() {
    test_runner_->Identify(name_, [] {});
    start_ = zx::clock::get_monotonic();
  }

  void Finish(bool failed, const std::string& message) {
    TestResult result;
    result.name = name_;
    result.elapsed = (zx::clock::get_monotonic() - start_).to_nsecs() / 1000000.0;
    result.failed = failed;
    result.message = message;

    test_runner_->ReportResult(std::move(result));
    test_runner_->Teardown([this] { loop_->Quit(); });
    loop_->Run();
  }

 private:
  async::Loop* const loop_;
  std::string name_;
  TestRunner* test_runner_;
  zx::time start_;
};

void ReadPipe(int pipe, std::stringstream* stream) {
  char buffer[1024];
  int size;
  while ((size = read(pipe, buffer, 1024))) {
    stream->write(buffer, size);
    std::cout.write(buffer, size);
  }
}

// Runs a command specified by argv, and based on its exit code reports success
// or failure to the TestRunner FIDL service.
int main(int argc, char** argv) {
  std::string name;
  bool command_provided;
  if (argc > 1) {
    command_provided = true;
    name = argv[1];
  } else {
    command_provided = false;
    name = "report_result";
  }

  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  auto app_context = sys::ComponentContext::CreateAndServeOutgoingDirectory();
  auto test_runner = app_context->svc()->Connect<TestRunner>();
  Reporter reporter(&loop, name, test_runner.get());

  if (!command_provided) {
    reporter.Start();
    reporter.Finish(true, "No command provided");
    return 1;
  }

  int stdout_pipe = -1;
  int stderr_pipe = -1;
  fdio_spawn_action_t actions[2] = {};

  if (AddPipe(STDOUT_FILENO, &stdout_pipe, &actions[0]) != ZX_OK) {
    reporter.Start();
    reporter.Finish(true, "Failed to create stdout pipe");
    return 1;
  }

  if (AddPipe(STDERR_FILENO, &stderr_pipe, &actions[1]) != ZX_OK) {
    reporter.Start();
    reporter.Finish(true, "Failed to create stderr pipe");
    return 1;
  }

  reporter.Start();

  char error[FDIO_SPAWN_ERR_MSG_MAX_LENGTH];
  zx_handle_t handle = ZX_HANDLE_INVALID;
  zx_status_t status =
      fdio_spawn_etc(ZX_HANDLE_INVALID, FDIO_SPAWN_CLONE_ALL & ~FDIO_SPAWN_CLONE_STDIO, argv[1],
                     argv + 1, nullptr, std::size(actions), actions, &handle, error);
  if (status < 0) {
    reporter.Finish(true, error);
    return 1;
  }

  std::stringstream stream;
  stream << "[stdout]\n";
  ReadPipe(stdout_pipe, &stream);
  stream << "[stderr]\n";
  ReadPipe(stderr_pipe, &stream);

  status = zx_object_wait_one(handle, ZX_PROCESS_TERMINATED, ZX_TIME_INFINITE, NULL);
  if (status != ZX_OK) {
    reporter.Finish(true, "Failed to wait for exit");
    return 1;
  }

  zx_info_process_v2_t proc_info;
  status =
      zx_object_get_info(handle, ZX_INFO_PROCESS_V2, &proc_info, sizeof(proc_info), NULL, NULL);
  zx_handle_close(handle);
  if (status < 0) {
    reporter.Finish(true, "Failed to get return code");
    return 1;
  }

  reporter.Finish(proc_info.return_code, stream.str());
  return 0;
}
