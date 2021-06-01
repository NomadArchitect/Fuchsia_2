// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/ddk/binding.h>
#include <lib/ddk/platform-defs.h>
#include <lib/simple-codec/simple-codec-server.h>

#include <algorithm>
#include <memory>

#include <fbl/algorithm.h>
#include <fbl/alloc_checker.h>
#include <fbl/auto_lock.h>

namespace audio {

namespace audio_fidl = ::fuchsia::hardware::audio;

zx_status_t SimpleCodecServer::CreateInternal() {
  simple_codec_ = inspect_.GetRoot().CreateChild("simple_codec");
  state_ = simple_codec_.CreateString("state", "created");
  start_time_ = simple_codec_.CreateInt("start_time", 0);

  number_of_channels_ = simple_codec_.CreateUint("number_of_channels", 0);
  channels_to_use_bitmask_ = simple_codec_.CreateUint("channels_to_use_bitmask", 0);
  frame_rate_ = simple_codec_.CreateUint("frame_rate", 0);
  bits_per_slot_ = simple_codec_.CreateUint("bits_per_slot", 0);
  bits_per_sample_ = simple_codec_.CreateUint("bits_per_sample", 0);
  sample_format_ = simple_codec_.CreateString("sample_format", "not_set");
  frame_format_ = simple_codec_.CreateString("frame_format", "not_set");

  auto res = Initialize();
  if (res.is_error()) {
    return res.error_value();
  }
  loop_.StartThread();
  driver_ids_ = res.value();
  Info info = GetInfo();
  simple_codec_.CreateString("manufacturer", info.manufacturer.c_str(), &inspect_);
  simple_codec_.CreateString("product", info.product_name.c_str(), &inspect_);
  simple_codec_.CreateString("unique_id", info.unique_id.c_str(), &inspect_);
  if (driver_ids_.instance_count != 0) {
    zx_device_prop_t props[] = {
        {BIND_PLATFORM_DEV_VID, 0, driver_ids_.vendor_id},
        {BIND_PLATFORM_DEV_DID, 0, driver_ids_.device_id},
        {BIND_CODEC_INSTANCE, 0, driver_ids_.instance_count},
    };
    return DdkAdd(ddk::DeviceAddArgs(info.product_name.c_str())
                      .set_props(props)
                      .set_inspect_vmo(inspect_.DuplicateVmo())
                      .set_flags(DEVICE_ADD_ALLOW_MULTI_COMPOSITE));
  }
  zx_device_prop_t props[] = {
      {BIND_PLATFORM_DEV_VID, 0, driver_ids_.vendor_id},
      {BIND_PLATFORM_DEV_DID, 0, driver_ids_.device_id},
  };
  return DdkAdd(ddk::DeviceAddArgs(info.product_name.c_str())
                    .set_props(props)
                    .set_inspect_vmo(inspect_.DuplicateVmo())
                    .set_flags(DEVICE_ADD_ALLOW_MULTI_COMPOSITE));
}

zx_status_t SimpleCodecServer::CodecConnect(zx::channel channel) {
  return BindClient(std::move(channel), loop_.dispatcher());
}

template <class T>
SimpleCodecServerInternal<T>::SimpleCodecServerInternal() {
  plug_time_ = zx::clock::get_monotonic().get();
}

template <class T>
zx_status_t SimpleCodecServerInternal<T>::BindClient(zx::channel channel,
                                                     async_dispatcher_t* dispatcher) {
  auto instance = std::make_unique<SimpleCodecServerInstance<SimpleCodecServer>>(std::move(channel),
                                                                                 dispatcher, this);
  fbl::AutoLock lock(&instances_lock_);
  instances_.push_back(std::move(instance));
  return ZX_OK;
}

template <class T>
void SimpleCodecServerInternal<T>::OnUnbound(SimpleCodecServerInstance<T>* instance) {
  fbl::AutoLock lock(&instances_lock_);
  instances_.erase(*instance);
}

template <class T>
void SimpleCodecServerInternal<T>::Reset(Codec::ResetCallback callback,
                                         SimpleCodecServerInstance<T>* instance) {
  auto status = static_cast<T*>(this)->Reset();
  if (status != ZX_OK) {
    instance->binding_.Unbind();
    fbl::AutoLock lock(&instances_lock_);
    instances_.erase(*instance);
  }
  callback();
}

template <class T>
void SimpleCodecServerInternal<T>::Stop(Codec::StopCallback callback,
                                        SimpleCodecServerInstance<T>* instance) {
  auto status = static_cast<T*>(this)->Stop();
  if (status != ZX_OK) {
    instance->binding_.Unbind();
    fbl::AutoLock lock(&instances_lock_);
    instances_.erase(*instance);
  }
  static_cast<T*>(this)->state_.Set("stopped");
  callback();
}

template <class T>
void SimpleCodecServerInternal<T>::Start(Codec::StartCallback callback,
                                         SimpleCodecServerInstance<T>* instance) {
  auto status = static_cast<T*>(this)->Start();
  if (status != ZX_OK) {
    instance->binding_.Unbind();
    fbl::AutoLock lock(&instances_lock_);
    instances_.erase(*instance);
  }
  static_cast<T*>(this)->state_.Set("started");
  static_cast<T*>(this)->start_time_.Set(zx::clock::get_monotonic().get());
  callback();
}

template <class T>
void SimpleCodecServerInternal<T>::GetInfo(Codec::GetInfoCallback callback) {
  callback(static_cast<T*>(this)->GetInfo());
}

template <class T>
void SimpleCodecServerInternal<T>::IsBridgeable(Codec::IsBridgeableCallback callback) {
  callback(static_cast<T*>(this)->IsBridgeable());
}

template <class T>
void SimpleCodecServerInternal<T>::GetDaiFormats(Codec::GetDaiFormatsCallback callback) {
  auto formats = static_cast<T*>(this)->GetDaiFormats();
  std::vector<audio_fidl::DaiFrameFormat> frame_formats;
  for (FrameFormat i : formats.frame_formats) {
    audio_fidl::DaiFrameFormat frame_format;
    frame_format.set_frame_format_standard(i);
    frame_formats.emplace_back(std::move(frame_format));
  }
  audio_fidl::Codec_GetDaiFormats_Response response;
  response.formats.emplace_back(audio_fidl::DaiSupportedFormats{
      .number_of_channels = std::move(formats.number_of_channels),
      .sample_formats = std::move(formats.sample_formats),
      .frame_formats = std::move(frame_formats),
      .frame_rates = std::move(formats.frame_rates),
      .bits_per_slot = std::move(formats.bits_per_slot),
      .bits_per_sample = std::move(formats.bits_per_sample),
  });
  audio_fidl::Codec_GetDaiFormats_Result result;
  result.set_response(std::move(response));
  callback(std::move(result));
}

template <class T>
void SimpleCodecServerInternal<T>::SetDaiFormat(audio_fidl::DaiFormat format,
                                                Codec::SetDaiFormatCallback callback) {
  DaiFormat format2 = {};
  format2.number_of_channels = format.number_of_channels;
  format2.channels_to_use_bitmask = format.channels_to_use_bitmask;
  format2.sample_format = format.sample_format;
  format2.frame_format = format.frame_format.frame_format_standard();
  format2.frame_rate = format.frame_rate;
  format2.bits_per_slot = format.bits_per_slot;
  format2.bits_per_sample = format.bits_per_sample;
  auto* thiz = static_cast<T*>(this);
  thiz->number_of_channels_.Set(format2.number_of_channels);
  thiz->channels_to_use_bitmask_.Set(format2.channels_to_use_bitmask);
  thiz->frame_rate_.Set(format2.frame_rate);
  thiz->bits_per_slot_.Set(format2.bits_per_slot);
  thiz->bits_per_sample_.Set(format2.bits_per_sample);
  using FidlSampleFormat = audio_fidl::DaiSampleFormat;
  // clang-format off
  switch (format2.sample_format) {
    case FidlSampleFormat::PDM:          thiz->sample_format_.Set("PDM");          break;
    case FidlSampleFormat::PCM_SIGNED:   thiz->sample_format_.Set("PCM_signed");   break;
    case FidlSampleFormat::PCM_UNSIGNED: thiz->sample_format_.Set("PCM_unsigned"); break;
    case FidlSampleFormat::PCM_FLOAT:    thiz->sample_format_.Set("PCM_float");    break;
  }
  // clang-format on
  using FidlFrameFormat = audio_fidl::DaiFrameFormatStandard;
  // clang-format off
  switch (format2.frame_format) {
    case FidlFrameFormat::NONE:         thiz->frame_format_.Set("NONE");         break;
    case FidlFrameFormat::I2S:          thiz->frame_format_.Set("I2S");          break;
    case FidlFrameFormat::STEREO_LEFT:  thiz->frame_format_.Set("Stereo_left");  break;
    case FidlFrameFormat::STEREO_RIGHT: thiz->frame_format_.Set("Stereo_right"); break;
    case FidlFrameFormat::TDM1:         thiz->frame_format_.Set("TDM1");         break;
  }
  // clang-format on
  zx_status_t status = thiz->SetDaiFormat(std::move(format2));
  if (status != ZX_OK) {
    thiz->state_.Set(std::string("Set DAI format error: ") + std::to_string(status));
  }
  callback(status);
}

template <class T>
void SimpleCodecServerInternal<T>::GetGainFormat(Codec::GetGainFormatCallback callback) {
  auto format = static_cast<T*>(this)->GetGainFormat();
  audio_fidl::GainFormat format2;
  format2.set_type(audio_fidl::GainType::DECIBELS);  // Only decibels in simple codec.
  format2.set_min_gain(format.min_gain);
  format2.set_max_gain(format.max_gain);
  format2.set_gain_step(format.gain_step);
  format2.set_can_mute(format.can_mute);
  format2.set_can_agc(format.can_agc);
  callback(std::move(format2));
}

template <class T>
void SimpleCodecServerInstance<T>::WatchGainState(Codec::WatchGainStateCallback callback) {
  // Only reply the first time, then don't reply anymore. In simple codecs gain must only be
  // changed by SetGainState and hence we don't expect any watch calls to determine gain changes.
  if (watch_gain_state_first_time_) {
    parent_->WatchGainState(std::move(callback));
    watch_gain_state_first_time_ = false;
  }
}

template <class T>
void SimpleCodecServerInternal<T>::WatchGainState(Codec::WatchGainStateCallback callback) {
  audio_fidl::GainState gain_state;
  auto state = static_cast<T*>(this)->GetGainState();
  gain_state.set_muted(state.muted);
  gain_state.set_agc_enabled(state.agc_enabled);
  gain_state.set_gain_db(state.gain);
  callback(std::move(gain_state));
}

template <class T>
void SimpleCodecServerInternal<T>::SetGainState(audio_fidl::GainState state) {
  GainState state2;
  state2.gain = state.gain_db();
  state2.muted = state.muted();
  state2.agc_enabled = state.agc_enabled();
  static_cast<T*>(this)->SetGainState(std::move(state2));
}

template <class T>
void SimpleCodecServerInternal<T>::GetPlugDetectCapabilities(
    Codec::GetPlugDetectCapabilitiesCallback callback) {
  // Only hardwired in simple codec.
  callback(::fuchsia::hardware::audio::PlugDetectCapabilities::HARDWIRED);
}

template <class T>
void SimpleCodecServerInstance<T>::WatchPlugState(WatchPlugStateCallback callback) {
  // Only reply the first time, then don't reply anymore. Simple codec does not support changes to
  // plug state, also clients using simple codec do not issue WatchPlugState calls.
  if (watch_plug_state_first_time_) {
    parent_->WatchPlugState(std::move(callback));
    watch_plug_state_first_time_ = false;
  }
}

template <class T>
void SimpleCodecServerInternal<T>::WatchPlugState(Codec::WatchPlugStateCallback callback) {
  audio_fidl::PlugState plug_state;
  plug_state.set_plugged(true);
  plug_state.set_plug_state_time(plug_time_);
  callback(std::move(plug_state));
}

template class SimpleCodecServerInternal<SimpleCodecServer>;

}  // namespace audio
