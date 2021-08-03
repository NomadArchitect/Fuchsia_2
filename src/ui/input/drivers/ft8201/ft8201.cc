// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "ft8201.h"

#include <endian.h>
#include <lib/ddk/debug.h>
#include <lib/ddk/platform-defs.h>
#include <lib/zx/profile.h>
#include <threads.h>
#include <zircon/threads.h>

#include <ddktl/fidl.h>
#include <fbl/auto_lock.h>

#include "src/ui/input/drivers/ft8201/ft8201-bind.h"

namespace {

// TODO(bradenkell): Double-check these values.
constexpr int64_t kMaxContactX = 1279;
constexpr int64_t kMaxContactY = 799;
constexpr int64_t kMaxContactPressure = 0xff;

// Registers and possible values
constexpr uint8_t kContactsReg = 0x02;
constexpr uint8_t kContactsStartReg = 0x03;
constexpr size_t kContactSize = 6;

constexpr uint8_t kFlashStatusReg = 0x6a;
constexpr uint16_t kFlashEccDone = 0xf055;
constexpr uint16_t kFlashEraseDone = 0xf0aa;

constexpr uint8_t kFirmwareEccReg = 0x66;

constexpr uint8_t kBootIdReg = 0x90;
constexpr int kGetBootIdRetries = 10;
constexpr zx::duration kBootIdWaitAfterUnlock = zx::msec(12);

constexpr uint16_t kRombootId = 0x8006;
constexpr uint16_t kPrambootId = 0x80c6;

constexpr uint8_t kChipCoreReg = 0xa3;
constexpr uint8_t kChipCoreFirmwareValid = 0x82;

constexpr uint8_t kFirmwareVersionReg = 0xa6;

constexpr uint8_t kPrambootEccReg = 0xcc;

constexpr uint8_t kWorkModeReg = 0xfc;
constexpr uint8_t kWorkModeSoftwareReset1 = 0xaa;
constexpr uint8_t kWorkModeSoftwareReset2 = 0x55;

// Commands and parameters
constexpr uint8_t kResetCommand = 0x07;
constexpr uint8_t kStartPrambootCommand = 0x08;

constexpr uint8_t kFlashEraseCommand = 0x09;
constexpr uint8_t kFlashEraseAppArea = 0x0b;

constexpr uint8_t kUnlockBootCommand = 0x55;
constexpr uint8_t kFlashStatusCommand = 0x61;
constexpr uint8_t kEccInitializationCommand = 0x64;
constexpr uint8_t kEccCalculateCommand = 0x65;

// Pramboot/firmware download
constexpr size_t kFirmwareOffset = 0x5000;
constexpr size_t kFirmwareVersionOffset = 0x510e;

constexpr size_t kMaxPacketAddress = 0x00ff'ffff;
constexpr size_t kMaxPacketSize = 128;

constexpr size_t kMaxEraseSize = 0xfffe;

constexpr zx::duration EraseStatusSleep(const size_t firmware_size) {
  return zx::msec((firmware_size / 4096) * 60);
}

constexpr zx::duration CalculateEccSleep(const size_t check_size) {
  return zx::msec(check_size / 256);
}

constexpr uint16_t ExpectedWriteStatus(const uint32_t address, const size_t packet_size) {
  return 0x1000 + (address / packet_size);
}

}  // namespace

namespace touch {

void Ft8201InputReport::ToFidlInputReport(fuchsia_input_report::wire::InputReport& input_report,
                                          fidl::AnyArena& allocator) {
  fidl::VectorView<fuchsia_input_report::wire::ContactInputReport> input_contacts(allocator,
                                                                                  num_contacts);
  for (size_t i = 0; i < num_contacts; i++) {
    fuchsia_input_report::wire::ContactInputReport contact(allocator);
    contact.set_contact_id(allocator, contacts[i].contact_id);
    contact.set_position_x(allocator, contacts[i].position_x);
    contact.set_position_y(allocator, contacts[i].position_y);
    contact.set_pressure(allocator, contacts[i].pressure);
    input_contacts[i] = std::move(contact);
  }

  fuchsia_input_report::wire::TouchInputReport touch_report(allocator);
  touch_report.set_contacts(allocator, std::move(input_contacts));

  input_report.set_event_time(allocator, event_time.get());
  input_report.set_touch(allocator, std::move(touch_report));
}

zx::status<Ft8201Device*> Ft8201Device::CreateAndGetDevice(void* ctx, zx_device_t* parent) {
  ddk::I2cChannel i2c(parent, "i2c");
  if (!i2c.is_valid()) {
    zxlogf(ERROR, "Ft8201: Failed to get I2C fragment");
    return zx::error(ZX_ERR_NO_RESOURCES);
  }

  ddk::GpioProtocolClient interrupt_gpio(parent, "gpio-int");
  if (!interrupt_gpio.is_valid()) {
    zxlogf(ERROR, "Ft8201: Failed to get interrupt GPIO fragment");
    return zx::error(ZX_ERR_NO_RESOURCES);
  }

  ddk::GpioProtocolClient reset_gpio(parent, "gpio-reset");
  if (!reset_gpio.is_valid()) {
    zxlogf(ERROR, "Ft8201: Failed to get reset GPIO fragment");
    return zx::error(ZX_ERR_NO_RESOURCES);
  }

  std::unique_ptr<Ft8201Device> device =
      std::make_unique<Ft8201Device>(parent, i2c, interrupt_gpio, reset_gpio);
  if (!device) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  zx_status_t status = device->Init();
  if (status != ZX_OK) {
    return zx::error(status);
  }

  if ((status = device->DdkAdd("ft8201")) != ZX_OK) {
    zxlogf(ERROR, "Ft8201: DdkAdd failed: %d", status);
    return zx::error(status);
  }

  return zx::ok(device.release());
}

zx_status_t Ft8201Device::Create(void* ctx, zx_device_t* parent) {
  auto status = CreateAndGetDevice(ctx, parent);
  return status.is_error() ? status.error_value() : ZX_OK;
}

bool Ft8201Device::RunUnitTests(void* ctx, zx_device_t* parent, zx_handle_t channel) {
  ddk::I2cChannel i2c(parent, "i2c");
  if (!i2c.is_valid()) {
    zxlogf(ERROR, "Ft8201: Failed to get I2C fragment");
    return false;
  }

  std::unique_ptr<Ft8201Device> device = std::make_unique<Ft8201Device>(parent, i2c);
  if (!device) {
    return false;
  }

  return device->FirmwareDownloadIfNeeded() == ZX_OK;
}

void Ft8201Device::DdkUnbind(ddk::UnbindTxn txn) {
  Shutdown();
  txn.Reply();
}

void Ft8201Device::GetInputReportsReader(GetInputReportsReaderRequestView request,
                                         GetInputReportsReaderCompleter::Sync& completer) {
  zx_status_t status =
      input_report_readers_.CreateReader(loop_.dispatcher(), std::move(request->reader));
  if (status == ZX_OK) {
    sync_completion_signal(&next_reader_wait_);  // Only for tests.
  }
}

void Ft8201Device::GetDescriptor(GetDescriptorRequestView request,
                                 GetDescriptorCompleter::Sync& completer) {
  constexpr size_t kDescriptorBufferSize = 512;

  constexpr fuchsia_input_report::wire::Axis kAxisX = {
      .range = {.min = 0, .max = kMaxContactX},
      .unit = {.type = fuchsia_input_report::wire::UnitType::kNone, .exponent = 0},
  };

  constexpr fuchsia_input_report::wire::Axis kAxisY = {
      .range = {.min = 0, .max = kMaxContactY},
      .unit = {.type = fuchsia_input_report::wire::UnitType::kNone, .exponent = 0},
  };

  constexpr fuchsia_input_report::wire::Axis kAxisPressure = {
      .range = {.min = 0, .max = kMaxContactPressure},
      .unit = {.type = fuchsia_input_report::wire::UnitType::kNone, .exponent = 0},
  };

  fidl::Arena<kDescriptorBufferSize> allocator;

  fuchsia_input_report::wire::DeviceInfo device_info;
  device_info.vendor_id = static_cast<uint32_t>(fuchsia_input_report::wire::VendorId::kGoogle);
  device_info.product_id = static_cast<uint32_t>(
      fuchsia_input_report::wire::VendorGoogleProductId::kFocaltechTouchscreen);

  fidl::VectorView<fuchsia_input_report::wire::ContactInputDescriptor> touch_input_contacts(
      allocator, kNumContacts);
  for (uint32_t i = 0; i < kNumContacts; i++) {
    touch_input_contacts[i].Allocate(allocator);
    touch_input_contacts[i].set_position_x(allocator, kAxisX);
    touch_input_contacts[i].set_position_y(allocator, kAxisY);
    touch_input_contacts[i].set_pressure(allocator, kAxisPressure);
  }

  fuchsia_input_report::wire::TouchInputDescriptor touch_input_descriptor(allocator);
  touch_input_descriptor.set_contacts(allocator, std::move(touch_input_contacts));
  touch_input_descriptor.set_max_contacts(allocator, kNumContacts);
  touch_input_descriptor.set_touch_type(allocator,
                                        fuchsia_input_report::wire::TouchType::kTouchscreen);

  fuchsia_input_report::wire::TouchDescriptor touch_descriptor(allocator);
  touch_descriptor.set_input(allocator, std::move(touch_input_descriptor));

  fuchsia_input_report::wire::DeviceDescriptor descriptor(allocator);
  descriptor.set_device_info(allocator, device_info);
  descriptor.set_touch(allocator, std::move(touch_descriptor));

  completer.Reply(std::move(descriptor));
}

void Ft8201Device::SendOutputReport(SendOutputReportRequestView request,
                                    SendOutputReportCompleter::Sync& completer) {
  completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
}

void Ft8201Device::GetFeatureReport(GetFeatureReportRequestView request,
                                    GetFeatureReportCompleter::Sync& completer) {
  completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
}

void Ft8201Device::SetFeatureReport(SetFeatureReportRequestView request,
                                    SetFeatureReportCompleter::Sync& completer) {
  completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
}

void Ft8201Device::WaitForNextReader() {
  sync_completion_wait(&next_reader_wait_, ZX_TIME_INFINITE);
  sync_completion_reset(&next_reader_wait_);
}

Ft8201Contact Ft8201Device::ParseContact(const uint8_t* const contact_buffer) {
  Ft8201Contact ret = {};
  ret.contact_id = contact_buffer[2] >> 4;
  ret.position_x = ((contact_buffer[0] & 0b1111) << 8) | contact_buffer[1];
  ret.position_y = ((contact_buffer[2] & 0b1111) << 8) | contact_buffer[3];
  ret.pressure = contact_buffer[4];
  return ret;
}

uint8_t Ft8201Device::CalculateEcc(const uint8_t* const buffer, const size_t size,
                                   uint8_t initial) {
  for (size_t i = 0; i < size; i++) {
    initial ^= buffer[i];
  }
  return initial;
}

zx_status_t Ft8201Device::Init() {
  zx_status_t status = interrupt_gpio_.ConfigIn(GPIO_NO_PULL);
  if (status != ZX_OK) {
    zxlogf(ERROR, "Ft8201: ConfigIn failed: %d", status);
    return status;
  }

  if ((status = interrupt_gpio_.GetInterrupt(ZX_INTERRUPT_MODE_EDGE_LOW, &interrupt_)) != ZX_OK) {
    zxlogf(ERROR, "Ft8201: GetInterrupt failed: %d", status);
    return status;
  }

  if ((status = FirmwareDownloadIfNeeded()) != ZX_OK) {
    return status;
  }

  status = thrd_create_with_name(
      &thread_, [](void* arg) -> int { return reinterpret_cast<Ft8201Device*>(arg)->Thread(); },
      this, "ft8201-thread");
  if (status != thrd_success) {
    zxlogf(ERROR, "Ft8201: Failed to create thread: %d", status);
    return thrd_status_to_zx_status(status);
  }

  // Copied from //src/ui/input/drivers/focaltech/ft_device.cc

  // Set profile for device thread.
  // TODO(fxbug.dev/40858): Migrate to the role-based API when available, instead of hard
  // coding parameters.
  {
    const zx::duration capacity = zx::usec(200);
    const zx::duration deadline = zx::msec(1);
    const zx::duration period = deadline;

    zx::profile profile;
    status = device_get_deadline_profile(zxdev(), capacity.get(), deadline.get(), period.get(),
                                         "ft8201-thread", profile.reset_and_get_address());
    if (status != ZX_OK) {
      zxlogf(WARNING, "Ft8201: Failed to get deadline profile: %d", status);
    } else {
      status = zx_object_set_profile(thrd_get_zx_handle(thread_), profile.get(), 0);
      if (status != ZX_OK) {
        zxlogf(WARNING, "Ft8201: Failed to apply deadline profile to device thread: %d", status);
      }
    }
  }

  if ((status = loop_.StartThread("ft8201-reader-thread")) != ZX_OK) {
    zxlogf(ERROR, "Ft8201: Failed to start loop: %d", status);
    Shutdown();
    return status;
  }

  return ZX_OK;
}

zx_status_t Ft8201Device::FirmwareDownloadIfNeeded() {
  zx::vmo pramboot_vmo;
  zx::vmo firmware_vmo;

  size_t pramboot_size = 0;
  zx_status_t status = load_firmware(parent(), FT8201_PRAMBOOT_PATH,
                                     pramboot_vmo.reset_and_get_address(), &pramboot_size);
  if (status != ZX_OK) {
    zxlogf(WARNING, "Ft8201: Failed to load pramboot binary, skipping firmware download");
    return ZX_OK;
  }

  size_t firmware_size = 0;
  status = load_firmware(parent(), FT8201_FIRMWARE_PATH, firmware_vmo.reset_and_get_address(),
                         &firmware_size);
  if (status != ZX_OK) {
    zxlogf(WARNING, "Ft8201: Failed to load firmware binary, skipping firmware download");
    return ZX_OK;
  }
  if (firmware_size <= kFirmwareVersionOffset) {
    zxlogf(ERROR, "Ft8201: Firmware binary is too small: %zu", pramboot_size);
    return ZX_ERR_WRONG_TYPE;
  }

  uint8_t firmware_version = 0;
  status = firmware_vmo.read(&firmware_version, kFirmwareVersionOffset, sizeof(firmware_version));
  if (status != ZX_OK) {
    zxlogf(ERROR, "Ft8201: Failed to read from firmware VMO: %d", status);
    return status;
  }

  zx::status<bool> firmware_status = CheckFirmwareAndStartRomboot(firmware_version);
  if (firmware_status.is_error()) {
    return firmware_status.error_value();
  }
  if (!firmware_status.value()) {
    zxlogf(INFO, "Ft8201: Firmware version is current, skipping download");
    return ZX_OK;
  }

  zxlogf(INFO, "Ft8201: Starting firmware download");

  if ((status = WaitForBootId(kRombootId, zx::msec(1), /*send_reset=*/true)) != ZX_OK) {
    return status;
  }

  if ((status = SendPramboot(pramboot_vmo, pramboot_size)) != ZX_OK) {
    return status;
  }

  if ((status = WaitForBootId(kPrambootId, zx::msec(20), /*send_reset=*/false)) != ZX_OK) {
    return status;
  }

  if ((status = EraseFlash(firmware_size)) != ZX_OK) {
    return status;
  }

  if ((status = SendFirmware(firmware_vmo, firmware_size)) != ZX_OK) {
    return status;
  }

  if ((status = Write8(kResetCommand)) != ZX_OK) {
    return status;
  }

  zxlogf(INFO, "Ft8201: Firmware download completed");
  return ZX_OK;
}

zx::status<bool> Ft8201Device::CheckFirmwareAndStartRomboot(const uint8_t firmware_version) {
  zx::status<uint8_t> chip_core = ReadReg8(kChipCoreReg);
  if (chip_core.is_error()) {
    return zx::error_status(chip_core.status_value());
  }

  if (chip_core.value() != kChipCoreFirmwareValid) {
    zxlogf(INFO, "Ft8201: Chip firmware is not valid: 0x%02x", chip_core.value());
    return zx::ok(true);
  }

  zx::status<uint8_t> current_firmware_version = ReadReg8(kFirmwareVersionReg);
  if (current_firmware_version.is_error()) {
    return zx::error_status(current_firmware_version.error_value());
  }

  if (current_firmware_version == firmware_version) {
    return zx::ok(false);
  }

  zxlogf(INFO, "Ft8201: Chip firmware (0x%02x) doesn't match our version (0x%02x)",
         current_firmware_version.value(), firmware_version);

  // Tell the firmware to enter romboot.
  zx_status_t status = WriteReg8(kWorkModeReg, kWorkModeSoftwareReset1);
  if (status != ZX_OK) {
    return zx::error_status(status);
  }
  zx::nanosleep(zx::deadline_after(zx::msec(10)));

  if ((status = WriteReg8(kWorkModeReg, kWorkModeSoftwareReset2)) != ZX_OK) {
    return zx::error_status(status);
  }
  zx::nanosleep(zx::deadline_after(zx::msec(80)));

  return zx::ok(true);
}

zx_status_t Ft8201Device::WaitForBootId(const uint16_t expected_id, const zx::duration retry_sleep,
                                        const bool send_reset) {
  zx::status<uint16_t> boot_id = GetBootId();
  if (!boot_id.is_error() && boot_id.value() != expected_id && send_reset) {
    zx_status_t status = Write8(kResetCommand);
    if (status != ZX_OK) {
      zxlogf(ERROR, "Ft8201: Failed to send reset command: %d", status);
      return status;
    }
    zx::nanosleep(zx::deadline_after(zx::msec(10)));
  }

  for (int i = 0; i < kGetBootIdRetries; i++) {
    if (boot_id.is_error() || boot_id.value() == expected_id) {
      break;
    }

    zx::nanosleep(zx::deadline_after(retry_sleep));
    boot_id = GetBootId();
  }

  if (boot_id.is_error()) {
    return boot_id.error_value();
  }
  if (boot_id.value() != expected_id) {
    zxlogf(ERROR, "Ft8201: Timed out waiting for boot ID 0x%04x, got 0x%04x", expected_id,
           boot_id.value());
    return ZX_ERR_TIMED_OUT;
  }
  return ZX_OK;
}

zx::status<uint16_t> Ft8201Device::GetBootId() {
  zx_status_t status = Write8(kUnlockBootCommand);
  if (status != ZX_OK) {
    zxlogf(ERROR, "Ft8201: Failed to send unlock command: %d", status);
    return zx::error_status(status);
  }

  zx::nanosleep(zx::deadline_after(kBootIdWaitAfterUnlock));

  return ReadReg16(kBootIdReg);
}

zx::status<bool> Ft8201Device::WaitForFlashStatus(const uint16_t expected_value, const int tries,
                                                  const zx::duration retry_sleep) {
  zx::status<uint16_t> value = ReadReg16(kFlashStatusReg);
  for (int i = 0; i < tries; i++) {
    if (value.is_error()) {
      return zx::error_status(value.error_value());
    }
    if (value.value() == expected_value) {
      return zx::ok(true);
    }

    zx::nanosleep(zx::deadline_after(retry_sleep));
    value = ReadReg16(kFlashStatusReg);
  }

  return zx::ok(false);
}

zx_status_t Ft8201Device::SendDataPacket(const uint8_t command, const uint32_t address,
                                         const uint8_t* buffer, const size_t size) {
  constexpr size_t kPacketHeaderSize = 1 + 3 + 2;  // command + address + length

  if (address > kMaxPacketAddress) {
    return ZX_ERR_INVALID_ARGS;
  }
  if (size > kMaxPacketSize) {
    return ZX_ERR_INVALID_ARGS;
  }

  uint8_t packet_buffer[kPacketHeaderSize + kMaxPacketSize];
  packet_buffer[0] = command;
  packet_buffer[1] = static_cast<uint8_t>((address >> 16) & 0xff);
  packet_buffer[2] = static_cast<uint8_t>((address >> 8) & 0xff);
  packet_buffer[3] = static_cast<uint8_t>(address & 0xff);
  packet_buffer[4] = static_cast<uint8_t>((size >> 8) & 0xff);
  packet_buffer[5] = static_cast<uint8_t>(size & 0xff);
  memcpy(packet_buffer + kPacketHeaderSize, buffer, size);

  zx_status_t status = i2c_.WriteSync(packet_buffer, kPacketHeaderSize + size);
  if (status != ZX_OK) {
    zxlogf(ERROR, "Ft8201: Failed to write %zu bytes to 0x%06x: %d", size, address, status);
    return status;
  }

  return ZX_OK;
}

zx_status_t Ft8201Device::SendPramboot(const zx::vmo& vmo, const size_t size) {
  uint32_t offset = 0;
  uint8_t expected_ecc = 0;
  zx_status_t status;
  for (size_t bytes_remaining = size; bytes_remaining > 0;) {
    uint8_t buffer[kMaxPacketSize];
    const size_t send_size = std::min(sizeof(buffer), bytes_remaining);

    if ((status = vmo.read(buffer, offset, send_size)) != ZX_OK) {
      zxlogf(ERROR, "Ft8201: Failed to read from pramboot VMO: %d", status);
      return status;
    }

    expected_ecc = CalculateEcc(buffer, send_size, expected_ecc);

    if ((status = SendPrambootPacket(offset, buffer, send_size)) != ZX_OK) {
      return status;
    }

    bytes_remaining -= send_size;
    offset += send_size;
  }

  zx::status<uint8_t> ecc = ReadReg8(kPrambootEccReg);
  if (ecc.is_error()) {
    return ecc.error_value();
  }

  if (ecc.value() != expected_ecc) {
    zxlogf(ERROR, "Ft8201: Pramboot ECC mismatch, got 0x%02x expected 0x%02x", ecc.value(),
           expected_ecc);
    return ZX_ERR_IO_DATA_INTEGRITY;
  }

  if ((status = Write8(kStartPrambootCommand)) != ZX_OK) {
    zxlogf(ERROR, "Ft8201: Failed to start pramboot: %d", status);
    return status;
  }

  zx::nanosleep(zx::deadline_after(zx::msec(10)));
  return ZX_OK;
}

zx_status_t Ft8201Device::EraseFlash(const size_t size) {
  const size_t firmware_size = size - kFirmwareOffset;

  zx_status_t status = WriteReg8(kFlashEraseCommand, kFlashEraseAppArea);
  if (status != ZX_OK) {
    return status;
  }

  if ((status = Write8(kFlashStatusCommand)) != ZX_OK) {
    return status;
  }

  zx::nanosleep(zx::deadline_after(EraseStatusSleep(firmware_size)));

  zx::status<bool> erase_done = WaitForFlashStatus(kFlashEraseDone, 50, zx::msec(400));
  if (erase_done.is_error()) {
    return erase_done.error_value();
  }
  if (!erase_done.value()) {
    zxlogf(ERROR, "Ft8201: Timed out waiting for flash erase");
    return ZX_ERR_TIMED_OUT;
  }

  return ZX_OK;
}

zx_status_t Ft8201Device::SendFirmware(const zx::vmo& vmo, const size_t size) {
  const size_t firmware_size = size - kFirmwareOffset;

  uint32_t offset = kFirmwareOffset;
  uint8_t expected_ecc = 0;
  zx_status_t status;
  for (size_t bytes_remaining = firmware_size; bytes_remaining > 0;) {
    uint8_t buffer[kMaxPacketSize];
    const size_t send_size = std::min(sizeof(buffer), bytes_remaining);

    if ((status = vmo.read(buffer, offset, send_size)) != ZX_OK) {
      zxlogf(ERROR, "Ft8201: Failed to read from firmware VMO: %d", status);
      return status;
    }

    expected_ecc = CalculateEcc(buffer, send_size, expected_ecc);

    if ((status = SendFirmwarePacket(offset, buffer, send_size)) != ZX_OK) {
      return status;
    }

    zx::nanosleep(zx::deadline_after(zx::msec(1)));

    const uint16_t expected_status = ExpectedWriteStatus(offset, send_size);
    zx::status<bool> write_done = WaitForFlashStatus(expected_status, 100, zx::msec(1));
    if (write_done.is_error()) {
      return write_done.error_value();
    }
    if (!write_done.value()) {
      zxlogf(WARNING, "Ft8201: Timed out waiting for correct flash write status");
    }

    bytes_remaining -= send_size;
    offset += send_size;
  }

  return CheckFirmwareEcc(firmware_size, expected_ecc);
}

zx_status_t Ft8201Device::CheckFirmwareEcc(const size_t size, const uint8_t expected_ecc) {
  zx_status_t status = Write8(kEccInitializationCommand);
  if (status != ZX_OK) {
    return status;
  }

  size_t offset = kFirmwareOffset;
  for (size_t bytes_remaining = size; bytes_remaining > 0;) {
    const size_t check_size = std::min<size_t>(kMaxEraseSize, bytes_remaining);

    const uint8_t check_buffer[] = {
        kEccCalculateCommand,
        static_cast<uint8_t>((offset >> 16) & 0xff),
        static_cast<uint8_t>((offset >> 8) & 0xff),
        static_cast<uint8_t>(offset & 0xff),
        static_cast<uint8_t>((check_size >> 8) & 0xff),
        static_cast<uint8_t>(check_size & 0xff),
    };
    if ((status = i2c_.WriteSync(check_buffer, sizeof(check_buffer))) != ZX_OK) {
      zxlogf(ERROR, "Ft8201: Failed to send ECC calculate command: %d", status);
      return status;
    }

    zx::status<bool> ecc_done =
        WaitForFlashStatus(kFlashEccDone, 10, CalculateEccSleep(check_size));
    if (ecc_done.is_error()) {
      return ecc_done.error_value();
    }
    if (!ecc_done.value()) {
      zxlogf(ERROR, "Ft8201: Timed out waiting for ECC calculation");
      return ZX_ERR_TIMED_OUT;
    }

    bytes_remaining -= check_size;
    offset += check_size;
  }

  zx::status<uint8_t> ecc = ReadReg8(kFirmwareEccReg);
  if (ecc.is_error()) {
    return ecc.error_value();
  }

  if (ecc.value() != expected_ecc) {
    zxlogf(ERROR, "Ft8201: Firmware ECC mismatch, got 0x%02x, expected 0x%02x", ecc.value(),
           expected_ecc);
    return ZX_ERR_IO_DATA_LOSS;
  }

  return ZX_OK;
}

zx::status<uint8_t> Ft8201Device::ReadReg8(const uint8_t address) {
  uint8_t value = 0;
  zx_status_t status = i2c_.ReadSync(address, &value, sizeof(value));
  if (status != ZX_OK) {
    zxlogf(ERROR, "Ft8201: Failed to read from 0x%02x: %d", address, status);
    return zx::error_status(status);
  }

  return zx::ok(value);
}

zx::status<uint16_t> Ft8201Device::ReadReg16(const uint8_t address) {
  uint8_t buffer[2];
  zx_status_t status = i2c_.ReadSync(address, buffer, sizeof(buffer));
  if (status != ZX_OK) {
    zxlogf(ERROR, "Ft8201: Failed to read from 0x%02x: %d", address, status);
    return zx::error_status(status);
  }

  return zx::ok((buffer[0] << 8) | buffer[1]);
}

zx_status_t Ft8201Device::Write8(const uint8_t value) {
  zx_status_t status = i2c_.WriteSync(&value, sizeof(value));
  if (status != ZX_OK) {
    zxlogf(ERROR, "Ft8201: Failed to write 0x%02x: %d", value, status);
  }
  return status;
}

zx_status_t Ft8201Device::WriteReg8(const uint8_t address, const uint8_t value) {
  const uint8_t buffer[] = {address, value};
  zx_status_t status = i2c_.WriteSync(buffer, sizeof(buffer));
  if (status != ZX_OK) {
    zxlogf(ERROR, "Ft8201: Failed to write 0x%02x to 0x%02x: %d", value, address, status);
  }
  return status;
}

int Ft8201Device::Thread() {
  zx::time timestamp;
  while (interrupt_.wait(&timestamp) == ZX_OK) {
    uint8_t contacts = 0;

    zx_status_t status = i2c_.ReadSync(kContactsReg, &contacts, sizeof(contacts));
    if (status != ZX_OK) {
      zxlogf(ERROR, "Ft8201: Failed to read number of touch points: %d", status);
      return thrd_error;
    }

    if (contacts == 0 || contacts > kNumContacts) {
      // The contacts register can take time to settle after the firmware download.
      continue;
    }

    uint8_t contacts_buffer[kContactSize * kNumContacts] = {};
    status = i2c_.ReadSync(kContactsStartReg, contacts_buffer, contacts * kContactSize);
    if (status != ZX_OK) {
      zxlogf(ERROR, "Ft8201: Failed to read touch data: %d", status);
      return thrd_error;
    }

    Ft8201InputReport report = {.event_time = timestamp, .contacts = {}, .num_contacts = contacts};
    for (uint8_t i = 0; i < contacts; i++) {
      report.contacts[i] = ParseContact(&contacts_buffer[i * kContactSize]);
    }

    input_report_readers_.SendReportToAllReaders(report);
  }

  return thrd_success;
}

void Ft8201Device::Shutdown() {
  interrupt_.destroy();
  thrd_join(thread_, nullptr);
}

static zx_driver_ops_t ft8201_driver_ops = []() -> zx_driver_ops_t {
  zx_driver_ops_t ops = {};
  ops.version = DRIVER_OPS_VERSION;
  ops.bind = Ft8201Device::Create;
  ops.run_unit_tests = Ft8201Device::RunUnitTests;
  return ops;
}();

}  // namespace touch

ZIRCON_DRIVER(Ft8201Device, touch::ft8201_driver_ops, "zircon", "0.1");
