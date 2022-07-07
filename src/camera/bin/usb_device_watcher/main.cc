// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fuchsia/camera/test/cpp/fidl.h>
#include <fuchsia/camera3/cpp/fidl.h>
#include <fuchsia/hardware/camera/cpp/fidl.h>
#include <fuchsia/sys/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/fdio/directory.h>
#include <lib/fdio/fdio.h>
#include <lib/fidl/cpp/binding_set.h>
#include <lib/sys/cpp/component_context.h>
#include <lib/syslog/cpp/log_settings.h>
#include <lib/syslog/cpp/macros.h>

#include "src/camera/bin/usb_device_watcher/device_watcher_impl.h"
#include "src/lib/fsl/io/device_watcher.h"

constexpr auto kCameraPath = "/dev/class/camera";

static fpromise::result<fuchsia::hardware::camera::DeviceHandle, zx_status_t> GetCamera(
    std::string path) {
  fuchsia::hardware::camera::DeviceHandle camera;
  zx_status_t status =
      fdio_service_connect(path.c_str(), camera.NewRequest().TakeChannel().release());
  if (status != ZX_OK) {
    FX_PLOGS(ERROR, status);
    return fpromise::error(status);
  }

  return fpromise::ok(std::move(camera));
}

int main(int argc, char* argv[]) {
  syslog::SetLogSettings({.min_log_level = CAMERA_MIN_LOG_LEVEL},
                         {"camera", "camera_device_watcher"});

  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);

  auto context = sys::ComponentContext::Create();

  auto directory = sys::ComponentContext::CreateAndServeOutgoingDirectory();

  auto result = DeviceWatcherImpl::Create(std::move(context), loop.dispatcher());
  if (result.is_error()) {
    FX_PLOGS(FATAL, result.error());
    return EXIT_FAILURE;
  }

  auto server = result.take_value();
  auto watcher = fsl::DeviceWatcher::CreateWithIdleCallback(
      kCameraPath,
      [&](int dir_fd, std::string path) {
        auto full_path = std::string(kCameraPath) + "/" + path;
        auto result = GetCamera(full_path);
        if (result.is_error()) {
          FX_PLOGS(INFO, result.error()) << "Couldn't get camera from " << full_path
                                         << ". This device will not be exposed to clients.";
          return;
        }
        auto add_result = server->AddDevice(result.take_value());
        if (add_result.is_error()) {
          FX_PLOGS(WARNING, add_result.error()) << "Failed to add camera from " << full_path
                                                << ". This device will not be exposed to clients.";
          return;
        }
      },
      [&]() { server->UpdateClients(); });
  if (!watcher) {
    FX_LOGS(FATAL);
    return EXIT_FAILURE;
  }

  directory->outgoing()->AddPublicService(server->GetHandler());

  // TODO(ernesthua) - Removed tester interface. Need to restore it on merge back.

  loop.Run();
  return EXIT_SUCCESS;
}
