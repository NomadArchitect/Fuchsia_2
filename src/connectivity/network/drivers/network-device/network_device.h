// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_NETWORK_DEVICE_H_
#define SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_NETWORK_DEVICE_H_

#include <fuchsia/hardware/network/llcpp/fidl.h>
#include <fuchsia/hardware/network/mac/cpp/banjo.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/ddk/driver.h>

#include <ddktl/device.h>
#include <ddktl/fidl.h>
#include <ddktl/protocol/empty-protocol.h>

#include "device/public/network_device.h"
#include "mac/public/network_mac.h"

namespace network {

class NetworkDevice;
using DeviceType = ddk::Device<NetworkDevice, ddk::Messageable, ddk::Unbindable>;

class NetworkDevice : public DeviceType,
                      public ddk::EmptyProtocol<ZX_PROTOCOL_NETWORK_DEVICE>,
                      public fidl::WireInterface<fuchsia_hardware_network::DeviceInstance> {
 public:
  explicit NetworkDevice(zx_device_t* parent)
      : DeviceType(parent), loop_(&kAsyncLoopConfigNeverAttachToThread) {}
  ~NetworkDevice() override;

  static zx_status_t Create(void* ctx, zx_device_t* parent);

  zx_status_t DdkMessage(fidl_incoming_msg_t* msg, fidl_txn_t* txn);

  void DdkUnbind(ddk::UnbindTxn unbindTxn);

  void DdkRelease();

  void GetDevice(fidl::ServerEnd<fuchsia_hardware_network::Device> device,
                 GetDeviceCompleter::Sync& _completer) override;
  void GetMacAddressing(fidl::ServerEnd<fuchsia_hardware_network::MacAddressing> mac,
                        GetMacAddressingCompleter::Sync& _completer) override;

 private:
  std::optional<thrd_t> loop_thread_;
  async::Loop loop_;
  std::unique_ptr<NetworkDeviceInterface> device_;
  std::unique_ptr<MacAddrDeviceInterface> mac_;
};
}  // namespace network

#endif  // SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_NETWORK_DEVICE_H_
