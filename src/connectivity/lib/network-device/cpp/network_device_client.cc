// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "network_device_client.h"

#include <fcntl.h>
#include <lib/async/default.h>
#include <lib/fit/bridge.h>
#include <lib/fit/promise.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/time.h>
#include <zircon/device/network.h>
#include <zircon/status.h>

namespace network {
namespace client {

namespace {
// The buffer length used by `DefaultSessionConfig`.
constexpr uint32_t kDefaultBufferLength = 2048;
// The maximum FIFO depth that this client can handle.
// Set to the maximum number of `uint16`s that a zx FIFO can hold.
constexpr uint64_t kMaxDepth = ZX_PAGE_SIZE / sizeof(uint16_t);

constexpr zx_signals_t kFifoWaitReads = ZX_FIFO_READABLE | ZX_FIFO_PEER_CLOSED;
constexpr zx_signals_t kFifoWaitWrites = ZX_FIFO_WRITABLE;
}  // namespace

NetworkDeviceClient::NetworkDeviceClient(fidl::ClientEnd<netdev::Device> handle,
                                         async_dispatcher_t* dispatcher)
    : dispatcher_([dispatcher]() {
        if (dispatcher != nullptr) {
          return dispatcher;
        }
        return async_get_default_dispatcher();
      }()),
      device_handler_(std::make_shared<EventHandler<netdev::Device>>([this](fidl::UnbindInfo info) {
        if (info.status != ZX_OK) {
          FX_LOGS(ERROR) << "device handler error " << zx_status_get_string(info.status);
          ErrorTeardown(info.status);
        }
      })),
      device_(std::move(handle), dispatcher_, device_handler_),
      session_handler_(
          std::make_shared<EventHandler<netdev::Session>>([this](fidl::UnbindInfo info) {
            if (info.status != ZX_OK) {
              FX_LOGS(ERROR) << "session handler error " << zx_status_get_string(info.status);
              ErrorTeardown(info.status);
            }
          })),
      executor_(std::make_unique<async::Executor>(dispatcher_)) {}

NetworkDeviceClient::~NetworkDeviceClient() {
  device_handler_->Cancel();
  device_ = {};
  session_handler_->Cancel();
  session_ = {};
}

SessionConfig NetworkDeviceClient::DefaultSessionConfig(const netdev::wire::Info& dev_info) {
  const uint32_t buffer_length = std::min(kDefaultBufferLength, dev_info.max_buffer_length);
  SessionConfig config = {
      .buffer_length = buffer_length,
      .buffer_stride = buffer_length,
      .descriptor_length = sizeof(buffer_descriptor_t),
      .rx_descriptor_count = dev_info.rx_depth,
      .tx_descriptor_count = dev_info.tx_depth,
      .options = netdev::wire::SessionFlags::kPrimary,
  };
  std::copy(std::begin(dev_info.rx_types), std::end(dev_info.rx_types),
            std::back_inserter(config.rx_frames));
  if (config.buffer_stride % dev_info.buffer_alignment != 0) {
    // align back:
    config.buffer_stride -= (config.buffer_stride % dev_info.buffer_alignment);
    // align up if we have space:
    if (config.buffer_stride + dev_info.buffer_alignment <= dev_info.max_buffer_length) {
      config.buffer_stride += dev_info.buffer_alignment;
    }
  }
  return config;
}

void NetworkDeviceClient::OpenSession(const std::string& name,
                                      NetworkDeviceClient::OpenSessionCallback callback,
                                      NetworkDeviceClient::SessionConfigFactory config_factory) {
  if (session_running_) {
    callback(ZX_ERR_ALREADY_EXISTS);
    return;
  }
  session_running_ = true;
  fit::bridge<fuchsia_hardware_network::wire::Info, void> bridge;
  device_->GetInfo([res = std::move(bridge.completer)](
                       fidl::WireResponse<netdev::Device::GetInfo>* response) mutable {
    res.complete_ok(response->info);
  });
  auto prom =
      bridge.consumer.promise()
          .or_else([]() { return fit::error(ZX_ERR_INTERNAL); })
          .and_then(
              [this, cfg = std::move(config_factory)](fuchsia_hardware_network::wire::Info& info)
                  -> fit::result<netdev::wire::SessionInfo, zx_status_t> {
                session_config_ = cfg(info);
                device_info_ = info;
                zx_status_t status;
                if ((status = PrepareSession()) != ZX_OK) {
                  return fit::error(status);
                }
                return MakeSessionInfo();
              })
          .and_then([this, name](netdev::wire::SessionInfo& sessionInfo) {
            fit::bridge<void, zx_status_t> bridge;
            device_->OpenSession(
                fidl::StringView::FromExternal(name), std::move(sessionInfo),
                [this, res = std::move(bridge.completer)](
                    fidl::WireResponse<netdev::Device::OpenSession>* response) mutable {
                  netdev::wire::DeviceOpenSessionResult& result = response->result;
                  switch (result.which()) {
                    case netdev::wire::DeviceOpenSessionResult::Tag::kErr:
                      res.complete_error(result.err());
                      break;
                    case netdev::wire::DeviceOpenSessionResult::Tag::kResponse:
                      netdev::wire::DeviceOpenSessionResponse& response = result.mutable_response();
                      session_.Bind(std::move(response.session), dispatcher_, session_handler_);
                      rx_fifo_ = std::move(response.fifos.rx);
                      tx_fifo_ = std::move(response.fifos.tx);
                      res.complete_ok();
                      break;
                  }
                });
            return bridge.consumer.promise();
          })
          .and_then([this]() -> fit::result<void, zx_status_t> {
            zx_status_t status;
            if ((status = PrepareDescriptors()) != ZX_OK) {
              return fit::error(status);
            } else {
              return fit::ok();
            }
          })
          .then([this, cb = std::move(callback)](fit::result<void, zx_status_t>& result) {
            if (result.is_ok()) {
              cb(ZX_OK);
            } else {
              session_running_ = false;
              cb(result.error());
            }
          });
  fit::schedule_for_consumer(executor_.get(), std::move(prom));
}

template <class T>
NetworkDeviceClient::EventHandler<T>::EventHandler(fit::callback<void(fidl::UnbindInfo)> callback)
    : callback_(std::move(callback)) {}

template <class T>
void NetworkDeviceClient::EventHandler<T>::Unbound(fidl::UnbindInfo info) {
  if (callback_) {
    callback_(info);
  }
}

template <class T>
void NetworkDeviceClient::EventHandler<T>::Cancel() {
  callback_ = nullptr;
}

zx_status_t NetworkDeviceClient::PrepareSession() {
  zx_status_t status;

  if (session_config_.descriptor_length < sizeof(buffer_descriptor_t) ||
      (session_config_.descriptor_length % sizeof(uint64_t)) != 0) {
    FX_LOGS(ERROR) << "Invalid descriptor length " << session_config_.descriptor_length;
    return ZX_ERR_INVALID_ARGS;
  }

  if (session_config_.rx_descriptor_count > kMaxDepth ||
      session_config_.tx_descriptor_count > kMaxDepth) {
    FX_LOGS(ERROR) << "Invalid descriptor count  " << session_config_.rx_descriptor_count << "/"
                   << session_config_.tx_descriptor_count
                   << ", this client supports a maximum depth of " << kMaxDepth << " descriptors";
    return ZX_ERR_INVALID_ARGS;
  }

  if (session_config_.buffer_stride < session_config_.buffer_length) {
    FX_LOGS(ERROR) << "Stride in VMO can't be smaller than buffer length";
    return ZX_ERR_INVALID_ARGS;
  }

  if (session_config_.buffer_stride % device_info_.buffer_alignment != 0) {
    FX_LOGS(ERROR) << "Buffer stride " << session_config_.buffer_stride
                   << "does not meet buffer alignment requirement: "
                   << device_info_.buffer_alignment;
    return ZX_ERR_INVALID_ARGS;
  }

  descriptor_count_ = session_config_.rx_descriptor_count + session_config_.tx_descriptor_count;
  // Check if sum of descriptor count overflows.
  if (descriptor_count_ < session_config_.rx_descriptor_count ||
      descriptor_count_ < session_config_.tx_descriptor_count) {
    FX_LOGS(ERROR) << "Invalid descriptor count, maximum total descriptors must be less than 2^16";
    return ZX_ERR_INVALID_ARGS;
  }

  uint64_t data_vmo_size = descriptor_count_ * session_config_.buffer_stride;
  if ((status = data_.CreateAndMap(data_vmo_size, ZX_VM_PERM_READ | ZX_VM_PERM_WRITE, nullptr,
                                   &data_vmo_)) != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to create data VMO: " << zx_status_get_string(status);
    return status;
  }

  uint64_t descriptors_vmo_size = descriptor_count_ * session_config_.descriptor_length;
  if ((status = descriptors_.CreateAndMap(descriptors_vmo_size, ZX_VM_PERM_READ | ZX_VM_PERM_WRITE,
                                          nullptr, &descriptors_vmo_)) != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to create descriptors VMO: " << zx_status_get_string(status);
    return status;
  }

  if (session_config_.buffer_length <
      device_info_.min_tx_buffer_tail + device_info_.min_tx_buffer_head) {
    FX_LOGS(ERROR) << "Invalid buffer length, too small for requested Tx tail ("
                   << device_info_.min_tx_buffer_tail << ") + head: ("
                   << device_info_.min_tx_buffer_head << ")";
    return ZX_ERR_INVALID_ARGS;
  }

  return ZX_OK;
}

zx_status_t NetworkDeviceClient::SetPaused(bool paused) {
  if (!session_.is_valid()) {
    return ZX_ERR_BAD_STATE;
  }
  session_->SetPaused(paused);
  return ZX_OK;
}

zx_status_t NetworkDeviceClient::KillSession() {
  if (!session_.is_valid()) {
    return ZX_ERR_BAD_STATE;
  }
  // Cancel all the waits so we stop fetching frames.
  rx_wait_.Cancel();
  rx_writable_wait_.Cancel();
  tx_wait_.Cancel();
  tx_writable_wait_.Cancel();
  session_->Close();
  return ZX_OK;
}

zx::status<std::unique_ptr<NetworkDeviceClient::StatusWatchHandle>>
NetworkDeviceClient::WatchStatus(StatusCallback callback, uint32_t buffer) {
  zx::status endpoints = fidl::CreateEndpoints<netdev::StatusWatcher>();
  if (endpoints.is_error()) {
    return endpoints.take_error();
  }
  fidl::Result result = device_->GetStatusWatcher(std::move(endpoints->server), buffer);
  if (!result.ok()) {
    return zx::error(result.status());
  }
  return zx::ok(std::unique_ptr<StatusWatchHandle>(
      new StatusWatchHandle(std::move(endpoints->client), dispatcher_, std::move(callback))));
}

fit::result<netdev::wire::SessionInfo, zx_status_t> NetworkDeviceClient::MakeSessionInfo() {
  uint64_t descriptor_length_words = session_config_.descriptor_length / sizeof(uint64_t);
  ZX_DEBUG_ASSERT_MSG(descriptor_length_words <= std::numeric_limits<uint8_t>::max(),
                      "session descriptor length %ld (%ld words) overflows uint8_t",
                      session_config_.descriptor_length, descriptor_length_words);

  netdev::wire::SessionInfo session_info = {
      .descriptor_version = NETWORK_DEVICE_DESCRIPTOR_VERSION,
      .descriptor_length = static_cast<uint8_t>(descriptor_length_words),
      .descriptor_count = descriptor_count_,
      .options = session_config_.options,
      .rx_frames =
          fidl::VectorView<netdev::wire::FrameType>::FromExternal(session_config_.rx_frames),
  };

  zx_status_t status;
  if ((status = data_vmo_.duplicate(ZX_RIGHT_SAME_RIGHTS, &session_info.data)) != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to duplicate data VMO: " << zx_status_get_string(status);
    return fit::error(status);
  }
  if ((status = descriptors_vmo_.duplicate(ZX_RIGHT_SAME_RIGHTS, &session_info.descriptors)) !=
      ZX_OK) {
    FX_LOGS(ERROR) << "Failed to duplicate descriptors VMO: " << zx_status_get_string(status);
    return fit::error(status);
  }

  return fit::ok(std::move(session_info));
}

buffer_descriptor_t* NetworkDeviceClient::descriptor(uint16_t idx) {
  ZX_ASSERT(idx < descriptor_count_);
  return reinterpret_cast<buffer_descriptor_t*>(static_cast<uint8_t*>(descriptors_.start()) +
                                                session_config_.descriptor_length * idx);
}

void* NetworkDeviceClient::data(uint64_t offset) {
  ZX_ASSERT(offset < data_.size());
  return static_cast<uint8_t*>(data_.start()) + offset;
}

void NetworkDeviceClient::ResetRxDescriptor(buffer_descriptor_t* descriptor) {
  *descriptor = {
      .nxt = 0xFFFF,
      .info_type = static_cast<uint32_t>(netdev::wire::InfoType::kNoInfo),
      .offset = descriptor->offset,
      .data_length = session_config_.buffer_length,
  };
}

void NetworkDeviceClient::ResetTxDescriptor(buffer_descriptor_t* descriptor) {
  *descriptor = {
      .nxt = 0xFFFF,
      .info_type = static_cast<uint32_t>(netdev::wire::InfoType::kNoInfo),
      .offset = descriptor->offset,
      .head_length = device_info_.min_tx_buffer_head,
      .tail_length = device_info_.min_tx_buffer_tail,
      .data_length = session_config_.buffer_length - device_info_.min_tx_buffer_head -
                     device_info_.min_tx_buffer_tail,
  };
}

zx_status_t NetworkDeviceClient::PrepareDescriptors() {
  uint16_t desc = 0;
  uint64_t buff_off = 0;
  auto* pDesc = static_cast<uint8_t*>(descriptors_.start());
  rx_out_queue_.reserve(session_config_.rx_descriptor_count);
  tx_out_queue_.reserve(session_config_.tx_descriptor_count);

  for (; desc < session_config_.rx_descriptor_count; desc++) {
    auto* descriptor = reinterpret_cast<buffer_descriptor_t*>(pDesc);
    descriptor->offset = buff_off;
    ResetRxDescriptor(descriptor);

    buff_off += session_config_.buffer_stride;
    pDesc += session_config_.descriptor_length;
    rx_out_queue_.push_back(desc);
  }
  for (; desc < descriptor_count_; desc++) {
    auto* descriptor = reinterpret_cast<buffer_descriptor_t*>(pDesc);
    ResetTxDescriptor(descriptor);
    descriptor->offset = buff_off;

    buff_off += session_config_.buffer_stride;
    pDesc += session_config_.descriptor_length;
    tx_avail_.push(desc);
  }
  rx_wait_.set_object(rx_fifo_.get());
  rx_wait_.set_trigger(kFifoWaitReads);
  ZX_ASSERT(rx_wait_.Begin(dispatcher_) == ZX_OK);
  tx_wait_.set_object(tx_fifo_.get());
  tx_wait_.set_trigger(kFifoWaitReads);
  ZX_ASSERT(tx_wait_.Begin(dispatcher_) == ZX_OK);
  rx_writable_wait_.set_object(rx_fifo_.get());
  rx_writable_wait_.set_trigger(kFifoWaitWrites);
  tx_writable_wait_.set_object(tx_fifo_.get());
  tx_writable_wait_.set_trigger(kFifoWaitWrites);

  FlushRx();

  return ZX_OK;
}

void NetworkDeviceClient::FlushRx() {
  size_t flush = std::min(rx_out_queue_.size(), static_cast<size_t>(device_info_.rx_depth));
  ZX_ASSERT(flush != 0);
  zx_status_t status = rx_fifo_.write(sizeof(uint16_t), &rx_out_queue_[0], flush, &flush);
  bool sched_more;
  if (status == ZX_OK) {
    rx_out_queue_.erase(rx_out_queue_.begin(), rx_out_queue_.begin() + flush);
    sched_more = !rx_out_queue_.empty();
  } else {
    sched_more = status == ZX_ERR_SHOULD_WAIT;
  }

  if (sched_more && !rx_writable_wait_.is_pending()) {
    ZX_ASSERT(rx_writable_wait_.Begin(dispatcher_) == ZX_OK);
  }
}

void NetworkDeviceClient::FlushTx() {
  size_t flush = std::min(tx_out_queue_.size(), static_cast<size_t>(device_info_.tx_depth));
  ZX_ASSERT(flush != 0);
  zx_status_t status = tx_fifo_.write(sizeof(uint16_t), &tx_out_queue_[0], flush, &flush);
  bool sched_more;
  if (status == ZX_OK) {
    tx_out_queue_.erase(tx_out_queue_.begin(), tx_out_queue_.begin() + flush);
    sched_more = !tx_out_queue_.empty();
  } else {
    sched_more = status == ZX_ERR_SHOULD_WAIT;
  }

  if (sched_more && !tx_writable_wait_.is_pending()) {
    ZX_ASSERT(tx_writable_wait_.Begin(dispatcher_) == ZX_OK);
  }
}

void NetworkDeviceClient::ErrorTeardown(zx_status_t err) {
  session_running_ = false;
  data_.Unmap();
  data_vmo_.reset();
  descriptors_.Unmap();
  descriptors_vmo_.reset();
  session_ = {};
  if (err_callback_) {
    err_callback_(err);
  }
}

void NetworkDeviceClient::TxSignal(async_dispatcher_t* dispatcher, async::WaitBase* wait,
                                   zx_status_t status, const zx_packet_signal_t* signal) {
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "tx wait failed: " << zx_status_get_string(status);
    return;
  }
  if (signal->observed & wait->trigger() & ZX_FIFO_PEER_CLOSED) {
    FX_LOGS(ERROR) << "tx fifo was closed";
    ErrorTeardown(ZX_ERR_PEER_CLOSED);
    return;
  }
  if (signal->observed & wait->trigger() & ZX_FIFO_READABLE) {
    FetchTx();
  }
  if ((signal->observed & wait->trigger() & ZX_FIFO_WRITABLE) && !tx_out_queue_.empty()) {
    FlushTx();
  }

  if (wait != &tx_writable_wait_ || !tx_out_queue_.empty()) {
    ZX_ASSERT(wait->Begin(dispatcher_) == ZX_OK);
  }
}

void NetworkDeviceClient::RxSignal(async_dispatcher_t* dispatcher, async::WaitBase* wait,
                                   zx_status_t status, const zx_packet_signal_t* signal) {
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "rx wait failed: " << zx_status_get_string(status);
    return;
  }

  if (signal->observed & wait->trigger() & ZX_FIFO_PEER_CLOSED) {
    FX_LOGS(ERROR) << "rx fifo was closed";
    ErrorTeardown(ZX_ERR_PEER_CLOSED);
    return;
  }

  if (signal->observed & wait->trigger() & ZX_FIFO_READABLE) {
    FetchRx();
  }

  if ((signal->observed & wait->trigger() & ZX_FIFO_WRITABLE) && !rx_out_queue_.empty()) {
    FlushRx();
  }

  if (wait != &rx_writable_wait_ || !rx_out_queue_.empty()) {
    ZX_ASSERT(wait->Begin(dispatcher_) == ZX_OK);
  }
}

void NetworkDeviceClient::FetchRx() {
  uint16_t buff[kMaxDepth];
  size_t read;
  zx_status_t status;
  if ((status = rx_fifo_.read(sizeof(uint16_t), buff, kMaxDepth, &read)) != ZX_OK) {
    FX_LOGS(ERROR) << "Error reading from rx queue: " << zx_status_get_string(status);
    return;
  }
  uint16_t* desc_idx = buff;
  while (read > 0) {
    if (rx_callback_) {
      rx_callback_(Buffer(this, *desc_idx, true));
    } else {
      ReturnRxDescriptor(*desc_idx);
    }

    read--;
    desc_idx++;
  }
}

zx_status_t NetworkDeviceClient::Send(NetworkDeviceClient::Buffer* buffer) {
  if (!buffer->is_valid()) {
    return ZX_ERR_UNAVAILABLE;
  }
  if (buffer->rx_) {
    // If this is an rx buffer, we need to get a tx buffer from the pool and return it as an rx
    // buffer in place of this.
    auto tx_buffer = AllocTx();
    if (!tx_buffer.is_valid()) {
      return ZX_ERR_NO_RESOURCES;
    }
    // Flip the buffer, it'll be returned to the rx queue on destruction.
    tx_buffer.rx_ = true;
    buffer->rx_ = false;
  }
  if (!tx_writable_wait_.is_pending()) {
    zx_status_t status = tx_writable_wait_.Begin(dispatcher_);
    if (status != ZX_OK) {
      return status;
    }
  }
  tx_out_queue_.push_back(buffer->descriptor_);

  // Don't return this buffer on destruction.
  // Also invalidate it.
  buffer->parent_ = nullptr;
  return ZX_OK;
}

void NetworkDeviceClient::ReturnTxDescriptor(uint16_t idx) {
  auto* desc = descriptor(idx);
  if (desc->chain_length != 0) {
    ReturnTxDescriptor(desc->nxt);
  }
  ResetTxDescriptor(desc);
  tx_avail_.push(idx);
}

void NetworkDeviceClient::ReturnRxDescriptor(uint16_t idx) {
  auto* desc = descriptor(idx);
  if (desc->chain_length != 0) {
    ReturnRxDescriptor(desc->nxt);
  }
  ResetRxDescriptor(desc);
  rx_out_queue_.push_back(idx);
  if (!rx_writable_wait_.is_pending()) {
    ZX_ASSERT(rx_writable_wait_.Begin(dispatcher_) == ZX_OK);
  }
}

void NetworkDeviceClient::FetchTx() {
  uint16_t buff[kMaxDepth];
  size_t read;
  zx_status_t status;
  if ((status = tx_fifo_.read(sizeof(uint16_t), buff, kMaxDepth, &read)) != ZX_OK) {
    FX_LOGS(ERROR) << "Error reading from tx queue: " << zx_status_get_string(status);
    return;
  }
  uint16_t* desc_idx = buff;
  while (read > 0) {
    // TODO count and log tx errors
    ReturnTxDescriptor(*desc_idx);
    read--;
    desc_idx++;
  }
}

NetworkDeviceClient::Buffer NetworkDeviceClient::AllocTx() {
  if (tx_avail_.empty()) {
    return Buffer();
  } else {
    auto idx = tx_avail_.front();
    tx_avail_.pop();
    return Buffer(this, idx, false);
  }
}

NetworkDeviceClient::Buffer::Buffer() : parent_(nullptr), descriptor_(0), rx_(false) {}

NetworkDeviceClient::Buffer::Buffer(NetworkDeviceClient* parent, uint16_t descriptor, bool rx)
    : parent_(parent), descriptor_(descriptor), rx_(rx) {}

NetworkDeviceClient::Buffer::Buffer(NetworkDeviceClient::Buffer&& other) noexcept
    : parent_(other.parent_),
      descriptor_(other.descriptor_),
      rx_(other.rx_),
      data_(std::move(other.data_)) {
  other.parent_ = nullptr;
}

NetworkDeviceClient::Buffer::~Buffer() {
  if (parent_) {
    if (rx_) {
      parent_->ReturnRxDescriptor(descriptor_);
    } else {
      parent_->ReturnTxDescriptor(descriptor_);
    }
  }
}

NetworkDeviceClient::BufferData& NetworkDeviceClient::Buffer::data() {
  ZX_ASSERT(is_valid());
  if (!data_.is_loaded()) {
    data_.Load(parent_, descriptor_);
  }
  return data_;
}

const NetworkDeviceClient::BufferData& NetworkDeviceClient::Buffer::data() const {
  ZX_ASSERT(is_valid());
  if (!data_.is_loaded()) {
    data_.Load(parent_, descriptor_);
  }
  return data_;
}

zx_status_t NetworkDeviceClient::Buffer::Send() {
  if (!is_valid()) {
    return ZX_ERR_UNAVAILABLE;
  }
  zx_status_t status = data_.PadTo(parent_->device_info_.min_tx_buffer_length);
  if (status != ZX_OK) {
    return status;
  }
  return parent_->Send(this);
}

void NetworkDeviceClient::BufferData::Load(NetworkDeviceClient* parent, uint16_t idx) {
  auto* desc = parent->descriptor(idx);
  while (desc) {
    auto& cur = parts_[parts_count_];
    cur.base_ = parent->data(desc->offset + desc->head_length);
    cur.desc_ = desc;
    parts_count_++;
    if (desc->chain_length != 0) {
      desc = parent->descriptor(desc->nxt);
    } else {
      desc = nullptr;
    }
  }
}

NetworkDeviceClient::BufferRegion& NetworkDeviceClient::BufferData::part(size_t idx) {
  ZX_ASSERT(idx < parts_count_);
  return parts_[idx];
}

const NetworkDeviceClient::BufferRegion& NetworkDeviceClient::BufferData::part(size_t idx) const {
  ZX_ASSERT(idx < parts_count_);
  return parts_[idx];
}

uint32_t NetworkDeviceClient::BufferData::len() const {
  uint32_t c = 0;
  for (uint32_t i = 0; i < parts_count_; i++) {
    c += parts_[i].len();
  }
  return c;
}

netdev::wire::FrameType NetworkDeviceClient::BufferData::frame_type() const {
  return static_cast<netdev::wire::FrameType>(part(0).desc_->frame_type);
}

void NetworkDeviceClient::BufferData::SetFrameType(netdev::wire::FrameType type) {
  part(0).desc_->frame_type = static_cast<uint8_t>(type);
}

netdev::wire::InfoType NetworkDeviceClient::BufferData::info_type() const {
  return static_cast<netdev::wire::InfoType>(part(0).desc_->frame_type);
}

uint32_t NetworkDeviceClient::BufferData::inbound_flags() const {
  return part(0).desc_->inbound_flags;
}

uint32_t NetworkDeviceClient::BufferData::return_flags() const {
  return part(0).desc_->return_flags;
}

void NetworkDeviceClient::BufferData::SetTxRequest(netdev::wire::TxFlags tx_flags) {
  part(0).desc_->inbound_flags = static_cast<uint32_t>(tx_flags);
}

size_t NetworkDeviceClient::BufferData::Write(const void* src, size_t len) {
  const auto* ptr = static_cast<const uint8_t*>(src);
  size_t written = 0;
  for (uint32_t i = 0; i < parts_count_; i++) {
    auto& part = parts_[i];
    uint32_t wr = std::min(static_cast<uint32_t>(len - written), part.len());
    part.Write(ptr, wr);
    ptr += wr;
    written += wr;
  }
  return written;
}

size_t NetworkDeviceClient::BufferData::Write(const BufferData& data) {
  size_t count = 0;

  size_t idx_me = 0;
  size_t offset_me = 0;
  size_t offset_other = 0;
  for (size_t idx_o = 0; idx_o < data.parts_count_ && idx_me < parts_count_;) {
    size_t wr = parts_[idx_me].Write(offset_me, data.parts_[idx_o], offset_other);
    offset_me += wr;
    offset_other += wr;
    count += wr;
    if (offset_me >= parts_[idx_me].len()) {
      idx_me++;
      offset_me = 0;
    }
    if (offset_other >= data.parts_[idx_o].len()) {
      idx_o++;
      offset_other = 0;
    }
  }
  // Update the length on the last descriptor.
  if (idx_me < parts_count_) {
    ZX_DEBUG_ASSERT(offset_me <= std::numeric_limits<uint32_t>::max());
    parts_[idx_me].CapLength(static_cast<uint32_t>(offset_me));
  }

  return count;
}

zx_status_t NetworkDeviceClient::BufferData::PadTo(size_t size) {
  size_t total_size = 0;
  for (uint32_t i = 0; i < parts_count_ && total_size < size; i++) {
    total_size += parts_[i].PadTo(size - total_size);
  }
  if (total_size < size) {
    return ZX_ERR_BUFFER_TOO_SMALL;
  }
  return ZX_OK;
}

size_t NetworkDeviceClient::BufferData::Read(void* dst, size_t len) {
  auto* ptr = static_cast<uint8_t*>(dst);
  size_t actual = 0;
  for (uint32_t i = 0; i < parts_count_ && len > 0; i++) {
    auto& part = parts_[i];
    size_t rd = part.Read(ptr, len);
    len -= rd;
    ptr += rd;
    actual += rd;
  }
  return actual;
}

void NetworkDeviceClient::BufferRegion::CapLength(uint32_t len) {
  if (len <= desc_->data_length) {
    desc_->tail_length += desc_->data_length - len;
    desc_->data_length = len;
  }
}

uint32_t NetworkDeviceClient::BufferRegion::len() const { return desc_->data_length; }

fbl::Span<uint8_t> NetworkDeviceClient::BufferRegion::data() {
  return fbl::Span(static_cast<uint8_t*>(base_), len());
}

fbl::Span<const uint8_t> NetworkDeviceClient::BufferRegion::data() const {
  return fbl::Span(static_cast<const uint8_t*>(base_), len());
}

size_t NetworkDeviceClient::BufferRegion::Write(const void* src, size_t len, size_t offset) {
  uint32_t nlen = std::min(desc_->data_length, static_cast<uint32_t>(len + offset));
  CapLength(nlen);
  std::copy_n(static_cast<const uint8_t*>(src), this->len() - offset, data().begin() + offset);
  return this->len();
}

size_t NetworkDeviceClient::BufferRegion::Read(void* dst, size_t len, size_t offset) {
  if (offset >= desc_->data_length) {
    return 0;
  }
  len = std::min(len, desc_->data_length - offset);
  std::copy_n(data().begin() + offset, len, static_cast<uint8_t*>(dst));
  return len;
}

size_t NetworkDeviceClient::BufferRegion::Write(size_t offset, const BufferRegion& src,
                                                size_t src_offset) {
  if (offset >= desc_->data_length || src_offset >= src.desc_->data_length) {
    return 0;
  }
  size_t wr = std::min(desc_->data_length - offset, src.desc_->data_length - src_offset);
  std::copy_n(src.data().begin() + src_offset, wr, data().begin() + offset);
  return wr;
}

size_t NetworkDeviceClient::BufferRegion::PadTo(size_t size) {
  if (size > desc_->data_length) {
    size -= desc_->data_length;
    fbl::Span<uint8_t> pad(static_cast<uint8_t*>(base_) + desc_->head_length + desc_->data_length,
                           std::min(size, static_cast<size_t>(desc_->tail_length)));
    memset(pad.data(), 0x00, pad.size());
    desc_->data_length += pad.size();
    desc_->tail_length -= pad.size();
  }
  return desc_->data_length;
}

void NetworkDeviceClient::StatusWatchHandle::Watch() {
  watcher_->WatchStatus([this](fidl::WireResponse<netdev::StatusWatcher::WatchStatus>* response) {
    callback_(std::move(response->device_status));
    // Watch again, we only stop watching when StatusWatchHandle is destroyed.
    Watch();
  });
}

}  // namespace client
}  // namespace network
