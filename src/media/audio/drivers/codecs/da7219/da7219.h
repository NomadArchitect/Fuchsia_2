// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_AUDIO_DRIVERS_CODECS_DA7219_DA7219_H_
#define SRC_MEDIA_AUDIO_DRIVERS_CODECS_DA7219_DA7219_H_

#include <fidl/fuchsia.hardware.audio/cpp/wire.h>
#include <fidl/fuchsia.hardware.i2c/cpp/wire.h>
#include <fuchsia/hardware/audio/cpp/banjo.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async/cpp/irq.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/zx/interrupt.h>

#include <ddktl/device.h>
#include <ddktl/fidl.h>

namespace audio {

class Da7219;
using Da7219Base =
    ddk::Device<Da7219, ddk::Messageable<fuchsia_hardware_audio::CodecConnector>::Mixin,
                ddk::Suspendable, ddk::Unbindable>;

class Da7219 : public Da7219Base,
               public fidl::WireServer<fuchsia_hardware_audio::Codec>,
               public ddk::CodecProtocol<Da7219, ddk::base_protocol> {
 public:
  explicit Da7219(zx_device_t* parent, fidl::ClientEnd<fuchsia_hardware_i2c::Device> i2c,
                  zx::interrupt irq);
  ~Da7219() override = default;

  void Shutdown();

  static zx_status_t Bind(void* ctx, zx_device_t* dev);
  void DdkInit(ddk::InitTxn txn);
  void DdkRelease();
  void DdkUnbind(ddk::UnbindTxn txn) {
    Shutdown();
    txn.Reply();
  }
  void DdkSuspend(ddk::SuspendTxn txn) {
    Shutdown();
    txn.Reply(ZX_OK, txn.requested_state());
  }

  zx_status_t CodecConnect(zx::channel channel) {
    fidl::BindServer<fidl::WireServer<fuchsia_hardware_audio::Codec>>(
        loop_->dispatcher(), fidl::ServerEnd<fuchsia_hardware_audio::Codec>(std::move(channel)),
        this);
    return ZX_OK;
  }

 protected:
  zx_status_t Initialize();  // protected for unit tests.

 private:
  // LLCPP implementation for the Codec API.
  void Reset(ResetRequestView request, ResetCompleter::Sync& completer) override;
  void Stop(StopRequestView request, StopCompleter::Sync& completer) override;
  void Start(StartRequestView request, StartCompleter::Sync& completer) override;
  void GetInfo(GetInfoRequestView request, GetInfoCompleter::Sync& completer) override;
  void GetHealthState(GetHealthStateRequestView request,
                      GetHealthStateCompleter::Sync& completer) override;
  void IsBridgeable(IsBridgeableRequestView request,
                    IsBridgeableCompleter::Sync& completer) override;
  void SetBridgedMode(SetBridgedModeRequestView request,
                      SetBridgedModeCompleter::Sync& completer) override;
  void GetDaiFormats(GetDaiFormatsRequestView request,
                     GetDaiFormatsCompleter::Sync& completer) override;
  void SetDaiFormat(SetDaiFormatRequestView request,
                    SetDaiFormatCompleter::Sync& completer) override;
  void GetGainFormat(GetGainFormatRequestView request,
                     GetGainFormatCompleter::Sync& completer) override;
  void WatchGainState(WatchGainStateRequestView request,
                      WatchGainStateCompleter::Sync& completer) override;
  void SetGainState(SetGainStateRequestView request,
                    SetGainStateCompleter::Sync& completer) override;
  void GetPlugDetectCapabilities(GetPlugDetectCapabilitiesRequestView request,
                                 GetPlugDetectCapabilitiesCompleter::Sync& completer) override;
  void WatchPlugState(WatchPlugStateRequestView request,
                      WatchPlugStateCompleter::Sync& completer) override;
  void SignalProcessingConnect(SignalProcessingConnectRequestView request,
                               SignalProcessingConnectCompleter::Sync& completer) override {}
  void Connect(ConnectRequestView request, ConnectCompleter::Sync& completer) override;

  zx_status_t Reset();

  void HandleIrq(async_dispatcher_t* dispatcher, async::IrqBase* irq, zx_status_t status,
                 const zx_packet_interrupt_t* interrupt);
  void PlugDetected(bool plugged);

  fidl::ClientEnd<fuchsia_hardware_i2c::Device> i2c_;
  zx::interrupt irq_;
  async::IrqMethod<Da7219, &Da7219::HandleIrq> irq_handler_{this};

  // Plug state. Must reply to the first Watch request, if there is no plug state update before the
  // first Watch, reply with unplugged at time 0.
  bool plugged_ = false;
  zx::time plugged_time_;
  bool plug_state_updated_ = true;
  std::optional<WatchPlugStateCompleter::Async> plug_state_completer_;
  bool gain_state_replied_ = false;
  std::optional<WatchGainStateCompleter::Async> gain_state_completer_;
  std::optional<async::Loop> loop_;
  bool bound_ = false;
};

}  // namespace audio

#endif  // SRC_MEDIA_AUDIO_DRIVERS_CODECS_DA7219_DA7219_H_
