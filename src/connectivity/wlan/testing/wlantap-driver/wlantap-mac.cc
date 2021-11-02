// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "wlantap-mac.h"

#include <fuchsia/hardware/wlan/mac/cpp/banjo.h>
#include <fuchsia/wlan/common/c/banjo.h>
#include <fuchsia/wlan/device/cpp/fidl.h>
#include <fuchsia/wlan/ieee80211/c/banjo.h>
#include <fuchsia/wlan/internal/cpp/banjo.h>
#include <lib/ddk/debug.h>
#include <lib/ddk/driver.h>

#include <mutex>

#include <wlan/common/channel.h>

#include "utils.h"

namespace wlan {

namespace wlantap = ::fuchsia::wlan::tap;
namespace wlan_device = ::fuchsia::wlan::device;

namespace {

struct WlantapMacImpl : WlantapMac {
  WlantapMacImpl(zx_device_t* phy_device, uint16_t id, wlan_device::MacRole role,
                 const wlantap::WlantapPhyConfig* phy_config, Listener* listener,
                 zx::channel sme_channel)
      : id_(id),
        role_(role),
        phy_config_(phy_config),
        listener_(listener),
        sme_channel_(std::move(sme_channel)) {}

  static void DdkUnbind(void* ctx) {
    auto& self = *static_cast<WlantapMacImpl*>(ctx);
    self.Unbind();
  }

  static void DdkRelease(void* ctx) { delete static_cast<WlantapMacImpl*>(ctx); }

  // Wlanmac protocol impl

  static zx_status_t WlanmacQuery(void* ctx, uint32_t options, wlanmac_info_t* mac_info) {
    auto& self = *static_cast<WlantapMacImpl*>(ctx);
    ConvertTapPhyConfig(mac_info, *self.phy_config_);
    return ZX_OK;
  }

  static zx_status_t WlanmacStart(void* ctx, const wlanmac_ifc_protocol_t* ifc,
                                  zx_handle_t* out_sme_channel) {
    auto& self = *static_cast<WlantapMacImpl*>(ctx);
    {
      std::lock_guard<std::mutex> guard(self.lock_);
      if (self.ifc_.is_valid()) {
        return ZX_ERR_ALREADY_BOUND;
      }
      if (!self.sme_channel_.is_valid()) {
        return ZX_ERR_ALREADY_BOUND;
      }
      self.ifc_ = ddk::WlanmacIfcProtocolClient(ifc);
    }
    self.listener_->WlantapMacStart(self.id_);
    *out_sme_channel = self.sme_channel_.release();
    return ZX_OK;
  }

  static void WlanmacStop(void* ctx) {
    auto& self = *static_cast<WlantapMacImpl*>(ctx);
    {
      std::lock_guard<std::mutex> guard(self.lock_);
      self.ifc_.clear();
    }
    self.listener_->WlantapMacStop(self.id_);
  }

  static zx_status_t WlanmacQueueTx(void* ctx, uint32_t options, const wlan_tx_packet_t* packet) {
    auto& self = *static_cast<WlantapMacImpl*>(ctx);
    self.listener_->WlantapMacQueueTx(self.id_, packet);
    return ZX_OK;
  }

  static zx_status_t WlanmacSetChannel(void* ctx, uint32_t options, const wlan_channel_t* channel) {
    auto& self = *static_cast<WlantapMacImpl*>(ctx);
    if (options != 0) {
      return ZX_ERR_INVALID_ARGS;
    }
    if (!wlan::common::IsValidChan(*channel)) {
      return ZX_ERR_INVALID_ARGS;
    }
    self.listener_->WlantapMacSetChannel(self.id_, channel);
    return ZX_OK;
  }

  static zx_status_t WlanmacConfigureBss(void* ctx, uint32_t options, const bss_config_t* config) {
    auto& self = *static_cast<WlantapMacImpl*>(ctx);
    if (options != 0) {
      return ZX_ERR_INVALID_ARGS;
    }
    bool expected_remote = self.role_ == wlan_device::MacRole::CLIENT;
    if (config->remote != expected_remote) {
      return ZX_ERR_INVALID_ARGS;
    }
    self.listener_->WlantapMacConfigureBss(self.id_, config);
    return ZX_OK;
  }

  static zx_status_t WlanmacEnableBeaconing(void* ctx, uint32_t options,
                                            const wlan_bcn_config_t* bcn_cfg) {
    if (options != 0) {
      return ZX_ERR_INVALID_ARGS;
    }
    // This is the test driver, so we can just pretend beaconing was enabled.
    (void)bcn_cfg;
    return ZX_OK;
  }

  static zx_status_t WlanmacConfigureBeacon(void* ctx, uint32_t options,
                                            const wlan_tx_packet_t* pkt) {
    if (options != 0) {
      return ZX_ERR_INVALID_ARGS;
    }
    // This is the test driver, so we can just pretend the beacon was configured.
    (void)pkt;
    return ZX_OK;
  }

  static zx_status_t WlanmacSetKey(void* ctx, uint32_t options,
                                   const wlan_key_config_t* key_config) {
    auto& self = *static_cast<WlantapMacImpl*>(ctx);
    if (options != 0) {
      return ZX_ERR_INVALID_ARGS;
    }
    self.listener_->WlantapMacSetKey(self.id_, key_config);
    return ZX_OK;
  }

  static zx_status_t WlanmacConfigureAssoc(void* ctx, uint32_t options,
                                           const wlan_assoc_ctx* assoc_ctx) {
    if (options != 0) {
      return ZX_ERR_INVALID_ARGS;
    }
    // This is the test driver, so we can just pretend the association was configured.
    (void)assoc_ctx;
    // TODO(fxbug.dev/28907): Evalute the use and implement
    return ZX_OK;
  }

  static zx_status_t WlanmacClearAssoc(
      void* ctx, uint32_t options, const uint8_t peer_addr[fuchsia_wlan_ieee80211_MAC_ADDR_LEN]) {
    if (options != 0) {
      return ZX_ERR_INVALID_ARGS;
    }
    if (!peer_addr) {
      return ZX_ERR_INVALID_ARGS;
    }
    // TODO(fxbug.dev/28907): Evalute the use and implement
    return ZX_OK;
  }

  // WlantapMac impl

  virtual void Rx(const std::vector<uint8_t>& data, const wlantap::WlanRxInfo& rx_info) override {
    std::lock_guard<std::mutex> guard(lock_);
    if (ifc_.is_valid()) {
      wlan_rx_info_t converted_info = {.rx_flags = rx_info.rx_flags,
                                       .valid_fields = rx_info.valid_fields,
                                       .phy = rx_info.phy,
                                       .data_rate = rx_info.data_rate,
                                       .channel = {.primary = rx_info.channel.primary,
                                                   .cbw = static_cast<uint8_t>(rx_info.channel.cbw),
                                                   .secondary80 = rx_info.channel.secondary80},
                                       .mcs = rx_info.mcs,
                                       .rssi_dbm = rx_info.rssi_dbm,
                                       .snr_dbh = rx_info.snr_dbh};
      wlan_rx_packet_t rx_packet = {
          .mac_frame_buffer = data.data(), .mac_frame_size = data.size(), .info = converted_info};
      ifc_.Recv(&rx_packet);
    }
  }

  virtual void Status(uint32_t status) override {
    std::lock_guard<std::mutex> guard(lock_);
    if (ifc_.is_valid()) {
      ifc_.Status(status);
    }
  }

  virtual void ReportTxStatus(const wlantap::WlanTxStatus& ts) override {
    std::lock_guard<std::mutex> guard(lock_);
    if (ifc_.is_valid()) {
      wlan_tx_status_t converted_tx_status = ConvertTxStatus(ts);
      ifc_.ReportTxStatus(&converted_tx_status);
    }
  }

  void Unbind() {
    {
      std::lock_guard<std::mutex> guard(lock_);
      ifc_.clear();
    }
    device_unbind_reply(device_);
  }

  virtual void RemoveDevice() override { device_async_remove(device_); }

  zx_device_t* device_ = nullptr;
  uint16_t id_;
  wlan_device::MacRole role_;
  std::mutex lock_;
  ddk::WlanmacIfcProtocolClient ifc_ __TA_GUARDED(lock_);
  const wlantap::WlantapPhyConfig* phy_config_;
  Listener* listener_;
  zx::channel sme_channel_;
};

}  // namespace

zx_status_t CreateWlantapMac(zx_device_t* parent_phy, const wlan_device::MacRole role,
                             const wlantap::WlantapPhyConfig* phy_config, uint16_t id,
                             WlantapMac::Listener* listener, zx::channel sme_channel,
                             WlantapMac** ret) {
  char name[ZX_MAX_NAME_LEN + 1];
  snprintf(name, sizeof(name), "%s-mac%u", device_get_name(parent_phy), id);
  std::unique_ptr<WlantapMacImpl> wlanmac(
      new WlantapMacImpl(parent_phy, id, role, phy_config, listener, std::move(sme_channel)));
  static zx_protocol_device_t device_ops = {.version = DEVICE_OPS_VERSION,
                                            .unbind = &WlantapMacImpl::DdkUnbind,
                                            .release = &WlantapMacImpl::DdkRelease};
  static wlanmac_protocol_ops_t proto_ops = {
      .query = &WlantapMacImpl::WlanmacQuery,
      .start = &WlantapMacImpl::WlanmacStart,
      .stop = &WlantapMacImpl::WlanmacStop,
      .queue_tx = &WlantapMacImpl::WlanmacQueueTx,
      .set_channel = &WlantapMacImpl::WlanmacSetChannel,
      .configure_bss = &WlantapMacImpl::WlanmacConfigureBss,
      .enable_beaconing = &WlantapMacImpl::WlanmacEnableBeaconing,
      .configure_beacon = &WlantapMacImpl::WlanmacConfigureBeacon,
      .set_key = &WlantapMacImpl::WlanmacSetKey,
      .configure_assoc = &WlantapMacImpl::WlanmacConfigureAssoc,
      .clear_assoc = &WlantapMacImpl::WlanmacClearAssoc,
  };
  device_add_args_t args = {.version = DEVICE_ADD_ARGS_VERSION,
                            .name = name,
                            .ctx = wlanmac.get(),
                            .ops = &device_ops,
                            .proto_id = ZX_PROTOCOL_WLANMAC,
                            .proto_ops = &proto_ops};
  zx_status_t status = device_add(parent_phy, &args, &wlanmac->device_);
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s: could not add device: %d", __func__, status);
    return status;
  }
  // Transfer ownership to devmgr
  *ret = wlanmac.release();
  return ZX_OK;
}

}  // namespace wlan
