// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BIN_DRIVER_MANAGER_V2_DRIVER_HOST_H_
#define SRC_DEVICES_BIN_DRIVER_MANAGER_V2_DRIVER_HOST_H_

#include <fidl/fuchsia.driver.host/cpp/wire.h>

#include <fbl/intrusive_double_list.h>

namespace dfv2 {

class DriverHost {
 public:
  virtual zx::status<fidl::ClientEnd<fuchsia_driver_host::Driver>> Start(
      fidl::ClientEnd<fuchsia_driver_framework::Node> client_end,
      fidl::VectorView<fuchsia_driver_framework::wire::NodeSymbol> symbols,
      fuchsia_component_runner::wire::ComponentStartInfo start_info) = 0;

  virtual zx::status<uint64_t> GetProcessKoid() const = 0;
};

class DriverHostComponent final
    : public DriverHost,
      public fbl::DoublyLinkedListable<std::unique_ptr<DriverHostComponent>> {
 public:
  DriverHostComponent(fidl::ClientEnd<fuchsia_driver_host::DriverHost> driver_host,
                      async_dispatcher_t* dispatcher,
                      fbl::DoublyLinkedList<std::unique_ptr<DriverHostComponent>>* driver_hosts);

  zx::status<fidl::ClientEnd<fuchsia_driver_host::Driver>> Start(
      fidl::ClientEnd<fuchsia_driver_framework::Node> client_end,
      fidl::VectorView<fuchsia_driver_framework::wire::NodeSymbol> symbols,
      fuchsia_component_runner::wire::ComponentStartInfo start_info) override;

  zx::status<uint64_t> GetProcessKoid() const override;

 private:
  fidl::WireSharedClient<fuchsia_driver_host::DriverHost> driver_host_;
};

}  // namespace dfv2

#endif  // SRC_DEVICES_BIN_DRIVER_MANAGER_V2_DRIVER_HOST_H_
