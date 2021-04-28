// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_DEVICE_DEVICE_INTERFACE_H_
#define SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_DEVICE_DEVICE_INTERFACE_H_

#include <fuchsia/hardware/network/device/cpp/banjo.h>
#include <lib/async/dispatcher.h>
#include <lib/fit/function.h>

#include "data_structs.h"
#include "definitions.h"
#include "device_port.h"
#include "locks.h"
#include "public/network_device.h"
#include "status_watcher.h"

namespace network::internal {
class RxQueue;
class RxSessionTransaction;

class TxQueue;

class Session;
class AttachedPort;
using SessionList = fbl::DoublyLinkedList<std::unique_ptr<Session>>;

struct RefCountedFifo : public fbl::RefCounted<RefCountedFifo> {
  zx::fifo fifo;
};

enum class DeviceStatus { STARTING, STARTED, STOPPING, STOPPED };

enum class PendingDeviceOperation { NONE, START, STOP };

class DeviceInterface;

class DeviceInterface : public fidl::WireServer<netdev::Device>,
                        public ddk::NetworkDeviceIfcProtocol<DeviceInterface>,
                        public ::network::NetworkDeviceInterface {
 public:
  // TODO(http://fxbug.dev/64310): Delete this constant once FIDL supports ports and we're not
  // hard-coding port number 0 as the "default port".
  static constexpr uint8_t kPort0 = 0;

  static zx::status<std::unique_ptr<DeviceInterface>> Create(
      async_dispatcher_t* dispatcher, ddk::NetworkDeviceImplProtocolClient parent,
      const char* parent_name);
  ~DeviceInterface() override;

  // Public NetworkDevice API.
  void Teardown(fit::callback<void()> callback) override;
  zx_status_t Bind(fidl::ServerEnd<netdev::Device> req) override;
  zx_status_t BindMac(fidl::ServerEnd<netdev::MacAddressing> req) override;

  // NetworkDevice interface implementation.
  void NetworkDeviceIfcPortStatusChanged(uint8_t port_id, const port_status_t* new_status);
  void NetworkDeviceIfcAddPort(uint8_t port_id, const network_port_protocol_t* port);
  void NetworkDeviceIfcRemovePort(uint8_t port_id);
  void NetworkDeviceIfcCompleteRx(const rx_buffer_t* rx_list, size_t rx_count);
  void NetworkDeviceIfcCompleteTx(const tx_result_t* tx_list, size_t tx_count);
  void NetworkDeviceIfcSnoop(const rx_buffer_t* rx_list, size_t rx_count);

  uint16_t rx_fifo_depth() const;
  uint16_t tx_fifo_depth() const;

  // Returns the device-owned buffer count threshold at which we should trigger RxQueue work. If the
  // number of buffers on device is less than or equal to the threshold, we should attempt to fetch
  // more buffers.
  uint16_t rx_notify_threshold() const { return device_info_.rx_threshold; }

  TxQueue& tx_queue() { return *tx_queue_; }

  SharedLock& control_lock() __TA_RETURN_CAPABILITY(control_lock_) { return control_lock_; }
  fbl::Mutex& rx_lock() __TA_RETURN_CAPABILITY(rx_lock_) { return rx_lock_; }
  fbl::Mutex& tx_lock() __TA_RETURN_CAPABILITY(tx_lock_) { return tx_lock_; }
  fbl::Mutex& tx_buffers_lock() __TA_RETURN_CAPABILITY(tx_buffers_lock_) {
    return tx_buffers_lock_;
  }

  const device_info_t& info() { return device_info_; }

  // Loads rx path descriptors from the primary session into a session transaction.
  zx_status_t LoadRxDescriptors(RxSessionTransaction& transact);

  // Operates workflow for when a session is started. If the session is eligible to take over the
  // primary spot, it'll be elected the new primary session. If there was no primary session before,
  // the data path will be started BEFORE the new session is elected as primary,
  void SessionStarted(Session& session);
  // Operates workflow for when a session is stopped. If there's another session that is eligible to
  // take over the primary spot, it'll be elected the new primary session. Otherwise, the data path
  // will be stopped.
  void SessionStopped(Session& session);

  // If a primary session exists, primary_rx_fifo returns a reference-counted pointer to the primary
  // session's Rx FIFO. Otherwise, the returned pointer is null.
  fbl::RefPtr<RefCountedFifo> primary_rx_fifo();

  // Commits all pending rx buffers in all active sessions.
  void CommitAllSessions() __TA_REQUIRES_SHARED(control_lock_);
  // Copies the received data described by `buff` to all sessions other than `owner`.
  void CopySessionData(const Session& owner, uint16_t owner_index, const rx_buffer_t* buff)
      __TA_REQUIRES_SHARED(control_lock_);
  // Notifies all listening sessions of a new tx transaction from session `owner` and descriptor
  // `owner_index`. Returns true if any listening sessions copied the data.
  bool ListenSessionData(const Session& owner, uint16_t owner_index)
      __TA_REQUIRES_SHARED(control_lock_);

  // Notifies that a batch of Tx frames has been returned.
  //
  // If was_full is true, all active sessions are notified that device tx space has freed up.
  // Checks if dead sessions are ready to be destroyed due to buffers returning.
  void NotifyTxReturned(bool was_full);
  // Sends the provided space buffers in `rx` to the device implementation.
  void QueueRxSpace(const rx_space_buffer_t* rx, size_t count);
  // Sends the provided transmit buffers in `rx` to the device implementation.
  void QueueTx(const tx_buffer_t* tx, size_t count);
  bool IsDataPlaneOpen();

  // Called by sessions when they're no longer running. If the dead session has any outstanding
  // buffers with the device implementation, it'll be kept in `dead_sessions_` until all the buffers
  // are safely returned and we own all the buffers again.
  void NotifyDeadSession(Session& dead_session);

  // Registers `vmo` as a data vmo that will be shared with the device implementation.
  //
  // Returns the generated identifier and an unowned pointer to the vmo holder.
  zx::status<std::pair<uint8_t, DataVmoStore::StoredVmo*>> RegisterDataVmo(zx::vmo vmo)
      __TA_REQUIRES(control_lock_);

  // Fidl protocol implementation.
  void GetInfo(GetInfoRequestView request, GetInfoCompleter::Sync& completer) override;
  void GetStatus(GetStatusRequestView request, GetStatusCompleter::Sync& completer) override;
  void OpenSession(OpenSessionRequestView request, OpenSessionCompleter::Sync& completer) override;
  void GetStatusWatcher(GetStatusWatcherRequestView request,
                        GetStatusWatcherCompleter::Sync& completer) override;

  // Serves the OpenSession FIDL handle method synchronously.
  zx::status<netdev::wire::DeviceOpenSessionResponse> OpenSession(
      fidl::StringView name, netdev::wire::SessionInfo session_info);

 protected:
  friend Session;
  // Acquires a port for use in a Session.
  //
  // Sessions are notified of ports that are no longer safe to use by the DeviceInterface through
  // Session::DetachPort.
  //
  // NB: The validity of the returned AttachedPort is not really guaranteed by the type system, but
  // by the fact that DeviceInterface will detach all ports from sessions before continuing.
  zx::status<AttachedPort> AcquirePort(uint8_t port_id, fbl::Span<const uint8_t> rx_frame_types)
      __TA_REQUIRES(control_lock_);

 private:
  // Helper class to keep track of clients bound to DeviceInterface.
  class Binding : public fbl::DoublyLinkedListable<std::unique_ptr<Binding>> {
   public:
    static zx_status_t Bind(DeviceInterface* interface, fidl::ServerEnd<netdev::Device> channel)
        __TA_REQUIRES(interface->control_lock_);
    void Unbind();

   private:
    Binding() = default;
    std::optional<fidl::ServerBindingRef<netdev::Device>> binding_;
  };
  using BindingList = fbl::DoublyLinkedList<std::unique_ptr<Binding>>;

  enum class TeardownState { RUNNING, BINDINGS, PORTS, SESSIONS, FINISHED };

  explicit DeviceInterface(async_dispatcher_t* dispatcher,
                           ddk::NetworkDeviceImplProtocolClient parent);
  zx_status_t Init(const char* parent_name);

  // Starts the data path with the device implementation.
  void StartDevice() __TA_EXCLUDES(control_lock_, tx_lock_, rx_lock_);
  // Stops the data path with the device implementation.
  //
  // If continue_teardown is provided, teardown continuation will be attempted before notifying the
  // underlying device of stoppage.
  void StopDevice(std::optional<TeardownState> continue_teardown = std::nullopt)
      __TA_RELEASE(control_lock_) __TA_EXCLUDES(tx_lock_, rx_lock_);
  // Starts the device implementation with `DeviceStarted` as its callback.
  void StartDeviceInner() __TA_EXCLUDES(control_lock_);
  // Stops the device implementation with `DeviceStopped` as its callback.
  void StopDeviceInner() __TA_EXCLUDES(control_lock_);
  // Helper inner function to stop a session.
  //
  // Returns true if the device should be stopped.
  bool SessionStoppedInner(Session& session) __TA_REQUIRES(control_lock_);

  // Callback given to the device implementation for the `Start` call. The data path is considered
  // open only once the device is started.
  void DeviceStarted();
  // Callback given to the device implementation for the `Stop` call. All outstanding buffers are
  // automatically reclaimed once the device is considered stopped. If a teardown is pending,
  // `DeviceStopped` will complete the teardown BEFORE all buffers are reclaimed and all the
  // sessions are destroyed.
  void DeviceStopped();

  PendingDeviceOperation SetDeviceStatus(DeviceStatus status)
      __TA_REQUIRES(control_lock_, tx_lock_, rx_lock_, tx_buffers_lock_);

  // Notifies the device implementation that the VMO used by the provided session will no longer be
  // used. It is called right before sessions are destroyed.
  // ReleaseVMO acquires the vmos_lock_ internally, so we mark it as excluding the vmos_lock_.
  void ReleaseVmo(Session& session) __TA_REQUIRES(control_lock_);

  // Continues a teardown process, if one is running.
  //
  // The provided state is the expected state that the teardown process is in. If the given state is
  // not the current teardown state, no processing will happen. Otherwise, the teardown process will
  // continue if the pre-conditions to move between teardown states are met.
  //
  // Returns true if the teardown is completed and execution should be stopped.
  // ContinueTeardown is marked with many thread analysis lock exclusions so it can acquire those
  // locks internally and evaluate the teardown progress.
  bool ContinueTeardown(TeardownState state) __TA_RELEASE(control_lock_)
      __TA_EXCLUDES(tx_lock_, tx_buffers_lock_, rx_lock_);

  // Calls f with a const std::unique_ptr<DevicePort>& to the DevicePort referenced by port_id
  // or nullptr if no ports with that id are installed.
  //
  // Returns the value returned by the call to f.
  //
  // It is unsafe to use the provided DevicePort outside of the scope of the callback f.
  template <typename F>
  auto WithPort(uint8_t port_id, F f) __TA_REQUIRES_SHARED(control_lock_) {
    if (port_id >= ports_.size()) {
      const std::unique_ptr<DevicePort> null_port;
      return f(null_port);
    }
    return f(ports_[port_id]);
  }
  void OnPortTeardownComplete(DevicePort& port);

  // Destroys all dead sessions that report they can be destroyed through `Session::CanDestroy`.
  void PruneDeadSessions() __TA_REQUIRES_SHARED(control_lock_);
  // Notifies all sessions that the transmit queue has available spots to take in transmit frames.
  void NotifyTxQueueAvailable() __TA_REQUIRES_SHARED(control_lock_);

  // Immutable information BEFORE initialization:
  device_info_t device_info_{};
  // dispatcher used for slow-path operations:
  async_dispatcher_t* const dispatcher_;
  const ddk::NetworkDeviceImplProtocolClient device_;
  std::array<uint8_t, netdev::wire::kMaxAccelFlags> accel_rx_{};
  std::array<uint8_t, netdev::wire::kMaxAccelFlags> accel_tx_{};

  std::unique_ptr<Session> primary_session_ __TA_GUARDED(control_lock_);
  SessionList sessions_ __TA_GUARDED(control_lock_);
  uint32_t active_primary_sessions_ __TA_GUARDED(control_lock_) = 0;

  std::array<std::unique_ptr<DevicePort>, MAX_PORTS> ports_ __TA_GUARDED(control_lock_);

  SessionList dead_sessions_ __TA_GUARDED(control_lock_);

  // We don't need to keep any data associated with the VMO ids, we use the slab to guarantee
  // non-overlapping unique identifiers within a set of valid IDs.
  DataVmoStore vmo_store_ __TA_GUARDED(control_lock_);
  BindingList bindings_ __TA_GUARDED(control_lock_);

  TeardownState teardown_state_ __TA_GUARDED(control_lock_) = TeardownState::RUNNING;
  fit::callback<void()> teardown_callback_ __TA_GUARDED(control_lock_);

  PendingDeviceOperation pending_device_op_ = PendingDeviceOperation::NONE;
  std::atomic_bool has_listen_sessions_ = false;

  std::unique_ptr<TxQueue> tx_queue_;
  std::unique_ptr<RxQueue> rx_queue_;

  DeviceStatus device_status_ __TA_GUARDED(control_lock_) = DeviceStatus::STOPPED;

  fbl::Mutex rx_lock_ __TA_ACQUIRED_BEFORE(control_lock_, tx_lock_);
  fbl::Mutex tx_lock_ __TA_ACQUIRED_BEFORE(control_lock_);
  fbl::Mutex tx_buffers_lock_ __TA_ACQUIRED_AFTER(tx_lock_) __TA_ACQUIRED_BEFORE(control_lock_);
  SharedLock control_lock_;

 public:
  // Event hooks used in tests:
  fit::function<void(const char*)> evt_session_started;
  fit::function<void(uint64_t)> evt_rx_queue_packet;

  // Unsafe accessors used in tests:
  const SessionList& sessions_unsafe() const { return sessions_; }
};

}  // namespace network::internal

#endif  // SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_DEVICE_DEVICE_INTERFACE_H_
