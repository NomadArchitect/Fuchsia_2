// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "audio-stream-out.h"

#include <lib/ddk/platform-defs.h>
#include <lib/zx/clock.h>

#include <optional>
#include <utility>

#include <ddk/debug.h>
#include <ddk/driver.h>
#include <ddk/metadata.h>
#include <ddktl/metadata/audio.h>
#include <fbl/array.h>
#include <soc/mt8167/mt8167-clk-regs.h>

#include "src/media/audio/drivers/mt8167-tdm-output/mt8167_audio_out_bind.h"

namespace {

// Expects L+R.
constexpr size_t kNumberOfChannels = 2;
// Calculate ring buffer size for 1 second of 16-bit, 48kHz.
constexpr size_t kRingBufferSize =
    fbl::round_up<size_t, size_t>(48000 * 2 * kNumberOfChannels, PAGE_SIZE);
}  // namespace

namespace audio {
namespace mt8167 {

Mt8167AudioStreamOut::Mt8167AudioStreamOut(zx_device_t* parent)
    : SimpleAudioStream(parent, false), pdev_(parent) {}

zx_status_t Mt8167AudioStreamOut::InitPdev() {
  pdev_ = ddk::PDev::FromFragment(parent());
  if (!pdev_.is_valid()) {
    return ZX_ERR_NO_RESOURCES;
  }

  size_t actual;
  metadata::CodecType codec;
  zx_status_t status = device_get_metadata(parent(), DEVICE_METADATA_PRIVATE, &codec,
                                           sizeof(metadata::CodecType), &actual);
  if (status != ZX_OK || sizeof(metadata::CodecType) != actual) {
    zxlogf(ERROR, "device_get_metadata failed %d", status);
    return status;
  }

  status = codec_.SetProtocol(ddk::CodecProtocolClient(parent(), "codec"));
  if (status != ZX_OK) {
    zxlogf(ERROR, "could set codec protocol %d", status);
    return ZX_ERR_NO_RESOURCES;
  }

  status = pdev_.GetBti(0, &bti_);
  if (status != ZX_OK) {
    zxlogf(ERROR, "could not obtain bti %d", status);
    return status;
  }

  std::optional<ddk::MmioBuffer> mmio_audio, mmio_clk, mmio_pll;
  status = pdev_.MapMmio(0, &mmio_audio);
  if (status != ZX_OK) {
    return status;
  }
  status = pdev_.MapMmio(1, &mmio_clk);
  if (status != ZX_OK) {
    return status;
  }
  status = pdev_.MapMmio(2, &mmio_pll);
  if (status != ZX_OK) {
    return status;
  }

  // I2S2 corresponds to I2S_8CH.
  mt_audio_ = MtAudioOutDevice::Create(*std::move(mmio_audio), MtAudioOutDevice::I2S2);
  if (mt_audio_ == nullptr) {
    zxlogf(ERROR, "failed to create device");
    return ZX_ERR_NO_MEMORY;
  }

  // Initialize the ring buffer
  InitBuffer(kRingBufferSize);

  mt_audio_->SetBuffer(pinned_ring_buffer_.region(0).phys_addr, pinned_ring_buffer_.region(0).size);

  // Configure XO and PLLs for interface aud1.

  // Power up playback for I2S2 by clearing the power down bit for div1.
  CLK_SEL_9::Get().ReadFrom(&*mmio_clk).set_apll12_div1_pdn(0).WriteTo(&*mmio_clk);

  // Enable aud1 PLL.
  APLL1_CON0::Get().ReadFrom(&*mmio_pll).set_APLL1_EN(1).WriteTo(&*mmio_pll);
  zx_nanosleep(zx_deadline_after(ZX_MSEC(2)));  // For I2S clocks to settle, arbitrary.

  // Reset and initialize codec after we have configured I2S.
  status = codec_.Reset();
  if (status != ZX_OK) {
    return status;
  }

  status = codec_.SetBridgedMode(false);
  if (status != ZX_OK) {
    return status;
  }

  DaiFormat format = {};
  format.number_of_channels = 2;
  format.channels_to_use_bitmask = 3;
  format.sample_format = SampleFormat::PCM_SIGNED;
  format.frame_format = FrameFormat::I2S;
  format.frame_rate = 48'000;
  format.bits_per_sample = 32;
  format.bits_per_slot = 32;
  status = codec_.SetDaiFormat(format);
  if (status != ZX_OK) {
    return status;
  }

  return ZX_OK;
}

zx_status_t Mt8167AudioStreamOut::Init() {
  zx_status_t status;

  status = InitPdev();
  if (status != ZX_OK) {
    return status;
  }

  status = AddFormats();
  if (status != ZX_OK) {
    return status;
  }

  // Get our gain capabilities.
  auto state = codec_.GetGainState();
  if (state.is_error()) {
    zxlogf(ERROR, "failed to get gain state");
    return state.error_value();
  }
  cur_gain_state_.cur_gain = state->gain;
  cur_gain_state_.cur_mute = state->muted;
  cur_gain_state_.cur_agc = state->agc_enabled;

  auto format = codec_.GetGainFormat();
  if (format.is_error()) {
    zxlogf(ERROR, "failed to get gain format");
    return format.error_value();
  }

  cur_gain_state_.min_gain = format->min_gain;
  cur_gain_state_.max_gain = format->max_gain;
  cur_gain_state_.gain_step = format->gain_step;
  cur_gain_state_.can_mute = false;
  cur_gain_state_.can_agc = false;

  snprintf(device_name_, sizeof(device_name_), "mt8167-audio-out");
  snprintf(mfr_name_, sizeof(mfr_name_), "unknown");
  snprintf(prod_name_, sizeof(prod_name_), "mt8167");

  unique_id_ = AUDIO_STREAM_UNIQUE_ID_BUILTIN_SPEAKERS;

  // TODO(mpuryear): change this to the domain of the clock received from the board driver
  clock_domain_ = 0;

  return ZX_OK;
}

// Timer handler for sending out position notifications.
void Mt8167AudioStreamOut::ProcessRingNotification() {
  ScopedToken t(domain_token());
  ZX_ASSERT(us_per_notification_ != 0);

  notify_timer_.PostDelayed(dispatcher(), zx::usec(us_per_notification_));

  audio_proto::RingBufPositionNotify resp = {};
  resp.hdr.cmd = AUDIO_RB_POSITION_NOTIFY;

  resp.monotonic_time = zx::clock::get_monotonic().get();
  resp.ring_buffer_pos = mt_audio_->GetRingPosition();
  NotifyPosition(resp);
}

zx_status_t Mt8167AudioStreamOut::ChangeFormat(const audio_proto::StreamSetFmtReq& req) {
  fifo_depth_ = mt_audio_->fifo_depth();
  external_delay_nsec_ = 0;

  // At this time only one format is supported, and hardware is initialized
  // during driver binding, so nothing to do at this time.
  return ZX_OK;
}

void Mt8167AudioStreamOut::ShutdownHook() { mt_audio_->Shutdown(); }

zx_status_t Mt8167AudioStreamOut::SetGain(const audio_proto::SetGainReq& req) {
  GainState state;
  state.gain = req.gain;
  state.muted = cur_gain_state_.cur_mute;
  state.agc_enabled = cur_gain_state_.cur_agc;
  codec_.SetGainState(std::move(state));
  cur_gain_state_.cur_gain = state.gain;
  return ZX_OK;
}

zx_status_t Mt8167AudioStreamOut::GetBuffer(const audio_proto::RingBufGetBufferReq& req,
                                            uint32_t* out_num_rb_frames, zx::vmo* out_buffer) {
  uint32_t rb_frames = static_cast<uint32_t>(pinned_ring_buffer_.region(0).size / frame_size_);

  if (req.min_ring_buffer_frames > rb_frames) {
    return ZX_ERR_OUT_OF_RANGE;
  }
  zx_status_t status;
  constexpr uint32_t rights = ZX_RIGHT_READ | ZX_RIGHT_WRITE | ZX_RIGHT_MAP | ZX_RIGHT_TRANSFER;
  status = ring_buffer_vmo_.duplicate(rights, out_buffer);
  if (status != ZX_OK) {
    return status;
  }

  *out_num_rb_frames = rb_frames;

  mt_audio_->SetBuffer(pinned_ring_buffer_.region(0).phys_addr, rb_frames * frame_size_);

  return ZX_OK;
}

zx_status_t Mt8167AudioStreamOut::Start(uint64_t* out_start_time) {
  *out_start_time = mt_audio_->Start();
  uint32_t notifs = LoadNotificationsPerRing();
  if (notifs) {
    us_per_notification_ = static_cast<uint32_t>(1000 * pinned_ring_buffer_.region(0).size /
                                                 (frame_size_ * 48 * notifs));
    notify_timer_.PostDelayed(dispatcher(), zx::usec(us_per_notification_));
  } else {
    us_per_notification_ = 0;
  }
  return ZX_OK;
}

zx_status_t Mt8167AudioStreamOut::Stop() {
  notify_timer_.Cancel();
  us_per_notification_ = 0;
  mt_audio_->Stop();
  return ZX_OK;
}

zx_status_t Mt8167AudioStreamOut::AddFormats() {
  fbl::AllocChecker ac;
  supported_formats_.reserve(1, &ac);
  if (!ac.check()) {
    zxlogf(ERROR, "Out of memory, can not create supported formats list");
    return ZX_ERR_NO_MEMORY;
  }

  // Add the range for basic audio support.
  audio_stream_format_range_t range;

  range.min_channels = kNumberOfChannels;
  range.max_channels = kNumberOfChannels;
  range.sample_formats = AUDIO_SAMPLE_FORMAT_16BIT;
  range.min_frames_per_second = 48000;
  range.max_frames_per_second = 48000;
  range.flags = ASF_RANGE_FLAG_FPS_48000_FAMILY;

  supported_formats_.push_back(range);

  return ZX_OK;
}

zx_status_t Mt8167AudioStreamOut::InitBuffer(size_t size) {
  zx_status_t status;
  status = zx_vmo_create_contiguous(bti_.get(), size, 0, ring_buffer_vmo_.reset_and_get_address());
  if (status != ZX_OK) {
    zxlogf(ERROR, "failed to allocate ring buffer vmo - %d", status);
    return status;
  }

  status = pinned_ring_buffer_.Pin(ring_buffer_vmo_, bti_, ZX_VM_PERM_READ | ZX_VM_PERM_WRITE);
  if (status != ZX_OK) {
    zxlogf(ERROR, "failed to pin ring buffer vmo - %d", status);
    return status;
  }
  if (pinned_ring_buffer_.region_count() != 1) {
    zxlogf(ERROR, "buffer is not contiguous");
    return ZX_ERR_NO_MEMORY;
  }

  return ZX_OK;
}

}  // namespace mt8167
}  // namespace audio

static zx_status_t mt_audio_out_bind(void* ctx, zx_device_t* device) {
  auto stream = audio::SimpleAudioStream::Create<audio::mt8167::Mt8167AudioStreamOut>(device);
  if (stream == nullptr) {
    return ZX_ERR_NO_MEMORY;
  }

  return ZX_OK;
}

static constexpr zx_driver_ops_t mt_audio_out_driver_ops = []() {
  zx_driver_ops_t ops = {};
  ops.version = DRIVER_OPS_VERSION;
  ops.bind = mt_audio_out_bind;
  return ops;
}();

ZIRCON_DRIVER(mt8167_audio_out, mt_audio_out_driver_ops, "zircon", "0.1");
