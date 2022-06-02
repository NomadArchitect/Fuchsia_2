// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_SYS_FUZZING_COMMON_COMPONENT_CONTEXT_H_
#define SRC_SYS_FUZZING_COMMON_COMPONENT_CONTEXT_H_

#include <lib/async-loop/cpp/loop.h>
#include <lib/fidl/cpp/interface_request.h>
#include <lib/sys/cpp/outgoing_directory.h>
#include <lib/sys/cpp/service_directory.h>
#include <lib/syslog/cpp/macros.h>
#include <zircon/status.h>

#include "src/lib/fxl/macros.h"
#include "src/sys/fuzzing/common/async-types.h"

namespace fuzzing {

// This class is a wrapper around |sys::ComponentContext| that provides some additional common
// behaviors, such as making an |async::Loop| and scheduling a primary task on an |async::Executor|.
class ComponentContext final {
 public:
  // This constructor is rarely used directly. Instead, most clients create a
  // component context using one of |Create...| static methods below.
  using LoopPtr = std::unique_ptr<async::Loop>;
  using ServiceDirectoryPtr = std::shared_ptr<sys::ServiceDirectory>;
  using OutgoingDirectoryPtr = std::shared_ptr<sys::OutgoingDirectory>;
  ComponentContext(LoopPtr loop, ExecutorPtr executor, ServiceDirectoryPtr svc,
                   OutgoingDirectoryPtr outgoing);
  ~ComponentContext();

  // Creates a component context. This method consumes startup handles in order to serve FIDL
  // protocols, and can therefore be called at most once per process.
  static std::unique_ptr<ComponentContext> Create();

  // Creates an "auxiliary" context that does not have an outgoing directory. Such a context can
  // only be used for creating FIDL clients, but does not consume any startup handles and thus does
  // not preclude creating other component contexts.
  static std::unique_ptr<ComponentContext> CreateAuxillary();

  // Creates a context that does not own its |executor|'s loop. This is useful for tests which
  // provide and executor from a test loop.
  static std::unique_ptr<ComponentContext> CreateWithExecutor(ExecutorPtr executor);

  const ExecutorPtr& executor() const { return executor_; }

  // Adds an interface request handler for a protocol capability provided by this component.
  template <typename Interface>
  zx_status_t AddPublicService(fidl::InterfaceRequestHandler<Interface> handler) const {
    return outgoing_->AddPublicService(std::move(handler));
  }

  // Connects a |request| to a protocol capability provided by another component.
  template <typename Interface>
  zx_status_t Connect(fidl::InterfaceRequest<Interface> request) {
    return Connect(svc_, std::move(request));
  }

  // Returns a handler to connect |request|s to a protocol capability provided by another component.
  template <typename Interface>
  fidl::InterfaceRequestHandler<Interface> MakeRequestHandler() {
    return [svc = svc_](fidl::InterfaceRequest<Interface> request) {
      Connect(svc, std::move(request));
    };
  }

  // Schedules a task to be executed when |Run| is invoked.
  template <typename Task>
  void ScheduleTask(Task task) {
    executor_->schedule_task(std::move(task));
  }

  // Runs the message loop on the current thread. This method should only be called at most once.
  zx_status_t Run();

  // Runs until there are no tasks that can make progress.
  zx_status_t RunUntilIdle();

 private:
  // Connects a |request| to a protocol capability provided by another component.
  template <typename Interface>
  static zx_status_t Connect(ServiceDirectoryPtr svc, fidl::InterfaceRequest<Interface> request) {
    if (auto status = svc->Connect(std::move(request)); status != ZX_OK) {
      FX_LOGS(ERROR) << "Failed to connect to " << Interface::Name_ << ": "
                     << zx_status_get_string(status);
      return status;
    }
    return ZX_OK;
  }

  LoopPtr loop_;
  ExecutorPtr executor_;
  ServiceDirectoryPtr svc_;
  OutgoingDirectoryPtr outgoing_;

  FXL_DISALLOW_COPY_ASSIGN_AND_MOVE(ComponentContext);
};

}  // namespace fuzzing

#endif  // SRC_SYS_FUZZING_COMMON_COMPONENT_CONTEXT_H_
