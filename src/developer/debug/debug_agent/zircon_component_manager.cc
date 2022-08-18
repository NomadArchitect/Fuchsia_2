// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/debug_agent/zircon_component_manager.h"

#include <fuchsia/diagnostics/cpp/fidl.h>
#include <fuchsia/io/cpp/fidl.h>
#include <fuchsia/sys/cpp/fidl.h>
#include <fuchsia/sys2/cpp/fidl.h>
#include <fuchsia/test/manager/cpp/fidl.h>
#include <lib/fit/defer.h>
#include <lib/sys/cpp/termination_reason.h>
#include <lib/syslog/cpp/macros.h>
#include <unistd.h>
#include <zircon/processargs.h>
#include <zircon/types.h>

#include <cstdint>
#include <memory>
#include <optional>
#include <string>
#include <utility>

#include "src/developer/debug/debug_agent/debug_agent.h"
#include "src/developer/debug/debug_agent/debugged_process.h"
#include "src/developer/debug/debug_agent/filter.h"
#include "src/developer/debug/debug_agent/stdio_handles.h"
#include "src/developer/debug/debug_agent/zircon_utils.h"
#include "src/developer/debug/ipc/records.h"
#include "src/developer/debug/shared/component_utils.h"
#include "src/developer/debug/shared/logging/file_line_function.h"
#include "src/developer/debug/shared/logging/logging.h"
#include "src/developer/debug/shared/message_loop.h"
#include "src/developer/debug/shared/status.h"
#include "src/lib/fxl/memory/ref_counted.h"
#include "src/lib/fxl/memory/ref_ptr.h"

namespace debug_agent {

namespace {

// Maximum time we wait for reading "elf/job_id" in the runtime directory.
constexpr uint64_t kMaxWaitMsForJobId = 1000;

// Maximum time we wait for a component to start.
constexpr uint64_t kMaxWaitMsForComponent = 1000;

// Attempts to link a zircon socket into the new component's file descriptor number represented by
// |fd|. If successful, the socket will be connected and a (one way) communication channel with that
// file descriptor will be made.
zx::socket AddStdio(int fd, fuchsia::sys::LaunchInfo* launch_info) {
  zx::socket local;
  zx::socket target;

  zx_status_t status = zx::socket::create(ZX_SOCKET_STREAM, &local, &target);
  if (status != ZX_OK)
    return zx::socket();

  auto io = std::make_unique<fuchsia::sys::FileDescriptor>();
  io->type0 = PA_HND(PA_FD, fd);
  io->handle0 = std::move(target);

  if (fd == STDOUT_FILENO) {
    launch_info->out = std::move(io);
  } else if (fd == STDERR_FILENO) {
    launch_info->err = std::move(io);
  } else {
    FX_NOTREACHED() << "Invalid file descriptor: " << fd;
    return zx::socket();
  }

  return local;
}

// Read the content of "elf/job_id" in the runtime directory of an ELF component.
//
// |callback| will be issued with ZX_KOID_INVALID if there's any error.
// |moniker| is only used for error logging.
void ReadElfJobId(fuchsia::io::DirectoryHandle runtime_dir_handle, const std::string& moniker,
                  fit::callback<void(zx_koid_t)> cb) {
  fuchsia::io::DirectoryPtr runtime_dir = runtime_dir_handle.Bind();
  fuchsia::io::FilePtr job_id_file;
  runtime_dir->Open(
      fuchsia::io::OpenFlags::RIGHT_READABLE, 0, "elf/job_id",
      fidl::InterfaceRequest<fuchsia::io::Node>(job_id_file.NewRequest().TakeChannel()));
  job_id_file.set_error_handler(
      [cb = cb.share()](zx_status_t err) mutable { cb(ZX_KOID_INVALID); });
  job_id_file->Read(
      fuchsia::io::MAX_TRANSFER_SIZE,
      [cb = cb.share(), moniker](fuchsia::io::File2_Read_Result res) mutable {
        if (!res.is_response()) {
          return cb(ZX_KOID_INVALID);
        }
        std::string job_id_str(reinterpret_cast<const char*>(res.response().data.data()),
                               res.response().data.size());
        // We use std::strtoull here because std::stoull is not exception-safe.
        char* end;
        zx_koid_t job_id = std::strtoull(job_id_str.c_str(), &end, 10);
        if (end != job_id_str.c_str() + job_id_str.size()) {
          FX_LOGS(ERROR) << "Invalid elf/job_id for " << moniker << ": " << job_id_str;
          return cb(ZX_KOID_INVALID);
        }
        cb(job_id);
      });
  debug::MessageLoop::Current()->PostTimer(
      FROM_HERE, kMaxWaitMsForJobId,
      [cb = std::move(cb), file = std::move(job_id_file), moniker]() mutable {
        if (cb) {
          FX_LOGS(WARNING) << "Timeout reading elf/job_id for " << moniker;
          file.Unbind();
          cb(ZX_KOID_INVALID);
        }
      });
}

std::string to_string(fuchsia::component::Error err) {
  static const char* const errors[] = {
      "INTERNAL",                   // 1
      "INVALID_ARGUMENTS",          // 2
      "UNSUPPORTED",                // 3
      "ACCESS_DENIED",              // 4
      "INSTANCE_NOT_FOUND",         // 5
      "INSTANCE_ALREADY_EXISTS",    // 6
      "INSTANCE_CANNOT_START",      // 7
      "INSTANCE_CANNOT_RESOLVE",    // 8
      "COLLECTION_NOT_FOUND",       // 9
      "RESOURCE_UNAVAILABLE",       // 10
      "INSTANCE_DIED",              // 11
      "RESOURCE_NOT_FOUND",         // 12
      "INSTANCE_CANNOT_UNRESOLVE",  // 13
  };
  int n = static_cast<int>(err);
  if (n < 1 || n > 13) {
    return "Invalid error";
  }
  return errors[n - 1];
}

}  // namespace

ZirconComponentManager::ZirconComponentManager(SystemInterface* system_interface,
                                               std::shared_ptr<sys::ServiceDirectory> services)
    : ComponentManager(system_interface),
      services_(std::move(services)),
      event_stream_binding_(this),
      weak_factory_(this) {
  // 1. Subscribe to "debug_started" and "stopped" events.
  fuchsia::sys2::EventSourceSyncPtr event_source;
  services_->Connect(event_source.NewRequest());
  std::vector<fuchsia::sys2::EventSubscription> subscriptions;
  subscriptions.resize(2);
  subscriptions[0].set_event_name("debug_started");
  subscriptions[1].set_event_name("stopped");
  fuchsia::sys2::EventStreamHandle stream;
  event_stream_binding_.Bind(stream.NewRequest());
  fuchsia::sys2::EventSource_Subscribe_Result subscribe_res;
  event_source->Subscribe(std::move(subscriptions), std::move(stream), &subscribe_res);
  if (subscribe_res.is_err()) {
    FX_LOGS(ERROR) << "Failed to Subscribe: " << static_cast<uint32_t>(subscribe_res.err());
  }

  // 2. List existing components via fuchsia.sys2.RealmExplorer and fuchsia.sys2.RealmQuery.
  fuchsia::sys2::RealmExplorerSyncPtr realm_explorer;
  fuchsia::sys2::RealmQuerySyncPtr realm_query;
  services_->Connect(realm_explorer.NewRequest(), "fuchsia.sys2.RealmExplorer.root");
  services_->Connect(realm_query.NewRequest(), "fuchsia.sys2.RealmQuery.root");

  fuchsia::sys2::RealmExplorer_GetAllInstanceInfos_Result all_instance_infos_res;
  realm_explorer->GetAllInstanceInfos(&all_instance_infos_res);
  if (all_instance_infos_res.is_err()) {
    FX_LOGS(ERROR) << "Failed to GetAllInstanceInfos: "
                   << static_cast<uint32_t>(all_instance_infos_res.err());
    return;
  }
  fuchsia::sys2::InstanceInfoIteratorSyncPtr instance_it =
      all_instance_infos_res.response().iterator.BindSync();

  auto deferred_ready =
      std::make_shared<fit::deferred_callback>([weak_this = weak_factory_.GetWeakPtr()] {
        if (weak_this && weak_this->ready_callback_)
          weak_this->ready_callback_();
      });
  while (true) {
    std::vector<fuchsia::sys2::InstanceInfo> infos;
    instance_it->Next(&infos);
    if (infos.empty()) {
      break;
    }
    for (auto& info : infos) {
      if (info.state != fuchsia::sys2::InstanceState::STARTED || info.moniker.empty()) {
        continue;
      }
      fuchsia::sys2::RealmQuery_GetInstanceInfo_Result instace_info_res;
      realm_query->GetInstanceInfo(info.moniker, &instace_info_res);
      if (!instace_info_res.is_response() || !instace_info_res.response().resolved ||
          !instace_info_res.response().resolved->started ||
          !instace_info_res.response().resolved->started->runtime_dir) {
        continue;
      }
      // Remove the "." at the beginning of the moniker. It's safe because moniker is not empty.
      std::string moniker = info.moniker.substr(1);
      ReadElfJobId(std::move(instace_info_res.response().resolved->started->runtime_dir), moniker,
                   [weak_this = weak_factory_.GetWeakPtr(), moniker, url = std::move(info.url),
                    deferred_ready](zx_koid_t job_id) {
                     if (weak_this && job_id != ZX_KOID_INVALID) {
                       weak_this->running_component_info_[job_id] = {.moniker = moniker,
                                                                     .url = url};
                     }
                   });
    }
  }
}

void ZirconComponentManager::SetReadyCallback(fit::callback<void()> callback) {
  if (ready_callback_) {
    ready_callback_ = std::move(callback);
  } else {
    debug::MessageLoop::Current()->PostTask(FROM_HERE,
                                            [cb = std::move(callback)]() mutable { cb(); });
  }
}

void ZirconComponentManager::OnEvent(fuchsia::sys2::Event event) {
  if (!event.has_header() || !event.header().has_moniker() || event.header().moniker().empty() ||
      !event.has_event_result() || !event.event_result().is_payload()) {
    return;
  }
  // Remove the "." at the beginning of the moniker. It's safe because moniker is not empty.
  std::string moniker = event.header().moniker().substr(1);
  switch (event.header().event_type()) {
    case fuchsia::sys2::EventType::DEBUG_STARTED:
      if (event.event_result().payload().is_debug_started() &&
          event.event_result().payload().debug_started().has_runtime_dir()) {
        ReadElfJobId(
            std::move(
                *event.mutable_event_result()->payload().debug_started().mutable_runtime_dir()),
            moniker,
            [weak_this = weak_factory_.GetWeakPtr(), moniker,
             url = event.header().component_url()](zx_koid_t job_id) {
              if (weak_this && job_id != ZX_KOID_INVALID) {
                weak_this->running_component_info_[job_id] = {.moniker = moniker, .url = url};
                DEBUG_LOG(Process)
                    << "Component started job_id=" << job_id
                    << " moniker=" << weak_this->running_component_info_[job_id].moniker
                    << " url=" << weak_this->running_component_info_[job_id].url;
              }
            });
      }
      break;
    case fuchsia::sys2::EventType::STOPPED: {
      for (auto it = running_component_info_.begin(); it != running_component_info_.end(); it++) {
        if (it->second.moniker == moniker) {
          DEBUG_LOG(Process) << "Component stopped job_id=" << it->first
                             << " moniker=" << it->second.moniker << " url=" << it->second.url;
          running_component_info_.erase(it);
          expected_v2_components_.erase(moniker);
          break;
        }
      }
      break;
    }
    default:
      FX_NOTREACHED();
  }
}

std::optional<debug_ipc::ComponentInfo> ZirconComponentManager::FindComponentInfo(
    zx_koid_t job_koid) const {
  if (auto it = running_component_info_.find(job_koid); it != running_component_info_.end())
    return it->second;
  return std::nullopt;
}

// We need a class to help to launch a test because the lifecycle of GetEvents callbacks
// are undetermined.
class ZirconComponentManager::TestLauncher : public fxl::RefCountedThreadSafe<TestLauncher> {
 public:
  // This function can only be called once.
  debug::Status Launch(std::string url, std::vector<std::string> case_filters,
                       ZirconComponentManager* component_manager, DebugAgent* debug_agent) {
    test_url_ = std::move(url);
    component_manager_ = component_manager->GetWeakPtr();
    debug_agent_ = debug_agent->GetWeakPtr();

    if (component_manager->running_tests_info_.count(test_url_))
      return debug::Status("Test " + test_url_ + " is already launched");

    fuchsia::test::manager::RunBuilderSyncPtr run_builder;
    auto status = component_manager->services_->Connect(run_builder.NewRequest());
    if (status != ZX_OK)
      return debug::ZxStatus(status);

    DEBUG_LOG(Process) << "Launching test url=" << test_url_;

    fuchsia::test::manager::RunOptions run_options;
    run_options.set_case_filters_to_run(std::move(case_filters));
    run_options.set_arguments({"--gtest_break_on_failure"});  // does no harm to rust tests.
    run_builder->AddSuite(test_url_, std::move(run_options), suite_controller_.NewRequest());
    run_builder->Build(run_controller_.NewRequest());
    run_controller_->GetEvents(
        [self = fxl::RefPtr<TestLauncher>(this)](auto res) { self->OnRunEvents(std::move(res)); });
    suite_controller_->GetEvents([self = fxl::RefPtr<TestLauncher>(this)](auto res) {
      self->OnSuiteEvents(std::move(res));
    });
    component_manager->running_tests_info_[test_url_] = {};
    return debug::Status();
  }

 private:
  // Stdout and stderr are in case_artifact. Logging is in suite_artifact. Others are ignored.
  // NOTE: custom.component_moniker in suite_artifact is NOT the moniker of the test!
  void OnSuiteEvents(fuchsia::test::manager::SuiteController_GetEvents_Result result) {
    if (!component_manager_ || result.is_err() || result.response().events.empty()) {
      suite_controller_.Unbind();  // Otherwise the run_controller won't return.
      if (result.is_err())
        FX_LOGS(WARNING) << "Failed to launch test: " << result.err();
      DEBUG_LOG(Process) << "Test finished url=" << test_url_;
      if (component_manager_)
        component_manager_->running_tests_info_.erase(test_url_);
      return;
    }
    for (auto& event : result.response().events) {
      FX_CHECK(event.has_payload());
      if (event.payload().is_case_found()) {
        auto& test_info = component_manager_->running_tests_info_[test_url_];
        // Test cases should come in order.
        FX_CHECK(test_info.case_names.size() == event.payload().case_found().identifier);
        test_info.case_names.push_back(event.payload().case_found().test_case_name);
        if (event.payload().case_found().test_case_name.find_first_of('.') != std::string::npos) {
          test_info.ignored_process = 1;
        }
      } else if (event.payload().is_case_artifact()) {
        if (auto proc = GetDebuggedProcess(event.payload().case_artifact().identifier)) {
          auto& artifact = event.mutable_payload()->case_artifact().artifact;
          if (artifact.is_stdout_()) {
            proc->SetStdout(std::move(artifact.stdout_()));
          } else if (artifact.is_stderr_()) {
            proc->SetStderr(std::move(artifact.stderr_()));
          }
        } else {
          FX_LOGS(ERROR) << "Cannot find the process to set stdout/stderr for test case "
                         << event.payload().case_artifact().identifier;
        }
      } else if (event.payload().is_suite_artifact()) {
        auto& artifact = event.mutable_payload()->suite_artifact().artifact;
        if (artifact.is_log()) {
          FX_CHECK(artifact.log().is_batch());
          log_listener_ = artifact.log().batch().Bind();
          log_listener_->GetNext(
              [self = fxl::RefPtr<TestLauncher>(this)](auto res) { self->OnLog(std::move(res)); });
        }
      }
    }
    suite_controller_->GetEvents([self = fxl::RefPtr<TestLauncher>(this)](auto res) {
      self->OnSuiteEvents(std::move(res));
    });
  }

  // See the comment above |running_tests_info_| for the logic.
  DebuggedProcess* GetDebuggedProcess(uint32_t test_identifier) {
    auto& test_info = component_manager_->running_tests_info_[test_url_];
    auto& pids = test_info.pids;
    auto proc_idx = test_identifier + test_info.ignored_process;
    if (proc_idx < pids.size() && debug_agent_) {
      return debug_agent_->GetDebuggedProcess(pids[proc_idx]);
    }
    return nullptr;
  }

  // Unused but required by the test framework.
  void OnRunEvents(std::vector<::fuchsia::test::manager::RunEvent> events) {
    if (!events.empty()) {
      FX_LOGS_FIRST_N(WARNING, 1) << "Not implemented yet";
      run_controller_->GetEvents([self = fxl::RefPtr<TestLauncher>(this)](auto res) {
        self->OnRunEvents(std::move(res));
      });
    } else {
      run_controller_.Unbind();
    }
  }

  // Unused but required.
  void OnLog(fuchsia::diagnostics::BatchIterator_GetNext_Result result) {
    if (result.is_response() && !result.response().batch.empty()) {
      FX_LOGS_FIRST_N(WARNING, 1) << "Not implemented yet";
      log_listener_->GetNext(
          [self = fxl::RefPtr<TestLauncher>(this)](auto res) { self->OnLog(std::move(res)); });
    } else {
      if (result.is_err())
        FX_LOGS(ERROR) << "Failed to read log";
      log_listener_.Unbind();  // Otherwise archivist won't terminate.
    }
  }

  fxl::WeakPtr<DebugAgent> debug_agent_;
  fxl::WeakPtr<ZirconComponentManager> component_manager_;
  std::string test_url_;
  fuchsia::test::manager::RunControllerPtr run_controller_;
  fuchsia::test::manager::SuiteControllerPtr suite_controller_;
  fuchsia::diagnostics::BatchIteratorPtr log_listener_;
};

debug::Status ZirconComponentManager::LaunchComponent(const std::vector<std::string>& argv) {
  if (argv.empty()) {
    return debug::Status("No argument provided for LaunchComponent");
  }
  if (cpp20::ends_with(std::string_view{argv[0]}, ".cmx")) {
    return LaunchV1Component(argv);
  }
  return LaunchV2Component(argv);
}

debug::Status ZirconComponentManager::LaunchTest(std::string url,
                                                 std::vector<std::string> case_filters,
                                                 DebugAgent* debug_agent) {
  return fxl::MakeRefCounted<TestLauncher>()->Launch(std::move(url), std::move(case_filters), this,
                                                     debug_agent);
}

debug::Status ZirconComponentManager::LaunchV1Component(const std::vector<std::string>& argv) {
  std::string url = argv.front();
  std::string name = url.substr(url.find_last_of('/') + 1);

  if (expected_v1_components_.count(name)) {
    return debug::Status(name + " is being launched");
  }

  // Prepare the launch info. The parameters to the component do not include the component URL.
  fuchsia::sys::LaunchInfo launch_info;
  launch_info.url = url;
  if (argv.size() > 1) {
    launch_info.arguments.emplace();
    for (size_t i = 1; i < argv.size(); i++)
      launch_info.arguments->push_back(argv[i]);
  }

  StdioHandles handles;
  handles.out = AddStdio(STDOUT_FILENO, &launch_info);
  handles.err = AddStdio(STDERR_FILENO, &launch_info);

  DEBUG_LOG(Process) << "Launching component url=" << url;

  fuchsia::sys::LauncherSyncPtr launcher;
  services_->Connect(launcher.NewRequest());
  fuchsia::sys::ComponentControllerPtr controller;
  auto status = launcher->CreateComponent(std::move(launch_info), controller.NewRequest());
  if (status != ZX_OK)
    return debug::ZxStatus(status);

  // We don't need to wait for the termination.
  controller->Detach();

  expected_v1_components_.emplace(name, std::move(handles));

  debug::MessageLoop::Current()->PostTimer(
      FROM_HERE, kMaxWaitMsForComponent, [weak_this = weak_factory_.GetWeakPtr(), name]() {
        if (weak_this && weak_this->expected_v1_components_.count(name)) {
          FX_LOGS(WARNING) << "Timeout waiting for component " << name << " to start.";
          weak_this->expected_v1_components_.erase(name);
        }
      });

  return debug::Status();
}

debug::Status ZirconComponentManager::LaunchV2Component(const std::vector<std::string>& argv) {
  constexpr char kParentMoniker[] = "./core";
  constexpr char kCollection[] = "ffx-laboratory";

  // url: fuchsia-pkg://fuchsia.com/crasher#meta/cpp_crasher.cm
  std::string url = argv[0];
  size_t name_start = url.find_last_of('/') + 1;
  // name: cpp_crasher
  std::string name = url.substr(name_start, url.find_last_of('.') - name_start);
  // moniker: /core/ffx-laboratory:cpp_crasher
  std::string moniker = std::string(kParentMoniker + 1) + "/" + kCollection + ":" + name;

  if (argv.size() != 1) {
    return debug::Status("v2 components cannot accept command line arguments");
  }
  if (expected_v2_components_.count(moniker)) {
    return debug::Status(url + " is already launched");
  }

  fuchsia::sys2::LifecycleControllerSyncPtr lifecycle_controller;
  auto status = services_->Connect(lifecycle_controller.NewRequest(),
                                   "fuchsia.sys2.LifecycleController.root");
  if (status != ZX_OK)
    return debug::ZxStatus(status);

  DEBUG_LOG(Process) << "Launching component url=" << url << " moniker=" << moniker;

  fuchsia::sys2::LifecycleController_CreateChild_Result create_res;
  auto create_child = [&]() {
    fuchsia::component::decl::Child child_decl;
    child_decl.set_name(name);
    child_decl.set_url(url);
    child_decl.set_startup(fuchsia::component::decl::StartupMode::LAZY);
    return lifecycle_controller->CreateChild(kParentMoniker, {kCollection}, std::move(child_decl),
                                             {}, &create_res);
  };
  status = create_child();
  if (status != ZX_OK)
    return debug::ZxStatus(status);

  if (create_res.is_err() &&
      create_res.err() == fuchsia::component::Error::INSTANCE_ALREADY_EXISTS) {
    fuchsia::sys2::LifecycleController_DestroyChild_Result destroy_res;
    fuchsia::component::decl::ChildRef child_ref{.name = name, .collection = kCollection};
    status = lifecycle_controller->DestroyChild(kParentMoniker, child_ref, &destroy_res);
    if (status != ZX_OK)
      return debug::ZxStatus(status);
    if (destroy_res.is_err())
      return debug::Status("Failed to destroy component " + moniker + ": " +
                           to_string(destroy_res.err()));
    status = create_child();
    if (status != ZX_OK)
      return debug::ZxStatus(status);
  }
  if (create_res.is_err())
    return debug::Status("Failed to create the component: " + to_string(create_res.err()));

  fuchsia::sys2::LifecycleController_Start_Result start_res;
  // LifecycleController::Start accepts relative monikers.
  status = lifecycle_controller->Start("." + moniker, &start_res);
  if (status != ZX_OK)
    return debug::ZxStatus(status);
  if (start_res.is_err())
    return debug::Status("Failed to start the component: " + to_string(start_res.err()));

  expected_v2_components_.insert(moniker);
  return debug::Status();
}

bool ZirconComponentManager::OnProcessStart(const ProcessHandle& process, StdioHandles* out_stdio,
                                            std::string* process_name_override) {
  if (auto it = expected_v1_components_.find(process.GetName());
      it != expected_v1_components_.end()) {
    *out_stdio = std::move(it->second);
    expected_v1_components_.erase(it);
    return true;
  }
  if (auto component = ComponentManager::FindComponentInfo(process)) {
    if (expected_v2_components_.count(component->moniker)) {
      // It'll be erased in the stopped event.
      return true;
    }
    if (auto it = running_tests_info_.find(component->url); it != running_tests_info_.end()) {
      size_t idx = it->second.pids.size();
      it->second.pids.push_back(process.GetKoid());
      if (idx < it->second.ignored_process) {
        return false;
      }
      idx -= it->second.ignored_process;
      if (idx < it->second.case_names.size()) {
        *process_name_override = it->second.case_names[idx];
      }
      return true;
    }
  }
  return false;
}

}  // namespace debug_agent
