// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_LIGHT_DRIVERS_AML_LIGHT_AML_LIGHT_H_
#define SRC_UI_LIGHT_DRIVERS_AML_LIGHT_AML_LIGHT_H_

#include <fuchsia/hardware/gpio/cpp/banjo.h>
#include <fuchsia/hardware/light/llcpp/fidl.h>
#include <fuchsia/hardware/pwm/cpp/banjo.h>
#include <lib/ddk/debug.h>
#include <threads.h>

#include <optional>
#include <string>
#include <vector>

#include <ddktl/device.h>
#include <ddktl/protocol/empty-protocol.h>
#include <fbl/array.h>
#include <soc/aml-common/aml-pwm-regs.h>

namespace aml_light {

class AmlLight;
using AmlLightType = ddk::Device<AmlLight, ddk::Messageable>;
using fuchsia_hardware_light::Light;
using fuchsia_hardware_light::wire::Capability;
using fuchsia_hardware_light::wire::LightError;
using fuchsia_hardware_light::wire::Rgb;

class LightDevice {
 public:
  LightDevice(std::string name, ddk::GpioProtocolClient gpio,
              std::optional<ddk::PwmProtocolClient> pwm)
      : name_(std::move(name)), gpio_(gpio), pwm_(pwm) {}

  zx_status_t Init(bool init_on);

  const std::string GetName() const { return name_; }
  Capability GetCapability() const {
    return pwm_.has_value() ? Capability::BRIGHTNESS : Capability::SIMPLE;
  }
  bool GetCurrentSimpleValue() const { return (value_ != 0); }
  zx_status_t SetSimpleValue(bool value);
  double GetCurrentBrightnessValue() const { return value_; }
  zx_status_t SetBrightnessValue(double value);

 private:
  std::string name_;
  ddk::GpioProtocolClient gpio_;
  std::optional<ddk::PwmProtocolClient> pwm_;

  double value_ = 0;
};

class AmlLight : public AmlLightType,
                 public fidl::WireInterface<Light>,
                 public ddk::EmptyProtocol<ZX_PROTOCOL_LIGHT> {
 public:
  explicit AmlLight(zx_device_t* parent) : AmlLightType(parent) {}

  static zx_status_t Create(void* ctx, zx_device_t* parent);

  // Device protocol implementation.
  zx_status_t DdkMessage(fidl_incoming_msg_t* msg, fidl_txn_t* txn);
  void DdkRelease();

  // FIDL messages.
  void GetNumLights(GetNumLightsCompleter::Sync& completer);
  void GetNumLightGroups(GetNumLightGroupsCompleter::Sync& completer);
  void GetInfo(uint32_t index, GetInfoCompleter::Sync& completer);
  void GetCurrentSimpleValue(uint32_t index, GetCurrentSimpleValueCompleter::Sync& completer);
  void SetSimpleValue(uint32_t index, bool value, SetSimpleValueCompleter::Sync& completer);
  void GetCurrentBrightnessValue(uint32_t index,
                                 GetCurrentBrightnessValueCompleter::Sync& completer);
  void SetBrightnessValue(uint32_t index, double value,
                          SetBrightnessValueCompleter::Sync& completer);
  void GetCurrentRgbValue(uint32_t index, GetCurrentRgbValueCompleter::Sync& completer);
  void SetRgbValue(uint32_t index, Rgb value, SetRgbValueCompleter::Sync& completer);

  void GetGroupInfo(uint32_t group_id, GetGroupInfoCompleter::Sync& completer) {
    completer.ReplyError(LightError::NOT_SUPPORTED);
  }
  void GetGroupCurrentSimpleValue(uint32_t group_id,
                                  GetGroupCurrentSimpleValueCompleter::Sync& completer) {
    completer.ReplyError(LightError::NOT_SUPPORTED);
  }
  void SetGroupSimpleValue(uint32_t group_id, ::fidl::VectorView<bool> values,
                           SetGroupSimpleValueCompleter::Sync& completer) {
    completer.ReplyError(LightError::NOT_SUPPORTED);
  }
  void GetGroupCurrentBrightnessValue(uint32_t group_id,
                                      GetGroupCurrentBrightnessValueCompleter::Sync& completer) {
    completer.ReplyError(LightError::NOT_SUPPORTED);
  }
  void SetGroupBrightnessValue(uint32_t group_id, ::fidl::VectorView<double> values,
                               SetGroupBrightnessValueCompleter::Sync& completer) {
    completer.ReplyError(LightError::NOT_SUPPORTED);
  }
  void GetGroupCurrentRgbValue(uint32_t group_id,
                               GetGroupCurrentRgbValueCompleter::Sync& completer) {
    completer.ReplyError(LightError::NOT_SUPPORTED);
  }
  void SetGroupRgbValue(uint32_t group_id, ::fidl::VectorView<Rgb> values,
                        SetGroupRgbValueCompleter::Sync& completer) {
    completer.ReplyError(LightError::NOT_SUPPORTED);
  }

 private:
  DISALLOW_COPY_ASSIGN_AND_MOVE(AmlLight);
  friend class FakeAmlLight;

  zx_status_t Init();

  static constexpr size_t kNameLength = ZX_MAX_NAME_LEN;

  std::vector<LightDevice> lights_;
};

}  // namespace aml_light

#endif  // SRC_UI_LIGHT_DRIVERS_AML_LIGHT_AML_LIGHT_H_
