// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_SYS_FUZZING_COMMON_CONTROLLER_PROVIDER_H_
#define SRC_SYS_FUZZING_COMMON_CONTROLLER_PROVIDER_H_

#include <fuchsia/fuzzer/cpp/fidl.h>
#include <lib/fidl/cpp/binding.h>
#include <lib/fidl/cpp/interface_request.h>
#include <lib/zx/channel.h>

#include "src/lib/fxl/macros.h"
#include "src/sys/fuzzing/common/async-types.h"
#include "src/sys/fuzzing/common/controller.h"
#include "src/sys/fuzzing/common/runner.h"

namespace fuzzing {

using ::fuchsia::fuzzer::Controller;
using ::fuchsia::fuzzer::ControllerProvider;
using ::fuchsia::fuzzer::RegistrarPtr;

class ControllerProviderImpl final : public ControllerProvider {
 public:
  explicit ControllerProviderImpl(ExecutorPtr executor);
  ~ControllerProviderImpl() override = default;

  // FIDL methods.
  void Connect(fidl::InterfaceRequest<Controller> request, ConnectCallback callback) override;
  void Stop() override;

  // Sets the runner. Except for unit tests, callers should prefer |Run|.
  void SetRunner(RunnerPtr runner);

  // Promises to register with the fuzz-registrar as being able to fulfill requests to connect to
  // this object's |Controller|. Except for unit tests, callers should prefer |Run|.
  Promise<> Serve(zx::channel channel);

 private:
  fidl::Binding<ControllerProvider> binding_;
  ControllerImpl controller_;
  RegistrarPtr registrar_;

  FXL_DISALLOW_COPY_ASSIGN_AND_MOVE(ControllerProviderImpl);
};

}  // namespace fuzzing

#endif  // SRC_SYS_FUZZING_COMMON_CONTROLLER_PROVIDER_H_
