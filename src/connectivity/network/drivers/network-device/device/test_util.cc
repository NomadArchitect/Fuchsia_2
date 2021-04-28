// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "test_util.h"

#include <gtest/gtest.h>

#include "src/lib/testing/predicates/status.h"

namespace network {
namespace testing {

zx_status_t TxBuffer::GetData(std::vector<uint8_t>* copy,
                              const AnyBuffer::VmoProvider& vmo_provider) {
  if (!vmo_provider) {
    return ZX_ERR_INTERNAL;
  }
  auto vmo = vmo_provider(buffer_.data.vmo_id);
  // can't use this with the current test set up, return internal error
  if (buffer_.data.parts_count != 1 || !vmo->is_valid()) {
    return ZX_ERR_INTERNAL;
  }
  copy->resize(buffer_.data.parts_list[0].length);
  return vmo->read(&copy->at(0), buffer_.data.parts_list[0].offset,
                   buffer_.data.parts_list[0].length);
}

zx_status_t RxBuffer::WriteData(const uint8_t* data, size_t len,
                                const AnyBuffer::VmoProvider& vmo_provider) {
  if (!vmo_provider) {
    return ZX_ERR_INTERNAL;
  }
  auto vmo = vmo_provider(buffer_.data.vmo_id);
  // we only support simple buffers here for testing
  if (buffer_.data.parts_count != 1 || !vmo->is_valid()) {
    return ZX_ERR_INTERNAL;
  }
  if (buffer_.data.parts_list[0].length < len) {
    return ZX_ERR_INVALID_ARGS;
  }
  return_.length = len;
  return vmo->write(data, buffer_.data.parts_list[0].offset, len);
}

void RxBuffer::FillReturn() {
  return_.length = 0;
  return_.meta.info_type = static_cast<uint32_t>(netdev::wire::InfoType::kNoInfo);
  return_.meta.flags = 0;
  return_.meta.frame_type = static_cast<uint8_t>(netdev::wire::FrameType::kEthernet);
  return_.id = buffer_.id;
}

FakeNetworkPortImpl::FakeNetworkPortImpl()
    : port_info_({
          .device_class = static_cast<uint8_t>(netdev::wire::DeviceClass::kEthernet),
          .rx_types_list = rx_types_.data(),
          .rx_types_count = 1,
          .tx_types_list = tx_types_.data(),
          .tx_types_count = 1,
      }) {
  rx_types_[0] = static_cast<uint8_t>(netdev::wire::FrameType::kEthernet);
  tx_types_[0].type = static_cast<uint8_t>(netdev::wire::FrameType::kEthernet);
  tx_types_[0].supported_flags = 0;
  tx_types_[0].features = netdev::wire::kFrameFeaturesRaw;
  EXPECT_OK(zx::event::create(0, &event_));
}

FakeNetworkPortImpl::~FakeNetworkPortImpl() {
  if (port_added_) {
    EXPECT_TRUE(port_removed_) << "port was added but remove was not called";
  }
}

void FakeNetworkPortImpl::NetworkPortGetInfo(port_info_t* out_info) { *out_info = port_info_; }

void FakeNetworkPortImpl::NetworkPortGetStatus(port_status_t* out_status) { *out_status = status_; }

void FakeNetworkPortImpl::NetworkPortSetActive(bool active) {
  port_active_ = active;
  ASSERT_OK(event_.signal(0, kEventPortActiveChanged));
}

void FakeNetworkPortImpl::NetworkPortGetMac(mac_addr_protocol_t* out_mac_ifc) {
  *out_mac_ifc = mac_proto_;
}

void FakeNetworkPortImpl::NetworkPortRemoved() {
  EXPECT_FALSE(port_removed_) << "removed same port twice";
  port_removed_ = true;
}

void FakeNetworkPortImpl::AddPort(uint8_t port_id,
                                  ddk::NetworkDeviceIfcProtocolClient* ifc_client) {
  ASSERT_FALSE(port_added_) << "can't add the same port object twice";
  port_added_ = true;
  ifc_client->AddPort(port_id, this, &network_port_protocol_ops_);
}

FakeNetworkDeviceImpl::FakeNetworkDeviceImpl()
    : ddk::NetworkDeviceImplProtocol<FakeNetworkDeviceImpl>(),
      info_({
          .tx_depth = kTxDepth,
          .rx_depth = kRxDepth,
          .rx_threshold = kRxDepth / 2,
          .max_buffer_length = ZX_PAGE_SIZE / 2,
          .buffer_alignment = ZX_PAGE_SIZE,
      }) {
  EXPECT_OK(zx::event::create(0, &event_));
}

FakeNetworkDeviceImpl::~FakeNetworkDeviceImpl() {
  // ensure that all VMOs were released
  for (auto& vmo : vmos_) {
    ZX_ASSERT(!vmo.is_valid());
  }
}

zx_status_t FakeNetworkDeviceImpl::NetworkDeviceImplInit(
    const network_device_ifc_protocol_t* iface) {
  port0_.SetStatus(
      {.mtu = 2048, .flags = static_cast<uint32_t>(netdev::wire::StatusFlags::kOnline)});
  device_client_ = ddk::NetworkDeviceIfcProtocolClient(iface);

  auto port_protocol = port0_.protocol();
  device_client_.AddPort(kPort0, port_protocol.ctx, port_protocol.ops);
  return ZX_OK;
}

void FakeNetworkDeviceImpl::NetworkDeviceImplStart(network_device_impl_start_callback callback,
                                                   void* cookie) {
  if (auto_start_) {
    callback(cookie);
  } else {
    ZX_ASSERT(!(pending_start_callback_ || pending_stop_callback_));
    pending_start_callback_ = [cookie, callback]() { callback(cookie); };
  }
  event_.signal(0, kEventStart);
}

void FakeNetworkDeviceImpl::NetworkDeviceImplStop(network_device_impl_stop_callback callback,
                                                  void* cookie) {
  if (auto_stop_) {
    callback(cookie);
  } else {
    ZX_ASSERT(!(pending_start_callback_ || pending_stop_callback_));
    pending_stop_callback_ = [cookie, callback]() { callback(cookie); };
  }
  event_.signal(0, kEventStop);
}

void FakeNetworkDeviceImpl::NetworkDeviceImplGetInfo(device_info_t* out_info) { *out_info = info_; }

void FakeNetworkDeviceImpl::NetworkDeviceImplQueueTx(const tx_buffer_t* buf_list,
                                                     size_t buf_count) {
  EXPECT_NE(buf_count, 0u);
  ASSERT_TRUE(device_client_.is_valid());
  if (auto_return_tx_) {
    ASSERT_TRUE(buf_count < kTxDepth);
    tx_result_t results[kTxDepth];
    auto* r = results;
    for (size_t i = buf_count; i; i--) {
      r->status = ZX_OK;
      r->id = buf_list->id;
      buf_list++;
      r++;
    }
    device_client_.CompleteTx(results, buf_count);
  } else {
    while (buf_count--) {
      auto back = std::make_unique<TxBuffer>(buf_list);
      tx_buffers_.push_back(std::move(back));
      buf_list++;
    }
  }
  event_.signal(0, kEventTx);
}

void FakeNetworkDeviceImpl::NetworkDeviceImplQueueRxSpace(const rx_space_buffer_t* buf_list,
                                                          size_t buf_count) {
  ASSERT_TRUE(device_client_.is_valid());
  while (buf_count--) {
    auto back = std::make_unique<RxBuffer>(buf_list);
    rx_buffers_.push_back(std::move(back));
    buf_list++;
  }
  event_.signal(0, kEventRxAvailable);
}

fit::function<zx::unowned_vmo(uint8_t)> FakeNetworkDeviceImpl::VmoGetter() {
  return [this](uint8_t id) { return zx::unowned_vmo(vmos_[id]); };
}

void FakeNetworkDeviceImpl::ReturnAllTx() {
  ASSERT_TRUE(device_client_.is_valid());

  TxReturnTransaction tx(this);
  while (!tx_buffers_.is_empty()) {
    tx.Enqueue(tx_buffers_.pop_front());
  }
  tx.Commit();
}

bool FakeNetworkDeviceImpl::TriggerStart() {
  if (pending_start_callback_) {
    pending_start_callback_();
    pending_start_callback_ = nullptr;
    return true;
  }
  return false;
}

bool FakeNetworkDeviceImpl::TriggerStop() {
  if (pending_stop_callback_) {
    pending_stop_callback_();
    pending_stop_callback_ = nullptr;
    return true;
  }
  return false;
}

void FakeNetworkDeviceImpl::SetOnline(bool online) {
  port_status_t status = port0_.status();
  status.flags = static_cast<uint32_t>(online ? netdev::wire::StatusFlags::kOnline
                                              : netdev::wire::StatusFlags());
  SetStatus(status);
}

void FakeNetworkDeviceImpl::SetStatus(const port_status_t& status) {
  port0_.SetStatus(status);
  device_client_.PortStatusChanged(kPort0, &status);
}

zx::status<std::unique_ptr<NetworkDeviceInterface>> FakeNetworkDeviceImpl::CreateChild(
    async_dispatcher_t* dispatcher) {
  auto protocol = proto();
  zx::status device = internal::DeviceInterface::Create(
      dispatcher, ddk::NetworkDeviceImplProtocolClient(&protocol), "FakeImpl");
  if (device.is_error()) {
    return device.take_error();
  }

  auto& value = device.value();
  value->evt_session_started = [this](const char* session) {
    event_.signal(0, kEventSessionStarted);
  };
  return zx::ok(std::move(value));
}

zx_status_t TestSession::Open(fidl::WireSyncClient<netdev::Device>& netdevice, const char* name,
                              netdev::wire::SessionFlags flags, uint16_t num_descriptors,
                              uint64_t buffer_size,
                              fidl::VectorView<netdev::wire::FrameType> frame_types) {
  netdev::wire::FrameType supported_frames[1];
  supported_frames[0] = netdev::wire::FrameType::kEthernet;
  netdev::wire::SessionInfo info{};
  if (frame_types.count() == 0) {
    // default to just ethernet
    info.rx_frames = fidl::VectorView<netdev::wire::FrameType>::FromExternal(supported_frames, 1);
  } else {
    info.rx_frames = std::move(frame_types);
  }
  info.options = flags;
  zx_status_t status;
  if ((status = Init(num_descriptors, buffer_size)) != ZX_OK) {
    return status;
  }
  if ((status = GetInfo(&info)) != ZX_OK) {
    return status;
  }

  auto session_name = fidl::StringView::FromExternal(name);

  auto res = netdevice.OpenSession(std::move(session_name), std::move(info));
  if (res.status() != ZX_OK) {
    printf("OpenSession FIDL failure: %s %s\n", zx_status_get_string(res.status()), res.error());
    return res.status();
  }
  if (res.value().result.is_err()) {
    printf("OpenSession failed: %s\n", zx_status_get_string(res.status()));
    return res.value().result.err();
  }

  Setup(std::move(res.value().result.mutable_response().session),
        std::move(res.value().result.mutable_response().fifos));

  return ZX_OK;
}

zx_status_t TestSession::Init(uint16_t descriptor_count, uint64_t buffer_size) {
  zx_status_t status;
  if (descriptors_vmo_.is_valid() || data_vmo_.is_valid() || session_.is_valid()) {
    return ZX_ERR_BAD_STATE;
  }

  if ((status = descriptors_.CreateAndMap(descriptor_count * sizeof(buffer_descriptor_t),
                                          ZX_VM_PERM_READ | ZX_VM_PERM_WRITE, nullptr,
                                          &descriptors_vmo_)) != ZX_OK) {
    printf("ERROR: failed to create descriptors map\n");
    return status;
  }

  if ((status = data_.CreateAndMap(descriptor_count * buffer_size,
                                   ZX_VM_PERM_READ | ZX_VM_PERM_WRITE, nullptr, &data_vmo_)) !=
      ZX_OK) {
    printf("ERROR: failed to create data map");
    return status;
  }
  descriptors_count_ = descriptor_count;
  buffer_length_ = buffer_size;
  return ZX_OK;
}

zx_status_t TestSession::GetInfo(netdev::wire::SessionInfo* info) {
  zx_status_t status;
  if (!data_vmo_.is_valid() || !descriptors_vmo_.is_valid()) {
    return ZX_ERR_BAD_STATE;
  }
  if ((status = data_vmo_.duplicate(ZX_RIGHT_SAME_RIGHTS, &info->data)) != ZX_OK) {
    return status;
  }
  if ((status = descriptors_vmo_.duplicate(ZX_RIGHT_SAME_RIGHTS, &info->descriptors)) != ZX_OK) {
    return status;
  }

  info->descriptor_version = NETWORK_DEVICE_DESCRIPTOR_VERSION;
  info->descriptor_length = sizeof(buffer_descriptor_t) / sizeof(uint64_t);
  info->descriptor_count = descriptors_count_;
  return ZX_OK;
}

void TestSession::Setup(fidl::ClientEnd<netdev::Session> session, netdev::wire::Fifos fifos) {
  session_ = std::move(session);
  fifos_ = std::move(fifos);
}

zx_status_t TestSession::SetPaused(bool paused) {
  return fidl::WireCall<netdev::Session>(session_).SetPaused(paused).status();
}

zx_status_t TestSession::Close() {
  return fidl::WireCall<netdev::Session>(session_).Close().status();
}

zx_status_t TestSession::WaitClosed(zx::time deadline) {
  return session_.channel().wait_one(ZX_CHANNEL_PEER_CLOSED, deadline, nullptr);
}

buffer_descriptor_t* TestSession::ResetDescriptor(uint16_t index) {
  auto* desc = descriptor(index);
  desc->frame_type = static_cast<uint8_t>(netdev::wire::FrameType::kEthernet);
  desc->offset = canonical_offset(index);
  desc->info_type = static_cast<uint32_t>(netdev::wire::InfoType::kNoInfo);
  desc->head_length = 0;
  desc->data_length = static_cast<uint32_t>(buffer_length_);
  desc->tail_length = 0;
  desc->inbound_flags = 0;
  desc->return_flags = 0;
  desc->chain_length = 0;
  desc->nxt = 0;
  return desc;
}

void TestSession::ZeroVmo() { memset(data_.start(), 0x00, buffer_length_ * descriptors_count_); }

buffer_descriptor_t* TestSession::descriptor(uint16_t index) {
  if (index < descriptors_count_) {
    return reinterpret_cast<buffer_descriptor_t*>(reinterpret_cast<uint8_t*>(descriptors_.start()) +
                                                  (index * sizeof(buffer_descriptor_t)));
  }
  return nullptr;
}

uint8_t* TestSession::buffer(uint64_t offset) {
  return reinterpret_cast<uint8_t*>(data_.start()) + offset;
}

zx_status_t TestSession::FetchRx(uint16_t* descriptors, size_t count, size_t* actual) const {
  return fifos_.rx.read(sizeof(uint16_t), descriptors, count, actual);
}

zx_status_t TestSession::FetchTx(uint16_t* descriptors, size_t count, size_t* actual) const {
  return fifos_.tx.read(sizeof(uint16_t), descriptors, count, actual);
}

zx_status_t TestSession::SendRx(const uint16_t* descriptor, size_t count, size_t* actual) const {
  return fifos_.rx.write(sizeof(uint16_t), descriptor, count, actual);
}

zx_status_t TestSession::SendTx(const uint16_t* descriptor, size_t count, size_t* actual) const {
  return fifos_.tx.write(sizeof(uint16_t), descriptor, count, actual);
}

zx_status_t TestSession::SendTxData(uint16_t descriptor_index, const std::vector<uint8_t>& data) {
  auto* desc = ResetDescriptor(descriptor_index);
  zx_status_t status;
  if ((status = data_vmo_.write(&data.at(0), desc->offset, data.size())) != ZX_OK) {
    return status;
  }
  desc->data_length = static_cast<uint32_t>(data.size());
  return SendTx(descriptor_index);
}

}  // namespace testing
}  // namespace network
