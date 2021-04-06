// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fuchsia/gpu/magma/llcpp/fidl.h>
#include <fuchsia/hardware/intelgpucore/c/banjo.h>
#include <lib/ddk/device.h>
#include <lib/ddk/driver.h>
#include <lib/zx/channel.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <zircon/process.h>
#include <zircon/types.h>

#include <atomic>
#include <set>
#include <thread>

#include <ddktl/fidl.h>

#include "magma_util/dlog.h"
#include "msd_defs.h"
#include "msd_intel_pci_device.h"
#include "platform_trace.h"
#include "platform_trace_provider.h"
#include "platform_trace_provider_with_fdio.h"
#include "src/graphics/drivers/msd-intel-gen/msd_intel_bind.h"
#include "src/graphics/lib/magma/src/magma_util/platform/zircon/magma_performance_counter_device.h"
#include "sys_driver/magma_driver.h"

#if MAGMA_TEST_DRIVER
void magma_indriver_test(magma::PlatformPciDevice* platform_device);
#endif

struct sysdrv_device_t;
static int magma_start(sysdrv_device_t* dev);
#if MAGMA_TEST_DRIVER
static int magma_stop(sysdrv_device_t* dev);
#endif

using FidlStatus = fuchsia_gpu_magma::wire::Status;

struct sysdrv_device_t : public fuchsia_gpu_magma::Device::Interface {
 public:
  void Query2(uint64_t query_id, Query2Completer::Sync& _completer) override {
    DLOG("sysdrv_device_t::Query2");
    DASSERT(this->magma_system_device);

    uint64_t result;
    switch (query_id) {
      case MAGMA_QUERY_IS_TEST_RESTART_SUPPORTED:
#if MAGMA_TEST_DRIVER
        result = 1;
#else
        result = 0;
#endif
        break;
      default:
        magma::Status status = this->magma_system_device->Query(query_id, &result);
        if (!status.ok()) {
          _completer.ReplyError(static_cast<FidlStatus>(status.getFidlStatus()));
          return;
        }
    }
    DLOG("query query_id 0x%" PRIx64 " returning 0x%" PRIx64, query_id, result);

    _completer.ReplySuccess(result);
  }

  void QueryReturnsBuffer(uint64_t query_id,
                          QueryReturnsBufferCompleter::Sync& _completer) override {
    DLOG("sysdrv_device_t::QueryReturnsBuffer");

    zx_handle_t result;
    magma::Status status = this->magma_system_device->QueryReturnsBuffer(query_id, &result);
    if (!status.ok()) {
      _completer.ReplyError(static_cast<FidlStatus>(status.getFidlStatus()));
      return;
    }
    DLOG("query extended query_id 0x%" PRIx64 " returning 0x%x", query_id, result);
    _completer.ReplySuccess(zx::vmo(result));
  }

  void Connect(uint64_t client_id, ConnectCompleter::Sync& _completer) override {
    DLOG("sysdrv_device_t::Connect");

    auto connection =
        MagmaSystemDevice::Open(this->magma_system_device, client_id, /*thread_profile*/ nullptr);

    if (!connection) {
      DLOG("MagmaSystemDevice::Open failed");
      _completer.Close(ZX_ERR_INTERNAL);
      return;
    }

    _completer.Reply(zx::channel(connection->GetClientEndpoint()),
                     zx::channel(connection->GetClientNotificationEndpoint()));

    this->magma_system_device->StartConnectionThread(std::move(connection));
  }

  void DumpState(uint32_t dump_type, DumpStateCompleter::Sync& _completer) override {
    DLOG("sysdrv_device_t::DumpState");
    if (dump_type & ~(MAGMA_DUMP_TYPE_NORMAL | MAGMA_DUMP_TYPE_PERF_COUNTERS |
                      MAGMA_DUMP_TYPE_PERF_COUNTER_ENABLE)) {
      DLOG("Invalid dump type %d", dump_type);
      return;
    }

    std::unique_lock<std::mutex> lock(this->magma_mutex);
    if (this->magma_system_device)
      this->magma_system_device->DumpStatus(dump_type);
  }

  void GetIcdList(GetIcdListCompleter::Sync& completer) override {
    fidl::FidlAllocator allocator;
    std::vector<msd_icd_info_t> msd_icd_infos;
    this->magma_system_device->GetIcdList(&msd_icd_infos);
    std::vector<fuchsia_gpu_magma::wire::IcdInfo> icd_infos;
    for (auto& item : msd_icd_infos) {
      fuchsia_gpu_magma::wire::IcdInfo icd_info(allocator);
      icd_info.set_component_url(allocator, item.component_url);
      fuchsia_gpu_magma::wire::IcdFlags flags;
      if (item.support_flags & ICD_SUPPORT_FLAG_VULKAN)
        flags |= fuchsia_gpu_magma::wire::IcdFlags::SUPPORTS_VULKAN;
      icd_info.set_flags(allocator, flags);
      icd_infos.push_back(std::move(icd_info));
    }

    completer.Reply(fidl::VectorView<fuchsia_gpu_magma::wire::IcdInfo>::FromExternal(icd_infos));
  }

  void TestRestart(TestRestartCompleter::Sync& _completer) override {
#if MAGMA_TEST_DRIVER
    DLOG("sysdrv_device_t::TestRestart");
    std::unique_lock<std::mutex> lock(this->magma_mutex);
    zx_status_t status = magma_stop(this);
    if (status != ZX_OK) {
      DLOG("magma_stop failed: %d", status);
      return;
    }
    status = magma_start(this);
    if (status != ZX_OK) {
      DLOG("magma_start failed: %d", status);
    }
#endif
  }

  void GetUnitTestStatus(GetUnitTestStatusCompleter::Sync& _completer) override {
    DLOG("sysdrv_device_t::GetUnitTestStatus");
    _completer.Reply(ZX_ERR_NOT_SUPPORTED);
  }

  zx_device_t* parent_device;
  zx_device_t* zx_device_gpu;

  zx_intel_gpu_core_protocol_t gpu_core_protocol;

  std::unique_ptr<MagmaDriver> magma_driver;
  std::shared_ptr<MagmaSystemDevice> magma_system_device;
  std::mutex magma_mutex;
  zx_koid_t perf_count_access_token_id = 0;
};

sysdrv_device_t* get_device(void* context) { return static_cast<sysdrv_device_t*>(context); }

static void sysdrv_gpu_init(void* context) {
  auto* gpu = static_cast<sysdrv_device_t*>(context);
  if (!magma::MagmaPerformanceCounterDevice::AddDevice(gpu->zx_device_gpu,
                                                       &gpu->perf_count_access_token_id)) {
    device_init_reply(gpu->zx_device_gpu, ZX_ERR_INTERNAL, nullptr);
    return;
  }

  gpu->magma_system_device->set_perf_count_access_token_id(gpu->perf_count_access_token_id);
  device_init_reply(gpu->zx_device_gpu, ZX_OK, nullptr);
}

static zx_status_t sysdrv_gpu_message(void* context, fidl_incoming_msg_t* message,
                                      fidl_txn_t* transaction) {
  sysdrv_device_t* device = get_device(context);
  DdkTransaction ddk_transaction(transaction);
  fidl::WireDispatch<fuchsia_gpu_magma::Device>(device, message, &ddk_transaction);
  return ddk_transaction.Status();
}

static void sysdrv_gpu_release(void* ctx) {
  // TODO(fxbug.dev/31113) - when testable:
  // Free context if sysdrv_display_release has already been called
  DASSERT(false);
}

static zx_protocol_device_t sysdrv_gpu_device_proto = {
    .version = DEVICE_OPS_VERSION,
    .init = sysdrv_gpu_init,
    .release = sysdrv_gpu_release,
    .message = sysdrv_gpu_message,
};

// implement driver object:

static zx_status_t sysdrv_bind(void* ctx, zx_device_t* zx_device) {
  DLOG("sysdrv_bind start zx_device %p", zx_device);

  // map resources and initialize the device
  auto device = std::make_unique<sysdrv_device_t>();

  zx_status_t status =
      device_get_protocol(zx_device, ZX_PROTOCOL_INTEL_GPU_CORE, &device->gpu_core_protocol);
  if (status != ZX_OK)
    return DRET_MSG(status, "device_get_protocol failed: %d", status);

  device->magma_driver = MagmaDriver::Create();
  if (!device->magma_driver)
    return DRET_MSG(ZX_ERR_INTERNAL, "MagmaDriver::Create failed");

  if (magma::PlatformTraceProvider::Get())
    magma::InitializeTraceProviderWithFdio(magma::PlatformTraceProvider::Get());

#if MAGMA_TEST_DRIVER
  DLOG("running magma indriver test");
  {
    auto platform_device = MsdIntelPciDevice::CreateShim(&device->gpu_core_protocol);
    magma_indriver_test(platform_device.get());
  }
#endif

  device->parent_device = zx_device;

  status = magma_start(device.get());
  if (status != ZX_OK)
    return DRET_MSG(status, "magma_start failed");

  device_add_args_t args = {};
  args.version = DEVICE_ADD_ARGS_VERSION;
  args.name = "msd-intel-gen";
  args.ctx = device.get();
  args.ops = &sysdrv_gpu_device_proto;
  args.proto_id = ZX_PROTOCOL_GPU;
  args.proto_ops = nullptr;

  status = device_add(zx_device, &args, &device->zx_device_gpu);
  if (status != ZX_OK)
    return DRET_MSG(status, "gpu device_add failed: %d", status);

  device.release();

  DLOG("initialized magma system driver");

  return ZX_OK;
}

static constexpr zx_driver_ops_t msd_driver_ops = []() {
  zx_driver_ops_t ops = {};
  ops.version = DRIVER_OPS_VERSION;
  ops.bind = sysdrv_bind;
  return ops;
}();

static int magma_start(sysdrv_device_t* device) {
  DLOG("magma_start");

  device->magma_system_device = device->magma_driver->CreateDevice(&device->gpu_core_protocol);
  if (!device->magma_system_device)
    return DRET_MSG(ZX_ERR_NO_RESOURCES, "Failed to create device");

  DLOG("Created device %p", device->magma_system_device.get());
  device->magma_system_device->set_perf_count_access_token_id(device->perf_count_access_token_id);

  return ZX_OK;
}

#if MAGMA_TEST_DRIVER
static int magma_stop(sysdrv_device_t* device) {
  DLOG("magma_stop");

  device->magma_system_device->Shutdown();
  device->magma_system_device.reset();

  return ZX_OK;
}
#endif

ZIRCON_DRIVER(gpu, msd_driver_ops, "magma", "0.1");
