// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/media/audio/audio_core/mix_stage.h"

#include <lib/fit/defer.h>
#include <lib/trace/event.h>
#include <lib/zx/clock.h>
#include <zircon/status.h>

#include <iomanip>
#include <limits>
#include <memory>

#include "src/media/audio/audio_core/base_renderer.h"
#include "src/media/audio/audio_core/mixer/mixer.h"
#include "src/media/audio/audio_core/mixer/no_op.h"
#include "src/media/audio/audio_core/reporter.h"
#include "src/media/audio/lib/clock/utils.h"
#include "src/media/audio/lib/logging/logging.h"

namespace media::audio {
namespace {

TimelineFunction ReferenceClockToIntegralFrames(
    TimelineFunction ref_time_to_frac_presentation_frame) {
  TimelineRate frames_per_fractional_frame = TimelineRate(1, Fixed(1).raw_value());
  return TimelineFunction::Compose(TimelineFunction(frames_per_fractional_frame),
                                   ref_time_to_frac_presentation_frame);
}

zx::duration LeadTimeForMixer(const Format& format, const Mixer& mixer) {
  auto delay_frames = mixer.pos_filter_width().Ceiling();
  TimelineRate ticks_per_frame = format.frames_per_ns().Inverse();
  return zx::duration(ticks_per_frame.Scale(delay_frames));
}

}  // namespace

// If source position error becomes greater than this, we stop trying to smoothly synchronize and
// instead 'snap' to the expected pos (sometimes referred to as "jam sync"). This will surface as a
// discontinuity (if jumping backward) or a dropout (if jumping forward), for this source stream.
constexpr zx::duration kMaxErrorThresholdDuration = zx::msec(5);

MixStage::MixStage(const Format& output_format, uint32_t block_size,
                   TimelineFunction ref_time_to_frac_presentation_frame, AudioClock& audio_clock)
    : MixStage(output_format, block_size,
               fbl::MakeRefCounted<VersionedTimelineFunction>(ref_time_to_frac_presentation_frame),
               audio_clock) {}

MixStage::MixStage(const Format& output_format, uint32_t block_size,
                   fbl::RefPtr<VersionedTimelineFunction> ref_time_to_frac_presentation_frame,
                   AudioClock& audio_clock)
    : ReadableStream(output_format),
      output_buffer_frames_(block_size),
      output_buffer_(block_size * output_format.channels()),
      output_ref_clock_(audio_clock),
      output_ref_clock_to_fractional_frame_(ref_time_to_frac_presentation_frame) {}

std::shared_ptr<Mixer> MixStage::AddInput(std::shared_ptr<ReadableStream> stream,
                                          std::optional<float> initial_dest_gain_db,
                                          Mixer::Resampler resampler_hint) {
  TRACE_DURATION("audio", "MixStage::AddInput");
  if (!stream) {
    FX_LOGS(ERROR) << "Null stream, cannot add";
    return nullptr;
  }

  resampler_hint = AudioClock::UpgradeResamplerIfNeeded(resampler_hint, stream->reference_clock(),
                                                        reference_clock());

  auto mixer = std::shared_ptr<Mixer>(
      Mixer::Select(stream->format().stream_type(), format().stream_type(), resampler_hint)
          .release());
  if (!mixer) {
    mixer = std::make_unique<audio::mixer::NoOp>();
  }

  if (initial_dest_gain_db) {
    mixer->bookkeeping().gain.SetDestGain(*initial_dest_gain_db);
  }

  stream->SetPresentationDelay(GetPresentationDelay() + LeadTimeForMixer(stream->format(), *mixer));

  FX_LOGS(DEBUG) << "AddInput "
                 << (stream->reference_clock().is_adjustable() ? "adjustable " : "static ")
                 << (stream->reference_clock().is_device_clock() ? "device" : "client") << " (self "
                 << (reference_clock().is_adjustable() ? "adjustable " : "static ")
                 << (reference_clock().is_device_clock() ? "device)" : "client)");
  {
    std::lock_guard<std::mutex> lock(stream_lock_);
    streams_.emplace_back(StreamHolder{std::move(stream), mixer});
  }
  return mixer;
}

void MixStage::RemoveInput(const ReadableStream& stream) {
  TRACE_DURATION("audio", "MixStage::RemoveInput");
  std::lock_guard<std::mutex> lock(stream_lock_);
  auto it = std::find_if(streams_.begin(), streams_.end(), [stream = &stream](const auto& holder) {
    return holder.stream.get() == stream;
  });

  if (it == streams_.end()) {
    FX_LOGS(ERROR) << "Input not found, cannot remove";
    return;
  }

  FX_LOGS(DEBUG) << "RemoveInput "
                 << (it->stream->reference_clock().is_adjustable() ? "adjustable " : "static ")
                 << (it->stream->reference_clock().is_device_clock() ? "device" : "client")
                 << " (self " << (reference_clock().is_adjustable() ? "adjustable " : "static ")
                 << (reference_clock().is_device_clock() ? "device)" : "client)");

  streams_.erase(it);
}

std::optional<ReadableStream::Buffer> MixStage::ReadLock(Fixed dest_frame, size_t frame_count) {
  TRACE_DURATION("audio", "MixStage::ReadLock", "frame", dest_frame.Floor(), "length", frame_count);

  // If we have a partially consumed block, return that here.
  // Otherwise, the cached block, if any, is no longer needed.
  if (cached_buffer_.Contains(dest_frame)) {
    return cached_buffer_.Get();
  }
  cached_buffer_.Reset();

  memset(&cur_mix_job_, 0, sizeof(cur_mix_job_));

  auto snapshot = ref_time_to_frac_presentation_frame();

  cur_mix_job_.buf = &output_buffer_[0];
  cur_mix_job_.buf_frames = std::min<uint32_t>(frame_count, output_buffer_frames_);
  cur_mix_job_.dest_start_frame = dest_frame.Floor();
  cur_mix_job_.dest_ref_clock_to_frac_dest_frame = snapshot.timeline_function;
  cur_mix_job_.applied_gain_db = fuchsia::media::audio::MUTED_GAIN_DB;

  // Fill the output buffer with silence.
  size_t bytes_to_zero = cur_mix_job_.buf_frames * format().bytes_per_frame();
  std::memset(cur_mix_job_.buf, 0, bytes_to_zero);
  ForEachSource(TaskType::Mix, dest_frame);

  if (cur_mix_job_.applied_gain_db <= fuchsia::media::audio::MUTED_GAIN_DB) {
    // Either we mixed no streams, or all the streams mixed were muted. Either way we can just
    // return nullopt to signify we have no audible frames.
    return std::nullopt;
  }

  // Cache the buffer in case it is not fully read by the caller.
  cached_buffer_.Set(ReadableStream::Buffer(
      Fixed(dest_frame.Floor()), Fixed(cur_mix_job_.buf_frames), cur_mix_job_.buf, true,
      cur_mix_job_.usages_mixed, cur_mix_job_.applied_gain_db));
  return cached_buffer_.Get();
}

BaseStream::TimelineFunctionSnapshot MixStage::ref_time_to_frac_presentation_frame() const {
  TRACE_DURATION("audio", "MixStage::ref_time_to_frac_presentation_frame");
  auto [timeline_function, generation] = output_ref_clock_to_fractional_frame_->get();
  return {
      .timeline_function = timeline_function,
      .generation = generation,
  };
}

void MixStage::SetPresentationDelay(zx::duration external_delay) {
  TRACE_DURATION("audio", "MixStage::SetPresentationDelay");
  ReadableStream::SetPresentationDelay(external_delay);

  // Propagate time to our sources.
  std::lock_guard<std::mutex> lock(stream_lock_);
  for (const auto& holder : streams_) {
    FX_DCHECK(holder.stream);
    FX_DCHECK(holder.mixer);

    zx::duration mixer_lead_time = LeadTimeForMixer(holder.stream->format(), *holder.mixer);
    holder.stream->SetPresentationDelay(external_delay + mixer_lead_time);
  }
}

void MixStage::Trim(Fixed dest_frame) {
  TRACE_DURATION("audio", "MixStage::Trim", "frame", dest_frame.Floor());
  ForEachSource(TaskType::Trim, dest_frame);
}

void MixStage::ForEachSource(TaskType task_type, Fixed dest_frame) {
  TRACE_DURATION("audio", "MixStage::ForEachSource");

  std::vector<StreamHolder> sources;
  {
    std::lock_guard<std::mutex> lock(stream_lock_);
    for (const auto& holder : streams_) {
      sources.emplace_back(StreamHolder{holder.stream, holder.mixer});
    }
  }

  for (auto& source : sources) {
    if (task_type == TaskType::Mix) {
      auto& source_info = source.mixer->source_info();
      auto& bookkeeping = source.mixer->bookkeeping();
      ReconcileClocksAndSetStepSize(source_info, bookkeeping, *source.stream);
      MixStream(*source.mixer, *source.stream);
    } else {
      auto dest_ref_time = RefTimeAtFracPresentationFrame(dest_frame);
      auto mono_time = reference_clock().MonotonicTimeFromReferenceTime(dest_ref_time);
      auto source_ref_time =
          source.stream->reference_clock().ReferenceTimeFromMonotonicTime(mono_time);
      auto source_frame = source.stream->FracPresentationFrameAtRefTime(source_ref_time);
      source.stream->Trim(source_frame);
    }
  }
}

void MixStage::MixStream(Mixer& mixer, ReadableStream& stream) {
  TRACE_DURATION("audio", "MixStage::MixStream");
  auto& info = mixer.source_info();
  info.frames_produced = 0;

  // If the renderer is currently paused, subject_delta (not just step_size) is zero. This packet
  // may be relevant eventually, but currently it contributes nothing.
  if (!info.dest_frames_to_frac_source_frames.subject_delta()) {
    return;
  }

  // Calculate the first sampling point for the initial job, in source sub-frames. Use timestamps
  // for the first and last dest frames we need, translated into the source (frac_frame) timeline.
  auto frac_source_for_first_mix_job_frame =
      Fixed::FromRaw(info.dest_frames_to_frac_source_frames(cur_mix_job_.dest_start_frame));

  while (true) {
    // At this point we know we need to consume some source data, but we don't yet know how much.
    // Here is how many destination frames we still need to produce, for this mix job.
    FX_DCHECK(cur_mix_job_.buf_frames >= info.frames_produced);
    uint32_t dest_frames_left = cur_mix_job_.buf_frames - info.frames_produced;
    if (dest_frames_left == 0) {
      break;
    }

    // Calculate this job's last sampling point.
    Fixed source_frames =
        Fixed::FromRaw(info.dest_frames_to_frac_source_frames.rate().Scale(dest_frames_left)) +
        mixer.pos_filter_width();

    // Try to grab the front of the packet queue (or ring buffer, if capturing).
    auto stream_buffer =
        stream.ReadLock(frac_source_for_first_mix_job_frame, source_frames.Ceiling());

    // If the queue is empty, then we are done.
    if (!stream_buffer) {
      break;
    }

    // If the packet is discontinuous, reset our mixer's internal filter state.
    if (!stream_buffer->is_continuous()) {
      // Reset any cached state from previous buffer (but not our long-running position state).
      mixer.Reset();
    }

    // If a packet has no frames, there's no need to mix it; it may be skipped.
    if (stream_buffer->end() == stream_buffer->start()) {
      stream_buffer->set_is_fully_consumed(true);
      continue;
    }

    // Now process the packet at the front of the renderer's queue. If the packet has been
    // entirely consumed, pop it off the front and proceed to the next. Otherwise, we are done.
    auto fully_consumed = ProcessMix(mixer, stream, *stream_buffer);
    stream_buffer->set_is_fully_consumed(fully_consumed);

    // If we have mixed enough destination frames, we are done with this mix, regardless of what
    // we should now do with the source packet.
    if (info.frames_produced == cur_mix_job_.buf_frames) {
      break;
    }
    // If we still need to produce more destination data, but could not complete this source
    // packet (we're paused, or the packet is in the future), then we are done.
    if (!fully_consumed) {
      break;
    }

    frac_source_for_first_mix_job_frame = stream_buffer->end();
  }

  // If there was insufficient supply to meet our demand, we may not have mixed enough frames, but
  // we advance our destination frame count as if we did, because time rolls on. Same for source.
  auto& bookkeeping = mixer.bookkeeping();
  info.AdvanceRunningPositionsTo(cur_mix_job_.dest_start_frame + cur_mix_job_.buf_frames,
                                 bookkeeping);
  cur_mix_job_.accumulate = true;
}

bool MixStage::ProcessMix(Mixer& mixer, ReadableStream& stream,
                          const ReadableStream::Buffer& source_buffer) {
  TRACE_DURATION("audio", "MixStage::ProcessMix");

  // We are only called by MixStream, which has guaranteed these.
  auto& info = mixer.source_info();
  auto& bookkeeping = mixer.bookkeeping();
  FX_DCHECK(cur_mix_job_.buf_frames > 0);
  FX_DCHECK(info.frames_produced < cur_mix_job_.buf_frames);
  FX_DCHECK(info.dest_frames_to_frac_source_frames.subject_delta());

  // At this point we know we need to consume some source data, but we don't yet know how much.
  // Here is how many destination frames we still need to produce, for this mix job.
  uint32_t dest_frames_left = cur_mix_job_.buf_frames - info.frames_produced;
  float* buf = cur_mix_job_.buf + (info.frames_produced * format().channels());

  // Determine this job's first and last sampling points, in source sub-frames. Use the next
  // expected source position (in frac_frames) saved in our long-running position accounting.
  Fixed frac_source_for_first_mix_job_frame = info.next_frac_source_frame;

  // This represents the last possible source frame we need for this mix. Note that it is 1 subframe
  // short of the source needed for the SUBSEQUENT dest frame, floored to an integral source frame.
  // We cannot just subtract one integral frame from the source corresponding to the next start dest
  // because very large or small step_size values make this 1-frame assumption invalid.
  //
  auto frac_source_for_final_mix_job_frame =
      Fixed::FromRaw(frac_source_for_first_mix_job_frame.raw_value() +
                     (bookkeeping.step_size * dest_frames_left +
                      (bookkeeping.rate_modulo * dest_frames_left + bookkeeping.src_pos_modulo) /
                          bookkeeping.denominator) -
                     1);

  // The above two calculated values characterize our demand. Now reason about our supply.
  //
  // Assert our implementation-defined limit is compatible with the FIDL limit. The latter is
  // already enforced by the renderer implementation.
  static_assert(fuchsia::media::MAX_FRAMES_PER_RENDERER_PACKET <= Fixed::Max().Floor());
  FX_DCHECK(source_buffer.end() > source_buffer.start());
  FX_DCHECK(source_buffer.length() <= Fixed(Fixed::Max()));

  // Calculate the actual first and final frame times in the source packet.
  Fixed frac_source_for_first_packet_frame = source_buffer.start();
  Fixed frac_source_for_final_packet_frame = source_buffer.end() - Fixed(1);

  // If this source packet's final audio frame occurs before our filter's negative edge, centered at
  // our first sampling point, then this packet is entirely in the past and may be skipped.
  // Returning true means we're done with the packet (it can be completed) and we would like another
  if (frac_source_for_final_packet_frame <
      (frac_source_for_first_mix_job_frame - mixer.neg_filter_width())) {
    Fixed source_frac_frames_late = frac_source_for_first_mix_job_frame - mixer.neg_filter_width() -
                                    frac_source_for_first_packet_frame;
    auto clock_mono_late = zx::nsec(info.clock_mono_to_frac_source_frames.rate().Inverse().Scale(
        source_frac_frames_late.raw_value()));

    stream.ReportUnderflow(frac_source_for_first_packet_frame, frac_source_for_first_mix_job_frame,
                           clock_mono_late);
    return true;
  }

  // If this source packet's first audio frame occurs after our filter's positive edge, centered at
  // our final sampling point, then this packet is entirely in the future and should be held.
  // Returning false (based on requirement that packets must be presented in timestamp-chronological
  // order) means that we have consumed all of the available packet "supply" as we can at this time.
  if (frac_source_for_first_packet_frame >
      (frac_source_for_final_mix_job_frame + mixer.pos_filter_width())) {
    return false;
  }

  // If neither of the above, then evidently this source packet intersects our mixer's filter.
  // Compute the offset into the dest buffer where our first generated sample should land, and the
  // offset into the source packet where we should start sampling.
  int64_t dest_offset_64 = 0;
  Fixed frac_source_offset_64 =
      frac_source_for_first_mix_job_frame - frac_source_for_first_packet_frame;
  Fixed frac_source_pos_edge_first_mix_frame =
      frac_source_for_first_mix_job_frame + mixer.pos_filter_width();

  // If the packet's first frame comes after the filter window's positive edge,
  // then we should skip some frames in the destination buffer before starting to produce data.
  if (frac_source_for_first_packet_frame > frac_source_pos_edge_first_mix_frame) {
    const TimelineRate& dest_to_src = info.dest_frames_to_frac_source_frames.rate();
    // The dest_buffer offset is based on the distance from mix job start to packet start (measured
    // in frac_frames), converted into frames in the destination timeline. As we scale the
    // frac_frame delta into dest frames, we want to "round up" any subframes that are present; any
    // src subframes should push our dest frame up to the next integer. Because we entered this IF
    // in the first place, we have at least some fractional src delta, thus dest_offset_64 is
    // guaranteed to become greater than zero.
    //
    // When a position is round-trip converted to another timeline and back again, there is no
    // guarantee that it will result in the exact original value. To make source -> dest -> source
    // as accurate as possible (and critically, to ensure that source position does not move
    // backward), we "round up" when translating from source (fractional) to dest (integral).
    Fixed first_source_mix_point =
        frac_source_for_first_packet_frame - frac_source_pos_edge_first_mix_frame;
    dest_offset_64 = dest_to_src.Inverse().Scale(first_source_mix_point.raw_value(),
                                                 TimelineRate::RoundingMode::Ceiling);
    FX_DCHECK(dest_offset_64 > 0);

    frac_source_offset_64 += Fixed::FromRaw(dest_to_src.Scale(dest_offset_64));

    // Packet is within the mix window but starts after mix start. MixStream breaks mix jobs into
    // multiple pieces so that each packet gets its own ProcessMix call; this means there was no
    // contiguous packet immediately before this one. For now we don't report this as a problem;
    // eventually (when we can rely on clients to accurately set STREAM_PACKET_FLAG_DISCONTINUITY),
    // we should report this as a minor discontinuity if that flag is NOT set -- via something like
    //    stream.ReportPartialUnderflow(frac_source_offset_64,dest_offset_64)
    //
    // TODO(mpuryear): move packet discontinuity (gap/overlap) detection up into the
    // Renderer/PacketQueue, and remove PartialUnderflow reporting and the metric altogether.
  }

  FX_DCHECK(dest_offset_64 >= 0);
  FX_DCHECK(dest_offset_64 <= static_cast<int64_t>(dest_frames_left));
  auto dest_offset = static_cast<uint32_t>(dest_offset_64);

  FX_DCHECK(frac_source_offset_64 <= std::numeric_limits<int32_t>::max());
  FX_DCHECK(frac_source_offset_64 >= std::numeric_limits<int32_t>::min());
  auto frac_source_offset = Fixed(frac_source_offset_64);

  // Looks like we are ready to go. Mix.
  FX_DCHECK(frac_source_offset + mixer.pos_filter_width() >= Fixed(0));
  bool consumed_source;
  if (dest_offset >= dest_frames_left) {
    // We initially needed to source frames from this packet in order to finish this mix. After
    // realigning our sampling point to the nearest dest frame, that dest frame is now at or beyond
    // the end of this mix job. We have no need to mix any source material now, just exit.
    consumed_source = false;
  } else if (frac_source_offset + mixer.pos_filter_width() >= source_buffer.length()) {
    // This packet was initially within our mix window. After realigning our sampling point to the
    // nearest dest frame, it is now entirely in the past. This can only occur when down-sampling
    // and is made more likely if the rate conversion ratio is very high. We've already reported
    // a partial underflow when realigning, so just complete the packet and move on to the next.
    consumed_source = true;
  } else {
    // When calling Mix(), we communicate the resampling rate with three parameters. We augment
    // step_size with rate_modulo and denominator arguments that capture the remaining rate
    // component that cannot be expressed by a 19.13 fixed-point step_size. Note: step_size and
    // frac_source_offset use the same format -- they have the same limitations in what they can and
    // cannot communicate.
    //
    // For perfect position accuracy, just as we track incoming/outgoing fractional source offset,
    // we also need to track the ongoing subframe_position_modulo. This is now added to Mix() and
    // maintained across calls, but not initially set to any value other than zero. For now, we are
    // deferring that work, since any error would be less than 1 fractional frame.
    //
    // Q: Why did we solve this issue for Rate but not for initial Position?
    // A: We solved this issue for *rate* because its effect accumulates over time, causing clearly
    // measurable distortion that becomes crippling with larger jobs. For *position*, there is no
    // accumulated magnification over time -- in analyzing the distortion that this should cause,
    // mix job size affects the distortion's frequency but not its amplitude. We expect the effects
    // to be below audible thresholds. Until the effects are measurable and attributable to this
    // jitter, we will defer this work.
    auto prev_dest_offset = dest_offset;
    auto dest_ref_clock_to_integral_dest_frame =
        ReferenceClockToIntegralFrames(cur_mix_job_.dest_ref_clock_to_frac_dest_frame);

    // Check whether we are still ramping
    bool ramping = bookkeeping.gain.IsRamping();
    if (ramping) {
      bookkeeping.gain.GetScaleArray(
          bookkeeping.scale_arr.get(),
          std::min(dest_frames_left - dest_offset, Mixer::Bookkeeping::kScaleArrLen),
          dest_ref_clock_to_integral_dest_frame.rate());
    }

    {
      int32_t raw_source_offset = frac_source_offset.raw_value();
      consumed_source = mixer.Mix(buf, dest_frames_left, &dest_offset, source_buffer.payload(),
                                  source_buffer.length().raw_value(), &raw_source_offset,
                                  cur_mix_job_.accumulate);
      frac_source_offset = Fixed::FromRaw(raw_source_offset);
      cur_mix_job_.usages_mixed.insert_all(source_buffer.usage_mask());
      // The gain for the stream will be any previously applied gain combined with any additional
      // gain that will be applied at this stage. In terms of the applied gain of the mixed stream,
      // we consider that to be the max gain of any single source stream.
      float stream_gain_db =
          Gain::CombineGains(source_buffer.gain_db(), bookkeeping.gain.GetGainDb());
      cur_mix_job_.applied_gain_db = std::max(cur_mix_job_.applied_gain_db, stream_gain_db);
    }

    // If src is ramping, advance that ramp by the amount of dest that was just mixed.
    if (ramping) {
      bookkeeping.gain.Advance(dest_offset - prev_dest_offset,
                               dest_ref_clock_to_integral_dest_frame.rate());
    }
  }

  FX_DCHECK(dest_offset <= dest_frames_left);
  info.AdvanceRunningPositionsBy(dest_offset, bookkeeping);

  if (consumed_source) {
    FX_DCHECK(frac_source_offset + mixer.pos_filter_width() >= source_buffer.length());
  }

  info.frames_produced += dest_offset;
  FX_DCHECK(info.frames_produced <= cur_mix_job_.buf_frames);

  return consumed_source;
}

// We compose the effects of clock reconciliation into our sample-rate-conversion step size, but
// only for streams that use neither our adjustable clock, nor the clock designated as driving our
// hardware-rate-adjustments. We apply this micro-SRC via an intermediate "slew away the error"
// rate-correction factor driven by a PID control. Why use a PID? Sources do not merely chase the
// other clock's rate -- they chase its position. Note that even if we don't adjust our rate, we
// still want a composed transformation for offsets.
//
// Calculate the composed dest-to-src transformation and update the mixer's bookkeeping for
// step_size etc. These are the only deliverables for this method.
void MixStage::ReconcileClocksAndSetStepSize(Mixer::SourceInfo& info,
                                             Mixer::Bookkeeping& bookkeeping,
                                             ReadableStream& stream) {
  TRACE_DURATION("audio", "MixStage::ReconcileClocksAndSetStepSize");

  auto& source_clock = stream.reference_clock();
  auto& dest_clock = reference_clock();

  // Right upfront, capture current states for the source and destination clocks.
  auto source_ref_to_clock_mono = source_clock.ref_clock_to_clock_mono();
  auto dest_ref_to_mono = dest_clock.ref_clock_to_clock_mono();

  // UpdateSourceTrans
  //
  // Ensure the mappings from source-frame to source-ref-time and monotonic-time are up-to-date.
  auto snapshot = stream.ref_time_to_frac_presentation_frame();
  info.source_ref_clock_to_frac_source_frames = snapshot.timeline_function;

  if (info.source_ref_clock_to_frac_source_frames.subject_delta() == 0) {
    info.clock_mono_to_frac_source_frames = TimelineFunction();
    info.dest_frames_to_frac_source_frames = TimelineFunction();
    bookkeeping.step_size = 0;
    bookkeeping.rate_modulo = 0;  // we need not also clear rate_mod and pos_mod

    return;
  }

  // Ensure the mappings from source-frame to monotonic-time is up-to-date.
  auto frac_source_frame_to_clock_mono =
      source_ref_to_clock_mono * info.source_ref_clock_to_frac_source_frames.Inverse();
  info.clock_mono_to_frac_source_frames = frac_source_frame_to_clock_mono.Inverse();
  FX_LOGS(TRACE) << clock::TimelineFunctionToString(info.clock_mono_to_frac_source_frames,
                                                    "mono-to-frac-src");

  // Assert we can map from local monotonic-time to fractional source frames.
  FX_DCHECK(info.clock_mono_to_frac_source_frames.rate().reference_delta());

  // UpdateDestTrans
  //
  // Ensure the mappings from dest-frame to monotonic-time is up-to-date.
  // We should only be here if we have a valid mix job. This means a job which supplies a valid
  // transformation from reference time to destination frames (based on dest frame rate).
  FX_DCHECK(cur_mix_job_.dest_ref_clock_to_frac_dest_frame.rate().reference_delta());
  if (cur_mix_job_.dest_ref_clock_to_frac_dest_frame.rate().subject_delta() == 0) {
    info.dest_frames_to_frac_source_frames = TimelineFunction();
    bookkeeping.step_size = 0;
    bookkeeping.rate_modulo = 0;  // we need not also clear rate_mod and pos_mod

    return;
  }

  auto dest_frames_to_dest_ref =
      ReferenceClockToIntegralFrames(cur_mix_job_.dest_ref_clock_to_frac_dest_frame).Inverse();

  // Compose our transformation from local monotonic-time to dest frames.
  auto dest_frames_to_clock_mono = dest_ref_to_mono * dest_frames_to_dest_ref;
  FX_LOGS(TRACE) << clock::TimelineFunctionToString(dest_frames_to_clock_mono, "dest-to-mono");

  // ComposeDestToSource
  //
  // Compose our transformation from destination frames to source fractional frames.
  info.dest_frames_to_frac_source_frames =
      info.clock_mono_to_frac_source_frames * dest_frames_to_clock_mono;
  FX_LOGS(TRACE) << clock::TimelineRateToString(info.dest_frames_to_frac_source_frames.rate(),
                                                "dest-to-frac-src (with clocks)");

  // ComputeFrameRateConversionRatio
  //
  // Calculate the TimelineRate for step_size. No clock effects are included; any "micro-SRC" is
  // applied separately as a subsequent correction factor.
  TimelineRate frac_src_frames_per_dest_frame = TimelineRate::Product(
      dest_frames_to_dest_ref.rate(), info.source_ref_clock_to_frac_source_frames.rate());
  FX_LOGS(TRACE) << clock::TimelineRateToString(frac_src_frames_per_dest_frame,
                                                "dest-to-frac-src rate (no clock effects)");

  // Check for dest position discontinuity. If so, reset positions and rate adjustments.
  auto dest_frame = cur_mix_job_.dest_start_frame;
  auto mono_now_from_dest = zx::time{dest_frames_to_clock_mono.Apply(dest_frame)};

  // TODO(fxbug.dev/63750): pass through a signal if we expect discontinuity (Play, Pause, packet
  // discontinuity bit); use it to log (or report to inspect) only unexpected discontinuities.
  // Add a test to validate that we log discontinuities only when we should.
  if (!info.initial_position_is_set || info.next_dest_frame != dest_frame) {
    // These are only needed for the FX_LOG
    auto prev_running_dest_frame = info.next_dest_frame;
    auto prev_running_frac_src_frame = info.next_frac_source_frame;
    auto position_was_set = info.initial_position_is_set;

    // Set new running positions, based on the E2E clock (not just from step_size)
    info.ResetPositions(dest_frame, bookkeeping);

    if (position_was_set) {
      FX_LOGS(DEBUG) << "Dest discontinuity ["
                     << (dest_clock.is_client_clock() ? "Client" : "Device")
                     << (dest_clock.is_adjustable() ? "Adjustable" : "Fixed") << "] of "
                     << dest_frame - prev_running_dest_frame << " frames (expect "
                     << prev_running_dest_frame << ", actual " << dest_frame << ")";
      FX_LOGS(DEBUG) << "Updated source [" << (source_clock.is_client_clock() ? "Client" : "Device")
                     << (source_clock.is_adjustable() ? "Adjustable" : "Fixed")
                     << "] position from " << prev_running_frac_src_frame.raw_value() << " to "
                     << info.next_frac_source_frame.raw_value();
    }

    // If source/dest clocks are the same, they're always in-sync, but above we will still reset our
    // dest offset (if we have not previously established this, or if there was a discontinuity).
    if (source_clock != dest_clock) {
      source_clock.ResetRateAdjustment(mono_now_from_dest);
      dest_clock.ResetRateAdjustment(mono_now_from_dest);
    }

    SetStepSize(info, bookkeeping, frac_src_frames_per_dest_frame);
    return;
  }

  auto mono_now_from_src = zx::time{
      info.clock_mono_to_frac_source_frames.ApplyInverse(info.next_frac_source_frame.raw_value())};

  FX_LOGS(TRACE) << "Dest " << dest_frame << ", frac_src "
                 << info.next_frac_source_frame.raw_value() << ", mono_now_from_dest "
                 << mono_now_from_dest.get() << ", mono_now_from_src " << mono_now_from_src.get();

  // Convert both positions to monotonic time and get the delta -- this is source position error
  info.src_pos_error = mono_now_from_src - mono_now_from_dest;
  FX_LOGS(TRACE) << "mono_now_from_src " << mono_now_from_src.get() << ", mono_now_from_dest "
                 << mono_now_from_dest.get() << ", src_pos_err " << info.src_pos_error.get();

  // For start dest frame, measure [predicted - actual] error (in monotonic) since last mix,
  // even if clocks are same on both sides. This allows us to perform an initial sync-up between
  // running position accounting and the initial clock transforms -- even those with offsets.
  auto abs_pos_err = std::abs(info.src_pos_error.get());
  if (abs_pos_err > kMaxErrorThresholdDuration.get()) {
    Reporter::Singleton().MixerClockSkewDiscontinuity(info.src_pos_error);

    FX_LOGS(INFO) << "Stream " << static_cast<void*>(&stream) << " is out of sync by "
                  << (static_cast<double>(info.src_pos_error.get()) / ZX_MSEC(1))
                  << " msec (limit: "
                  << (static_cast<double>(kMaxErrorThresholdDuration.get()) / ZX_MSEC(1))
                  << " msec); resetting stream position.";
    AudioClock::DisplaySyncInfo(source_clock, dest_clock);

    // Source error exceeds our threshold; reset rate adjustment altogether; allow a discontinuity
    auto src_frac_pos = Fixed::FromRaw(info.dest_frames_to_frac_source_frames(dest_frame));
    info.next_frac_source_frame = src_frac_pos;
    info.src_pos_error = zx::duration(0);

    // Reset PID controls in the relevant clocks.
    source_clock.ResetRateAdjustment(mono_now_from_dest);
    dest_clock.ResetRateAdjustment(mono_now_from_dest);

    SetStepSize(info, bookkeeping, frac_src_frames_per_dest_frame);
    return;
  }

  auto micro_src_ppm = AudioClock::SynchronizeClocks(source_clock, dest_clock, mono_now_from_dest,
                                                     info.src_pos_error);

  if (micro_src_ppm) {
    TimelineRate micro_src_factor{static_cast<uint64_t>(1'000'000 + micro_src_ppm), 1'000'000};

    // Product might exceed uint64/uint64, so allow reduction. Approximation is OK, since clocks
    // (not SRC/step_size) determines a stream absolute position. SRC just chases the position.
    frac_src_frames_per_dest_frame =
        TimelineRate::Product(frac_src_frames_per_dest_frame, micro_src_factor, false);
  }

  SetStepSize(info, bookkeeping, frac_src_frames_per_dest_frame);
}

void MixStage::SetStepSize(Mixer::SourceInfo& info, Mixer::Bookkeeping& bookkeeping,
                           TimelineRate& frac_src_frames_per_dest_frame) {
  // SetStepSize
  //
  // Convert the TimelineRate into [step_size, denominator, rate_modulo] as usual.
  FX_DCHECK(frac_src_frames_per_dest_frame.reference_delta());
  int64_t tmp_step_size = frac_src_frames_per_dest_frame.Scale(1);

  FX_DCHECK(tmp_step_size >= 0);
  FX_DCHECK(tmp_step_size <= std::numeric_limits<uint32_t>::max());

  auto old_denominator = bookkeeping.denominator;
  bookkeeping.step_size = static_cast<uint32_t>(tmp_step_size);
  bookkeeping.denominator = frac_src_frames_per_dest_frame.reference_delta();
  bookkeeping.rate_modulo = frac_src_frames_per_dest_frame.subject_delta() -
                            (bookkeeping.denominator * bookkeeping.step_size);

  // If the denominator is changing, update the source position modulos.
  if (old_denominator != bookkeeping.denominator) {
    // Reinterpret previous per-job and long-running src_pos_mod values for the new denominator
    if (bookkeeping.src_pos_modulo) {
      __uint128_t tmp_src_pos_modulo =
          static_cast<__uint128_t>(bookkeeping.src_pos_modulo) * bookkeeping.denominator;
      bookkeeping.src_pos_modulo = tmp_src_pos_modulo / old_denominator;
    }
    if (info.next_src_pos_modulo) {
      __uint128_t tmp_next_src_pos_modulo =
          static_cast<__uint128_t>(info.next_src_pos_modulo) * bookkeeping.denominator;
      info.next_src_pos_modulo = tmp_next_src_pos_modulo / old_denominator;
    }
  }
  // Otherwise, preserve the previous source position modulo values
}

}  // namespace media::audio
