// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "gpio-light.h"

#include <assert.h>
#include <lib/ddk/platform-defs.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <threads.h>
#include <unistd.h>
#include <zircon/assert.h>
#include <zircon/process.h>
#include <zircon/syscalls.h>

#include <memory>

#include <ddk/debug.h>
#include <ddk/metadata.h>
#include <ddktl/fidl.h>
#include <fbl/alloc_checker.h>

#include "src/ui/light/drivers/gpio-light/gpio-light-bind.h"

namespace gpio_light {

void GpioLight::GetNumLights(GetNumLightsCompleter::Sync& completer) {
  completer.Reply(gpio_count_);
}

void GpioLight::GetNumLightGroups(GetNumLightGroupsCompleter::Sync& completer) {
  completer.Reply(0);
}

void GpioLight::GetInfo(uint32_t index, GetInfoCompleter::Sync& completer) {
  if (index >= gpio_count_) {
    completer.ReplyError(fuchsia_hardware_light::wire::LightError::INVALID_INDEX);
    return;
  }

  char name[20];
  if (names_.size() > 0) {
    snprintf(name, sizeof(name), "%s\n", names_.data() + index * kNameLength);
  } else {
    // Return "gpio-X" if no metadata was provided.
    snprintf(name, sizeof(name), "gpio-%u\n", index);
  }

  completer.ReplySuccess({
      .name = ::fidl::StringView(name, strlen(name)),
      .capability = fuchsia_hardware_light::wire::Capability::SIMPLE,
  });
}

void GpioLight::GetCurrentSimpleValue(uint32_t index,
                                      GetCurrentSimpleValueCompleter::Sync& completer) {
  if (index >= gpio_count_) {
    completer.ReplyError(fuchsia_hardware_light::wire::LightError::INVALID_INDEX);
    return;
  }

  uint8_t value;
  if (gpios_[index].Read(&value) != ZX_OK) {
    completer.ReplyError(fuchsia_hardware_light::wire::LightError::FAILED);
  } else {
    completer.ReplySuccess(value);
  }
}

void GpioLight::SetSimpleValue(uint32_t index, bool value,
                               SetSimpleValueCompleter::Sync& completer) {
  if (index >= gpio_count_) {
    completer.ReplyError(fuchsia_hardware_light::wire::LightError::INVALID_INDEX);
    return;
  }

  if (gpios_[index].Write(value) != ZX_OK) {
    completer.ReplyError(fuchsia_hardware_light::wire::LightError::FAILED);
  } else {
    completer.ReplySuccess();
  }
}

void GpioLight::GetCurrentBrightnessValue(uint32_t index,
                                          GetCurrentBrightnessValueCompleter::Sync& completer) {
  completer.ReplyError(fuchsia_hardware_light::wire::LightError::NOT_SUPPORTED);
}

void GpioLight::SetBrightnessValue(uint32_t index, double value,
                                   SetBrightnessValueCompleter::Sync& completer) {
  completer.ReplyError(fuchsia_hardware_light::wire::LightError::NOT_SUPPORTED);
}

void GpioLight::GetCurrentRgbValue(uint32_t index, GetCurrentRgbValueCompleter::Sync& completer) {
  completer.ReplyError(fuchsia_hardware_light::wire::LightError::NOT_SUPPORTED);
}

void GpioLight::SetRgbValue(uint32_t index, fuchsia_hardware_light::wire::Rgb value,
                            SetRgbValueCompleter::Sync& completer) {
  completer.ReplyError(fuchsia_hardware_light::wire::LightError::NOT_SUPPORTED);
}

zx_status_t GpioLight::DdkMessage(fidl_incoming_msg_t* msg, fidl_txn_t* txn) {
  DdkTransaction transaction(txn);
  fuchsia_hardware_light::Light::Dispatch(this, msg, &transaction);
  return transaction.Status();
}

void GpioLight::DdkRelease() { delete this; }

zx_status_t GpioLight::Create(void* ctx, zx_device_t* parent) {
  fbl::AllocChecker ac;
  auto dev = std::unique_ptr<GpioLight>(new (&ac) GpioLight(parent));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }

  auto status = dev->Init();
  if (status != ZX_OK) {
    return status;
  }

  // devmgr is now in charge of the device.
  __UNUSED auto* dummy = dev.release();
  return ZX_OK;
}

zx_status_t GpioLight::Init() {
  auto fragment_count = DdkGetFragmentCount();
  if (fragment_count <= 0) {
    return ZX_ERR_INTERNAL;
  }
  // fragment 0 is platform device, only used for passing metadata.
  gpio_count_ = fragment_count - 1;

  size_t metadata_size;
  size_t expected = gpio_count_ * kNameLength;
  auto status = device_get_metadata_size(parent(), DEVICE_METADATA_NAME, &metadata_size);
  if (status == ZX_OK) {
    if (expected != metadata_size) {
      zxlogf(ERROR, "%s: expected metadata size %zu, got %zu", __func__, expected, metadata_size);
      status = ZX_ERR_INTERNAL;
    }
  }
  if (status == ZX_OK) {
    fbl::AllocChecker ac;
    names_.reset(new (&ac) char[metadata_size], metadata_size);
    if (!ac.check()) {
      return ZX_ERR_NO_MEMORY;
    }

    status = device_get_metadata(parent(), DEVICE_METADATA_NAME, names_.data(), metadata_size,
                                 &expected);
    if (status != ZX_OK) {
      return status;
    }
  }

  composite_device_fragment_t fragments[fragment_count];
  size_t actual;
  DdkGetFragments(fragments, fragment_count, &actual);
  if (actual != fragment_count) {
    return ZX_ERR_INTERNAL;
  }

  fbl::AllocChecker ac;
  auto* gpios = new (&ac) ddk::GpioProtocolClient[gpio_count_];
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  gpios_.reset(gpios, gpio_count_);

  for (uint32_t i = 0; i < gpio_count_; i++) {
    auto* gpio = &gpios_[i];
    auto status = device_get_protocol(fragments[i + 1].device, ZX_PROTOCOL_GPIO, gpio);
    if (status != ZX_OK) {
      return status;
    }
    status = gpio->ConfigOut(0);
    if (status != ZX_OK) {
      zxlogf(ERROR, "gpio-light: ConfigOut failed for gpio %u", i);
      return status;
    }
  }

  return DdkAdd("gpio-light", DEVICE_ADD_NON_BINDABLE);
}

static constexpr zx_driver_ops_t driver_ops = []() {
  zx_driver_ops_t ops = {};
  ops.version = DRIVER_OPS_VERSION;
  ops.bind = GpioLight::Create;
  return ops;
}();

}  // namespace gpio_light

ZIRCON_DRIVER(gpio_light, gpio_light::driver_ops, "zircon", "0.1");
