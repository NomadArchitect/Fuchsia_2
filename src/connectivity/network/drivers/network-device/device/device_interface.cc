// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "device_interface.h"

#include <lib/async/cpp/task.h>
#include <lib/fit/defer.h>
#include <zircon/device/network.h>

#include <fbl/alloc_checker.h>

#include "log.h"
#include "rx_queue.h"
#include "session.h"
#include "tx_queue.h"

// Static sanity assertions from far-away defined buffer_descriptor_t.
// A buffer descriptor is always described in 64 bit words.
static_assert(sizeof(buffer_descriptor_t) % 8 == 0);
// Verify no unseen padding is being added by the compiler and all padding reservation fields are
// working as expected, check the offset of every 64 bit word in the struct.
static_assert(offsetof(buffer_descriptor_t, frame_type) == 0);
static_assert(offsetof(buffer_descriptor_t, port_id) == 8);
static_assert(offsetof(buffer_descriptor_t, offset) == 16);
static_assert(offsetof(buffer_descriptor_t, head_length) == 24);
static_assert(offsetof(buffer_descriptor_t, inbound_flags) == 32);

namespace network {

zx::status<std::unique_ptr<NetworkDeviceInterface>> NetworkDeviceInterface::Create(
    async_dispatcher_t* dispatcher, ddk::NetworkDeviceImplProtocolClient parent,
    const char* parent_name) {
  return internal::DeviceInterface::Create(dispatcher, parent, parent_name);
}

namespace internal {

uint16_t TransformFifoDepth(uint16_t device_depth) {
  // We're going to say the depth is twice the depth of the device to account for in-flight
  // buffers, as long as it doesn't go over the maximum fifo depth.

  // Check for overflow.
  if (device_depth > (std::numeric_limits<uint16_t>::max() >> 1)) {
    return kMaxFifoDepth;
  }

  return std::min(kMaxFifoDepth, static_cast<uint16_t>(device_depth << 1));
}

zx::status<std::unique_ptr<DeviceInterface>> DeviceInterface::Create(
    async_dispatcher_t* dispatcher, ddk::NetworkDeviceImplProtocolClient parent,
    const char* parent_name) {
  fbl::AllocChecker ac;
  std::unique_ptr<DeviceInterface> device(new (&ac) DeviceInterface(dispatcher, parent));
  if (!ac.check()) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }
  zx_status_t status = device->Init(parent_name);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(std::move(device));
}

DeviceInterface::~DeviceInterface() {
  ZX_ASSERT_MSG(primary_session_ == nullptr,
                "Can't destroy DeviceInterface with active primary session. (%s)",
                primary_session_->name());
  ZX_ASSERT_MSG(sessions_.is_empty(), "Can't destroy DeviceInterface with %ld pending session(s).",
                sessions_.size_slow());
  ZX_ASSERT_MSG(dead_sessions_.is_empty(),
                "Can't destroy DeviceInterface with %ld pending dead session(s).",
                dead_sessions_.size_slow());
  ZX_ASSERT_MSG(bindings_.is_empty(), "Can't destroy device interface with %ld attached bindings.",
                bindings_.size_slow());
  size_t active_ports = std::count_if(
      ports_.begin(), ports_.end(),
      [](const std::unique_ptr<DevicePort>& port) { return static_cast<bool>(port); });
  ZX_ASSERT_MSG(!active_ports, "Can't destroy device interface with %ld ports", active_ports);
}

zx_status_t DeviceInterface::Init(const char* parent_name) {
  LOG_TRACE("network-device: Init");
  if (!device_.is_valid()) {
    LOG_ERROR("network-device: init: no protocol");
    return ZX_ERR_INTERNAL;
  }

  network_device_impl_protocol_t proto;
  device_.GetProto(&proto);
  if (proto.ops == nullptr) {
    LOG_ERROR("network-device: init: null protocol ops");
    return ZX_ERR_INTERNAL;
  }
  network_device_impl_protocol_ops_t& ops = *proto.ops;
  if (ops.init == nullptr || ops.get_info == nullptr || ops.stop == nullptr ||
      ops.start == nullptr || ops.queue_tx == nullptr || ops.queue_rx_space == nullptr ||
      ops.prepare_vmo == nullptr || ops.release_vmo == nullptr || ops.set_snoop == nullptr) {
    LOGF_ERROR("network-device: init: device '%s': incomplete protocol", parent_name);
    return ZX_ERR_NOT_SUPPORTED;
  }

  device_.GetInfo(&device_info_);
  if (device_info_.buffer_alignment == 0) {
    LOGF_ERROR("network-device: init: device '%s' reports invalid zero buffer alignment",
               parent_name);
    return ZX_ERR_NOT_SUPPORTED;
  }
  if (device_info_.rx_threshold > device_info_.rx_depth) {
    LOGF_ERROR("network-device: init: device'%s' reports rx_threshold = %d larger than rx_depth %d",
               parent_name, device_info_.rx_threshold, device_info_.rx_depth);
    return ZX_ERR_NOT_SUPPORTED;
  }

  if (device_info_.rx_accel_count > netdev::wire::kMaxAccelFlags ||
      device_info_.tx_accel_count > netdev::wire::kMaxAccelFlags) {
    LOGF_ERROR("network-device: init: device '%s' reports too many acceleration flags",
               parent_name);
    return ZX_ERR_NOT_SUPPORTED;
  }
  // Copy the vectors of supported acceleration flags.
  std::copy_n(device_info_.rx_accel_list, device_info_.rx_accel_count, accel_rx_.begin());
  device_info_.rx_accel_list = accel_rx_.data();
  std::copy_n(device_info_.tx_accel_list, device_info_.tx_accel_count, accel_tx_.begin());
  device_info_.tx_accel_list = accel_tx_.data();

  if (device_info_.rx_depth > kMaxFifoDepth || device_info_.tx_depth > kMaxFifoDepth) {
    LOGF_ERROR("network-device: init: device '%s' reports too large FIFO depths: %d/%d (max=%d)",
               parent_name, device_info_.rx_depth, device_info_.tx_depth, kMaxFifoDepth);
    return ZX_ERR_NOT_SUPPORTED;
  }

  zx::status tx_queue = TxQueue::Create(this);
  if (tx_queue.is_error()) {
    LOGF_ERROR("network-device: init: device failed to start Tx Queue: %s",
               tx_queue.status_string());
    return tx_queue.status_value();
  }
  tx_queue_ = std::move(tx_queue.value());

  zx::status rx_queue = RxQueue::Create(this);
  if (rx_queue.is_error()) {
    LOGF_ERROR("network-device: init: device failed to start Rx Queue: %s",
               rx_queue.status_string());
    return rx_queue.status_value();
  }
  rx_queue_ = std::move(rx_queue.value());

  zx_status_t status;
  {
    fbl::AutoLock lock(&control_lock_);
    if ((status = vmo_store_.Reserve(MAX_VMOS)) != ZX_OK) {
      LOGF_ERROR("network-device: init: failed to init session identifiers %s",
                 zx_status_get_string(status));
      return status;
    }
  }

  // Init session with parent.
  if ((status = device_.Init(this, &network_device_ifc_protocol_ops_)) != ZX_OK) {
    LOGF_ERROR("network-device: init: NetworkDevice Init failed: %s", zx_status_get_string(status));
    return status;
  }

  return ZX_OK;
}

void DeviceInterface::Teardown(fit::callback<void()> teardown_callback) {
  // stop all rx queue operation immediately.
  rx_queue_->JoinThread();
  LOG_TRACE("network-device: Teardown");

  control_lock_.Acquire();
  // Can't call teardown again until the teardown process has ended.
  ZX_ASSERT(teardown_callback_ == nullptr);
  teardown_callback_ = std::move(teardown_callback);

  ContinueTeardown(TeardownState::RUNNING);
}

zx_status_t DeviceInterface::Bind(fidl::ServerEnd<netdev::Device> req) {
  fbl::AutoLock lock(&control_lock_);
  // Don't attach new bindings if we're tearing down.
  if (teardown_state_ != TeardownState::RUNNING) {
    return ZX_ERR_BAD_STATE;
  }
  return Binding::Bind(this, std::move(req));
}

// TODO(http://fxbug.dev/64310): Delete this method when ports are exposed over FIDL.
zx_status_t DeviceInterface::BindMac(fidl::ServerEnd<netdev::MacAddressing> req) {
  SharedAutoLock lock(&control_lock_);
  // Don't attach new bindings if we're tearing down.
  if (teardown_state_ != TeardownState::RUNNING) {
    return ZX_ERR_BAD_STATE;
  }
  // Always attempt to bind mac to port 0 until we're able to remove this method.
  return WithPort(kPort0, [req = std::move(req)](const std::unique_ptr<DevicePort>& port0) mutable {
    if (!port0) {
      return ZX_ERR_NOT_FOUND;
    }
    port0->BindMac(std::move(req));
    return ZX_OK;
  });
}

void DeviceInterface::NetworkDeviceIfcPortStatusChanged(uint8_t port_id,
                                                        const port_status_t* new_status) {
  SharedAutoLock lock(&control_lock_);
  // Skip port status changes if tearing down. During teardown ports may disappear and device
  // implementation may not be aware of it yet.
  if (teardown_state_ != TeardownState::RUNNING) {
    return;
  }
  WithPort(port_id, [&new_status, port_id](const std::unique_ptr<DevicePort>& port) {
    if (!port) {
      LOGF_ERROR("network-device: StatusChanged on unknown port=%d %d %d", port_id,
                 new_status->flags, new_status->mtu);
      return;
    }

    LOGF_TRACE("network-device: StatusChanged(port=%d) %d %d", port_id, new_status->flags,
               new_status->mtu);
    port->StatusChanged(*new_status);
  });
}

void DeviceInterface::NetworkDeviceIfcAddPort(uint8_t port_id,
                                              const network_port_protocol_t* port_proto) {
  auto port_client = ddk::NetworkPortProtocolClient(port_proto);
  auto release_port = fit::defer([&port_client]() {
    if (port_client.is_valid()) {
      port_client.Removed();
    }
  });
  fbl::AutoLock lock(&control_lock_);
  // Don't allow new ports if tearing down.
  if (teardown_state_ != TeardownState::RUNNING) {
    LOGF_WARN("network-device: port %d not added, teardown in progress", port_id);
    return;
  }
  if (port_id >= ports_.size()) {
    LOGF_ERROR("network-device: port id %d out of allowed range: [0, %ld)", port_id, ports_.size());
    return;
  }
  std::unique_ptr<DevicePort>& port_slot = ports_[port_id];
  if (port_slot) {
    LOGF_ERROR("network-device: port %d already exists", port_id);
    return;
  }

  std::unique_ptr<MacAddrDeviceInterface> mac;
  mac_addr_protocol_t mac_proto;
  port_client.GetMac(&mac_proto);
  ddk::MacAddrProtocolClient mac_client(&mac_proto);
  if (mac_client.is_valid()) {
    zx::status status = MacAddrDeviceInterface::Create(mac_client);
    if (status.is_error()) {
      LOGF_ERROR("network-device: failed to instantiate MAC information for port %d: %s", port_id,
                 status.status_string());
      return;
    }
    mac = std::move(status.value());
  }

  fbl::AllocChecker checker;
  std::unique_ptr<DevicePort> port(
      new (&checker) DevicePort(dispatcher_, port_id, port_client, std::move(mac),
                                fit::bind_member(this, &DeviceInterface::OnPortTeardownComplete)));
  if (!checker.check()) {
    LOGF_ERROR("network-device: failed to allocate port memory");
    return;
  }

  // Clear port_client to prevent deferred call from notifying removal.
  port_client.clear();
  port_slot = std::move(port);

  // TODO(http://fxbug.dev/64310): Notify port watchers.
}

void DeviceInterface::NetworkDeviceIfcRemovePort(uint8_t port_id) {
  SharedAutoLock lock(&control_lock_);
  // Ignore if we're tearing down, all ports will be removed as part of teardown.
  if (teardown_state_ != TeardownState::RUNNING) {
    return;
  }
  WithPort(port_id, [](const std::unique_ptr<DevicePort>& port) {
    if (port) {
      port->Teardown();
    }
  });
}

void DeviceInterface::NetworkDeviceIfcCompleteRx(const rx_buffer_t* rx_list, size_t rx_count) {
  rx_queue_->CompleteRxList(rx_list, rx_count);
}

void DeviceInterface::NetworkDeviceIfcCompleteTx(const tx_result_t* tx_list, size_t tx_count) {
  tx_queue_->CompleteTxList(tx_list, tx_count);
}

void DeviceInterface::NetworkDeviceIfcSnoop(const rx_buffer_t* rx_list, size_t rx_count) {
  // TODO(fxbug.dev/43028): Implement real version. Current implementation acts as if no LISTEN is
  // ever in place.
}

void DeviceInterface::GetInfo(GetInfoRequestView request, GetInfoCompleter::Sync& completer) {
  SharedAutoLock lock(&control_lock_);
  // TODO(http://fxbug.dev/64310): Remove port0 requirement once FIDL is migrated to multi-port
  // version.
  WithPort(kPort0, [this, &completer](const std::unique_ptr<DevicePort>& port0) {
    if (!port0) {
      completer.Close(ZX_ERR_INTERNAL);
      return;
    }
    auto& port_info = port0->info();

    LOG_TRACE("network-device: GetInfo");
    netdev::wire::Info info{
        .class_ = static_cast<netdev::wire::DeviceClass>(port_info.device_class),
        .min_descriptor_length = sizeof(buffer_descriptor_t) / sizeof(uint64_t),
        .descriptor_version = NETWORK_DEVICE_DESCRIPTOR_VERSION,
        .rx_depth = rx_fifo_depth(),
        .tx_depth = tx_fifo_depth(),
        .buffer_alignment = device_info_.buffer_alignment,
        .max_buffer_length = device_info_.max_buffer_length,
        .min_rx_buffer_length = device_info_.min_rx_buffer_length,
        .min_tx_buffer_length = device_info_.min_tx_buffer_length,
        .min_tx_buffer_head = device_info_.tx_head_length,
        .min_tx_buffer_tail = device_info_.tx_tail_length,
    };

    std::array<netdev::wire::FrameType, netdev::wire::kMaxFrameTypes> rx;
    std::array<netdev::wire::FrameTypeSupport, netdev::wire::kMaxFrameTypes> tx;
    for (size_t i = 0; i < port_info.rx_types_count; i++) {
      rx[i] = static_cast<netdev::wire::FrameType>(port_info.rx_types_list[i]);
    }
    for (size_t i = 0; i < port_info.tx_types_count; i++) {
      auto& dst = tx[i];
      auto& src = port_info.tx_types_list[i];
      dst.features = src.features;
      dst.supported_flags = netdev::wire::TxFlags::TruncatingUnknown(src.supported_flags);
      dst.type = static_cast<netdev::wire::FrameType>(src.type);
    }

    info.rx_types = fidl::VectorView<netdev::wire::FrameType>::FromExternal(
        rx.data(), port_info.rx_types_count);
    info.tx_types = fidl::VectorView<netdev::wire::FrameTypeSupport>::FromExternal(
        tx.data(), port_info.tx_types_count);

    std::array<netdev::wire::RxAcceleration, netdev::wire::kMaxAccelFlags> rx_accel;
    std::array<netdev::wire::TxAcceleration, netdev::wire::kMaxAccelFlags> tx_accel;
    for (size_t i = 0; i < device_info_.rx_accel_count; i++) {
      rx_accel[i] = static_cast<netdev::wire::RxAcceleration>(device_info_.rx_accel_list[i]);
    }
    for (size_t i = 0; i < device_info_.tx_accel_count; i++) {
      tx_accel[i] = static_cast<netdev::wire::TxAcceleration>(device_info_.tx_accel_list[i]);
    }
    info.rx_accel = fidl::VectorView<netdev::wire::RxAcceleration>::FromExternal(
        rx_accel.data(), device_info_.rx_accel_count);
    info.tx_accel = fidl::VectorView<netdev::wire::TxAcceleration>::FromExternal(
        tx_accel.data(), device_info_.tx_accel_count);

    completer.Reply(std::move(info));
  });
}

void DeviceInterface::GetStatus(GetStatusRequestView request, GetStatusCompleter::Sync& completer) {
  SharedAutoLock lock(&control_lock_);
  // TODO(http://fxbug.dev/64310): Transitionally only fulfill request if port 0 exists.
  WithPort(kPort0, [&completer](const std::unique_ptr<DevicePort>& port0) {
    if (!port0) {
      completer.Close(ZX_ERR_INTERNAL);
      return;
    }
    port_status_t status;
    port0->impl().GetStatus(&status);
    WithWireStatus([&completer](netdev::wire::Status wire_status) { completer.Reply(wire_status); },
                   status);
  });
}

void DeviceInterface::OpenSession(OpenSessionRequestView request,
                                  OpenSessionCompleter::Sync& completer) {
  zx::status response = OpenSession(request->session_name, std::move(request->session_info));
  if (response.is_error()) {
    completer.ReplyError(response.error_value());
  } else {
    auto& [session, fifos] = response.value();
    completer.ReplySuccess(std::move(session), std::move(fifos));
  }
}

void DeviceInterface::GetStatusWatcher(GetStatusWatcherRequestView request,
                                       GetStatusWatcherCompleter::Sync& _completer) {
  SharedAutoLock lock(&control_lock_);
  // TODO(http://fxbug.dev/64310): Remove port0 requirement once FIDL is migrated to multi-port
  // version.
  WithPort(kPort0, [watcher = std::move(request->watcher),
                    buffer = request->buffer](const std::unique_ptr<DevicePort>& port0) mutable {
    if (!port0) {
      watcher.Close(ZX_ERR_NOT_FOUND);
      return;
    }
    port0->BindStatusWatcher(std::move(watcher), buffer);
  });
}

zx::status<netdev::wire::DeviceOpenSessionResponse> DeviceInterface::OpenSession(
    fidl::StringView name, netdev::wire::SessionInfo session_info) {
  fbl::AutoLock lock(&control_lock_);
  // We're currently tearing down and can't open any new sessions.
  if (teardown_state_ != TeardownState::RUNNING) {
    return zx::error(ZX_ERR_UNAVAILABLE);
  }

  // TODO(http://fxbug.dev/64310): We need to validate the request against port0 to fulfill the FIDL
  // API. Remove this once the session API changes to be aware of ports.
  zx_status_t status = WithPort(kPort0, [&session_info](const std::unique_ptr<DevicePort>& port0) {
    if (!port0) {
      return ZX_ERR_UNAVAILABLE;
    }
    for (auto frame_type : session_info.rx_frames) {
      if (!port0->IsValidRxFrameType(static_cast<uint8_t>(frame_type))) {
        return ZX_ERR_INVALID_ARGS;
      }
    }
    return ZX_OK;
  });
  if (status != ZX_OK) {
    return zx::error(status);
  }

  zx::status endpoints = fidl::CreateEndpoints<netdev::Session>();
  if (endpoints.is_error()) {
    return endpoints.take_error();
  }

  zx::status session_creation =
      Session::Create(dispatcher_, session_info, name, this, std::move(endpoints->server));
  if (session_creation.is_error()) {
    return session_creation.take_error();
  }
  auto& [session, fifos] = session_creation.value();

  // NB: It's safe to register the VMO after session creation (and thread start) because sessions
  // always start in a paused state, so the tx path can't be running while we hold the control lock.
  zx::status vmo_registration = RegisterDataVmo(std::move(session_info.data));
  if (vmo_registration.is_error()) {
    return vmo_registration.take_error();
  }
  auto& [vmo_id, vmo] = vmo_registration.value();
  session->SetDataVmo(vmo_id, vmo);

  if (session->ShouldTakeOverPrimary(primary_session_.get())) {
    // Set this new session as the primary session.
    std::swap(primary_session_, session);
    rx_queue_->TriggerSessionChanged();
  }
  if (session) {
    // Add the new session (or the primary session if it the new session just took over) to the list
    // of sessions.
    sessions_.push_back(std::move(session));
  }

  return zx::ok(netdev::wire::DeviceOpenSessionResponse{
      .session = std::move(endpoints->client),
      .fifos = std::move(fifos),
  });
}

uint16_t DeviceInterface::rx_fifo_depth() const {
  return TransformFifoDepth(device_info_.rx_depth);
}

uint16_t DeviceInterface::tx_fifo_depth() const {
  return TransformFifoDepth(device_info_.tx_depth);
}

void DeviceInterface::SessionStarted(Session& session) {
  bool should_start = false;
  {
    fbl::AutoLock lock(&control_lock_);
    if (session.IsListen()) {
      has_listen_sessions_.store(true, std::memory_order_relaxed);
    }
    if (session.IsPrimary()) {
      active_primary_sessions_++;
      if (session.ShouldTakeOverPrimary(primary_session_.get())) {
        // Push primary session to sessions list.
        sessions_.push_back(std::move(primary_session_));
        // Find the session in the list and promote it to primary.
        primary_session_ = sessions_.erase(session);
        ZX_ASSERT(primary_session_);
        // Notify rx queue of primary session change.
        rx_queue_->TriggerSessionChanged();
      }
      should_start = active_primary_sessions_ != 0;
    }
  }

  if (should_start) {
    // Start the device if we haven't done so already.
    StartDevice();
  }

  if (evt_session_started) {
    evt_session_started(session.name());
  }
}

bool DeviceInterface::SessionStoppedInner(Session& session) {
  if (session.IsListen()) {
    bool any = primary_session_ && primary_session_->IsListen() && !primary_session_->IsPaused();
    for (auto& s : sessions_) {
      any |= s.IsListen() && !s.IsPaused();
    }
    has_listen_sessions_.store(any, std::memory_order_relaxed);
  }

  if (!session.IsPrimary()) {
    return false;
  }

  ZX_ASSERT(active_primary_sessions_ > 0);
  if (&session == primary_session_.get()) {
    // If this was the primary session, offer all other sessions to take over:
    Session* primary_candidate = &session;
    for (auto& i : sessions_) {
      if (i.ShouldTakeOverPrimary(primary_candidate)) {
        primary_candidate = &i;
      }
    }
    // If we found a candidate to take over primary...
    if (primary_candidate != primary_session_.get()) {
      // ...promote it.
      sessions_.push_back(std::move(primary_session_));
      primary_session_ = sessions_.erase(*primary_candidate);
      ZX_ASSERT(primary_session_);
    }
    if (teardown_state_ == TeardownState::RUNNING) {
      rx_queue_->TriggerSessionChanged();
    }
  }

  active_primary_sessions_--;
  return active_primary_sessions_ == 0;
}

void DeviceInterface::SessionStopped(Session& session) {
  control_lock_.Acquire();
  if (SessionStoppedInner(session)) {
    // Stop the device, no more sessions are running.
    StopDevice();
  } else {
    control_lock_.Release();
  }
}

void DeviceInterface::StartDevice() {
  LOG_TRACE("network-device: StartDevice");

  bool start = false;
  {
    fbl::AutoLock lock(&control_lock_);
    // Start the device if we haven't done so already.
    switch (device_status_) {
      case DeviceStatus::STARTED:
      case DeviceStatus::STARTING:
        // Remove any pending operations we may have.
        pending_device_op_ = PendingDeviceOperation::NONE;
        break;
      case DeviceStatus::STOPPING:
        // Device is currently stopping, let's record that we want to start it.
        pending_device_op_ = PendingDeviceOperation::START;
        break;
      case DeviceStatus::STOPPED:
        // Device is in STOPPED state, start it.
        device_status_ = DeviceStatus::STARTING;
        start = true;
        break;
    }
  }

  if (start) {
    StartDeviceInner();
  }
}

void DeviceInterface::StartDeviceInner() {
  LOG_TRACE("network-device: StartDeviceInner");
  device_.Start([](void* cookie) { reinterpret_cast<DeviceInterface*>(cookie)->DeviceStarted(); },
                this);
}

void DeviceInterface::StopDevice(std::optional<TeardownState> continue_teardown) {
  LOG_TRACE("network-device: StopDevice");
  bool stop = false;
  switch (device_status_) {
    case DeviceStatus::STOPPED:
    case DeviceStatus::STOPPING:
      // Remove any pending operations we may have.
      pending_device_op_ = PendingDeviceOperation::NONE;
      break;
    case DeviceStatus::STARTING:
      // Device is currently starting, let's record that we want to stop it.
      pending_device_op_ = PendingDeviceOperation::STOP;
      break;
    case DeviceStatus::STARTED:
      // Device is in STARTED state, stop it.
      device_status_ = DeviceStatus::STOPPING;
      stop = true;
  }
  if (continue_teardown.has_value()) {
    bool did_teardown = ContinueTeardown(continue_teardown.value());
    stop = stop && !did_teardown;
  } else {
    control_lock_.Release();
  }
  if (stop) {
    StopDeviceInner();
  }
}

void DeviceInterface::StopDeviceInner() {
  LOG_TRACE("network-device: StopDeviceInner");
  device_.Stop([](void* cookie) { reinterpret_cast<DeviceInterface*>(cookie)->DeviceStopped(); },
               this);
}

PendingDeviceOperation DeviceInterface::SetDeviceStatus(DeviceStatus status) {
  auto pending_op = pending_device_op_;
  device_status_ = status;
  pending_device_op_ = PendingDeviceOperation::NONE;
  if (status == DeviceStatus::STOPPED) {
    tx_queue_->AssertParentTxLocked(*this);
    tx_queue_->AssertParentTxBuffersLocked(*this);
    bool was_full = tx_queue_->Reclaim();

    rx_queue_->AssertParentRxLocked(*this);
    rx_queue_->Reclaim();

    if (was_full) {
      NotifyTxQueueAvailable();
    }
    PruneDeadSessions();
  }
  return pending_op;
}

void DeviceInterface::DeviceStarted() {
  LOG_TRACE("network-device: DeviceStarted");
  PendingDeviceOperation pending_op;
  {
    fbl::AutoLock tx_lock(&tx_lock_);
    fbl::AutoLock tx_buff_lock(&tx_buffers_lock_);
    fbl::AutoLock rx_lock(&rx_lock_);
    control_lock_.Acquire();
    pending_op = SetDeviceStatus(DeviceStatus::STARTED);
  }
  if (pending_op == PendingDeviceOperation::STOP) {
    StopDevice();
    return;
  }
  NotifyTxQueueAvailable();
  control_lock_.Release();
  // Notify Rx queue that the device has started.
  rx_queue_->TriggerRxWatch();
}

void DeviceInterface::DeviceStopped() {
  LOG_TRACE("network-device: DeviceStopped");
  PendingDeviceOperation pending_op;
  {
    fbl::AutoLock tx_lock(&tx_lock_);
    fbl::AutoLock tx_buff_lock(&tx_buffers_lock_);
    fbl::AutoLock rx_lock(&rx_lock_);
    control_lock_.Acquire();
    pending_op = SetDeviceStatus(DeviceStatus::STOPPED);
  }

  if (ContinueTeardown(TeardownState::SESSIONS)) {
    return;
  }

  if (pending_op == PendingDeviceOperation::START) {
    StartDevice();
  }
}

bool DeviceInterface::ContinueTeardown(network::internal::DeviceInterface::TeardownState state) {
  // The teardown process goes through different phases, encoded by the TeardownState enumeration.
  // - RUNNING: no teardown is in process. We move out of the RUNNING state by calling Unbind on all
  // the DeviceInterface's bindings.
  // - BINDINGS: Waiting for all bindings to close. Only moves to next state once all bindings are
  // closed, then calls unbind on all watchers and moves to the WATCHERS state.
  // - PORTS: Waiting for all ports to teardown. Only moves to the next state once all ports are
  // destroyed, then proceeds to stop and destroy all sessions.
  // - SESSIONS: Waiting for all sessions to be closed and destroyed (dead or alive). This is the
  // final stage, once all the sessions are properly destroyed the teardown_callback_ will be
  // triggered, marking the end of the teardown process.
  //
  // To protect the linearity of the teardown process, once it has started (the state is no longer
  // RUNNING) no more bindings, watchers, or sessions can be created.

  fit::callback<void()> teardown_callback =
      [this, state]() __TA_REQUIRES(control_lock_) -> fit::callback<void()> {
    if (state != teardown_state_) {
      return nullptr;
    }
    switch (teardown_state_) {
      case TeardownState::RUNNING: {
        teardown_state_ = TeardownState::BINDINGS;
        LOGF_TRACE("network-device: Teardown state is BINDINGS (%ld bindings to destroy)",
                   bindings_.size_slow());
        if (!bindings_.is_empty()) {
          for (auto& b : bindings_) {
            b.Unbind();
          }
          return nullptr;
        }
        // Let fallthrough, no bindings to destroy.
        __FALLTHROUGH;
      }
      case TeardownState::BINDINGS: {
        // Pre-condition to enter ports state: bindings must be empty.
        if (!bindings_.is_empty()) {
          return nullptr;
        }
        teardown_state_ = TeardownState::PORTS;
        size_t port_count = 0;
        for (auto& p : ports_) {
          if (p) {
            p->Teardown();
            port_count++;
          }
        }
        LOGF_TRACE("network-device: Teardown state is PORTS (%ld ports to destroy)", port_count);
        if (port_count != 0) {
          return nullptr;
        }
        // Let it fallthrough, no ports to destroy.
        __FALLTHROUGH;
      }
      case TeardownState::PORTS: {
        // Pre-condition to enter sessions state: ports must all be destroyed.
        if (std::any_of(ports_.begin(), ports_.end(), [](const std::unique_ptr<DevicePort>& port) {
              return static_cast<bool>(port);
            })) {
          return nullptr;
        }
        teardown_state_ = TeardownState::SESSIONS;
        LOG_TRACE("network-device: Teardown state is SESSIONS");
        if (primary_session_ || !sessions_.is_empty()) {
          // If we have any sessions, signal all of them to stop their threads callback. Each
          // session that finishes operating will go through the `NotifyDeadSession` machinery. The
          // teardown is only complete when all sessions are destroyed.
          LOG_TRACE("network-device: Teardown: sessions are running, scheduling teardown");
          if (primary_session_) {
            primary_session_->Kill();
          }
          for (auto& s : sessions_) {
            s.Kill();
          }
          // We won't check for dead sessions here, since all the sessions we just called `Kill` on
          // will go into the dead state asynchronously. Any sessions that are already in the dead
          // state will also get checked in `PruneDeadSessions` at a later time.
          return nullptr;
        }
        // No sessions are alive. Now check if we have any dead sessions that are waiting to reclaim
        // buffers.
        if (!dead_sessions_.is_empty()) {
          LOG_TRACE("network-device: Teardown: dead sessions pending, waiting for teardown");
          // We need to wait for the device to safely give us all the buffers back before completing
          // the teardown.
          return nullptr;
        }
        // We can teardown immediately, let it fall through
        __FALLTHROUGH;
      }
      case TeardownState::SESSIONS: {
        // Condition to finish teardown: no more sessions exists (dead or alive) and the device
        // state is STOPPED.
        if (sessions_.is_empty() && !primary_session_ && dead_sessions_.is_empty() &&
            device_status_ == DeviceStatus::STOPPED) {
          teardown_state_ = TeardownState::FINISHED;
          LOG_TRACE("network-device: Teardown finished");
          return std::move(teardown_callback_);
        }
        LOG_TRACE("network-device: Teardown: Still pending sessions teardown");
        return nullptr;
      }
      case TeardownState::FINISHED:
        ZX_PANIC("Nothing to do if the teardown state is finished.");
    }
  }();
  control_lock_.Release();
  if (teardown_callback) {
    teardown_callback();
    return true;
  }
  return false;
}

zx::status<AttachedPort> DeviceInterface::AcquirePort(uint8_t port_id,
                                                      fbl::Span<const uint8_t> rx_frame_types) {
  return WithPort(
      port_id,
      [this, &rx_frame_types](const std::unique_ptr<DevicePort>& port) -> zx::status<AttachedPort> {
        if (!port) {
          return zx::error(ZX_ERR_NOT_FOUND);
        }
        if (std::any_of(rx_frame_types.begin(), rx_frame_types.end(), [&port](uint8_t frame_type) {
              return !port->IsValidRxFrameType(frame_type);
            })) {
          return zx::error(ZX_ERR_INVALID_ARGS);
        }
        return zx::ok(AttachedPort(this, port.get(), rx_frame_types));
      });
}

void DeviceInterface::OnPortTeardownComplete(DevicePort& port) {
  LOGF_TRACE("network-device: OnPortTeardownComplete(%d)", port.id());

  control_lock_.Acquire();
  bool stop_device = false;
  // Go over the non-primary sessions first, so we don't mess with the primary session.
  for (auto& session : sessions_) {
    session.AssertParentControlLock(*this);
    if (session.OnPortDestroyed(port.id())) {
      stop_device |= SessionStoppedInner(session);
    }
  }
  if (primary_session_) {
    primary_session_->AssertParentControlLock(*this);
    if (primary_session_->OnPortDestroyed(port.id())) {
      stop_device |= SessionStoppedInner(*primary_session_);
    }
  }
  ports_[port.id()] = nullptr;
  if (stop_device) {
    StopDevice(TeardownState::PORTS);
  } else {
    ContinueTeardown(TeardownState::PORTS);
  }
}

void DeviceInterface::ReleaseVmo(Session& session) {
  uint8_t vmo;
  vmo = session.ClearDataVmo();
  zx::status result = vmo_store_.Unregister(vmo);
  if (result.is_error()) {
    // Avoid notifying the device implementation if unregistration fails.
    // A non-ok return here means we're either attempting to double-release a VMO or the sessions
    // didn't have a registered VMO.
    LOGF_WARN("network-device(%s): Failed to unregister VMO %d: %s", session.name(), vmo,
              result.status_string());
    return;
  }

  // NB: We're calling into the device layer with the control lock held here.
  device_.ReleaseVmo(vmo);
}

fbl::RefPtr<RefCountedFifo> DeviceInterface::primary_rx_fifo() {
  SharedAutoLock lock(&control_lock_);
  if (primary_session_) {
    return primary_session_->rx_fifo();
  }
  return nullptr;
}

void DeviceInterface::NotifyTxQueueAvailable() {
  if (primary_session_) {
    primary_session_->ResumeTx();
  }
  for (auto& session : sessions_) {
    session.ResumeTx();
  }
}

void DeviceInterface::NotifyTxReturned(bool was_full) {
  SharedAutoLock lock(&control_lock_);
  if (was_full) {
    NotifyTxQueueAvailable();
  }
  PruneDeadSessions();
}

void DeviceInterface::QueueRxSpace(const rx_space_buffer_t* rx, size_t count) {
  device_.QueueRxSpace(rx, count);
}

void DeviceInterface::QueueTx(const tx_buffer_t* tx, size_t count) { device_.QueueTx(tx, count); }

void DeviceInterface::NotifyDeadSession(Session& dead_session) {
  LOGF_TRACE("network-device: NotifyDeadSession '%s'", dead_session.name());
  // First of all, stop all data-plane operations with stopped session.
  if (!dead_session.IsPaused()) {
    // Stop the session.
    SessionStopped(dead_session);
  }
  if (dead_session.IsPrimary()) {
    // Tell rx queue this session can't be used anymore.
    rx_queue_->PurgeSession(dead_session);
  }

  // Now find it in sessions and remove it.
  std::unique_ptr<Session> session_ptr;
  control_lock_.Acquire();
  if (&dead_session == primary_session_.get()) {
    // Nullify primary session.
    session_ptr = std::move(primary_session_);
    rx_queue_->TriggerSessionChanged();
  } else {
    session_ptr = sessions_.erase(dead_session);
  }

  // we can destroy the session immediately.
  if (session_ptr->ShouldDestroy()) {
    LOGF_TRACE("network-device: NotifyDeadSession '%s' destroying session", dead_session.name());
    ReleaseVmo(*session_ptr);
    session_ptr = nullptr;
    ContinueTeardown(TeardownState::SESSIONS);
    return;
  }

  // otherwise, add it to the list of dead sessions so we can wait for buffers to be returned before
  // destroying it.
  LOGF_TRACE(
      "network-device: NotifyDeadSession: session '%s' is dead, waiting for buffers to be "
      "reclaimed",
      session_ptr->name());
  dead_sessions_.push_back(std::move(session_ptr));
  control_lock_.Release();
}

void DeviceInterface::PruneDeadSessions() __TA_REQUIRES_SHARED(control_lock_) {
  auto it = dead_sessions_.begin();
  while (it != dead_sessions_.end()) {
    Session& session = *it;
    // increment iterator before erasing, because of DoublyLinkedList
    ++it;
    if (session.ShouldDestroy()) {
      // Schedule for destruction.
      //
      // Destruction must happen later because we currently hold shared access to the control lock
      // and we need an exclusive lock to erase items from the dead sessions list.
      //
      // ShouldDestroy should only return true once in the lifetime of a session, which guarantees
      // that postponing the destruction on the dispatcher is always safe.
      async::PostTask(dispatcher_, [&session, this]() {
        fbl::AutoLock lock(&control_lock_);
        LOGF_TRACE("network-device: PruneDeadSessions: destroying %s", session.name());
        ReleaseVmo(session);
        dead_sessions_.erase(session);
      });
    } else {
      LOGF_TRACE("network-device: PruneDeadSessions: %s still pending", session.name());
    }
  }
}

zx::status<std::pair<uint8_t, DataVmoStore::StoredVmo*>> DeviceInterface::RegisterDataVmo(
    zx::vmo vmo) __TA_REQUIRES(control_lock_) {
  if (vmo_store_.is_full()) {
    return zx::error(ZX_ERR_NO_RESOURCES);
  }
  // Duplicate the VMO to share with device implementation.
  zx::vmo device_vmo;
  if (zx_status_t status = vmo.duplicate(ZX_RIGHT_SAME_RIGHTS, &device_vmo); status != ZX_OK) {
    return zx::error(status);
  }

  zx::status registration = vmo_store_.Register(std::move(vmo));
  if (registration.is_error()) {
    return registration.take_error();
  }
  uint8_t id = registration.value();
  DataVmoStore::StoredVmo* stored_vmo = vmo_store_.GetVmo(id);

  // NB: We're calling into the device implementation here while holding the control lock
  // exclusively which we generally try to avoid in case the device wants to call back into us.
  // Furthermore, `PrepareVmo` should have a response so that we can wait for the device to do its
  // registration before we start sending it buffers with that VMO id.
  // Irrelevant right now because this is a synchronous call.
  // TODO(https://fxbug.dev/75456): We should wait until PrepareVmo returns (possibly
  // asynchronously) before allowing the session to run.
  device_.PrepareVmo(id, std::move(device_vmo));

  return zx::ok(std::make_pair(id, stored_vmo));
}

void DeviceInterface::CommitAllSessions() {
  if (primary_session_) {
    primary_session_->AssertParentRxLock(*this);
    primary_session_->CommitRx();
  }
  for (auto& session : sessions_) {
    session.AssertParentRxLock(*this);
    session.CommitRx();
  }
  PruneDeadSessions();
}

void DeviceInterface::CopySessionData(const Session& owner, uint16_t owner_index,
                                      const rx_buffer_t* buff) {
  if (primary_session_ && primary_session_.get() != &owner) {
    primary_session_->AssertParentRxLock(*this);
    primary_session_->AssertParentControlLockShared(*this);
    primary_session_->CompleteRxWith(owner, owner_index, buff);
  }

  for (auto& session : sessions_) {
    if (&session != &owner) {
      session.AssertParentRxLock(*this);
      session.AssertParentControlLockShared(*this);
      session.CompleteRxWith(owner, owner_index, buff);
    }
  }
}

void DeviceInterface::ListenSessionData(const Session& owner,
                                        fbl::Span<const uint16_t> descriptors) {
  if ((device_info_.device_features & FEATURE_NO_AUTO_SNOOP) ||
      !has_listen_sessions_.load(std::memory_order_relaxed)) {
    // Avoid walking through sessions and acquiring Rx lock if we know no listen sessions are
    // attached.
    return;
  }
  fbl::AutoLock rx_lock(&rx_lock_);
  SharedAutoLock control(&control_lock_);
  bool copied = false;
  for (const uint16_t& descriptor : descriptors) {
    if (primary_session_ && primary_session_.get() != &owner && primary_session_->IsListen()) {
      primary_session_->AssertParentRxLock(*this);
      copied |= primary_session_->ListenFromTx(owner, descriptor);
    }
    for (auto& s : sessions_) {
      if (&s != &owner && s.IsListen()) {
        s.AssertParentRxLock(*this);
        copied |= s.ListenFromTx(owner, descriptor);
      }
    }
  }
  if (copied) {
    CommitAllSessions();
  }
}

zx_status_t DeviceInterface::LoadRxDescriptors(RxSessionTransaction& transact) {
  SharedAutoLock lock(&control_lock_);
  if (!primary_session_) {
    return ZX_ERR_BAD_STATE;
  }
  return primary_session_->LoadRxDescriptors(transact);
}

bool DeviceInterface::IsDataPlaneOpen() {
  SharedAutoLock lock(&control_lock_);
  return device_status_ == DeviceStatus::STARTED;
}

DeviceInterface::DeviceInterface(async_dispatcher_t* dispatcher,
                                 ddk::NetworkDeviceImplProtocolClient parent)
    : dispatcher_(dispatcher),
      device_(parent),
      vmo_store_(vmo_store::Options{
          vmo_store::MapOptions{ZX_VM_PERM_READ | ZX_VM_PERM_WRITE | ZX_VM_REQUIRE_NON_RESIZABLE,
                                nullptr},
          std::nullopt}) {}

zx_status_t DeviceInterface::Binding::Bind(DeviceInterface* interface,
                                           fidl::ServerEnd<netdev::Device> channel) {
  fbl::AllocChecker ac;
  std::unique_ptr<Binding> binding(new (&ac) Binding);
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  auto* binding_ptr = binding.get();
  binding->binding_ =
      fidl::BindServer(interface->dispatcher_, std::move(channel), interface,
                       [binding_ptr](DeviceInterface* interface, fidl::UnbindInfo /*unused*/,
                                     fidl::ServerEnd<fuchsia_hardware_network::Device> /*unused*/) {
                         bool bindings_empty;
                         interface->control_lock_.Acquire();
                         interface->bindings_.erase(*binding_ptr);
                         bindings_empty = interface->bindings_.is_empty();
                         if (bindings_empty) {
                           interface->ContinueTeardown(TeardownState::BINDINGS);
                         } else {
                           interface->control_lock_.Release();
                         }
                       });
  interface->bindings_.push_front(std::move(binding));
  return ZX_OK;
}

void DeviceInterface::Binding::Unbind() {
  auto binding = std::move(binding_);
  if (binding.has_value()) {
    binding->Unbind();
  }
}

}  // namespace internal
}  // namespace network
