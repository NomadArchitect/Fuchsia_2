// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/media/audio/audio_core/audio_renderer.h"

#include <lib/fit/defer.h>
#include <lib/syslog/cpp/macros.h>

#include "src/media/audio/audio_core/audio_admin.h"
#include "src/media/audio/audio_core/reporter.h"
#include "src/media/audio/audio_core/stream_usage.h"
#include "src/media/audio/lib/clock/utils.h"

namespace media::audio {

AudioRenderer::AudioRenderer(
    fidl::InterfaceRequest<fuchsia::media::AudioRenderer> audio_renderer_request, Context* context)
    : BaseRenderer(std::move(audio_renderer_request), context) {
  context->volume_manager().AddStream(this);
  reporter().SetUsage(RenderUsageFromFidlRenderUsage(usage_));
}

AudioRenderer::~AudioRenderer() {
  AudioRenderer::ReportStop();
  context().volume_manager().RemoveStream(this);
}

void AudioRenderer::OnLinkAdded() {
  context().volume_manager().NotifyStreamChanged(this);
  BaseRenderer::OnLinkAdded();
}

void AudioRenderer::Shutdown() {
  BaseRenderer::Shutdown();
  gain_control_bindings_.CloseAll();
}

void AudioRenderer::ReportStart() {
  BaseRenderer::ReportStart();
  context().audio_admin().UpdateRendererState(usage_, true, this);
}

void AudioRenderer::ReportStop() {
  BaseRenderer::ReportStop();
  context().audio_admin().UpdateRendererState(usage_, false, this);
}

bool AudioRenderer::GetStreamMute() const { return mute_; }

void AudioRenderer::SetUsage(fuchsia::media::AudioRenderUsage usage) {
  TRACE_DURATION("audio", "AudioRenderer::SetUsage");
  if (format_) {
    FX_LOGS(WARNING) << "SetUsage called after SetPcmStreamType.";
    context().route_graph().RemoveRenderer(*this);
    return;
  }
  reporter().SetUsage(RenderUsageFromFidlRenderUsage(usage));
  usage_ = usage;
}

// If received clock is null, use our adjustable clock. Else, use this new clock. Fail/disconnect,
// if the client-submitted clock has insufficient rights. Strip off other rights such as WRITE.
void AudioRenderer::SetReferenceClock(zx::clock ref_clock) {
  TRACE_DURATION("audio", "AudioRenderer::SetReferenceClock");
  auto cleanup = fit::defer([this]() { context().route_graph().RemoveRenderer(*this); });

  // Lock after storing |cleanup| to ensure |mutex_| is released upon function return, rather than
  // |cleanup| completion.
  std::lock_guard<std::mutex> lock(mutex_);

  // We cannot change the reference clock, once it is set. Also, calling `SetPcmStreamType` will
  // automatically sets the default reference clock, if one has not been explicitly set.
  if (reference_clock_is_set_) {
    FX_LOGS(WARNING) << "Attempted to change reference clock after setting it.";
    return;
  }

  zx_status_t status;
  if (ref_clock.is_valid()) {
    status = SetCustomReferenceClock(std::move(ref_clock));
  } else {
    status = SetAdjustableReferenceClock();
  }
  if (status != ZX_OK) {
    return;
  }

  reference_clock_is_set_ = true;

  cleanup.cancel();
}

void AudioRenderer::SetPcmStreamType(fuchsia::media::AudioStreamType stream_type) {
  TRACE_DURATION("audio", "AudioRenderer::SetPcmStreamType");
  std::lock_guard<std::mutex> lock(mutex_);

  auto cleanup = fit::defer([this]() { context().route_graph().RemoveRenderer(*this); });

  // We cannot change the format while we are currently operational
  if (IsOperating()) {
    FX_LOGS(WARNING) << "Attempted to set format while in operational mode.";
    return;
  }

  auto format_result = Format::Create(stream_type);
  if (format_result.is_error()) {
    FX_LOGS(WARNING) << "AudioRenderer: PcmStreamType is invalid";
    return;
  }
  format_ = {format_result.take_value()};

  reporter().SetFormat(*format_);

  context().route_graph().SetRendererRoutingProfile(
      *this, {.routable = true, .usage = StreamUsage::WithRenderUsage(usage_)});

  // Once we route the renderer, we accept the default reference clock if one hasn't yet been set.
  reference_clock_is_set_ = true;

  context().volume_manager().NotifyStreamChanged(this);

  // Things went well, cancel the cleanup hook. If our config had been validated previously, it will
  // have to be revalidated as we move into the operational phase of our life.
  InvalidateConfiguration();
  cleanup.cancel();
}

void AudioRenderer::BindGainControl(
    fidl::InterfaceRequest<fuchsia::media::audio::GainControl> request) {
  TRACE_DURATION("audio", "AudioRenderer::BindGainControl");

  gain_control_bindings_.AddBinding(GainControlBinding::Create(this), std::move(request));
}

fuchsia::media::Usage AudioRenderer::GetStreamUsage() const {
  return fuchsia::media::Usage::WithRenderUsage(fidl::Clone(usage_));
}

void AudioRenderer::RealizeVolume(VolumeCommand volume_command) {
  context().link_matrix().ForEachDestLink(*this, [this,
                                                  volume_command](LinkMatrix::LinkHandle link) {
    FX_CHECK(link.mix_domain) << "Renderer dest link should have a defined mix_domain";
    float gain_db = link.loudness_transform->Evaluate<3>({
        VolumeValue{volume_command.volume},
        GainDbFsValue{volume_command.gain_db_adjustment},
        GainDbFsValue{stream_gain_db_},
    });

    std::stringstream stream;
    stream << static_cast<const void*>(this) << " (link " << static_cast<const void*>(&link) << ") "
           << StreamUsage::WithRenderUsage(usage_).ToString() << " Gain(" << gain_db << "db) = "
           << "Vol(" << volume_command.volume << ") + GainAdjustment("
           << volume_command.gain_db_adjustment << "db) + StreamGain(" << stream_gain_db_ << "db)";
    std::string log_string = stream.str();

    // log_string is only included for log-display of loudness changes
    link.mix_domain->PostTask([link, volume_command, gain_db, log_string]() {
      auto& gain = link.mixer->bookkeeping().gain;
      // If not currently ramping, then exit early if asked to change to our current gain value --
      // or if asked to RAMP to our current gain value.
      if (!gain.IsRamping() && gain.GetGainDb() == gain_db) {
        return;
      }
      if (volume_command.ramp.has_value()) {
        // Stop any in-progress ramping; use this ramp instead
        gain.SetSourceGainWithRamp(gain_db, volume_command.ramp->duration,
                                   volume_command.ramp->ramp_type);
      } else {
        // Stop any in-progress ramping; snap to this new gain_db
        gain.SetSourceGain(gain_db);
      }

      // TODO(fxbug.dev/51049) Logging should be removed upon creation of inspect tool or
      // other real-time method for gain observation
      FX_LOGS(INFO) << log_string;
    });
    reporter().SetFinalGain(gain_db);
  });
}

// Set the stream gain, in each Renderer -> Output audio path. The Gain object contains multiple
// stages. In playback, renderer gain is pre-mix and hence is "source" gain; the Output device (or
// master) gain is "dest" gain.
void AudioRenderer::SetGain(float gain_db) {
  TRACE_DURATION("audio", "AudioRenderer::SetGain");
  FX_LOGS(DEBUG) << " (" << gain_db << " dB)";

  // Before setting stream_gain_db_, we should always perform this range check.
  if (gain_db > fuchsia::media::audio::MAX_GAIN_DB ||
      gain_db < fuchsia::media::audio::MUTED_GAIN_DB || isnan(gain_db)) {
    FX_LOGS(WARNING) << "SetGain(" << gain_db << " dB) out of range.";
    context().route_graph().RemoveRenderer(*this);
    return;
  }

  if (stream_gain_db_ == gain_db) {
    return;
  }

  stream_gain_db_ = gain_db;
  reporter().SetGain(gain_db);

  context().volume_manager().NotifyStreamChanged(this);

  NotifyGainMuteChanged();
}

// Set a stream gain ramp, in each Renderer -> Output audio path. Renderer gain is pre-mix and hence
// is the Source component in the Gain object.
void AudioRenderer::SetGainWithRamp(float gain_db, int64_t duration_ns,
                                    fuchsia::media::audio::RampType ramp_type) {
  TRACE_DURATION("audio", "AudioRenderer::SetGainWithRamp");
  zx::duration duration = zx::nsec(duration_ns);
  FX_LOGS(DEBUG) << " (" << gain_db << " dB, " << duration.to_usecs() << " usec)";

  if (gain_db > fuchsia::media::audio::MAX_GAIN_DB ||
      gain_db < fuchsia::media::audio::MUTED_GAIN_DB || isnan(gain_db)) {
    FX_LOGS(WARNING) << "SetGainWithRamp(" << gain_db << " dB) out of range.";
    context().route_graph().RemoveRenderer(*this);
    return;
  }

  reporter().SetGainWithRamp(gain_db, duration, ramp_type);

  context().volume_manager().NotifyStreamChanged(this, Ramp{duration, ramp_type});

  // TODO(mpuryear): implement GainControl notifications for gain ramps.
}

// Set a stream mute, in each Renderer -> Output audio path. For now, mute is handled by setting
// gain to a value guaranteed to be silent, but going forward we may pass this thru to the Gain
// object. Renderer gain/mute is pre-mix and hence is the Source component in the Gain object.
void AudioRenderer::SetMute(bool mute) {
  TRACE_DURATION("audio", "AudioRenderer::SetMute");
  // Only do the work if the request represents a change in state.
  if (mute_ == mute) {
    return;
  }
  FX_LOGS(DEBUG) << " (mute: " << mute << ")";

  reporter().SetMute(mute);
  mute_ = mute;

  context().volume_manager().NotifyStreamChanged(this);
  NotifyGainMuteChanged();
}

void AudioRenderer::NotifyGainMuteChanged() {
  TRACE_DURATION("audio", "AudioRenderer::NotifyGainMuteChanged");
  // TODO(mpuryear): consider whether GainControl events should be disable-able, like MinLeadTime.
  FX_LOGS(DEBUG) << " (" << stream_gain_db_ << " dB, mute: " << mute_ << ")";

  for (auto& gain_binding : gain_control_bindings_.bindings()) {
    gain_binding->events().OnGainMuteChanged(stream_gain_db_, mute_);
  }
}

void AudioRenderer::GainControlBinding::SetGain(float gain_db) {
  TRACE_DURATION("audio", "AudioRenderer::SetGain");
  owner_->SetGain(gain_db);
}

void AudioRenderer::GainControlBinding::SetGainWithRamp(float gain_db, int64_t duration_ns,
                                                        fuchsia::media::audio::RampType ramp_type) {
  TRACE_DURATION("audio", "AudioRenderer::SetSourceGainWithRamp");
  owner_->SetGainWithRamp(gain_db, duration_ns, ramp_type);
}

void AudioRenderer::GainControlBinding::SetMute(bool mute) {
  TRACE_DURATION("audio", "AudioRenderer::SetMute");
  owner_->SetMute(mute);
}

}  // namespace media::audio
