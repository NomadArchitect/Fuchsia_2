// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "lp50xx-light.h"

#include <assert.h>
#include <lib/ddk/platform-defs.h>
#include <lib/device-protocol/pdev.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <threads.h>
#include <unistd.h>
#include <zircon/assert.h>
#include <zircon/process.h>
#include <zircon/syscalls.h>

#include <cmath>
#include <memory>

#include <ddk/debug.h>
#include <ddk/metadata.h>
#include <ddk/metadata/lights.h>
#include <ddktl/fidl.h>
#include <fbl/alloc_checker.h>

#include "lp50xx-regs.h"
#include "src/ui/light/drivers/lp50xx-light/lp50xx_light-bind.h"

namespace lp50xx_light {

static bool run_blink_test(void* ctx, zx_device_t* parent, zx_handle_t channel) {
  fbl::AllocChecker ac;
  auto dev = std::unique_ptr<Lp50xxLight>(new (&ac) Lp50xxLight(parent));
  if (!ac.check()) {
    return false;
  }
  auto status = dev->Init();
  if (status != ZX_OK) {
    return false;
  }
  return dev->BlinkTest();
}

bool Lp50xxLight::BlinkTest() {
  zx_status_t status;
  fuchsia_hardware_light::wire::Rgb rgb = {};

  for (uint32_t led = 0; led < led_count_; led++) {
    // incrementing color in steps of 16 to reduce time taken for the test
    for (uint32_t red = 0; red <= 0xff; red += 16) {
      for (uint32_t green = 0; green <= 0xff; green += 16) {
        for (uint32_t blue = 0; blue <= 0xff; blue += 16) {
          rgb.red = static_cast<float>(red) / static_cast<float>(UINT8_MAX);
          rgb.blue = static_cast<float>(blue) / static_cast<float>(UINT8_MAX);
          rgb.green = static_cast<float>(green) / static_cast<float>(UINT8_MAX);
          status = SetRgbValue(led, rgb);
          if (status != ZX_OK) {
            zxlogf(ERROR, "%s: Failed to set color R:%d G:%d B:%d", __func__, red, green, blue);
          }
          status = GetRgbValue(led, &rgb);
          if (status != ZX_OK) {
            zxlogf(ERROR, "%s: Failed to get color R:%d G:%d B:%d", __func__, red, green, blue);
          }
        }
      }
    }
  }

  for (uint32_t i = 0; i < led_count_; i++) {
    rgb.red = 0.0;
    rgb.green = 0.0;
    rgb.blue = 0.0;
    status = SetRgbValue(i, rgb);
    if (status != ZX_OK) {
      zxlogf(ERROR, "%s: Failed to reset color", __PRETTY_FUNCTION__);
    }
  }

  zxlogf(INFO, "Lp50xxLight Blink test complete");
  return ZX_OK;
}

zx_status_t Lp50xxLight::Lp50xxRegConfig() {
  uint32_t led_count = 0;
  switch (pid_) {
    case PDEV_PID_TI_LP5018:
      led_count = 6;
      led_color_addr_ = 0x0f;
      reset_addr_ = 0x27;
      break;
    case PDEV_PID_TI_LP5024:
      led_count = 8;
      led_color_addr_ = 0x0f;
      reset_addr_ = 0x27;
      break;
    case PDEV_PID_TI_LP5030:
      led_count = 10;
      led_color_addr_ = 0x14;
      reset_addr_ = 0x38;
      break;
    case PDEV_PID_TI_LP5036:
      led_count = 12;
      led_color_addr_ = 0x14;
      reset_addr_ = 0x38;
      break;
    default:
      zxlogf(ERROR, "%s: unsupported PID %u", __func__, pid_);
      return ZX_ERR_NOT_SUPPORTED;
  }
  if (led_count != led_count_) {
    zxlogf(ERROR, "%s: incorrect number of LEDs %u != %u", __func__, led_count_, led_count);
    return ZX_ERR_INTERNAL;
  }

  return ZX_OK;
}

zx_status_t Lp50xxLight::SetRgbValue(uint32_t index, fuchsia_hardware_light::wire::Rgb rgb) {
  if ((rgb.red > 1.0) || (rgb.green > 1.0) || (rgb.blue > 1.0) || (rgb.red < 0.0) ||
      (rgb.green < 0.0) || (rgb.blue < 0.0) || std::isnan(rgb.red) || std::isnan(rgb.green) ||
      std::isnan(rgb.blue)) {
    return ZX_ERR_INVALID_ARGS;
  }

  auto status = RedColorReg::Get(led_color_addr_, index)
                    .FromValue(static_cast<uint8_t>(rgb.red * UINT8_MAX))
                    .WriteTo(i2c_);
  if (status != ZX_OK) {
    return status;
  }

  status = GreenColorReg::Get(led_color_addr_, index)
               .FromValue(static_cast<uint8_t>(rgb.green * UINT8_MAX))
               .WriteTo(i2c_);
  if (status != ZX_OK) {
    return status;
  }

  status = BlueColorReg::Get(led_color_addr_, index)
               .FromValue(static_cast<uint8_t>(rgb.blue * UINT8_MAX))
               .WriteTo(i2c_);
  if (status != ZX_OK) {
    return status;
  }

  return ZX_OK;
}

zx_status_t Lp50xxLight::GetRgbValue(uint32_t index, fuchsia_hardware_light::wire::Rgb* rgb) {
  auto red = RedColorReg::Get(led_color_addr_, index).FromValue(0);
  auto green = GreenColorReg::Get(led_color_addr_, index).FromValue(0);
  auto blue = BlueColorReg::Get(led_color_addr_, index).FromValue(0);

  if (red.ReadFrom(i2c_) || green.ReadFrom(i2c_) || blue.ReadFrom(i2c_)) {
    zxlogf(ERROR, "Failed to read I2C color registers");
    return ZX_ERR_INTERNAL;
  }

  rgb->red = static_cast<float>(red.reg_value()) / static_cast<float>(UINT8_MAX);
  rgb->green = static_cast<float>(green.reg_value()) / static_cast<float>(UINT8_MAX);
  rgb->blue = static_cast<float>(blue.reg_value()) / static_cast<float>(UINT8_MAX);

  return ZX_OK;
}

void Lp50xxLight::GetNumLights(GetNumLightsCompleter::Sync& completer) {
  completer.Reply(led_count_);
}

void Lp50xxLight::GetNumLightGroups(GetNumLightGroupsCompleter::Sync& completer) {
  completer.Reply(static_cast<uint32_t>(group_names_.size()));
}

void Lp50xxLight::GetInfo(uint32_t index, GetInfoCompleter::Sync& completer) {
  if (index >= led_count_) {
    completer.ReplyError(fuchsia_hardware_light::wire::LightError::INVALID_INDEX);
    return;
  }

  char name[kNameLength];
  if (names_.size() > 0) {
    // TODO(puneetha): Currently names_ is not set from metadata. This code will not be executed.
    snprintf(name, sizeof(name), "%s\n", names_[index]);
  } else {
    // Return "lp50xx-led-X" if no metadata was provided.
    snprintf(name, sizeof(name), "lp50xx-led-%u\n", index);
  }

  completer.ReplySuccess({
      .name = ::fidl::StringView(name, strlen(name)),
      .capability = fuchsia_hardware_light::wire::Capability::RGB,
  });

  return;
}

void Lp50xxLight::GetCurrentSimpleValue(uint32_t index,
                                        GetCurrentSimpleValueCompleter::Sync& completer) {
  completer.ReplyError(fuchsia_hardware_light::wire::LightError::NOT_SUPPORTED);
}

void Lp50xxLight::SetSimpleValue(uint32_t index, bool value,
                                 SetSimpleValueCompleter::Sync& completer) {
  completer.ReplyError(fuchsia_hardware_light::wire::LightError::NOT_SUPPORTED);
}

void Lp50xxLight::GetCurrentBrightnessValue(uint32_t index,
                                            GetCurrentBrightnessValueCompleter::Sync& completer) {
  completer.ReplyError(fuchsia_hardware_light::wire::LightError::NOT_SUPPORTED);
}

void Lp50xxLight::SetBrightnessValue(uint32_t index, double value,
                                     SetBrightnessValueCompleter::Sync& completer) {
  completer.ReplyError(fuchsia_hardware_light::wire::LightError::NOT_SUPPORTED);
}

void Lp50xxLight::GetCurrentRgbValue(uint32_t index, GetCurrentRgbValueCompleter::Sync& completer) {
  fuchsia_hardware_light::wire::Rgb rgb = {};
  if (index >= led_count_) {
    completer.ReplyError(fuchsia_hardware_light::wire::LightError::INVALID_INDEX);
    return;
  }

  if (GetRgbValue(index, &rgb) != ZX_OK) {
    completer.ReplyError(fuchsia_hardware_light::wire::LightError::FAILED);
  } else {
    completer.ReplySuccess(rgb);
  }
}

void Lp50xxLight::SetRgbValue(uint32_t index, fuchsia_hardware_light::wire::Rgb value,
                              SetRgbValueCompleter::Sync& completer) {
  if (index >= led_count_) {
    completer.ReplyError(fuchsia_hardware_light::wire::LightError::INVALID_INDEX);
    return;
  }

  if (SetRgbValue(index, value) != ZX_OK) {
    completer.ReplyError(fuchsia_hardware_light::wire::LightError::FAILED);
  } else {
    completer.ReplySuccess();
  }
}

void Lp50xxLight::GetGroupInfo(uint32_t group_id, GetGroupInfoCompleter::Sync& completer) {
  if (group_id >= group2led_.size()) {
    completer.ReplyError(fuchsia_hardware_light::wire::LightError::INVALID_INDEX);
    return;
  }

  char name[kNameLength];
  if (group_names_.size() > 0) {
    snprintf(name, sizeof(name), "%s\n", &group_names_[group_id * kNameLength]);
  } else {
    // Return "led-group-X" if no metadata was provided.
    snprintf(name, sizeof(name), "led-group-%u\n", group_id);
  }

  completer.ReplySuccess({
      .name = ::fidl::StringView(name, strlen(name)),
      .count = static_cast<uint32_t>(group2led_[group_id].size()),
      .capability = fuchsia_hardware_light::wire::Capability::RGB,
  });
}

void Lp50xxLight::GetGroupCurrentSimpleValue(uint32_t group_id,
                                             GetGroupCurrentSimpleValueCompleter::Sync& completer) {
  completer.ReplyError(fuchsia_hardware_light::wire::LightError::NOT_SUPPORTED);
}

void Lp50xxLight::SetGroupSimpleValue(uint32_t group_id, ::fidl::VectorView<bool> values,
                                      SetGroupSimpleValueCompleter::Sync& completer) {
  completer.ReplyError(fuchsia_hardware_light::wire::LightError::NOT_SUPPORTED);
}

void Lp50xxLight::GetGroupCurrentBrightnessValue(
    uint32_t group_id, GetGroupCurrentBrightnessValueCompleter::Sync& completer) {
  completer.ReplyError(fuchsia_hardware_light::wire::LightError::NOT_SUPPORTED);
}

void Lp50xxLight::SetGroupBrightnessValue(uint32_t group_id, ::fidl::VectorView<double> values,
                                          SetGroupBrightnessValueCompleter::Sync& completer) {
  completer.ReplyError(fuchsia_hardware_light::wire::LightError::NOT_SUPPORTED);
}

void Lp50xxLight::GetGroupCurrentRgbValue(uint32_t group_id,
                                          GetGroupCurrentRgbValueCompleter::Sync& completer) {
  ::fidl::VectorView<fuchsia_hardware_light::wire::Rgb> empty(nullptr, 0);
  if (group_id >= group2led_.size()) {
    completer.ReplyError(fuchsia_hardware_light::wire::LightError::INVALID_INDEX);
    return;
  }

  zx_status_t status = ZX_OK;
  std::vector<fuchsia_hardware_light::wire::Rgb> out;
  for (auto led : group2led_[group_id]) {
    if (led >= led_count_) {
      completer.ReplyError(fuchsia_hardware_light::wire::LightError::INVALID_INDEX);
      return;
    }

    fuchsia_hardware_light::wire::Rgb rgb = {};
    if ((status = GetRgbValue(led, &rgb)) != ZX_OK) {
      break;
    }
    out.emplace_back(rgb);
  }

  if (status != ZX_OK) {
    completer.ReplyError(fuchsia_hardware_light::wire::LightError::FAILED);
  } else {
    completer.ReplySuccess(::fidl::VectorView<fuchsia_hardware_light::wire::Rgb>(
        fidl::unowned_ptr(out.data()), out.size()));
  }
}

void Lp50xxLight::SetGroupRgbValue(uint32_t group_id,
                                   ::fidl::VectorView<fuchsia_hardware_light::wire::Rgb> values,
                                   SetGroupRgbValueCompleter::Sync& completer) {
  if (group_id >= group2led_.size()) {
    completer.ReplyError(fuchsia_hardware_light::wire::LightError::INVALID_INDEX);
    return;
  }

  zx_status_t status = ZX_OK;
  for (uint32_t i = 0; i < group2led_[group_id].size(); i++) {
    if (group2led_[group_id][i] >= led_count_) {
      completer.ReplyError(fuchsia_hardware_light::wire::LightError::INVALID_INDEX);
      return;
    }

    if ((status = SetRgbValue(group2led_[group_id][i], values[i])) != ZX_OK) {
      break;
    }
  }
  if (status != ZX_OK) {
    completer.ReplyError(fuchsia_hardware_light::wire::LightError::FAILED);
  } else {
    completer.ReplySuccess();
  }
}

zx_status_t Lp50xxLight::DdkMessage(fidl_incoming_msg_t* msg, fidl_txn_t* txn) {
  DdkTransaction transaction(txn);
  fuchsia_hardware_light::Light::Dispatch(this, msg, &transaction);
  return transaction.Status();
}

void Lp50xxLight::DdkRelease() { delete this; }

zx_status_t Lp50xxLight::InitHelper() {
  // Get Pdev and I2C protocol.
  ddk::I2cProtocolClient i2c(parent(), "i2c");
  if (!i2c.is_valid()) {
    zxlogf(ERROR, "ZX_PROTOCOL_I2C not found");
    return ZX_ERR_NO_RESOURCES;
  }

  auto pdev = ddk::PDev::FromFragment(parent());
  if (!pdev.is_valid()) {
    zxlogf(ERROR, "%s: Get PBusProtocolClient failed", __func__);
    return ZX_ERR_NO_RESOURCES;
  }

  pdev_device_info info = {};
  zx_status_t status = pdev.GetDeviceInfo(&info);
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s: GetDeviceInfo failed: %d", __func__, status);
    return status;
  }
  pid_ = info.pid;
  i2c_ = std::move(i2c);

  fbl::AllocChecker ac;
  size_t metadata_size;
  if ((status = device_get_metadata_size(parent(), DEVICE_METADATA_LIGHTS, &metadata_size)) !=
      ZX_OK) {
    zxlogf(ERROR, "%s: couldn't get metadata size", __func__);
    return status;
  }
  auto configs =
      fbl::Array<lights_config_t>(new (&ac) lights_config_t[metadata_size], metadata_size);
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  size_t actual = 0;
  if ((status = device_get_metadata(parent(), DEVICE_METADATA_LIGHTS, configs.data(), metadata_size,
                                    &actual)) != ZX_OK) {
    return status;
  }
  if ((actual != metadata_size) || (actual % sizeof(lights_config_t) != 0)) {
    zxlogf(ERROR, "%s: wrong metadata size", __func__);
    return ZX_ERR_INVALID_ARGS;
  }
  led_count_ = static_cast<uint32_t>(metadata_size) / sizeof(lights_config_t);

  for (uint32_t i = 0; i < led_count_; i++) {
    group2led_[configs[i].group_id].push_back(i);
  }

  if ((status = device_get_metadata_size(parent(), DEVICE_METADATA_LIGHTS_GROUP_NAME,
                                         &metadata_size)) != ZX_OK) {
    zxlogf(ERROR, "%s: couldn't get metadata size", __func__);
    return status;
  }
  group_names_ = fbl::Array<char>(new (&ac) char[metadata_size], metadata_size);
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  if ((status = device_get_metadata(parent(), DEVICE_METADATA_LIGHTS_GROUP_NAME,
                                    group_names_.data(), metadata_size, &actual)) != ZX_OK) {
    return status;
  }
  if ((actual != metadata_size) || (actual % kNameLength != 0) ||
      static_cast<uint32_t>(metadata_size / kNameLength) != group2led_.size()) {
    zxlogf(ERROR, "%s: wrong metadata size", __func__);
    return ZX_ERR_INVALID_ARGS;
  }

  return ZX_OK;
}

zx_status_t Lp50xxLight::Init() {
  InitHelper();

  // Set device specific register configuration
  zx_status_t status = Lp50xxRegConfig();
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s: Device register configuration failed %d", __func__, status);
    return status;
  }

  // Enable device.
  auto dev_conf0 = DeviceConfig0Reg::Get().FromValue(0);
  dev_conf0.set_chip_enable(1);

  status = dev_conf0.WriteTo(i2c_);
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s: Device enable failed %d", __func__, status);
    return status;
  }

  // Set Log_Scale_EN, Power_save_EN, Auto_incr_EN and PWm_Dithering_EN
  auto dev_conf1 = DeviceConfig1Reg::Get().FromValue(0);
  dev_conf1.set_log_scale_enable(1);
  dev_conf1.set_power_save_enable(1);
  dev_conf1.set_auto_incr_enable(1);
  dev_conf1.set_pwm_dithering_enable(1);
  status = dev_conf1.WriteTo(i2c_);
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s: Device conf1 failed %d", __func__, status);
    return status;
  }

  return ZX_OK;
}

zx_status_t Lp50xxLight::Create(void* ctx, zx_device_t* parent) {
  fbl::AllocChecker ac;
  auto dev = std::unique_ptr<Lp50xxLight>(new (&ac) Lp50xxLight(parent));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  auto status = dev->Init();
  if (status != ZX_OK) {
    return status;
  }

  status = dev->DdkAdd("lp50xx-light", DEVICE_ADD_NON_BINDABLE);
  if (status != ZX_OK) {
    return status;
  }

  // devmgr is now in charge of the device.
  __UNUSED auto* dummy = dev.release();
  return ZX_OK;
}

static zx_driver_ops_t driver_ops = {
    .version = DRIVER_OPS_VERSION,
    .init = nullptr,
    .bind = Lp50xxLight::Create,
    .create = nullptr,
    .release = nullptr,
    .run_unit_tests = run_blink_test,
};

}  // namespace lp50xx_light

// clang-format off
ZIRCON_DRIVER(lp50xx_light, lp50xx_light::driver_ops, "zircon", "0.1");

//clang-format on
