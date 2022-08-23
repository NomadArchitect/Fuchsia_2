// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/misc/drivers/compat/device.h"

#include <fidl/fuchsia.device.composite/cpp/wire.h>
#include <fidl/fuchsia.driver.framework/cpp/wire_types.h>
#include <lib/async/cpp/task.h>
#include <lib/ddk/binding_priv.h>
#include <lib/fit/defer.h>
#include <lib/fpromise/bridge.h>
#include <lib/stdcompat/span.h>
#include <zircon/compiler.h>
#include <zircon/errors.h>

#include "driver.h"
#include "src/devices/lib/compat/symbols.h"
#include "src/devices/misc/drivers/compat/composite.h"

namespace fdf {
using namespace fuchsia_driver_framework;
}
namespace fcd = fuchsia_component_decl;

namespace {

std::optional<fdf::wire::NodeProperty> fidl_offer_to_device_prop(fidl::AnyArena& arena,
                                                                 const char* fidl_offer) {
  static const std::unordered_map<std::string_view, uint32_t> kPropMap = {
#define DDK_FIDL_PROTOCOL_DEF(tag, val, name) \
  {                                           \
      name,                                   \
      val,                                    \
  },
#include <lib/ddk/fidl-protodefs.h>
  };

  auto prop = kPropMap.find(fidl_offer);
  if (prop == kPropMap.end()) {
    return std::nullopt;
  }

  auto& [key, value] = *prop;
  return fdf::wire::NodeProperty(arena)
      .set_key(arena, fdf::wire::NodePropertyKey::WithIntValue(BIND_FIDL_PROTOCOL))
      .set_value(arena, fdf::wire::NodePropertyValue::WithIntValue(value));
}

// Makes a valid name. This must be a valid component framework instance name.
std::string MakeValidName(std::string_view name) {
  std::string out;
  out.reserve(name.size());
  for (auto ch : name) {
    switch (ch) {
      case ':':
      case '.':
        out.push_back('_');
        break;
      default:
        out.push_back(ch);
    }
  }
  return out;
}

template <typename T>
bool HasOp(const zx_protocol_device_t* ops, T member) {
  return ops != nullptr && ops->*member != nullptr;
}

std::vector<std::string> MakeFidlServiceOffers(device_add_args_t* zx_args) {
  std::vector<std::string> offers;
  for (const auto& offer :
       cpp20::span(zx_args->fidl_service_offers, zx_args->fidl_service_offer_count)) {
    offers.push_back(std::string(offer));
  }
  return offers;
}

}  // namespace

namespace compat {

std::vector<fuchsia_driver_framework::wire::NodeProperty> CreateProperties(
    fidl::AnyArena& arena, driver::Logger& logger, device_add_args_t* zx_args) {
  std::vector<fuchsia_driver_framework::wire::NodeProperty> properties;
  properties.reserve(zx_args->prop_count + zx_args->str_prop_count +
                     zx_args->fidl_protocol_offer_count + 1);
  bool has_protocol = false;
  for (auto [id, _, value] : cpp20::span(zx_args->props, zx_args->prop_count)) {
    properties.emplace_back(arena)
        .set_key(arena, fdf::wire::NodePropertyKey::WithIntValue(id))
        .set_value(arena, fdf::wire::NodePropertyValue::WithIntValue(value));
    if (id == BIND_PROTOCOL) {
      has_protocol = true;
    }
  }

  for (auto [key, value] : cpp20::span(zx_args->str_props, zx_args->str_prop_count)) {
    auto& ref = properties.emplace_back(arena).set_key(
        arena,
        fdf::wire::NodePropertyKey::WithStringValue(arena, fidl::StringView::FromExternal(key)));
    switch (value.data_type) {
      case ZX_DEVICE_PROPERTY_VALUE_BOOL:
        ref.set_value(arena, fdf::wire::NodePropertyValue::WithBoolValue(value.data.bool_val));
        break;
      case ZX_DEVICE_PROPERTY_VALUE_STRING:
        ref.set_value(arena, fdf::wire::NodePropertyValue::WithStringValue(
                                 arena, fidl::StringView::FromExternal(value.data.str_val)));
        break;
      case ZX_DEVICE_PROPERTY_VALUE_INT:
        ref.set_value(arena, fdf::wire::NodePropertyValue::WithIntValue(value.data.int_val));
        break;
      case ZX_DEVICE_PROPERTY_VALUE_ENUM:
        ref.set_value(arena, fdf::wire::NodePropertyValue::WithEnumValue(
                                 arena, fidl::StringView::FromExternal(value.data.enum_val)));
        break;
      default:
        FDF_LOGL(ERROR, logger, "Unsupported property type, key: %s", key);
        break;
    }
  }

  for (auto value :
       cpp20::span(zx_args->fidl_protocol_offers, zx_args->fidl_protocol_offer_count)) {
    properties.emplace_back(arena)
        .set_key(arena, fdf::wire::NodePropertyKey::WithStringValue(
                            arena, fidl::StringView::FromExternal(value)))
        .set_value(arena, fdf::wire::NodePropertyValue::WithEnumValue(
                              arena, std::string(value) + ".ZirconTransport"));

    auto property = fidl_offer_to_device_prop(arena, value);
    if (property) {
      properties.push_back(*property);
    }
  }

  for (auto value : cpp20::span(zx_args->fidl_service_offers, zx_args->fidl_service_offer_count)) {
    properties.emplace_back(arena)
        .set_key(arena, fdf::wire::NodePropertyKey::WithStringValue(
                            arena, fidl::StringView::FromExternal(value)))
        .set_value(arena, fdf::wire::NodePropertyValue::WithEnumValue(
                              arena, std::string(value) + ".ZirconTransport"));

    auto property = fidl_offer_to_device_prop(arena, value);
    if (property) {
      properties.push_back(*property);
    }
  }

  // Some DFv1 devices expect to be able to set their own protocol, without specifying proto_id.
  // If we see a BIND_PROTOCOL property, don't add our own.
  if (!has_protocol) {
    // If we do not have a protocol id, set it to MISC to match DFv1 behavior.
    uint32_t proto_id = zx_args->proto_id == 0 ? ZX_PROTOCOL_MISC : zx_args->proto_id;
    properties.emplace_back(arena)
        .set_key(arena, fdf::wire::NodePropertyKey::WithIntValue(BIND_PROTOCOL))
        .set_value(arena, fdf::wire::NodePropertyValue::WithIntValue(proto_id));
  }
  return properties;
}

Device::Device(device_t device, const zx_protocol_device_t* ops, Driver* driver,
               std::optional<Device*> parent, driver::Logger& logger,
               async_dispatcher_t* dispatcher)
    : compat_child_(std::string(device.name), device.proto_ops.id, "", MetadataMap(), {}),
      name_(device.name),
      logger_(logger),
      dispatcher_(dispatcher),
      driver_(driver),
      compat_symbol_(device),
      ops_(ops),
      parent_(parent),
      executor_(dispatcher) {}

Device::~Device() {
  // We only shut down the devices that have a parent, since that means that *this* compat driver
  // owns the device. If the device does not have a parent, then ops_ belongs to another driver, and
  // it's that driver's responsibility to be shut down.
  if (parent_) {
    // Technically we shouldn't unbind here, since unbind should go parent to child.
    // However, this is much simpler than going parent to child, and this
    // *technically* upholds the same invariant, because at this point we know
    // the device does not have any children.
    // Also, if a device has unbind, it would be an error to call Release before
    // Unbind.
    // This may be a potential difference in behavior from DFv1, so this needs
    // to be investigated further. For now, it will let us run integration tests.
    // TODO(fxbug.dev/92196)
    if (HasOp(ops_, &zx_protocol_device_t::unbind)) {
      ops_->unbind(compat_symbol_.context);
    }

    // Call the parent's pre-release.
    if (HasOp((*parent_)->ops_, &zx_protocol_device_t::child_pre_release)) {
      (*parent_)->ops_->child_pre_release((*parent_)->compat_symbol_.context,
                                          compat_symbol_.context);
    }

    if (HasOp(ops_, &zx_protocol_device_t::release)) {
      ops_->release(compat_symbol_.context);
    }
  }

  for (auto& completer : remove_completers_) {
    completer.complete_ok();
  }
}

zx_device_t* Device::ZxDevice() { return static_cast<zx_device_t*>(this); }

void Device::Bind(fidl::WireSharedClient<fdf::Node> node) { node_ = std::move(node); }

void Device::Unbind() {
  // This closes the client-end of the node to signal to the driver framework
  // that node should be removed.
  //
  // `fidl::WireClient` does not provide a direct way to unbind a client, so we
  // assign a default client to unbind the existing client.
  node_ = {};
}

const char* Device::Name() const { return name_.data(); }

bool Device::HasChildren() const { return !children_.empty(); }

zx_status_t Device::Add(device_add_args_t* zx_args, zx_device_t** out) {
  device_t compat_device = {
      .proto_ops =
          {
              .ops = zx_args->proto_ops,
              .id = zx_args->proto_id,
          },
      .name = zx_args->name,
      .context = zx_args->ctx,
  };

  auto device =
      std::make_shared<Device>(compat_device, zx_args->ops, driver_, this, logger_, dispatcher_);
  // Update the compat symbol name pointer with a pointer the device owns.
  device->compat_symbol_.name = device->name_.data();

  device->topological_path_ = topological_path_;
  if (!device->topological_path_.empty()) {
    device->topological_path_ += "/";
  }
  device->topological_path_ += device->name_;

  device->dev_vnode_ = fbl::MakeRefCounted<DevfsVnode>(device->ZxDevice());
  device->compat_child_ =
      Child(std::string(zx_args->name), zx_args->proto_id, std::string(device->topological_path()),
            MetadataMap(), MakeFidlServiceOffers(zx_args));

  if (zx_args->outgoing_dir_channel != ZX_HANDLE_INVALID) {
    device->compat_child_.compat_device().set_dir(
        fidl::ClientEnd<fuchsia_io::Directory>(zx::channel(zx_args->outgoing_dir_channel)));
  }

  // Add the metadata from add_args:
  for (size_t i = 0; i < zx_args->metadata_count; i++) {
    auto status =
        device->AddMetadata(zx_args->metadata_list[i].type, zx_args->metadata_list[i].data,
                            zx_args->metadata_list[i].length);
    if (status != ZX_OK) {
      return status;
    }
  }

  device->properties_ = CreateProperties(arena_, logger_, zx_args);
  device->device_flags_ = zx_args->flags;

  bool has_init = HasOp(device->ops_, &zx_protocol_device_t::init);
  if (!has_init) {
    device->InitReply(ZX_OK);
  }

  if (out) {
    *out = device->ZxDevice();
  }
  children_.push_back(std::move(device));
  return ZX_OK;
}

fpromise::promise<void, zx_status_t> Device::Export() {
  zx_status_t status = driver()->interop().AddToOutgoing(&compat_child_, dev_vnode());
  if (status != ZX_OK) {
    FDF_LOG(INFO, "Device %s failed to add to outgoing directory: %s", topological_path_.c_str(),
            zx_status_get_string(status));
    return fpromise::make_error_promise(status);
  }

  bool has_init = HasOp(ops_, &zx_protocol_device_t::init);
  auto options = fuchsia_device_fs::wire::ExportOptions();
  if (has_init) {
    options |= fuchsia_device_fs::wire::ExportOptions::kInvisible;
  }

  status = driver()->interop().ExportToDevfsSync(&compat_child_, options);
  if (status != ZX_OK) {
    FDF_LOG(INFO, "Device %s failed to add to devfs: %s", topological_path_.c_str(),
            zx_status_get_string(status));
    return fpromise::make_error_promise(status);
  }
  // TODO(fxdebug.dev/90735): When DriverDevelopment works in DFv2, don't print
  // this.
  FDF_LOG(DEBUG, "Created /dev/%s", topological_path().data());

  // If the device is non-bindable we want to create the node now. This lets the driver
  // immediately create more children once we return.
  if (device_flags_ & DEVICE_ADD_NON_BINDABLE) {
    status = CreateNode();
    if (status != ZX_OK) {
      FDF_LOG(INFO, "Device %s failed to create NON_BINDABLE node: %s", topological_path_.c_str(),
              zx_status_get_string(status));
      return fpromise::make_error_promise(status);
    }
  }

  // Wait for the device to initialize, then export to dev, then
  // create the device's Node.
  return fpromise::make_promise([has_init, this]() {
           // Emulate fuchsia.device.manager.DeviceController behaviour, and run the
           // init task after adding the device.
           if (has_init) {
             ops_->init(compat_symbol_.context);
           }
           return fpromise::make_result_promise<void, zx_status_t>(fpromise::ok());
         })
      .and_then(WaitForInitToComplete())
      .and_then([has_init, interop = &driver()->interop(), this]() {
        // Make the device visible if it has an init function.
        if (has_init) {
          auto status = interop->devfs_exporter().exporter().sync()->MakeVisible(
              fidl::StringView::FromExternal(topological_path()));
          if (status->is_error()) {
            return fpromise::make_error_promise(status->error_value());
          }
        }

        // Create the node now that we are initialized.
        // If we were non bindable, we would've made the node earlier.
        if (!(device_flags_ & DEVICE_ADD_NON_BINDABLE)) {
          zx_status_t status = CreateNode();
          if (status != ZX_OK) {
            FDF_LOG(ERROR, "Failed to CreateNode for device: %s: %s", Name(),
                    zx_status_get_string(status));
            return fpromise::make_error_promise(status);
          }
        }

        return fpromise::make_result_promise(fpromise::result<void, zx_status_t>());
      })
      .or_else([this](const zx_status_t& status) {
        FDF_LOG(ERROR, "Failed to export /dev/%s to devfs: %s", topological_path().data(),
                zx_status_get_string(status));
        Remove();
        return fpromise::make_error_promise(status);
      })
      .wrap_with(scope());
}

zx_status_t Device::CreateNode() {
  // Create NodeAddArgs from `zx_args`.
  fidl::Arena arena;

  auto offers = compat_child_.CreateOffers(arena);

  std::vector<fdf::wire::NodeSymbol> symbols;
  symbols.emplace_back(arena)
      .set_name(arena, kDeviceSymbol)
      .set_address(arena, reinterpret_cast<uint64_t>(&compat_symbol_));
  symbols.emplace_back(arena)
      .set_name(arena, kOps)
      .set_address(arena, reinterpret_cast<uint64_t>(ops_));

  fdf::wire::NodeAddArgs args(arena);
  auto valid_name = MakeValidName(name_);
  args.set_name(arena, fidl::StringView::FromExternal(valid_name))
      .set_symbols(arena, fidl::VectorView<fdf::wire::NodeSymbol>::FromExternal(symbols))
      .set_offers(arena,
                  fidl::VectorView<fcd::wire::Offer>::FromExternal(offers.data(), offers.size()))
      .set_properties(arena, fidl::VectorView<fdf::wire::NodeProperty>::FromExternal(properties_));

  // Create NodeController, so we can control the device.
  auto controller_ends = fidl::CreateEndpoints<fdf::NodeController>();
  if (controller_ends.is_error()) {
    return controller_ends.status_value();
  }

  fpromise::bridge<> teardown_bridge;
  controller_teardown_finished_.emplace(teardown_bridge.consumer.promise());
  controller_.Bind(
      std::move(controller_ends->client), dispatcher_,
      fidl::ObserveTeardown([device = weak_from_this(),
                             completer = std::move(teardown_bridge.completer)]() mutable {
        // Because the dispatcher can be multi-threaded, we must use a
        // `fidl::WireSharedClient`. The `fidl::WireSharedClient` uses a
        // two-phase destruction to teardown the client.
        //
        // Because of this, the teardown might be happening after the
        // Device has already been erased. This is likely to occur if the
        // Driver is asked to shutdown. If that happens, the Driver will
        // free its Devices, the Device will release its NodeController,
        // and then this shutdown will occur later. In order to not have a
        // Use-After-Free here, only try to remove the Device if the
        // weak_ptr still exists.
        //
        // The weak pointer will be valid here if the NodeController
        // representing the Device exits on its own. This represents the
        // Device's child Driver exiting, and in that instance we want to
        // Remove the Device.
        if (auto ptr = device.lock()) {
          if (ptr->pending_removal_) {
            // TODO(fxbug.dev/100470): We currently do not remove the DFv1 child
            // if the NodeController is removed but the driver didn't asked to be
            // removed. We need to investigate the correct behavior here.
            FDF_LOGL(INFO, ptr->logger(), "Device %s has its NodeController unexpectedly removed",
                     (ptr)->topological_path_.data());
          }
          // Only remove us if the driver requested it (normally via device_async_remove)
          if (ptr->parent_.has_value() && ptr->pending_removal_ && !ptr->pending_rebind_) {
            (*ptr->parent_)->RemoveChild(ptr);
          }
        }
        completer.complete_ok();
      }));

  // If the node is not bindable, we own the node.
  fidl::ServerEnd<fdf::Node> node_server;
  if ((device_flags_ & DEVICE_ADD_NON_BINDABLE) != 0) {
    auto node_ends = fidl::CreateEndpoints<fdf::Node>();
    if (node_ends.is_error()) {
      return node_ends.status_value();
    }
    node_.Bind(std::move(node_ends->client), dispatcher_);
    node_server = std::move(node_ends->server);
  }

  // Add the device node.
  if (!(*parent_)->node_.is_valid()) {
    FDF_LOG(ERROR, "Cannot add device, as parent '%s' is not marked NON_BINDABLE.",
            (*parent_)->topological_path_.data());
    return ZX_ERR_NOT_SUPPORTED;
  }

  fpromise::bridge<void, std::variant<zx_status_t, fdf::NodeError>> bridge;
  auto callback = [completer = std::move(bridge.completer)](
                      fidl::WireUnownedResult<fdf::Node::AddChild>& result) mutable {
    if (!result.ok()) {
      completer.complete_error(result.error().status());
      return;
    }
    if (result->is_error()) {
      completer.complete_error(result->error_value());
      return;
    }
    completer.complete_ok();
  };
  (*parent_)
      ->node_->AddChild(args, std::move(controller_ends->server), std::move(node_server))
      .ThenExactlyOnce(std::move(callback));

  auto task =
      bridge.consumer.promise()
          .or_else([this](std::variant<zx_status_t, fdf::NodeError>& status) {
            if (std::holds_alternative<zx_status_t>(status)) {
              FDF_LOG(ERROR, "Failed to add device: status: '%s': %u", Name(),
                      std::get<zx_status_t>(status));
            } else if (std::holds_alternative<fdf::NodeError>(status)) {
              if (std::get<fdf::NodeError>(status) == fdf::NodeError::kNodeRemoved) {
                // This is not an error as it can happen if the parent driver is unbound while we
                // are still setting up.
                FDF_LOG(WARNING, "Failed to add device '%s' while parent was removed", Name());
              } else {
                FDF_LOG(ERROR, "Failed to add device: NodeError: '%s': %u", Name(),
                        std::get<fdf::NodeError>(status));
              }
            }
          })
          .wrap_with(scope_);
  executor_.schedule_task(std::move(task));
  return ZX_OK;
}

fpromise::promise<void> Device::RemoveChildren() {
  std::vector<fpromise::promise<void>> promises;
  for (auto& child : children_) {
    promises.push_back(child->Remove());
  }
  return fpromise::join_promise_vector(std::move(promises))
      .then([](fpromise::result<std::vector<fpromise::result<void>>>& results) {
        if (results.is_error()) {
          return fpromise::make_error_promise();
        }
        for (auto& result : results.value()) {
          if (result.is_error()) {
            return fpromise::make_error_promise();
          }
        }
        return fpromise::make_ok_promise();
      });
}

fpromise::promise<void> Device::Remove() {
  fpromise::bridge<void> finished_bridge;
  remove_completers_.push_back(std::move(finished_bridge.completer));

  executor_.schedule_task(
      WaitForInitToComplete().then([this](fpromise::result<void, zx_status_t>& init) {
        pending_removal_ = true;
        // This should be called if we hit an error trying to remove the controller.
        auto schedule_removal = fit::defer([this]() {
          if (parent_.has_value()) {
            auto shared = shared_from_this();
            // We schedule our removal on our parent's executor because we can't be removed
            // while being run in a promise on our own executor.
            (*parent_)->executor_.schedule_task(
                fpromise::make_promise([parent = *parent_, shared = std::move(shared)]() mutable {
                  parent->RemoveChild(shared);
                }));
          }
        });

        if (!controller_) {
          FDF_LOG(ERROR, "Failed to remove device '%s', invalid node controller", Name());
          return;
        }
        auto result = controller_->Remove();

        // If we hit an error calling remove, we should log it.
        // We don't need to log if the error is that we cannot connect
        // to the protocol, because that means we are already in the process
        // of shutting down.
        if (!result.ok() && !result.is_peer_closed() && !result.is_canceled()) {
          FDF_LOG(ERROR, "Failed to remove device '%s': %s", Name(),
                  result.FormatDescription().data());
        }
        schedule_removal.cancel();
      }));
  return finished_bridge.consumer.promise();
}

void Device::RemoveChild(std::shared_ptr<Device>& child) { children_.remove(child); }

void Device::InsertOrUpdateProperty(fuchsia_driver_framework::wire::NodePropertyKey key,
                                    fuchsia_driver_framework::wire::NodePropertyValue value) {
  bool found = false;
  for (auto& prop : properties_) {
    if (!prop.has_key()) {
      continue;
    }

    if (prop.key().Which() != key.Which()) {
      continue;
    }

    if (key.is_string_value()) {
      std::string_view prop_key_view(prop.key().string_value().data(),
                                     prop.key().string_value().size());
      std::string_view key_view(key.string_value().data(), key.string_value().size());
      if (key_view == prop_key_view) {
        found = true;
      }
    } else if (key.is_int_value()) {
      if (key.int_value() == prop.key().int_value()) {
        found = true;
      }
    }

    if (found) {
      prop.value() = value;
      break;
    }
  }
  if (!found) {
    properties_.emplace_back(arena_).set_key(arena_, key).set_value(arena_, value);
  }
}

zx_status_t Device::GetProtocol(uint32_t proto_id, void* out) const {
  if (HasOp(ops_, &zx_protocol_device_t::get_protocol)) {
    return ops_->get_protocol(compat_symbol_.context, proto_id, out);
  }

  if ((compat_symbol_.proto_ops.id != proto_id) || (compat_symbol_.proto_ops.ops == nullptr)) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  if (!out) {
    return ZX_OK;
  }

  struct GenericProtocol {
    const void* ops;
    void* ctx;
  };

  auto proto = static_cast<GenericProtocol*>(out);
  proto->ops = compat_symbol_.proto_ops.ops;
  proto->ctx = compat_symbol_.context;
  return ZX_OK;
}

zx_status_t Device::AddMetadata(uint32_t type, const void* data, size_t size) {
  return compat_child_.compat_device().AddMetadata(type, data, size);
}

zx_status_t Device::GetMetadata(uint32_t type, void* buf, size_t buflen, size_t* actual) {
  return compat_child_.compat_device().GetMetadata(type, buf, buflen, actual);
}

zx_status_t Device::GetMetadataSize(uint32_t type, size_t* out_size) {
  return compat_child_.compat_device().GetMetadataSize(type, out_size);
}

zx_status_t Device::MessageOp(fidl_incoming_msg_t* msg, fidl_txn_t* txn) {
  if (!HasOp(ops_, &zx_protocol_device_t::message)) {
    return ZX_ERR_NOT_SUPPORTED;
  }
  return ops_->message(compat_symbol_.context, msg, txn);
}

void Device::InitReply(zx_status_t status) {
  std::scoped_lock lock(init_lock_);
  init_is_finished_ = true;
  init_status_ = status;
  for (auto& waiter : init_waiters_) {
    if (status == ZX_OK) {
      waiter.complete_ok();
    } else {
      waiter.complete_error(init_status_);
    }
  }
  init_waiters_.clear();
}

zx_status_t Device::ReadOp(void* data, size_t len, size_t off, size_t* out_actual) {
  if (!HasOp(ops_, &zx_protocol_device_t::read)) {
    return ZX_ERR_NOT_SUPPORTED;
  }
  return ops_->read(compat_symbol_.context, data, len, off, out_actual);
}

zx_status_t Device::WriteOp(const void* data, size_t len, size_t off, size_t* out_actual) {
  if (!HasOp(ops_, &zx_protocol_device_t::write)) {
    return ZX_ERR_NOT_SUPPORTED;
  }
  return ops_->write(compat_symbol_.context, data, len, off, out_actual);
}

fpromise::promise<void, zx_status_t> Device::WaitForInitToComplete() {
  std::scoped_lock lock(init_lock_);
  if (init_is_finished_) {
    if (init_status_ == ZX_OK) {
      return fpromise::make_result_promise<void, zx_status_t>(fpromise::ok());
    }
    return fpromise::make_result_promise<void, zx_status_t>(fpromise::error(init_status_));
  }
  fpromise::bridge<void, zx_status_t> bridge;
  init_waiters_.push_back(std::move(bridge.completer));

  return bridge.consumer.promise_or(fpromise::error(ZX_ERR_UNAVAILABLE));
}

constexpr char kCompatKey[] = "fuchsia.compat.LIBNAME";
fpromise::promise<void, zx_status_t> Device::RebindToLibname(std::string_view libname) {
  if (controller_teardown_finished_ == std::nullopt) {
    FDF_LOG(ERROR, "Calling rebind before device is set up?");
    return fpromise::make_error_promise(ZX_ERR_BAD_STATE);
  }
  InsertOrUpdateProperty(
      fdf::wire::NodePropertyKey::WithStringValue(arena_,
                                                  fidl::StringView::FromExternal(kCompatKey)),
      fdf::wire::NodePropertyValue::WithStringValue(arena_, fidl::StringView(arena_, libname)));
  // Once the controller teardown is finished (and the device is safely deleted),
  // we re-create the device.
  pending_rebind_ = true;
  auto promise =
      std::move(controller_teardown_finished_.value())
          .or_else([]() -> fpromise::result<void, zx_status_t> {
            ZX_ASSERT_MSG(false, "Unbind should always succeed");
          })
          .and_then([weak = weak_from_this()]() mutable -> fpromise::result<void, zx_status_t> {
            auto ptr = weak.lock();
            if (!ptr) {
              return fpromise::error(ZX_ERR_CANCELED);
            }
            // Reset FIDL clients so they don't complain when rebound.
            ptr->controller_ = {};
            ptr->node_ = {};
            zx_status_t status = ptr->CreateNode();
            ptr->pending_rebind_ = false;
            if (status != ZX_OK) {
              FDF_LOGL(ERROR, ptr->logger(), "Failed to recreate node: %s",
                       zx_status_get_string(status));
              return fpromise::error(status);
            }

            return fpromise::ok();
          })
          .wrap_with(scope_);
  Remove();
  return promise;
}

zx_status_t Device::ConnectFragmentFidl(const char* fragment_name, const char* protocol_name,
                                        zx::channel request) {
  if (std::string_view(fragment_name) != "default") {
    bool fragment_exists = false;
    for (auto& fragment : fragments_) {
      if (fragment == fragment_name) {
        fragment_exists = true;
        break;
      }
    }
    if (!fragment_exists) {
      FDF_LOG(ERROR, "Tried to connect to fragment '%s' but it's not in the fragment list",
              fragment_name);
      return ZX_ERR_NOT_FOUND;
    }
  }

  auto connect_string = std::string("/svc/")
                            .append(fuchsia_driver_compat::Service::Name)
                            .append("/")
                            .append(fragment_name)
                            .append("/device");

  auto device = driver_->driver_namespace().Connect<fuchsia_driver_compat::Device>(connect_string);
  if (device.status_value() != ZX_OK) {
    FDF_LOG(ERROR, "Error connecting: %s", device.status_string());
    return device.status_value();
  }
  auto result = fidl::WireCall(*device)->ConnectFidl(fidl::StringView::FromExternal(protocol_name),
                                                     std::move(request));
  if (result.status() != ZX_OK) {
    FDF_LOG(ERROR, "Error calling connect fidl: %s", result.status_string());
    return result.status();
  }

  return ZX_OK;
}

zx_status_t Device::OpenFragmentFidlService(const char* fragment_name, const char* service_name,
                                            zx::channel request) {
  if (std::string_view(fragment_name) != "default") {
    bool fragment_exists = false;
    for (auto& fragment : fragments_) {
      if (fragment == fragment_name) {
        fragment_exists = true;
        break;
      }
    }
    if (!fragment_exists) {
      FDF_LOG(ERROR, "Tried to connect to fragment '%s' but it's not in the fragment list",
              fragment_name);
      return ZX_ERR_NOT_FOUND;
    }
  }

  auto service_path = std::string("/svc/").append(service_name).append("/").append(fragment_name);

  auto result = driver_->driver_namespace().Connect(service_path, std::move(request));
  if (result.is_error()) {
    FDF_LOG(ERROR, "Error connecting: %s", result.status_string());
    return result.status_value();
  }

  return ZX_OK;
}

zx_status_t Device::AddComposite(const char* name, const composite_device_desc_t* comp_desc) {
  auto creator =
      driver_->driver_namespace().Connect<fuchsia_device_composite::DeprecatedCompositeCreator>();
  if (creator.status_value() != ZX_OK) {
    FDF_LOG(ERROR, "Error connecting: %s", creator.status_string());
    return creator.status_value();
  }

  fidl::Arena allocator;
  auto composite = CreateComposite(allocator, comp_desc);
  if (composite.is_error()) {
    FDF_LOG(ERROR, "Error creating composite: %s", composite.status_string());
    return composite.error_value();
  }

  auto result = fidl::WireCall(*creator)->AddCompositeDevice(fidl::StringView::FromExternal(name),
                                                             std::move(composite.value()));
  if (result.status() != ZX_OK) {
    FDF_LOG(ERROR, "Error calling connect fidl: %s", result.status_string());
    return result.status();
  }

  return ZX_OK;
}

zx_status_t Device::ConnectRuntime(const char* protocol_name, fdf::Channel request) {
  auto endpoints = fidl::CreateEndpoints<fuchsia_driver_framework::RuntimeConnector>();
  if (endpoints.is_error()) {
    return endpoints.status_value();
  }
  zx_status_t status = ConnectFragmentFidl(
      "default", fidl::DiscoverableProtocolName<fuchsia_driver_framework::RuntimeConnector>,
      endpoints->server.TakeChannel());
  if (status != ZX_OK) {
    FDF_LOG(ERROR, "Error connecting to RuntimeConnector protocol: %s",
            zx_status_get_string(status));
    return status;
  }
  auto result =
      fidl::WireCall(endpoints->client)
          ->Connect(fidl::StringView::FromExternal(protocol_name),
                    fuchsia_driver_framework::wire::RuntimeProtocolServerEnd{request.release()});
  if (result.status() != ZX_OK) {
    FDF_LOG(ERROR, "Error calling RuntimeConnector::Connect fidl: %s", result.status_string());
    return result.status();
  }
  return ZX_OK;
}

}  // namespace compat
