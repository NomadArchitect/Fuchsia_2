// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_DEVICE_SESSION_H_
#define SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_DEVICE_SESSION_H_

#include <fuchsia/hardware/network/device/cpp/banjo.h>
#include <lib/async/dispatcher.h>
#include <lib/fidl/llcpp/server.h>
#include <lib/fzl/pinned-vmo.h>
#include <lib/fzl/vmo-mapper.h>
#include <lib/zx/event.h>
#include <lib/zx/fifo.h>
#include <lib/zx/vmo.h>
#include <threads.h>
#include <zircon/device/network.h>

#include <fbl/array.h>
#include <fbl/intrusive_double_list.h>
#include <fbl/ref_counted.h>
#include <fbl/span.h>

#include "data_structs.h"
#include "definitions.h"
#include "device_interface.h"
#include "device_port.h"
#include "locks.h"
#include "rx_queue.h"

namespace network::internal {

// A device port attached to a session.
//
// This class provides safe access to device ports owned by a DeviceInterface.
class AttachedPort {
 public:
  // Helper function with TA annotations that bridges the gap between parent's locks and local
  // locking requirements; TA is not otherwise able to tell that the |parent| and |parent_| are the
  // same entity.
  void AssertParentControlLockShared(DeviceInterface& parent)
      __TA_ASSERT_SHARED(parent_->control_lock()) __TA_REQUIRES_SHARED(parent.control_lock()) {
    ZX_DEBUG_ASSERT(parent_ == &parent);
  }

  // Calls provided function f with the attached port.
  template <typename F>
  auto WithPort(F f) __TA_REQUIRES_SHARED(parent_->control_lock()) {
    return f(*port_);
  }

  // Returns the Rx frame types we're subscribed to on this attached port.
  fbl::Span<const uint8_t> frame_types() const {
    return fbl::Span(frame_types_.begin(), frame_type_count_);
  }

 protected:
  friend DeviceInterface;

  AttachedPort(DeviceInterface* parent, DevicePort* port, fbl::Span<const uint8_t> frame_types)
      : parent_(parent),
        port_(port),
        frame_types_([&frame_types]() {
          decltype(frame_types_) t;
          ZX_ASSERT(frame_types.size() <= t.size());
          std::copy(frame_types.begin(), frame_types.end(), t.begin());
          return t;
        }()),
        frame_type_count_(static_cast<uint32_t>(frame_types.size())) {}

 private:
  // NB: Fields can't be const because we want AttachedPort to allow assignment operator.
  // Attached parent pointer, not owned;
  DeviceInterface* parent_ = nullptr;
  // Attached port pointer, not owned;
  DevicePort* port_ = nullptr;
  std::array<uint8_t, netdev::wire::kMaxFrameTypes> frame_types_{};
  uint32_t frame_type_count_ = 0;
};

// A client session with a network device interface.
//
// Session will spawn a thread that will handle the fuchsia.hardware.network.Session FIDL control
// plane calls and service the Tx FIFO associated with the client session.
//
// It is invalid to destroy a Session that has outstanding buffers, that is, buffers that are
// currently owned by the interface's Rx or Tx queues.
class Session : public fbl::DoublyLinkedListable<std::unique_ptr<Session>>,
                public fidl::WireServer<netdev::Session> {
 public:
  ~Session() override;
  // Creates a new session with the provided parameters.
  //
  // The session will service fuchsia.hardware.network.Session FIDL calls on the provided `control`
  // channel.
  //
  // All control plane calls are operated on the provided `dispatcher`, and a dedicated thread will
  // be spawned to handle data fast path operations (tx data plane).
  //
  // Returns the session and its data path FIFOs.
  static zx::status<std::pair<std::unique_ptr<Session>, netdev::wire::Fifos>> Create(
      async_dispatcher_t* dispatcher, netdev::wire::SessionInfo& info, fidl::StringView name,
      DeviceInterface* parent, fidl::ServerEnd<netdev::Session> control);
  bool IsPrimary() const;
  bool IsListen() const;
  bool IsPaused() const;
  // Checks if this session is eligible to take over the primary session from `current_primary`.
  // `current_primary` can be null, meaning there's no current primary session.
  bool ShouldTakeOverPrimary(const Session* current_primary) const;

  // Helper functions with TA annotations that bridges the gap between parent's locks and local
  // locking requirements; TA is not otherwise able to tell that the |parent| and |parent_| are the
  // same entity.
  void AssertParentControlLock(DeviceInterface& parent) __TA_ASSERT(parent_->control_lock())
      __TA_REQUIRES(parent.control_lock()) {
    ZX_DEBUG_ASSERT(parent_ == &parent);
  }
  void AssertParentControlLockShared(DeviceInterface& parent)
      __TA_ASSERT_SHARED(parent_->control_lock(), parent.control_lock())
          __TA_REQUIRES_SHARED(parent.control_lock()) {
    ZX_DEBUG_ASSERT(parent_ == &parent);
  }

  // FIDL interface implementation:
  void SetPaused(SetPausedRequestView request, SetPausedCompleter::Sync& _completer) override;
  void Close(CloseRequestView request, CloseCompleter::Sync& _completer) override;

  zx_status_t AttachPort(uint8_t port_id, fbl::Span<const uint8_t> frame_types);
  zx_status_t DetachPort(uint8_t port_id);

  // Sets the return code for a tx descriptor.
  void MarkTxReturnResult(uint16_t descriptor, zx_status_t status);
  // Returns tx descriptors to the session client.
  void ReturnTxDescriptors(const uint16_t* descriptors, uint32_t count);
  // Signals the session thread to observe the tx FIFO object.
  void ResumeTx();
  // Signals the session thread to stop servicing the session channel and FIFOs.
  // When the session thread is finished, it notifies the DeviceInterface parent through
  // `NotifyDeadSession`.
  void Kill();

  // Loads rx descriptors into the provided session transaction, fetching more from the rx FIFO if
  // needed.
  zx_status_t LoadRxDescriptors(RxQueue::SessionTransaction& transact);
  // Sets the data in the space buffer `buff` to region described by `descriptor_index`.
  zx_status_t FillRxSpace(uint16_t descriptor_index, rx_space_buffer_t* buff);
  // Completes rx for `descriptor_index`. Returns true if the buffer can be immediately reused.
  bool CompleteRx(uint16_t descriptor_index, const rx_buffer_t* buff)
      __TA_REQUIRES_SHARED(parent_->control_lock());
  // Completes rx by copying the data from another session into one of this session's available rx
  // buffers.
  void CompleteRxWith(const Session& owner, uint16_t owner_index, const rx_buffer_t* buff)
      __TA_REQUIRES_SHARED(parent_->control_lock());
  // Copies data from a tx frame from another session into one of this session's available rx
  // buffers.
  bool ListenFromTx(const Session& owner, uint16_t owner_index);
  // Commits pending rx buffers, sending them back to the session client.
  void CommitRx();
  // Returns true iff the session is subscribed to frame_type on port.
  bool IsSubscribedToFrameType(uint8_t port, uint8_t frame_type)
      __TA_REQUIRES_SHARED(parent_->control_lock());

  inline void TxTaken() { in_flight_tx_++; }
  inline void TxReturned() { ZX_ASSERT(in_flight_tx_-- != 0); }
  inline void RxTaken() { in_flight_rx_++; }
  inline bool RxReturned() {
    ZX_ASSERT(in_flight_rx_-- != 0);
    return rx_valid_;
  }
  inline void StopRx() { rx_valid_ = false; }
  [[nodiscard]] inline bool ShouldDestroy() {
    if (in_flight_rx_ == 0 && in_flight_tx_ == 0) {
      bool expect = false;
      // Only ever return true for ShouldDestroy once so the caller can schedule destruction
      // asynchronously after ShouldDestroy returns true and have a guarantee that it won't be
      // possible to schedule destruction for the same object twice.
      return scheduled_destruction_.compare_exchange_strong(expect, true);
    }
    return false;
  }

  const fbl::RefPtr<RefCountedFifo>& rx_fifo() { return fifo_rx_; }
  const char* name() const { return name_.data(); }

 protected:
  friend DeviceInterface;
  // Notifies session of port destruction.
  //
  // Returns true iff the session should be stopped after detaching from the port.
  bool OnPortDestroyed(uint8_t port_id) __TA_REQUIRES(parent_->control_lock());
  // Sets the internal references to the data VMO.
  //
  // Must only be called when the Session hasn't yet been allocated a VMO id, will abort otherwise.
  void SetDataVmo(uint8_t vmo_id, DataVmoStore::StoredVmo* vmo);
  // Clears internal references to data VMO, returning the vmo_id that was associated with this
  // session.
  uint8_t ClearDataVmo();

 private:
  Session(async_dispatcher_t* dispatcher, netdev::wire::SessionInfo& info, fidl::StringView name,
          DeviceInterface* parent);
  zx::status<netdev::wire::Fifos> Init();
  void Bind(fidl::ServerEnd<netdev::Session> channel);
  void StopTxThread();
  void OnUnbind(fidl::UnbindInfo::Reason reason, fidl::ServerEnd<netdev::Session> channel);
  int Thread();

  // Detaches a port from the session.
  //
  // Returns zx::ok(true) if the session was attached to the port and the session transitioned to
  // paused.
  // Returns zx::ok(false) if the session was attached to the port but it remains running attached
  // to other ports.
  // Returns zx::error otherwise.
  zx::status<bool> DetachPortLocked(uint8_t port_id) __TA_REQUIRES(parent_->control_lock());

  // Fetch tx descriptors from the FIFO and queue them in the parent `DeviceInterface`'s TxQueue.
  zx_status_t FetchTx();
  buffer_descriptor_t* descriptor(uint16_t index);

  const buffer_descriptor_t* descriptor(uint16_t index) const;
  fbl::Span<uint8_t> data_at(uint64_t offset, uint64_t len) const;
  // Loads a completed rx buffer information back into the descriptor with the provided index.
  zx_status_t LoadRxInfo(uint16_t descriptor_index, const rx_buffer_t* buff);
  // Loads all rx descriptors that are already available into the given transaction.
  bool LoadAvailableRxDescriptors(RxQueue::SessionTransaction& transact);
  // Fetches rx descriptors from the rx FIFO.
  zx_status_t FetchRxDescriptors();

  async_dispatcher_t* const dispatcher_;
  const std::array<char, netdev::wire::kMaxSessionName + 1> name_;
  // `MAX_VMOS` is used as a marker for invalid VMO identifier.
  // The destructor checks that vmo_id is set to `MAX_VMOS`, which verifies that `ReleaseDataVmo`
  // was called before destruction.
  uint8_t vmo_id_ = MAX_VMOS;
  // Unowned pointer to data VMO stored in DeviceInterface.
  // Set by Session::Create.
  DataVmoStore::StoredVmo* data_vmo_ = nullptr;
  zx::port tx_port_;
  std::optional<fidl::ServerBindingRef<netdev::Session>> binding_;
  // The control channel is only set by the session teardown process if an epitaph must be sent when
  // all the buffers are properly reclaimed. It is set to the channel that was previously bound in
  // the `binding_` Server.
  std::optional<fidl::ServerEnd<netdev::Session>> control_channel_;
  const zx::vmo vmo_descriptors_;
  fzl::VmoMapper descriptors_;
  fbl::RefPtr<RefCountedFifo> fifo_rx_;
  zx::fifo fifo_tx_;
  std::atomic<bool> paused_;
  const uint16_t descriptor_count_;
  const uint64_t descriptor_length_;
  const netdev::wire::SessionFlags flags_;
  const std::array<uint8_t, netdev::wire::kMaxFrameTypes> frame_types_;
  const uint32_t frame_type_count_;

  // AttachedPorts information. Parent device is responsible for detaching ports from sessions
  // before destroying them.
  std::array<std::optional<AttachedPort>, MAX_PORTS> attached_ports_
      __TA_GUARDED(parent_->control_lock());
  // Pointer to parent network device, not owned.
  DeviceInterface* const parent_;
  std::optional<thrd_t> thread_;
  std::unique_ptr<uint16_t[]> rx_return_queue_;
  size_t rx_return_queue_count_ = 0;
  std::unique_ptr<uint16_t[]> rx_avail_queue_;
  size_t rx_avail_queue_count_ = 0;

  std::atomic<size_t> in_flight_tx_ = 0;
  std::atomic<size_t> in_flight_rx_ = 0;
  std::atomic<bool> scheduled_destruction_ = false;
  std::atomic<bool> rx_valid_ = true;

  DISALLOW_COPY_ASSIGN_AND_MOVE(Session);
};

}  // namespace network::internal

#endif  // SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_DEVICE_SESSION_H_
