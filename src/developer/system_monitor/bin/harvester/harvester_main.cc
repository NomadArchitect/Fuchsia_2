// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/fdio/fdio.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/trace-provider/provider.h>
#include <lib/trace/event.h>
#include <lib/zx/clock.h>
#include <zircon/status.h>
#include <zircon/utc.h>

#include <string>

#include "dockyard_proxy.h"
#include "dockyard_proxy_grpc.h"
#include "dockyard_proxy_local.h"
#include "fuchsia_clock.h"
#include "harvester.h"
#include "info_resource.h"
#include "os.h"
#include "src/lib/fxl/command_line.h"
#include "src/lib/fxl/log_settings_command_line.h"
#include "src/lib/fxl/strings/string_number_conversions.h"
#include "src/lib/timekeeper/system_clock.h"

int main(int argc, char** argv) {
  constexpr int EXIT_CODE_OK = 0;
  // A broad 'something went wrong' error.
  constexpr int EXIT_CODE_GENERAL_ERROR = 1;

  // The wip number is incremented arbitrarily.
  // TODO(fxbug.dev/44): replace wip number with real version number.
  constexpr char VERSION_OUTPUT[] =
      "System Monitor Harvester 20191211\n"
      "- memory_digest\n"
      "+ separate cpu and memory gather\n";

  // Command line options.
  constexpr char COMMAND_LOCAL[] = "local";
  constexpr char COMMAND_VERSION[] = "version";
  constexpr char COMMAND_ONCE[] = "once";

  bool use_grpc = true;
  bool run_loop_once = false;

  // Parse command line.
  auto command_line = fxl::CommandLineFromArgcArgv(argc, argv);
  fxl::SetLogSettingsFromCommandLine(command_line, {"harvester"});

  FX_LOGS(INFO) << VERSION_OUTPUT;

  // If NTRACE is defined, TRACE_CATEGORY_ENABLED is always false.
  if (!TRACE_CATEGORY_ENABLED("harvester")) {
    FX_LOGS(INFO) << "Tracing disabled";
  }

  if (command_line.HasOption(COMMAND_VERSION)) {
    std::cout << VERSION_OUTPUT << std::endl;
    exit(EXIT_CODE_OK);
  }
  if (command_line.HasOption(COMMAND_LOCAL)) {
    FX_LOGS(INFO) << "Option: local only, not using transport to Dockyard.";
    use_grpc = false;
  }
  if (command_line.HasOption(COMMAND_ONCE)) {
    FX_LOGS(INFO) << "Option: Only run the update loop once, then exit.";
    run_loop_once = true;
  }

  // Note: Neither of the following loops are "fast" or "slow" on their own.
  //       It's just a matter of what we choose to run on them.
  // Create a separate loop for quick calls (don't run long running functions on
  // this loop).
  // The "slow" loop is used for potentially long running calls.
  async::Loop slow_calls_loop(&kAsyncLoopConfigAttachToCurrentThread);
  async::Loop fast_calls_loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  // The loop that runs quick calls is in a separate thread.
  zx_status_t status = fast_calls_loop.StartThread("fast-calls-thread");
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "fast_calls_loop.StartThread failed " << status;
    exit(EXIT_CODE_GENERAL_ERROR);
  }
  async::Loop trace_loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  status = trace_loop.StartThread("trace-thread");
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "trace-thread.StartThread failed " << status;
    exit(EXIT_CODE_GENERAL_ERROR);
  }
  FX_LOGS(INFO) << "main thread " << pthread_self();

  async_dispatcher_t* fast_dispatcher = fast_calls_loop.dispatcher();

  std::unique_ptr<harvester::FuchsiaClock> clock =
      std::make_unique<harvester::FuchsiaClock>(
          fast_dispatcher, std::make_unique<timekeeper::SystemClock>(),
          zx::unowned_clock(zx_utc_reference_get()));

  // Set up.
  std::unique_ptr<harvester::DockyardProxy> dockyard_proxy;
  if (use_grpc) {
    const auto& positional_args = command_line.positional_args();
    if (positional_args.empty()) {
      // TODO(fxbug.dev/30): Adhere to CLI tool requirements for --help.
      std::cerr << "Please specify an IP:Port, such as localhost:50051"
                << std::endl;
      exit(EXIT_CODE_GENERAL_ERROR);
    }

    // TODO(fxbug.dev/32): This channel isn't authenticated
    // (InsecureChannelCredentials()).
    dockyard_proxy = std::make_unique<harvester::DockyardProxyGrpc>(
        grpc::CreateChannel(positional_args[0],
                            grpc::InsecureChannelCredentials()),
        std::move(clock));

    if (!dockyard_proxy) {
      FX_LOGS(ERROR) << "unable to create dockyard_proxy";
      exit(EXIT_CODE_GENERAL_ERROR);
    }
    harvester::DockyardProxyStatus status = dockyard_proxy->Init();
    if (status != harvester::DockyardProxyStatus::OK) {
      FX_LOGS(ERROR) << harvester::DockyardErrorString("Init", status);
      exit(EXIT_CODE_GENERAL_ERROR);
    }
  } else {
    dockyard_proxy = std::make_unique<harvester::DockyardProxyLocal>();
  }

  zx_handle_t info_resource;
  zx_status_t ret = harvester::GetInfoResource(&info_resource);
  if (ret != ZX_OK) {
    exit(EXIT_CODE_GENERAL_ERROR);
  }

  std::unique_ptr<harvester::OS> os = std::make_unique<harvester::OSImpl>();

  harvester::Harvester harvester(info_resource, std::move(dockyard_proxy),
                                 std::move(os));
  harvester.GatherDeviceProperties();
  harvester.GatherFastData(fast_dispatcher);
  harvester.GatherSlowData(slow_calls_loop.dispatcher());
  harvester.GatherLogs();
  // Best practice across Fuchsia codebase is to always start the trace provider
  // even if NTRACE is defined.
  trace::TraceProviderWithFdio trace_provider(trace_loop.dispatcher(),
                                              "system_monitor_harvester");
  // The slow_calls_thread that runs heavier calls takes over this thread.
  slow_calls_loop.Run(zx::time::infinite(), run_loop_once);
  fast_calls_loop.Quit();
  fast_calls_loop.JoinThreads();
  trace_loop.Quit();
  trace_loop.JoinThreads();

  FX_LOGS(INFO) << "System Monitor Harvester - exiting";
  return 0;
}
