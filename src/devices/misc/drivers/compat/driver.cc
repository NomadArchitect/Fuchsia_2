// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/misc/drivers/compat/driver.h"

#include <fidl/fuchsia.scheduler/cpp/wire.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/ddk/binding_priv.h>
#include <lib/driver2/promise.h>
#include <lib/driver2/record_cpp.h>
#include <lib/driver2/start_args.h>
#include <lib/fidl/cpp/wire/connect_service.h>
#include <lib/fpromise/bridge.h>
#include <lib/fpromise/promise.h>
#include <lib/fpromise/single_threaded_executor.h>
#include <lib/service/llcpp/service.h>
#include <zircon/dlfcn.h>

#include "src/devices/lib/compat/connect.h"
#include "src/devices/misc/drivers/compat/devfs_vnode.h"
#include "src/devices/misc/drivers/compat/loader.h"

namespace fboot = fuchsia_boot;
namespace fdf {
using namespace fuchsia_driver_framework;
}
namespace fio = fuchsia_io;
namespace fldsvc = fuchsia_ldsvc;

using fpromise::bridge;
using fpromise::error;
using fpromise::join_promises;
using fpromise::ok;
using fpromise::promise;
using fpromise::result;

// This lock protects any globals, as globals could be accessed by other
// drivers and other threads within the process.
// Currently this protects the root resource and the loader service.
std::mutex kDriverGlobalsLock;
zx::resource kRootResource;

namespace {

constexpr auto kOpenFlags = fio::wire::OpenFlags::kRightReadable |
                            fio::wire::OpenFlags::kRightExecutable |
                            fio::wire::OpenFlags::kNotDirectory;
constexpr auto kVmoFlags = fio::wire::VmoFlags::kRead | fio::wire::VmoFlags::kExecute;
constexpr auto kLibDriverPath = "/pkg/driver/compat.so";

}  // namespace

namespace compat {

zx_status_t AddMetadata(Device* device,
                        fidl::VectorView<fuchsia_driver_compat::wire::Metadata> data) {
  for (auto& metadata : data) {
    size_t size;
    zx_status_t status = metadata.data.get_property(ZX_PROP_VMO_CONTENT_SIZE, &size, sizeof(size));
    if (status != ZX_OK) {
      return status;
    }
    std::vector<uint8_t> data(size);
    status = metadata.data.read(data.data(), 0, data.size());
    if (status != ZX_OK) {
      return status;
    }

    status = device->AddMetadata(metadata.type, data.data(), data.size());
    if (status != ZX_OK) {
      return status;
    }
  }
  return ZX_OK;
}

promise<void, zx_status_t> GetAndAddMetadata(
    fidl::WireSharedClient<fuchsia_driver_compat::Device>& client, Device* device) {
  bridge<void, zx_status_t> bridge;
  client->GetMetadata().Then(
      [device, completer = std::move(bridge.completer)](
          fidl::WireUnownedResult<fuchsia_driver_compat::Device::GetMetadata>& result) mutable {
        if (!result.ok()) {
          return;
        }
        auto* response = result.Unwrap();
        if (response->is_error()) {
          completer.complete_error(response->error_value());
          return;
        }
        zx_status_t status = AddMetadata(device, response->value()->metadata);
        if (status != ZX_OK) {
          completer.complete_error(status);
          return;
        }
        completer.complete_ok();
      });
  return bridge.consumer.promise_or(error(ZX_ERR_INTERNAL));
}

DriverList global_driver_list;

zx_driver_t* DriverList::ZxDriver() { return static_cast<zx_driver_t*>(this); }

void DriverList::AddDriver(Driver* driver) { drivers_.insert(driver); }

void DriverList::RemoveDriver(Driver* driver) { drivers_.erase(driver); }

void DriverList::Log(FuchsiaLogSeverity severity, const char* tag, const char* file, int line,
                     const char* msg, va_list args) {
  if (drivers_.empty()) {
    return;
  }
  (*drivers_.begin())->Log(severity, tag, file, line, msg, args);
}

Driver::Driver(async_dispatcher_t* dispatcher, fidl::WireSharedClient<fdf::Node> node,
               driver::Namespace ns, driver::Logger logger, std::string_view url, device_t device,
               const zx_protocol_device_t* ops, component::OutgoingDirectory outgoing)
    : dispatcher_(dispatcher),
      executor_(dispatcher),
      outgoing_(std::move(outgoing)),
      ns_(std::move(ns)),
      logger_(std::move(logger)),
      url_(url),
      inner_logger_(),
      device_(device, ops, this, std::nullopt, inner_logger_, dispatcher),
      sysmem_(this) {
  device_.Bind(std::move(node));
  global_driver_list.AddDriver(this);
}

Driver::~Driver() {
  if (record_ != nullptr && record_->ops->release != nullptr) {
    record_->ops->release(context_);
  }
  dlclose(library_);
  global_driver_list.RemoveDriver(this);
}

zx::status<std::unique_ptr<Driver>> Driver::Start(fdf::wire::DriverStartArgs& start_args,
                                                  fdf::UnownedDispatcher dispatcher,
                                                  fidl::WireSharedClient<fdf::Node> node,
                                                  driver::Namespace ns, driver::Logger logger) {
  auto compat_device =
      driver::GetSymbol<const device_t*>(start_args, kDeviceSymbol, &kDefaultDevice);
  const zx_protocol_device_t* ops =
      driver::GetSymbol<const zx_protocol_device_t*>(start_args, kOps);

  // Open the compat driver's binary within the package.
  auto compat = driver::ProgramValue(start_args.program(), "compat");
  if (compat.is_error()) {
    FDF_LOGL(ERROR, logger, "Field \"compat\" missing from component manifest");
    return compat.take_error();
  }

  auto outgoing = component::OutgoingDirectory::Create(dispatcher->async_dispatcher());

  auto driver = std::make_unique<Driver>(dispatcher->async_dispatcher(), std::move(node),
                                         std::move(ns), std::move(logger), start_args.url().get(),
                                         *compat_device, ops, std::move(outgoing));

  auto result = driver->Run(std::move(start_args.outgoing_dir()), "/pkg/" + *compat);
  if (result.is_error()) {
    FDF_LOGL(ERROR, logger, "Failed to run driver: %s", result.status_string());
    return result.take_error();
  }
  return zx::ok(std::move(driver));
}

zx::status<> Driver::Run(fidl::ServerEnd<fio::Directory> outgoing_dir,
                         std::string_view driver_path) {
  auto serve = outgoing_.Serve(std::move(outgoing_dir));
  if (serve.is_error()) {
    return serve.take_error();
  }

  devfs_vfs_ = std::make_unique<fs::SynchronousVfs>(dispatcher_);
  devfs_dir_ = fbl::MakeRefCounted<fs::PseudoDir>();

  auto endpoints = fidl::CreateEndpoints<fuchsia_io::Directory>();
  if (endpoints.is_error()) {
    return endpoints.take_error();
  }
  // Start the devfs exporter.
  auto serve_status = devfs_vfs_->Serve(devfs_dir_, endpoints->server.TakeChannel(),
                                        fs::VnodeConnectionOptions::ReadWrite());
  if (serve_status != ZX_OK) {
    return zx::error(serve_status);
  }

  auto exporter = driver::DevfsExporter::Create(
      ns_, dispatcher_, fidl::WireSharedClient(std::move(endpoints->client), dispatcher_));
  if (exporter.is_error()) {
    return zx::error(exporter.error_value());
  }
  devfs_exporter_ = std::move(*exporter);

  auto compat_connect =
      Driver::ConnectToParentDevices()
          .and_then(fit::bind_member<&Driver::GetDeviceInfo>(this))
          .then([this](result<void, zx_status_t>& result) -> fpromise::result<void, zx_status_t> {
            if (result.is_error()) {
              FDF_LOG(WARNING, "Getting DeviceInfo failed with: %s",
                      zx_status_get_string(result.error()));
            }
            return ok();
          });

  auto root_resource =
      fpromise::make_result_promise<zx::resource, zx_status_t>(error(ZX_ERR_ALREADY_BOUND)).box();
  {
    std::scoped_lock lock(kDriverGlobalsLock);
    if (!kRootResource.is_valid()) {
      // If the root resource is invalid, try fetching it. Once we've fetched it we might find that
      // we lost the race with another process -- we'll handle that later.
      auto connect_promise =
          driver::Connect<fboot::RootResource>(ns_, dispatcher_)
              .and_then(fit::bind_member<&Driver::GetRootResource>(this))
              .or_else([this](zx_status_t& status) {
                FDF_LOG(WARNING, "Failed to get root resource: %s", zx_status_get_string(status));
                FDF_LOG(WARNING, "Assuming test environment and continuing");
                return error(status);
              })
              .box();
      root_resource.swap(connect_promise);
    }
  }

  auto loader_vmo = driver::Open(ns_, dispatcher_, kLibDriverPath, kOpenFlags)
                        .and_then(fit::bind_member<&Driver::GetBuffer>(this));
  auto driver_vmo = driver::Open(ns_, dispatcher_, std::string(driver_path).c_str(), kOpenFlags)
                        .and_then(fit::bind_member<&Driver::GetBuffer>(this));
  auto start_driver =
      join_promises(std::move(root_resource), std::move(loader_vmo), std::move(driver_vmo))
          .then(fit::bind_member<&Driver::Join>(this))
          .and_then(fit::bind_member<&Driver::LoadDriver>(this))
          .and_then(std::move(compat_connect))
          .and_then(fit::bind_member<&Driver::StartDriver>(this))
          .or_else(fit::bind_member<&Driver::StopDriver>(this))
          .wrap_with(scope_);
  executor_.schedule_task(std::move(start_driver));

  return zx::ok();
}

promise<zx::resource, zx_status_t> Driver::GetRootResource(
    const fidl::WireSharedClient<fboot::RootResource>& root_resource) {
  bridge<zx::resource, zx_status_t> bridge;
  auto callback = [completer = std::move(bridge.completer)](
                      fidl::WireUnownedResult<fboot::RootResource::Get>& result) mutable {
    if (!result.ok()) {
      completer.complete_error(result.status());
      return;
    }
    completer.complete_ok(std::move(result.value().resource));
  };
  root_resource->Get().ThenExactlyOnce(std::move(callback));
  return bridge.consumer.promise();
}

promise<Driver::FileVmo, zx_status_t> Driver::GetBuffer(
    const fidl::WireSharedClient<fio::File>& file) {
  bridge<FileVmo, zx_status_t> bridge;
  auto callback = [completer = std::move(bridge.completer)](
                      fidl::WireUnownedResult<fio::File::GetBackingMemory>& result) mutable {
    if (!result.ok()) {
      completer.complete_error(result.status());
      return;
    }
    const auto* res = result.Unwrap();
    if (res->is_error()) {
      completer.complete_error(res->error_value());
      return;
    }
    zx::vmo& vmo = res->value()->vmo;
    uint64_t size;
    if (zx_status_t status = vmo.get_prop_content_size(&size); status != ZX_OK) {
      completer.complete_error(status);
      return;
    }
    completer.complete_ok(FileVmo{
        .vmo = std::move(vmo),
        .size = size,
    });
    return;
  };
  file->GetBackingMemory(kVmoFlags).ThenExactlyOnce(std::move(callback));
  return bridge.consumer.promise().or_else([this](zx_status_t& status) {
    FDF_LOG(WARNING, "Failed to get buffer: %s", zx_status_get_string(status));
    return error(status);
  });
}

result<std::tuple<zx::vmo, zx::vmo>, zx_status_t> Driver::Join(
    result<std::tuple<result<zx::resource, zx_status_t>, result<FileVmo, zx_status_t>,
                      result<FileVmo, zx_status_t>>>& results) {
  if (results.is_error()) {
    return error(ZX_ERR_INTERNAL);
  }
  auto& [root_resource, loader_vmo, driver_vmo] = results.value();
  if (root_resource.is_ok()) {
    std::scoped_lock lock(kDriverGlobalsLock);
    if (!kRootResource.is_valid()) {
      kRootResource = root_resource.take_value();
    }
  }
  if (loader_vmo.is_error()) {
    return loader_vmo.take_error_result();
  }
  if (driver_vmo.is_error()) {
    return driver_vmo.take_error_result();
  }
  return fpromise::ok(
      std::make_tuple(std::move((loader_vmo.value().vmo)), std::move((driver_vmo.value().vmo))));
}

result<void, zx_status_t> Driver::LoadDriver(std::tuple<zx::vmo, zx::vmo>& vmos) {
  auto& [loader_vmo, driver_vmo] = vmos;

  // Replace loader service to load the DFv1 driver, load the driver,
  // then place the original loader service back.
  {
    // This requires a lock because the loader is a global variable.
    std::scoped_lock lock(kDriverGlobalsLock);
    auto endpoints = fidl::CreateEndpoints<fldsvc::Loader>();
    if (endpoints.is_error()) {
      return error(endpoints.status_value());
    }
    zx::channel loader_channel(dl_set_loader_service(endpoints->client.channel().release()));
    fidl::ClientEnd<fldsvc::Loader> loader_client(std::move(loader_channel));
    auto clone = fidl::CreateEndpoints<fldsvc::Loader>();
    if (clone.is_error()) {
      return error(clone.status_value());
    }
    auto result = fidl::WireCall(loader_client)->Clone(std::move(clone->server));
    if (!result.ok()) {
      FDF_LOG(ERROR, "Failed to load driver '%s', cloning loader failed with FIDL status: %s",
              url_.data(), result.status_string());
      return error(result.status());
    }
    if (result.value().rv != ZX_OK) {
      FDF_LOG(ERROR, "Failed to load driver '%s', cloning loader failed with status: %s",
              url_.data(), zx_status_get_string(result.value().rv));
      return error(result.value().rv);
    }

    // Start loader.
    async::Loop loader_loop(&kAsyncLoopConfigNeverAttachToThread);
    zx_status_t status = loader_loop.StartThread("loader-loop");
    if (status != ZX_OK) {
      FDF_LOG(ERROR, "Failed to load driver '%s', could not start thread for loader loop: %s",
              url_.data(), zx_status_get_string(status));
      return error(status);
    }
    Loader loader(loader_loop.dispatcher());
    auto bind = loader.Bind(fidl::ClientEnd<fldsvc::Loader>(std::move(loader_client)),
                            std::move(loader_vmo));
    if (bind.is_error()) {
      return error(bind.status_value());
    }
    fidl::BindServer(loader_loop.dispatcher(), std::move(endpoints->server), &loader);

    // Open driver.
    library_ = dlopen_vmo(driver_vmo.get(), RTLD_NOW);
    if (library_ == nullptr) {
      FDF_LOG(ERROR, "Failed to load driver '%s', could not load library: %s", url_.data(),
              dlerror());
      return error(ZX_ERR_INTERNAL);
    }

    // Return original loader service.
    loader_channel.reset(dl_set_loader_service(clone->client.channel().release()));
  }

  // Load and verify symbols.
  auto note = static_cast<const zircon_driver_note_t*>(dlsym(library_, "__zircon_driver_note__"));
  if (note == nullptr) {
    FDF_LOG(ERROR, "Failed to load driver '%s', driver note not found", url_.data());
    return error(ZX_ERR_BAD_STATE);
  }
  FDF_LOG(INFO, "Loaded driver '%s'", note->payload.name);
  record_ = static_cast<zx_driver_rec_t*>(dlsym(library_, "__zircon_driver_rec__"));
  if (record_ == nullptr) {
    FDF_LOG(ERROR, "Failed to load driver '%s', driver record not found", url_.data());
    return error(ZX_ERR_BAD_STATE);
  }
  if (record_->ops == nullptr) {
    FDF_LOG(ERROR, "Failed to load driver '%s', missing driver ops", url_.data());
    return error(ZX_ERR_BAD_STATE);
  }
  if (record_->ops->version != DRIVER_OPS_VERSION) {
    FDF_LOG(ERROR, "Failed to load driver '%s', incorrect driver version", url_.data());
    return error(ZX_ERR_WRONG_TYPE);
  }
  if (record_->ops->bind == nullptr && record_->ops->create == nullptr) {
    FDF_LOG(ERROR, "Failed to load driver '%s', missing '%s'", url_.data(),
            (record_->ops->bind == nullptr ? "bind" : "create"));
    return error(ZX_ERR_BAD_STATE);
  } else if (record_->ops->bind != nullptr && record_->ops->create != nullptr) {
    FDF_LOG(ERROR, "Failed to load driver '%s', both 'bind' and 'create' are defined", url_.data());
    return error(ZX_ERR_INVALID_ARGS);
  }
  record_->driver = global_driver_list.ZxDriver();

  // Create logger.
  auto inner_logger = driver::Logger::Create(ns_, dispatcher_, note->payload.name);
  if (inner_logger.is_error()) {
    return error(inner_logger.status_value());
  }
  inner_logger_ = std::move(*inner_logger);

  return ok();
}

result<void, zx_status_t> Driver::StartDriver() {
  if (record_->ops->init != nullptr) {
    // If provided, run init.
    zx_status_t status = record_->ops->init(&context_);
    if (status != ZX_OK) {
      FDF_LOG(ERROR, "Failed to load driver '%s', 'init' failed: %s", url_.data(),
              zx_status_get_string(status));
      return error(status);
    }
  }
  if (record_->ops->bind != nullptr) {
    // If provided, run bind and return.
    zx_status_t status = record_->ops->bind(context_, device_.ZxDevice());
    if (status != ZX_OK) {
      FDF_LOG(ERROR, "Failed to load driver '%s', 'bind' failed: %s", url_.data(),
              zx_status_get_string(status));
      return error(status);
    }
  } else {
    // Else, run create and return.
    auto client_end = ns_.Connect<fboot::Items>();
    if (client_end.is_error()) {
      return error(client_end.status_value());
    }
    zx_status_t status = record_->ops->create(context_, device_.ZxDevice(), "proxy", "",
                                              client_end->channel().release());
    if (status != ZX_OK) {
      FDF_LOG(ERROR, "Failed to load driver '%s', 'create' failed: %s", url_.data(),
              zx_status_get_string(status));
      return error(status);
    }
  }
  if (!device_.HasChildren()) {
    FDF_LOG(ERROR, "Driver '%s' did not add a child device", url_.data());
    return error(ZX_ERR_BAD_STATE);
  }
  return ok();
}

result<> Driver::StopDriver(const zx_status_t& status) {
  FDF_LOG(ERROR, "Failed to start driver '%s': %s", url_.data(), zx_status_get_string(status));
  device_.Unbind();
  return ok();
}

fpromise::promise<void, zx_status_t> Driver::ConnectToParentDevices() {
  bridge<void, zx_status_t> bridge;
  compat::ConnectToParentDevices(
      dispatcher_, &ns_,
      [this, completer = std::move(bridge.completer)](
          zx::status<std::vector<compat::ParentDevice>> devices) mutable {
        if (devices.is_error()) {
          completer.complete_error(devices.error_value());
          return;
        }
        std::vector<std::string> parents_names;
        for (auto& device : devices.value()) {
          if (device.name == "default") {
            parent_client_ = fidl::WireSharedClient<fuchsia_driver_compat::Device>(
                std::move(device.client), dispatcher_);
            continue;
          }

          // TODO(fxbug.dev/100985): When services stop adding extra instances
          // separated by ',' then remove this check.
          if (device.name.find(',') != std::string::npos) {
            continue;
          }

          parents_names.push_back(device.name);
          parent_clients_[device.name] = fidl::WireSharedClient<fuchsia_driver_compat::Device>(
              std::move(device.client), dispatcher_);
        }
        device_.set_fragments(std::move(parents_names));
        completer.complete_ok();
      });
  return bridge.consumer.promise_or(error(ZX_ERR_INTERNAL));
}

promise<void, zx_status_t> Driver::GetDeviceInfo() {
  if (!parent_client_) {
    return fpromise::make_result_promise<void, zx_status_t>(error(ZX_ERR_PEER_CLOSED));
  }

  std::vector<promise<void, zx_status_t>> promises;

  bridge<void, zx_status_t> topo_bridge;
  parent_client_->GetTopologicalPath().Then(
      [this, completer = std::move(topo_bridge.completer)](
          fidl::WireUnownedResult<fuchsia_driver_compat::Device::GetTopologicalPath>&
              result) mutable {
        if (!result.ok()) {
          FDF_LOG(ERROR, "Failed to get topo path %s", zx_status_get_string(result.status()));
          return;
        }
        auto* response = result.Unwrap();
        device_.set_topological_path(std::string(response->path.data(), response->path.size()));
        completer.complete_ok();
      });

  promises.push_back(topo_bridge.consumer.promise_or(error(ZX_ERR_INTERNAL)));

  // Get the metadata for each one of our fragments.
  for (auto& client : parent_clients_) {
    promises.push_back(GetAndAddMetadata(client.second, &device_));
  }

  // We only want to get the default metadata if we don't have composites.
  // Otherwise this would be duplicate entries.
  if (parent_clients_.empty()) {
    promises.push_back(GetAndAddMetadata(parent_client_, &device_));
  }

  // Collect all our promises and return the first error we see.
  return join_promise_vector(std::move(promises))
      .then([](fpromise::result<std::vector<fpromise::result<void, zx_status_t>>>& results) {
        if (results.is_error()) {
          return fpromise::make_result_promise(error(ZX_ERR_INTERNAL));
        }
        for (auto& result : results.value()) {
          if (result.is_error()) {
            return fpromise::make_result_promise(error(result.error()));
          }
        }
        return fpromise::make_result_promise<void, zx_status_t>(ok());
      });
}

void* Driver::Context() const { return context_; }

void Driver::Log(FuchsiaLogSeverity severity, const char* tag, const char* file, int line,
                 const char* msg, va_list args) {
  inner_logger_.logvf(severity, tag, file, line, msg, args);
}

zx::status<zx::vmo> Driver::LoadFirmware(Device* device, const char* filename, size_t* size) {
  std::string full_filename = "/pkg/lib/firmware/";
  full_filename.append(filename);
  fpromise::result connect_result = fpromise::run_single_threaded(
      driver::Open(ns_, dispatcher_, full_filename.c_str(), kOpenFlags));
  if (connect_result.is_error()) {
    return zx::error(connect_result.take_error());
  }

  fidl::WireResult get_backing_memory_result =
      connect_result.take_value().sync()->GetBackingMemory(fio::wire::VmoFlags::kRead);
  if (!get_backing_memory_result.ok()) {
    if (get_backing_memory_result.is_peer_closed()) {
      return zx::error(ZX_ERR_NOT_FOUND);
    }
    return zx::error(get_backing_memory_result.status());
  }
  const auto* res = get_backing_memory_result.Unwrap();
  if (res->is_error()) {
    return zx::error(res->error_value());
  }
  zx::vmo& vmo = res->value()->vmo;
  if (zx_status_t status = vmo.get_prop_content_size(size); status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(std::move(vmo));
}

void Driver::LoadFirmwareAsync(Device* device, const char* filename,
                               load_firmware_callback_t callback, void* ctx) {
  std::string firmware_path = "/pkg/lib/firmware/";
  firmware_path.append(filename);
  executor_.schedule_task(driver::Open(ns_, dispatcher_, firmware_path.c_str(), kOpenFlags)
                              .and_then(fit::bind_member<&Driver::GetBuffer>(this))
                              .and_then([callback, ctx](FileVmo& result) {
                                callback(ctx, ZX_OK, result.vmo.release(), result.size);
                              })
                              .or_else([callback, ctx](zx_status_t& status) {
                                callback(ctx, ZX_ERR_NOT_FOUND, ZX_HANDLE_INVALID, 0);
                              })
                              .wrap_with(scope_));
}

zx_status_t Driver::AddDevice(Device* parent, device_add_args_t* args, zx_device_t** out) {
  zx_device_t* child;
  zx_status_t status = parent->Add(args, &child);
  if (status != ZX_OK) {
    FDF_LOG(ERROR, "Failed to add device %s: %s", args->name, zx_status_get_string(status));
    return status;
  }
  if (out) {
    *out = child;
  }

  executor_.schedule_task(child->Export());
  return ZX_OK;
}

zx::status<zx::profile> Driver::GetSchedulerProfile(uint32_t priority, const char* name) {
  auto profile_client = ns_.Connect<fuchsia_scheduler::ProfileProvider>();
  if (!profile_client.is_ok()) {
    return profile_client.take_error();
  }

  if (!profile_client->is_valid()) {
    return zx::error(ZX_ERR_NOT_CONNECTED);
  }
  fidl::WireResult result =
      fidl::WireCall(*profile_client)->GetProfile(priority, fidl::StringView::FromExternal(name));
  if (!result.ok()) {
    return zx::error(result.status());
  }
  fidl::WireResponse response = std::move(result.value());
  if (response.status != ZX_OK) {
    return zx::error(response.status);
  }
  return zx::ok(std::move(response.profile));
}

zx::status<zx::profile> Driver::GetDeadlineProfile(uint64_t capacity, uint64_t deadline,
                                                   uint64_t period, const char* name) {
  auto profile_client = ns_.Connect<fuchsia_scheduler::ProfileProvider>();
  if (!profile_client.is_ok()) {
    return profile_client.take_error();
  }

  if (!profile_client->is_valid()) {
    return zx::error(ZX_ERR_NOT_CONNECTED);
  }
  fidl::WireResult result =
      fidl::WireCall(*profile_client)
          ->GetDeadlineProfile(capacity, deadline, period, fidl::StringView::FromExternal(name));
  if (!result.ok()) {
    return zx::error(result.status());
  }
  fidl::WireResponse response = std::move(result.value());
  if (response.status != ZX_OK) {
    return zx::error(response.status);
  }
  return zx::ok(std::move(response.profile));
}

zx::status<fit::deferred_callback> Driver::ExportToDevfsSync(
    fuchsia_device_fs::wire::ExportOptions options, fbl::RefPtr<fs::Vnode> dev_node,
    std::string_view name, std::string_view topological_path, uint32_t proto_id) {
  zx_status_t add_status = devfs_dir_->AddEntry(name, dev_node);
  if (add_status != ZX_OK) {
    return zx::error(add_status);
  }
  // If this goes out of scope, close the devfs connection.
  auto auto_remove = [this, name = std::string(name), dev_node]() {
    (void)outgoing_.RemoveProtocol(name);
    devfs_vfs_->CloseAllConnectionsForVnode(*dev_node, {});
    devfs_dir_->RemoveEntry(name);
  };

  zx_status_t status = devfs_exporter_.ExportSync(name, topological_path, options, proto_id);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(std::move(auto_remove));
}

}  // namespace compat

FUCHSIA_DRIVER_RECORD_CPP_V1(compat::Driver);
