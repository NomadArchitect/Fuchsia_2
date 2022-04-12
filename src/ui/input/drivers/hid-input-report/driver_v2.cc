// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.driver.framework/cpp/wire.h>
#include <lib/async/cpp/executor.h>
#include <lib/ddk/debug.h>
#include <lib/fpromise/scope.h>
#include <lib/service/llcpp/outgoing_directory.h>
#include <lib/sys/component/llcpp/outgoing_directory.h>
#include <zircon/errors.h>

#include "src/devices/lib/compat/compat.h"
#include "src/devices/lib/compat/symbols.h"
#include "src/devices/lib/driver2/devfs_exporter.h"
#include "src/devices/lib/driver2/inspect.h"
#include "src/devices/lib/driver2/namespace.h"
#include "src/devices/lib/driver2/record_cpp.h"
#include "src/devices/lib/driver2/start_args.h"
#include "src/devices/lib/driver2/structured_logger.h"
#include "src/ui/input/drivers/hid-input-report/input-report.h"

namespace fdf2 = fuchsia_driver_framework;
namespace fio = fuchsia_io;

namespace {

class InputReportDriver {
 public:
  InputReportDriver(async_dispatcher_t* dispatcher, fidl::WireSharedClient<fdf2::Node> node,
                    driver::Namespace ns, component::OutgoingDirectory outgoing,
                    driver::Logger logger)
      : dispatcher_(dispatcher),
        outgoing_(std::move(outgoing)),
        node_(std::move(node)),
        ns_(std::move(ns)),
        logger_(std::move(logger)),
        executor_(dispatcher) {}

  static constexpr const char* Name() { return "InputReport"; }

  static zx::status<std::unique_ptr<InputReportDriver>> Start(
      fdf2::wire::DriverStartArgs& start_args, async_dispatcher_t* dispatcher,
      fidl::WireSharedClient<fdf2::Node> node, driver::Namespace ns, driver::Logger logger) {
    auto outgoing = component::OutgoingDirectory::Create(dispatcher);
    auto driver = std::make_unique<InputReportDriver>(dispatcher, std::move(node), std::move(ns),
                                                      std::move(outgoing), std::move(logger));
    fidl::VectorView<fdf2::wire::NodeSymbol> symbols;
    if (start_args.has_symbols()) {
      symbols = start_args.symbols();
    }

    auto parent_symbol = driver::GetSymbol<compat::device_t*>(symbols, compat::kDeviceSymbol);

    hid_device_protocol_t proto = {};
    if (parent_symbol->proto_ops.id != ZX_PROTOCOL_HID_DEVICE) {
      FDF_LOGL(ERROR, driver->logger_, "Didn't find HID_DEVICE protocol");
      return zx::error(ZX_ERR_NOT_FOUND);
    }
    proto.ctx = parent_symbol->context;
    proto.ops = reinterpret_cast<hid_device_protocol_ops_t*>(parent_symbol->proto_ops.ops);

    ddk::HidDeviceProtocolClient hiddev(&proto);
    if (!hiddev.is_valid()) {
      FDF_LOGL(ERROR, driver->logger_, "Failed to create hiddev");
      return zx::error(ZX_ERR_INTERNAL);
    }
    driver->input_report_.emplace(std::move(hiddev));

    auto result = driver->Run(std::move(start_args.outgoing_dir()));
    if (result.is_error()) {
      return result.take_error();
    }
    return zx::ok(std::move(driver));
  }

 private:
  zx::status<> ConnectToDevfsExporter() {
    // Connect to DevfsExporter.
    auto endpoints = fidl::CreateEndpoints<fuchsia_io::Directory>();
    if (endpoints.is_error()) {
      return endpoints.take_error();
    }
    // Serve a connection to outgoing.
    auto status = outgoing_.Serve(std::move(endpoints->server));
    if (status.is_error()) {
      return status.take_error();
    }

    auto exporter = driver::DevfsExporter::Create(
        ns_, dispatcher_, fidl::WireSharedClient(std::move(endpoints->client), dispatcher_));
    if (exporter.is_error()) {
      return zx::error(exporter.error_value());
    }
    exporter_ = std::move(*exporter);
    return zx::ok();
  }

  zx::status<> Run(fidl::ServerEnd<fio::Directory> outgoing_dir) {
    input_report_->Start();

    // Connect to DevfsExporter.
    auto status = ConnectToDevfsExporter();
    if (status.is_error()) {
      return status.take_error();
    }

    // Connect to our parent.
    auto parent_client = compat::ConnectToParentDevice(dispatcher_, &ns_);
    if (parent_client.is_error()) {
      FDF_LOG(WARNING, "Connecting to compat service failed with %s",
              zx_status_get_string(parent_client.error_value()));
      return parent_client.take_error();
    }
    parent_client_ = std::move(parent_client.value());

    auto compat_connect =
        fpromise::make_result_promise<void, zx_status_t>(fpromise::ok())
            .and_then([this]() {
              fpromise::bridge<void, zx_status_t> topo_bridge;
              parent_client_->GetTopologicalPath().Then(
                  [this, completer = std::move(topo_bridge.completer)](
                      fidl::WireUnownedResult<fuchsia_driver_compat::Device::GetTopologicalPath>&
                          result) mutable {
                    if (!result.ok()) {
                      completer.complete_error(result.status());
                      return;
                    }
                    auto* response = result.Unwrap();
                    parent_topo_path_ = std::string(response->path.data(), response->path.size());
                    completer.complete_ok();
                  });
              return topo_bridge.consumer.promise_or(fpromise::error(ZX_ERR_CANCELED));
            })
            // Create our child device and FIDL server.
            .and_then([this]() -> fpromise::promise<void, zx_status_t> {
              child_ = compat::Child("InputReport", ZX_PROTOCOL_INPUTREPORT,
                                     parent_topo_path_ + "/InputReport", {});
              auto status = outgoing_.AddNamedProtocol(
                  [this](zx::channel channel) {
                    fidl::BindServer<fidl::WireServer<fuchsia_input_report::InputDevice>>(
                        dispatcher_,
                        fidl::ServerEnd<fuchsia_input_report::InputDevice>(std::move(channel)),
                        &input_report_.value());
                  },
                  "InputReport");
              if (status.is_error()) {
                return fpromise::make_result_promise<void, zx_status_t>(
                    fpromise::error(status.error_value()));
              }
              child_->AddCallback(std::make_shared<fit::deferred_callback>([this]() {
                auto status = outgoing_.RemoveNamedProtocol("InputReport");
                if (status.is_error()) {
                  FDF_LOG(WARNING, "Removing protocol failed with: %s", status.status_string());
                }
              }));
              return exporter_.Export(std::string("svc/").append(child_->name()),
                                      child_->topological_path(), ZX_PROTOCOL_INPUTREPORT);
            })
            // Error handling.
            .or_else([this](zx_status_t& result) {
              FDF_LOG(WARNING, "Device setup failed with: %s", zx_status_get_string(result));
            });
    executor_.schedule_task(std::move(compat_connect));

    // TODO(fxbug.dev/96231): Move compat library to the correct OutgoingDir, and then add inspect
    // data here.

    return outgoing_.Serve(std::move(outgoing_dir));
  }

  async_dispatcher_t* dispatcher_;
  std::optional<hid_input_report_dev::InputReport> input_report_;
  component::OutgoingDirectory outgoing_;
  fidl::WireSharedClient<fdf2::Node> node_;
  driver::Namespace ns_;
  driver::Logger logger_;
  inspect::Inspector inspector_;
  zx::vmo inspect_vmo_;
  async::Executor executor_;

  std::optional<compat::Child> child_;
  std::string parent_topo_path_;
  fidl::WireSharedClient<fuchsia_driver_compat::Device> parent_client_;
  driver::DevfsExporter exporter_;

  // NOTE: Must be the last member.
  fpromise::scope scope_;
};

}  // namespace

// TODO(fxbug.dev/94884): Figure out how to get logging working.
zx_driver_rec_t __zircon_driver_rec__ = {};

void driver_logf_internal(const zx_driver_t* drv, fx_log_severity_t severity, const char* tag,
                          const char* file, int line, const char* msg, ...) {}

bool driver_log_severity_enabled_internal(const zx_driver_t* drv, fx_log_severity_t severity) {
  return true;
}

FUCHSIA_DRIVER_RECORD_CPP_V1(InputReportDriver);
