// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "sgm37603a.h"

#include <lib/ddk/platform-defs.h>

#include <algorithm>
#include <memory>

#include <ddk/debug.h>
#include <ddktl/fidl.h>
#include <fbl/algorithm.h>
#include <fbl/alloc_checker.h>

#include "src/ui/backlight/drivers/sg-micro/sgm37603a-bind.h"

namespace {

constexpr int64_t kEnableSleepTimeMs = 20;

}  // namespace

namespace backlight {

zx_status_t Sgm37603a::Create(void* ctx, zx_device_t* parent) {
  ddk::I2cChannel i2c(parent, "i2c");
  if (!i2c.is_valid()) {
    zxlogf(ERROR, "%s: could not get protocol ZX_PROTOCOL_I2C", __FILE__);
    return ZX_ERR_NO_RESOURCES;
  }

  ddk::GpioProtocolClient reset_gpio(parent, "gpio");
  if (!reset_gpio.is_valid()) {
    zxlogf(ERROR, "%s: could not get protocol ZX_PROTOCOL_GPIO", __FILE__);
    return ZX_ERR_NO_RESOURCES;
  }

  fbl::AllocChecker ac;
  std::unique_ptr<Sgm37603a> device(new (&ac) Sgm37603a(parent, i2c, reset_gpio));
  if (!ac.check()) {
    zxlogf(ERROR, "%s: Sgm37603a alloc failed", __FILE__);
    return ZX_ERR_NO_MEMORY;
  }

  zx_status_t status = device->SetBacklightState(true, 1.0);
  if (status != ZX_OK) {
    return status;
  }

  if ((status = device->DdkAdd("sgm37603a")) != ZX_OK) {
    zxlogf(ERROR, "%s: DdkAdd failed", __FILE__);
    return status;
  }

  __UNUSED auto* dummy = device.release();

  return ZX_OK;
}

zx_status_t Sgm37603a::EnableBacklight() {
  zx_status_t status = reset_gpio_.ConfigOut(1);
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s: Failed to enable backlight driver", __FILE__);
    return status;
  }

  zx::nanosleep(zx::deadline_after(zx::msec(kEnableSleepTimeMs)));

  for (size_t i = 0; i < countof(kDefaultRegValues); i++) {
    status = i2c_.WriteSync(kDefaultRegValues[i], sizeof(kDefaultRegValues[i]));
    if (status != ZX_OK) {
      zxlogf(ERROR, "%s: Failed to configure backlight driver", __FILE__);
      return status;
    }
  }

  return ZX_OK;
}

zx_status_t Sgm37603a::DisableBacklight() {
  zx_status_t status = reset_gpio_.ConfigOut(0);
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s: Failed to disable backlight driver", __FILE__);
    return status;
  }

  return ZX_OK;
}

void Sgm37603a::GetStateNormalized(GetStateNormalizedCompleter::Sync& completer) {
  FidlBacklight::wire::State state = {};
  auto status = GetBacklightState(&state.backlight_on, &state.brightness);
  if (status == ZX_OK) {
    completer.ReplySuccess(state);
  } else {
    completer.ReplyError(status);
  }
}

void Sgm37603a::SetStateNormalized(FidlBacklight::wire::State state,
                                   SetStateNormalizedCompleter::Sync& completer) {
  auto status = SetBacklightState(state.backlight_on, state.brightness);
  if (status == ZX_OK) {
    completer.ReplySuccess();
  } else {
    completer.ReplyError(status);
  }
}

void Sgm37603a::GetStateAbsolute(GetStateAbsoluteCompleter::Sync& completer) {
  completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
}

void Sgm37603a::SetStateAbsolute(FidlBacklight::wire::State state,
                                 SetStateAbsoluteCompleter::Sync& completer) {
  completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
}

void Sgm37603a::GetMaxAbsoluteBrightness(GetMaxAbsoluteBrightnessCompleter::Sync& completer) {
  completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
}

void Sgm37603a::SetNormalizedBrightnessScale(
    __UNUSED double scale, SetNormalizedBrightnessScaleCompleter::Sync& completer) {
  completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
}

void Sgm37603a::GetNormalizedBrightnessScale(
    GetNormalizedBrightnessScaleCompleter::Sync& completer) {
  completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
}

zx_status_t Sgm37603a::DdkMessage(fidl_incoming_msg_t* msg, fidl_txn_t* txn) {
  DdkTransaction transaction(txn);
  FidlBacklight::Device::Dispatch(this, msg, &transaction);
  return transaction.Status();
}

zx_status_t Sgm37603a::GetBacklightState(bool* power, double* brightness) {
  *power = enabled_;
  *brightness = brightness_;
  return ZX_OK;
}

zx_status_t Sgm37603a::SetBacklightState(bool power, double brightness) {
  if (!power) {
    enabled_ = false;
    brightness_ = 0;

    return DisableBacklight();
  } else if (!enabled_) {
    enabled_ = true;

    zx_status_t status = EnableBacklight();
    if (status != ZX_OK) {
      return status;
    }
  }

  brightness = std::max(brightness, 0.0);
  brightness = std::min(brightness, 1.0);

  uint16_t brightness_value = static_cast<uint16_t>(brightness * kMaxBrightnessRegValue);
  const uint8_t brightness_regs[][2] = {
      {kBrightnessLsb, static_cast<uint8_t>(brightness_value & kBrightnessLsbMask)},
      {kBrightnessMsb, static_cast<uint8_t>(brightness_value >> kBrightnessLsbBits)},
  };

  for (size_t i = 0; i < countof(brightness_regs); i++) {
    zx_status_t status = i2c_.WriteSync(brightness_regs[i], sizeof(brightness_regs[i]));
    if (status != ZX_OK) {
      zxlogf(ERROR, "%s: Failed to set brightness register", __FILE__);
      return status;
    }
  }

  brightness_ = brightness;
  return ZX_OK;
}

}  // namespace backlight

static constexpr zx_driver_ops_t sgm37603a_driver_ops = []() {
  zx_driver_ops_t ops = {};
  ops.version = DRIVER_OPS_VERSION;
  ops.bind = backlight::Sgm37603a::Create;
  return ops;
}();

ZIRCON_DRIVER(sgm37603a, sgm37603a_driver_ops, "zircon", "0.1");
