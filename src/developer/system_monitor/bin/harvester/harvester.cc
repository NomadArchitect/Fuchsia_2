// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "harvester.h"

#include <lib/async/cpp/task.h>
#include <lib/async/cpp/time.h>
#include <lib/async/dispatcher.h>
#include <lib/sys/cpp/service_directory.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/time.h>

#include <memory>

#include "gather_channels.h"
#include "gather_cpu.h"
#include "gather_device_info.h"
#include "gather_memory.h"
#include "gather_processes_and_memory.h"
#include "gather_threads_and_cpu.h"

namespace harvester {

Harvester::Harvester(zx_handle_t info_resource,
                     std::unique_ptr<DockyardProxy> dockyard_proxy,
                     std::unique_ptr<OS> os)
    : info_resource_(info_resource),
      dockyard_proxy_(std::move(dockyard_proxy)),
      os_(std::move(os)),
      log_listener_(sys::ServiceDirectory::CreateFromNamespace()) {}

void Harvester::GatherDeviceProperties() {
  FX_VLOGS(1) << "Harvester::GatherDeviceProperties";
  gather_device_info_.GatherDeviceProperties();
  gather_cpu_.GatherDeviceProperties();
  gather_memory_.GatherDeviceProperties();

  gather_vmos_.GatherDeviceProperties();
}

void Harvester::GatherLogs() {
  log_listener_.Listen([this](std::vector<const std::string> batch) {
    dockyard_proxy_->SendLogs(batch);
  });
}

void Harvester::GatherFastData(async_dispatcher_t* dispatcher) {
  FX_VLOGS(1) << "Harvester::GatherFastData";
  zx::time now = async::Now(dispatcher);
  gather_threads_and_cpu_.PostUpdate(dispatcher, now, zx::msec(100));
}

void Harvester::GatherSlowData(async_dispatcher_t* dispatcher) {
  FX_VLOGS(1) << "Harvester::GatherSlowData";
  zx::time now = async::Now(dispatcher);

  gather_channels_.PostUpdate(dispatcher, now, zx::sec(1));
  gather_processes_and_memory_.PostUpdate(dispatcher, now, zx::sec(2));
  gather_vmos_.PostUpdate(dispatcher, now, zx::sec(2));
  gather_device_info_.PostUpdate(dispatcher, now, zx::sec(5));
}

}  // namespace harvester
