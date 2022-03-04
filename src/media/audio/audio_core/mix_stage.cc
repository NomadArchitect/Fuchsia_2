// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/media/audio/audio_core/mix_stage.h"

#include <fuchsia/media/audio/cpp/fidl.h>
#include <lib/fit/defer.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/trace/event.h>
#include <lib/zx/clock.h>
#include <zircon/status.h>

#include <algorithm>
#include <iomanip>
#include <limits>
#include <memory>

#include <ffl/string.h>

#include "src/media/audio/audio_core/base_renderer.h"
#include "src/media/audio/audio_core/mixer/mixer.h"
#include "src/media/audio/audio_core/mixer/no_op.h"
#include "src/media/audio/audio_core/reporter.h"
#include "src/media/audio/audio_core/silence_padding_stream.h"
#include "src/media/audio/lib/clock/utils.h"
#include "src/media/audio/lib/timeline/timeline_rate.h"

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

// Source position errors generally represent only the rate difference between time sources. We
// reconcile clocks upon every ReadLock call, so even with wildly divergent clocks (+1000ppm vs.
// -1000ppm) source position error would be 1/50 of the duration between ReadLock calls. If source
// position error exceeds this limit, we stop rate-adjustment and instead 'snap' to the expected pos
// (referred to as "jam sync"). This manifests as a discontinuity or dropout for this stream only.
//
// For reference, micro-SRC can smoothly eliminate errors of this duration in less than 1 sec (at
// kMicroSrcAdjustmentPpmMax). If adjusting a zx::clock, this will take approx. 2 seconds.
constexpr zx::duration kMaxErrorThresholdDuration = zx::msec(2);

// To what extent should jam-synchronizations be logged? Worst-case logging can exceed 100/sec.
// We log each MixStage's first occurrence; for subsequent instances, depending on audio_core's
// logging level, we throttle the logging frequency depending on log_level.
// By default NDEBUG builds are WARNING, and DEBUG builds INFO. To disable jam-sync logging for a
// certain level, set the interval to 0. To disable all jam-sync logging, set kLogJamSyncs to false.
constexpr bool kLogJamSyncs = true;
constexpr uint16_t kJamSyncWarningInterval = 200;  // Log 1 of every 200 jam-syncs at WARNING
constexpr uint16_t kJamSyncInfoInterval = 20;      // Log 1 of every 20 jam-syncs at INFO
constexpr uint16_t kJamSyncTraceInterval = 1;      // Log all remaining jam-syncs at TRACE

constexpr bool kLogReconciledTimelineFunctions = false;
constexpr bool kLogInitialPositionSync = false;
constexpr bool kLogDestDiscontinuities = true;
// Use logging strides that are prime, to avoid seeing only certain message cadences.
constexpr int kPositionLogStride = 997;

}  // namespace

MixStage::MixStage(const Format& output_format, uint32_t block_size,
                   TimelineFunction ref_time_to_frac_presentation_frame, AudioClock& audio_clock,
                   std::optional<float> min_gain_db, std::optional<float> max_gain_db)
    : MixStage(output_format, block_size,
               fbl::MakeRefCounted<VersionedTimelineFunction>(ref_time_to_frac_presentation_frame),
               audio_clock, min_gain_db, max_gain_db) {}

MixStage::MixStage(const Format& output_format, uint32_t block_size,
                   fbl::RefPtr<VersionedTimelineFunction> ref_time_to_frac_presentation_frame,
                   AudioClock& audio_clock, std::optional<float> min_gain_db,
                   std::optional<float> max_gain_db)
    : ReadableStream("MixStage", output_format),
      output_buffer_frames_(block_size),
      output_buffer_(block_size * output_format.channels()),
      output_ref_clock_(audio_clock),
      output_ref_clock_to_fractional_frame_(ref_time_to_frac_presentation_frame),
      gain_limits_{
          .min_gain_db = min_gain_db,
          .max_gain_db = max_gain_db,
      } {
  FX_CHECK(format().sample_format() == fuchsia::media::AudioSampleFormat::FLOAT)
      << "MixStage must output FLOATs; got format = " << static_cast<int>(format().sample_format());
}

std::shared_ptr<Mixer> MixStage::AddInput(std::shared_ptr<ReadableStream> stream,
                                          std::optional<float> initial_dest_gain_db,
                                          Mixer::Resampler resampler_hint) {
  TRACE_DURATION("audio", "MixStage::AddInput");
  if (!stream) {
    FX_LOGS(ERROR) << "Null stream, cannot add";
    return nullptr;
  }

  if (resampler_hint == Mixer::Resampler::Default &&
      AudioClock::SynchronizationNeedsHighQualityResampler(stream->reference_clock(),
                                                           reference_clock())) {
    resampler_hint = Mixer::Resampler::WindowedSinc;
  }

  auto mixer =
      std::shared_ptr<Mixer>(Mixer::Select(stream->format().stream_type(), format().stream_type(),
                                           resampler_hint, gain_limits_)
                                 .release());
  if (!mixer) {
    mixer = std::make_unique<audio::mixer::NoOp>();
  }

  if (initial_dest_gain_db) {
    mixer->bookkeeping().gain.SetDestGain(*initial_dest_gain_db);
  }

  auto original_stream = stream;
  stream = SilencePaddingStream::WrapIfNeeded(
      stream, mixer->neg_filter_width() + mixer->pos_filter_width(),
      // PointSampler doesn't need ringout, so this doesn't matter.
      // SincSampler needs ringout and wants to keep fractional gaps, so round down.
      /*fractional_gaps_round_down=*/true);
  stream->SetPresentationDelay(GetPresentationDelay() + LeadTimeForMixer(stream->format(), *mixer));

  FX_LOGS(DEBUG) << "AddInput "
                 << (stream->reference_clock().is_adjustable() ? "adjustable " : "static ")
                 << (stream->reference_clock().is_device_clock() ? "device" : "client") << " (self "
                 << (reference_clock().is_adjustable() ? "adjustable " : "static ")
                 << (reference_clock().is_device_clock() ? "device)" : "client)");
  {
    std::lock_guard<std::mutex> lock(stream_lock_);
    streams_.emplace_back(StreamHolder{
        .stream = std::move(stream),
        .original_stream = std::move(original_stream),
        .mixer = mixer,
    });
  }
  return mixer;
}

void MixStage::RemoveInput(const ReadableStream& stream) {
  TRACE_DURATION("audio", "MixStage::RemoveInput");
  std::lock_guard<std::mutex> lock(stream_lock_);
  auto it = std::find_if(streams_.begin(), streams_.end(), [stream = &stream](const auto& holder) {
    return holder.original_stream.get() == stream;
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

std::optional<ReadableStream::Buffer> MixStage::ReadLockImpl(ReadLockContext& ctx, Fixed dest_frame,
                                                             int64_t frame_count) {
  memset(&cur_mix_job_, 0, sizeof(cur_mix_job_));

  auto snapshot = ref_time_to_frac_presentation_frame();

  cur_mix_job_.read_lock_ctx = &ctx;
  cur_mix_job_.buf = &output_buffer_[0];
  cur_mix_job_.buf_frames = std::min(static_cast<int64_t>(frame_count), output_buffer_frames_);
  cur_mix_job_.dest_start_frame = dest_frame.Floor();
  cur_mix_job_.dest_ref_clock_to_frac_dest_frame = snapshot.timeline_function;
  cur_mix_job_.total_applied_gain_db = fuchsia::media::audio::MUTED_GAIN_DB;

  // Fill the output buffer with silence.
  ssize_t bytes_to_zero = cur_mix_job_.buf_frames * format().bytes_per_frame();
  std::memset(cur_mix_job_.buf, 0, bytes_to_zero);
  ForEachSource(TaskType::Mix, dest_frame);

  if (cur_mix_job_.total_applied_gain_db <= fuchsia::media::audio::MUTED_GAIN_DB) {
    // Either we mixed no streams, or all the streams mixed were muted. Either way we can just
    // return nullopt to signify we have no audible frames.
    return std::nullopt;
  }

  return MakeCachedBuffer(Fixed(dest_frame.Floor()), cur_mix_job_.buf_frames, cur_mix_job_.buf,
                          cur_mix_job_.usages_mixed, cur_mix_job_.total_applied_gain_db);
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

  if constexpr (kLogPresentationDelay) {
    FX_LOGS(INFO) << "    (" << this << ") " << __FUNCTION__ << " given external_delay "
                  << external_delay.to_nsecs() << "ns";
  }

  ReadableStream::SetPresentationDelay(external_delay);

  // Propagate time to our sources.
  std::lock_guard<std::mutex> lock(stream_lock_);
  for (const auto& holder : streams_) {
    FX_DCHECK(holder.stream);
    FX_DCHECK(holder.mixer);

    zx::duration mixer_lead_time = LeadTimeForMixer(holder.stream->format(), *holder.mixer);

    if constexpr (kLogPresentationDelay) {
      FX_LOGS(INFO) << "Adding LeadTimeForMixer " << mixer_lead_time.to_nsecs()
                    << "ns to external_delay " << external_delay.to_nsecs() << "ns";
      FX_LOGS(INFO) << "    (" << this << ") " << __FUNCTION__
                    << " setting child stream total delay "
                    << (external_delay + mixer_lead_time).to_nsecs() << "ns";
    }

    holder.stream->SetPresentationDelay(external_delay + mixer_lead_time);
  }
}

void MixStage::TrimImpl(Fixed dest_frame) {
  TRACE_DURATION("audio", "MixStage::Trim", "frame", dest_frame.Floor());
  ForEachSource(TaskType::Trim, dest_frame);
}

void MixStage::ForEachSource(TaskType task_type, Fixed dest_frame) {
  TRACE_DURATION("audio", "MixStage::ForEachSource");

  std::vector<StreamHolder> sources;
  {
    std::lock_guard<std::mutex> lock(stream_lock_);
    for (const auto& holder : streams_) {
      sources.emplace_back(StreamHolder{holder});
    }
  }

  for (auto& source : sources) {
    if (task_type == TaskType::Mix) {
      auto& source_info = source.mixer->source_info();
      auto& bookkeeping = source.mixer->bookkeeping();
      ReconcileClocksAndSetStepSize(source_info, bookkeeping, *source.stream);
      MixStream(*source.mixer, *source.stream);
    } else {
      // Call this just once: it may be relatively expensive as it requires a lock and
      // (sometimes) additional computation.
      TimelineFunction source_ref_time_to_frac_presentation_frame =
          source.stream->ref_time_to_frac_presentation_frame().timeline_function;

      // If the source is currently paused, the translation from dest to source position
      // may not be defined, so don't Trim anything.
      if (!source_ref_time_to_frac_presentation_frame.subject_delta()) {
        continue;
      }

      auto dest_ref_time = RefTimeAtFracPresentationFrame(dest_frame);
      auto mono_time = reference_clock().MonotonicTimeFromReferenceTime(dest_ref_time);
      auto source_ref_time =
          source.stream->reference_clock().ReferenceTimeFromMonotonicTime(mono_time);
      auto source_frame =  // source.stream->FracPresentationFrameAtRefTime(source_ref_time);
          Fixed::FromRaw(source_ref_time_to_frac_presentation_frame.Apply(source_ref_time.get()));
      source.stream->Trim(source_frame);
    }
  }
}

void MixStage::MixStream(Mixer& mixer, ReadableStream& stream) {
  TRACE_DURATION("audio", "MixStage::MixStream");
  auto& info = mixer.source_info();
  auto& bookkeeping = mixer.bookkeeping();

  // If the source is currently paused, source frames do not advance hence there's nothing to mix.
  // However, destination frames continue to advance.
  if (!info.dest_frames_to_frac_source_frames.subject_delta()) {
    return;
  }

  // Each iteration through the loop, we grab a source buffer and produce as many destination
  // frames as possible. As we go, dest_offset tracks our position in our output buffer. Our
  // absolute position is cur_mix_job_.dest_start_frame + dest_offset.
  const int64_t dest_frames = cur_mix_job_.buf_frames;
  int64_t dest_offset = 0;

  while (dest_offset < dest_frames) {
    const int64_t prev_dest_offset = dest_offset;
    auto source_buffer = NextSourceBuffer(mixer, stream, dest_frames - dest_offset);
    if (!source_buffer) {
      break;
    }

    if constexpr (kMixerPositionTraceEvents) {
      TRACE_DURATION("audio", "MixStage::MixStream position", "start",
                     source_buffer->start().Integral().Floor(), "start.frac",
                     source_buffer->start().Fraction().raw_value(), "length",
                     source_buffer->length(), "next_source_frame",
                     info.next_source_frame.Integral().Floor(), "next_source_frame.frac",
                     info.next_source_frame.Fraction().raw_value(), "dest_offset", dest_offset,
                     "dest_frames", dest_frames);
    }

    // ReadLock guarantees that source_buffer must intersect our current mix job, hence
    // source_buffer should not be in the future nor the past.
    //
    // We'll start sampling at info.next_source_frame.
    // Compute the offset of this frame in our source buffer.
    Fixed source_offset = info.next_source_frame - source_buffer->start();

    // To compute the destination frame D centered at source frame S, we'll use frames from
    // a window surrounding S, defined by the pos and neg filter widths. For example, if we are
    // down-sampling, the streams may look like:
    //
    //    source stream ++++++++++++++S++++++++++++++++++++++
    //                          |     ^     |
    //                          +-----+-----+
    //                            neg | pos
    //                                |
    //                                V
    //      dest stream +   +   +   + D +   +   +   +   +   +
    //
    // At this point in the code, D = dest_offset and S = info.next_source_frame. This is our
    // starting point. There are two interesting cases:
    //
    //  1. S-1.0 < source_buffer->start() <= S + pos_filter_width
    //
    //     The first source_buffer frame can be used to produce frame D. This is the common case
    //     for continuous (gapless) streams of audio. In this case, our resampler has cached all
    //     source frames in the range [S-neg,X-1], where X = source_buffer->start(). We combine
    //     those cached frames with the first S+pos-X frames from the source_buffer to produce D.
    //
    //  2. source_buffer->start() > S + pos_filter_width
    //
    //     The first source_buffer frame is beyond the last frame needed to produce frame D. This
    //     means there is a gap in the source stream. Because our source is wrapped with a
    //     SilencePaddingStream, there must have been at least neg+pos silent frames before that
    //     gap, hence our resampler has quiesced to a "silent" state and will fill that gap with
    //     silence. This implies that all frames in the range [S-neg,S+pos] are silent, and hence
    //     D is silent as well. Since the destination buffer is zeroed before we start mixing, we
    //     don't need to produce frame D. Instead we advance dest_offset to the first frame D'
    //     whose sampling window includes source_buffer->start(). This is handled below.
    //
    int64_t initial_dest_advance = 0;
    if (source_buffer->start() > info.next_source_frame + mixer.pos_filter_width()) {
      // To illustrate:
      //
      //    source stream ++S+++++++++++++++++++++++S'++++X++++++++++++
      //                    ^     |           |     ^     |
      //                    +-----+           +-----+-----+
      //                    | pos               neg | pos
      //                    |                       |
      //                    V                       V
      //      dest stream + D +   +   +   +   +   + D'+   +   +   +   +
      //
      // S  = current source position (info.next_source_frame)
      // X  = source_buffer->start()
      // D  = current dest position (dest_offset)
      // D' = first dest frame whose sampling window overlaps with source_buffer->start()
      // S' = source position after advancing to D'

      // We need to advance at least this many source frames.
      auto mix_to_packet_gap =
          Fixed(source_buffer->start() - info.next_source_frame - mixer.pos_filter_width());

      // We need to advance this many destination frames to find a D' as illustrated above,
      // but don't advance past the end of the destination buffer.
      initial_dest_advance = Mixer::Bookkeeping::SourceLenToDestLen(
          mix_to_packet_gap, bookkeeping.step_size, bookkeeping.rate_modulo(),
          bookkeeping.denominator(), bookkeeping.source_pos_modulo);
      initial_dest_advance = std::clamp(initial_dest_advance, 0l, dest_frames - dest_offset);

      // Advance our long-running positions.
      auto initial_source_running_position = info.next_source_frame;
      auto initial_source_offset = source_offset;
      auto initial_source_pos_modulo = bookkeeping.source_pos_modulo;
      info.AdvanceAllPositionsBy(initial_dest_advance, bookkeeping);

      // Advance our local offsets.
      // We advance the source_offset the same amount as we advanced info.next_source_frame.
      dest_offset += initial_dest_advance;
      source_offset =
          Fixed(initial_source_offset + info.next_source_frame - initial_source_running_position);

      if constexpr (kMixerPositionTraceEvents) {
        TRACE_DURATION("audio", "initial_dest_advance", "initial_dest_advance",
                       initial_dest_advance);
      }

      FX_CHECK(source_offset + mixer.pos_filter_width() >= Fixed(0))
          << "source_offset (" << ffl::String::DecRational << source_offset << ") + pos_width ("
          << Fixed(-mixer.pos_filter_width()) << ") should >= 0 -- source running position was "
          << initial_source_running_position << " (+ " << initial_source_pos_modulo << "/"
          << bookkeeping.denominator() << " modulo), is now " << info.next_source_frame << " (+ "
          << bookkeeping.source_pos_modulo << "/" << bookkeeping.denominator()
          << " modulo); advanced dest by " << initial_dest_advance;

      FX_CHECK(dest_offset <= dest_frames)
          << ffl::String::DecRational << "dest_offset " << dest_offset << " advanced by "
          << initial_dest_advance << " to " << dest_frames << ", exceeding " << dest_frames << ";"
          << " mix_to_packet_gap=" << mix_to_packet_gap << " step_size=" << bookkeeping.step_size
          << " rate_modulo=" << bookkeeping.rate_modulo()
          << " denominator=" << bookkeeping.denominator()
          << " source_pos_modulo=" << bookkeeping.source_pos_modulo << " (was "
          << initial_source_pos_modulo << ")";
    }

    // Consume as much of this source buffer as possible.
    int64_t source_frames_consumed;

    // Invariant: dest_offset <= dest_frames (see FX_CHECK above).
    if (dest_offset == dest_frames) {
      // We skipped so many frames in the destination buffer that we overran the end of the buffer.
      // We are done with this job. This can happen when there is a large gap between our initial
      // source position and source_buffer->start().
      source_frames_consumed = 0;
    } else if (Fixed(source_offset) - mixer.neg_filter_width() >= Fixed(source_buffer->length())) {
      // The source buffer was initially within our mix window, but after skipping destination
      // frames, it is now entirely in the past. This can only occur when down-sampling and is made
      // more likely if the rate conversion ratio is very high. In the example below, D and S are
      // the initial dest and source positions, D' and S' are the new positions after skipping
      // destination frames, and X marks the source buffer, which is not in the sampling window for
      // either D or D'.
      //
      //    source stream ++++++++++++++++++S++++++++++XXXXXXXXXXXX+++++++++++++S'+++++
      //                              |     ^     |                       |     ^     |
      //                              +-----+-----+                       +-----+-----+
      //                                neg | pos                           neg | pos
      //                                    |                                   |
      //                                    V                                   V
      //      dest stream +                 D                 +                 D'
      //
      source_frames_consumed = source_buffer->length();
    } else {
      // We have source and destination frames available.
      auto dest_frames_per_dest_ref_clock_nsec =
          ReferenceClockToIntegralFrames(cur_mix_job_.dest_ref_clock_to_frac_dest_frame).rate();

      // Check whether we are still ramping
      float local_gain_db;
      const bool ramping = bookkeeping.gain.IsRamping();
      if (ramping) {
        // TODO(fxbug.dev/94160): make less error-prone
        auto scale_arr_max = bookkeeping.gain.CalculateScaleArray(
            bookkeeping.scale_arr.get(),
            std::min(dest_frames - dest_offset, Mixer::Bookkeeping::kScaleArrLen),
            dest_frames_per_dest_ref_clock_nsec);
        local_gain_db = Gain::ScaleToDb(scale_arr_max);
      } else {
        local_gain_db = bookkeeping.gain.GetGainDb();
      }

      StageMetricsTimer timer("Mixer::Mix");
      timer.Start();

      const int64_t dest_offset_before_mix = dest_offset;
      mixer.Mix(cur_mix_job_.buf, dest_frames, &dest_offset, source_buffer->payload(),
                source_buffer->length(), &source_offset, cur_mix_job_.accumulate);

      timer.Stop();
      cur_mix_job_.read_lock_ctx->AddStageMetrics(timer.Metrics());

      source_frames_consumed = std::min(Fixed(source_offset + mixer.pos_filter_width()).Floor(),
                                        source_buffer->length());
      cur_mix_job_.usages_mixed.insert_all(source_buffer->usage_mask());

      // Check that we did not overflow the buffer.
      FX_CHECK(dest_offset <= dest_frames)
          << ffl::String::DecRational << "dest_offset(before)=" << dest_offset_before_mix
          << " dest_offset(after)=" << dest_offset << " dest_frames=" << dest_frames
          << " source_buffer.start=" << source_buffer->start()
          << " source_buffer.length=" << source_buffer->length()
          << " source_offset(final)=" << source_offset;

      // Total applied gain: previously applied gain, plus any gain added at this stage.
      float total_applied_gain_db =
          Gain::CombineGains(source_buffer->total_applied_gain_db(), local_gain_db);
      // Record the max applied gain of any source stream.
      cur_mix_job_.total_applied_gain_db =
          std::max(cur_mix_job_.total_applied_gain_db, total_applied_gain_db);

      // If src is ramping, advance that ramp by the amount of dest that was just mixed.
      if (ramping) {
        bookkeeping.gain.Advance(dest_offset - dest_offset_before_mix,
                                 dest_frames_per_dest_ref_clock_nsec);
      }
    }

    source_buffer->set_frames_consumed(source_frames_consumed);

    // Advance positions by the number of frames mixed.
    // Note that we have already advanced by initial_dest_advance.
    info.UpdateRunningPositionsBy(dest_offset - prev_dest_offset - initial_dest_advance,
                                  bookkeeping);
  }

  // If there was insufficient supply to meet our demand, we may not have mixed enough frames, but
  // we advance our destination frame count as if we did, because time rolls on. Same for source.
  info.AdvanceAllPositionsTo(cur_mix_job_.dest_start_frame + cur_mix_job_.buf_frames, bookkeeping);
  cur_mix_job_.accumulate = true;
}

std::optional<ReadableStream::Buffer> MixStage::NextSourceBuffer(Mixer& mixer,
                                                                 ReadableStream& stream,
                                                                 int64_t dest_frames) {
  auto& info = mixer.source_info();
  auto& bookkeeping = mixer.bookkeeping();

  // Request enough source_frames to produce dest_frames.
  Fixed source_frames = Mixer::Bookkeeping::DestLenToSourceLen(
                            dest_frames, bookkeeping.step_size, bookkeeping.rate_modulo(),
                            bookkeeping.denominator(), bookkeeping.source_pos_modulo) +
                        mixer.pos_filter_width();

  Fixed source_start = info.next_source_frame;

  // Advance source_start to our source's next available frame. This is needed because our
  // source's current position may be ahead of info.next_source_frame by up to pos_filter_width
  // frames. While we could keep track of this delta ourselves, it's easier to simply ask the
  // source for its current position.
  auto next_available = stream.NextAvailableFrame();
  if (next_available && *next_available > source_start) {
    const Fixed source_end = source_start + source_frames;
    source_start = *next_available;
    source_frames = source_end - source_start;
    if (source_frames <= Fixed(0)) {
      // This shouldn't happen: the source should not be ahead of info.next_source_frame by
      // more than pos_filter_width and our initial source_frames should > pos_filter_width.
      FX_LOGS(WARNING) << ffl::String::DecRational << "Unexpectedly small source request"
                       << " [" << info.next_source_frame << ", " << source_end << ")"
                       << " is entirely before next available frame (" << (*next_available) << ")";
      return std::nullopt;
    }
  }

  // Round up so we always request an integral number of frames.
  return stream.ReadLock(*cur_mix_job_.read_lock_ctx, source_start, source_frames.Ceiling());
}

// We compose the effects of clock reconciliation into our sample-rate-conversion step size, but
// only for streams that use neither our adjustable clock, nor the clock designated as driving our
// hardware-rate-adjustments. We apply this micro-SRC via an intermediate "slew away the error"
// rate-correction factor driven by a PID control. Why use a PID? Sources do not merely chase the
// other clock's rate -- they chase its position. Note that even if we don't adjust our rate, we
// still want a composed transformation for offsets.
//
// Calculate the composed dest-to-source transformation and update the mixer's bookkeeping for
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
  auto clock_generation_for_previous_mix = info.source_ref_clock_to_frac_source_frames_generation;
  auto snapshot = stream.ref_time_to_frac_presentation_frame();
  info.source_ref_clock_to_frac_source_frames = snapshot.timeline_function;
  info.source_ref_clock_to_frac_source_frames_generation = snapshot.generation;

  // If source rate is zero, the stream is not running. Set rates/transforms to zero and exit.
  if (info.source_ref_clock_to_frac_source_frames.subject_delta() == 0) {
    info.clock_mono_to_frac_source_frames = TimelineFunction(TimelineRate::Zero);
    info.dest_frames_to_frac_source_frames = TimelineFunction(TimelineRate::Zero);

    SetStepSize(info, bookkeeping, TimelineRate::Zero);
    return;
  }

  // Ensure the mappings from source-frame to monotonic-time is up-to-date.
  auto frac_source_frame_to_clock_mono =
      source_ref_to_clock_mono * info.source_ref_clock_to_frac_source_frames.Inverse();
  info.clock_mono_to_frac_source_frames = frac_source_frame_to_clock_mono.Inverse();

  if constexpr (kLogReconciledTimelineFunctions) {
    FX_LOGS(INFO) << clock::TimelineFunctionToString(info.clock_mono_to_frac_source_frames,
                                                     "mono-to-frac-source");
  }

  // Assert we can map between local monotonic-time and fractional source frames
  // (neither numerator nor denominator can be zero).
  FX_DCHECK(info.clock_mono_to_frac_source_frames.subject_delta() *
            info.clock_mono_to_frac_source_frames.reference_delta());

  // UpdateDestTrans
  //
  // Ensure the mappings from dest-frame to monotonic-time is up-to-date.
  // We should only be here if we have a valid mix job. This means a job which supplies a valid
  // transformation from reference time to destination frames (based on dest frame rate).
  //
  // If dest rate is zero, the destination is not running. Set rates/transforms to zero and exit.
  FX_DCHECK(cur_mix_job_.dest_ref_clock_to_frac_dest_frame.rate().reference_delta());
  if (cur_mix_job_.dest_ref_clock_to_frac_dest_frame.subject_delta() == 0) {
    info.dest_frames_to_frac_source_frames = TimelineFunction(TimelineRate::Zero);

    SetStepSize(info, bookkeeping, TimelineRate::Zero);
    return;
  }

  auto dest_frames_to_dest_ref =
      ReferenceClockToIntegralFrames(cur_mix_job_.dest_ref_clock_to_frac_dest_frame).Inverse();

  // Compose our transformation from local monotonic-time to dest frames.
  auto dest_frames_to_clock_mono = dest_ref_to_mono * dest_frames_to_dest_ref;

  // ComposeDestToSource
  //
  // Compose our transformation from destination frames to source fractional frames (with clocks).
  info.dest_frames_to_frac_source_frames =
      info.clock_mono_to_frac_source_frames * dest_frames_to_clock_mono;

  // ComputeFrameRateConversionRatio
  //
  // Calculate the TimelineRate for step_size. No clock effects are included because any "micro-SRC"
  // is applied separately as a subsequent correction factor.
  TimelineRate frac_source_frames_per_dest_frame = TimelineRate::Product(
      dest_frames_to_dest_ref.rate(), info.source_ref_clock_to_frac_source_frames.rate());

  if constexpr (kLogReconciledTimelineFunctions) {
    FX_LOGS(INFO) << clock::TimelineFunctionToString(dest_frames_to_clock_mono, "dest-to-mono");
    FX_LOGS(INFO) << clock::TimelineFunctionToString(info.dest_frames_to_frac_source_frames,
                                                     "dest-to-frac-src (with clocks)");
    FX_LOGS(INFO) << clock::TimelineRateToString(frac_source_frames_per_dest_frame,
                                                 "dest-to-frac-source rate (no clock effects)");
  }

  // Project dest pos "cur_mix_job_.dest_start_frame" into monotonic time as "mono_now_from_dest".
  auto dest_frame = cur_mix_job_.dest_start_frame;
  auto mono_now_from_dest = zx::time{dest_frames_to_clock_mono.Apply(dest_frame)};

  // Redefine the relationship between source and dest clocks, if source timeline has changed.
  // Perform a stream's initial mix without error measurement or clock rate-adjustment.
  if (info.source_ref_clock_to_frac_source_frames_generation != clock_generation_for_previous_mix) {
    if constexpr (kLogInitialPositionSync) {
      FX_LOGS(INFO) << "MixStage(" << this << "), stream(" << &stream
                    << "): " << (source_clock.is_device_clock() ? "Device" : "Client")
                    << (source_clock.is_adjustable() ? "Adjustable" : "Fixed") << "("
                    << &source_clock << ") ==> "
                    << (dest_clock.is_device_clock() ? "Device" : "Client")
                    << (dest_clock.is_adjustable() ? "Adjustable" : "Fixed") << "(" << &dest_clock
                    << ")" << AudioClock::SyncInfo(source_clock, dest_clock)
                    << ": timeline changed ************";
    }
    SyncSourcePositionFromClocks(source_clock, dest_clock, info, bookkeeping, dest_frame,
                                 mono_now_from_dest, true);
    SetStepSize(info, bookkeeping, frac_source_frames_per_dest_frame);
    return;
  }

  // In most cases, we advance source position using step_size. For a dest discontinuity of N
  // frames, we update next_dest_frame by N and update next_source_frame by N * step_size. However,
  // if a discontinuity exceeds kMaxErrorThresholdDuration, clocks have diverged to such an extent
  // that we view the discontinuity as unrecoverable: we use JamSync to reset the source position
  // based on the dest and source clocks.
  if (dest_frame != info.next_dest_frame) {
    auto dest_gap_duration = zx::nsec(dest_frames_to_clock_mono.rate().Scale(
        std::abs(dest_frame - info.next_dest_frame), TimelineRate::RoundingMode::Ceiling));
    if constexpr (kLogDestDiscontinuities) {
      static int dest_discontinuity_count = 0;
      if (dest_discontinuity_count % kPositionLogStride == 0) {
        FX_LOGS(WARNING) << "MixStage(" << this << "), stream(" << &stream
                         << "): " << (source_clock.is_device_clock() ? "Device" : "Client")
                         << (source_clock.is_adjustable() ? "Adjustable" : "Fixed") << "("
                         << &source_clock << ") ==> "
                         << (dest_clock.is_device_clock() ? "Device" : "Client")
                         << (dest_clock.is_adjustable() ? "Adjustable" : "Fixed") << "("
                         << &dest_clock << "); " << AudioClock::SyncInfo(source_clock, dest_clock);
        FX_LOGS(WARNING) << "Dest discontinuity: " << info.next_dest_frame - dest_frame
                         << " frames (" << dest_gap_duration.to_nsecs() << " nsec), will "
                         << (dest_gap_duration < kMaxErrorThresholdDuration ? "NOT" : "")
                         << " SyncSourcePositionFromClocks **********";
      }
      dest_discontinuity_count = (dest_discontinuity_count + 1) % kPositionLogStride;
    }

    // If dest position discontinuity exceeds threshold, reset positions and rate adjustments.
    if (dest_gap_duration > kMaxErrorThresholdDuration) {
      // Set new running positions, based on E2E clock (not just step_size).
      SyncSourcePositionFromClocks(source_clock, dest_clock, info, bookkeeping, dest_frame,
                                   mono_now_from_dest, false);
      SetStepSize(info, bookkeeping, frac_source_frames_per_dest_frame);
      return;
    }

    // For discontinuity not large enough for jam-sync, advance via step_size; sync normally.
    info.AdvanceAllPositionsTo(dest_frame, bookkeeping);
  }

  // We know long-running dest position (info.next_dest_frame) matches MixJob start (dest_frame).
  // Clock-synchronization can now use long-running source pos as a reliable input.

  // If no synchronization is needed between these clocks (same clock, device clocks in same domain,
  // or clones of CLOCK_MONOTONIC that have not yet been adjusted), then source-to-dest is precisely
  // the relationship between each side's frame rate.
  if (AudioClock::NoSynchronizationRequired(source_clock, dest_clock)) {
    SetStepSize(info, bookkeeping, frac_source_frames_per_dest_frame);
    return;
  }

  // TODO(fxbug.dev/63750): pass through a signal if we expect discontinuity (Play, Pause, packet
  // discontinuity bit); use it to log (or report to inspect) only unexpected discontinuities.
  // Add a test to validate that we log discontinuities only when we should.

  // Project the source position info.next_source_frame (including pos_modulo effects) into
  // system MONOTONIC time as mono_now_from_source. Record the difference (in ns) between
  // mono_now_source and mono_now_from_dest as source position error.
  auto mono_now_from_source = Mixer::SourceInfo::MonotonicNsecFromRunningSource(
      info, bookkeeping.source_pos_modulo, bookkeeping.denominator());

  // Having converted both to monotonic time, now get the delta -- this is source position error
  info.source_pos_error = mono_now_from_source - mono_now_from_dest;

  // If source position error is less than 1 fractional source frame, disregard it. This keeps
  // us from overreacting to precision-limit-related errors, translated to higher-res nanosecs.
  // Beyond 1 frac-frame though, we rate-adjust clocks using nanosecond precision.
  zx::duration max_source_pos_error_to_not_tune =
      zx::nsec(info.clock_mono_to_frac_source_frames.rate().Inverse().Scale(
          1, TimelineRate::RoundingMode::Ceiling));
  if (abs(info.source_pos_error.to_nsecs()) <= max_source_pos_error_to_not_tune.to_nsecs()) {
    info.source_pos_error = zx::nsec(0);
  }

  // If source error exceeds our threshold, allow a discontinuity, reset position and rates, exit.
  if (std::abs(info.source_pos_error.get()) > kMaxErrorThresholdDuration.get()) {
    Reporter::Singleton().MixerClockSkewDiscontinuity(info.source_pos_error);

    SyncSourcePositionFromClocks(source_clock, dest_clock, info, bookkeeping, dest_frame,
                                 mono_now_from_dest, false);
    SetStepSize(info, bookkeeping, frac_source_frames_per_dest_frame);
    return;
  }

  // Allow the clocks to self-synchronize to eliminate the position error. A non-zero return value
  // indicates that they cannot, and we should apply a rate-conversion factor in software.
  auto micro_src_ppm = AudioClock::SynchronizeClocks(source_clock, dest_clock, mono_now_from_dest,
                                                     info.source_pos_error);

  // Incorporate the adjustment into frac_source_frames_per_dest_frame (which determines step size).
  if (micro_src_ppm) {
    TimelineRate micro_src_factor{static_cast<uint64_t>(1'000'000 + micro_src_ppm), 1'000'000};

    // Product may exceed uint64/uint64: allow reduction. step_size can be approximate, as clocks
    // (not SRC/step_size) determine a stream absolute position -- SRC just chases the position.
    frac_source_frames_per_dest_frame =
        TimelineRate::Product(frac_source_frames_per_dest_frame, micro_src_factor,
                              false /* don't require exact precision */);
  }

  SetStepSize(info, bookkeeping, frac_source_frames_per_dest_frame);
}

// Establish specific running position values rather than adjusting clock rates, to bring source and
// dest positions together. We do this when setting the initial position relationship, when dest
// running position jumps unexpectedly, and when the error in source position exceeds our threshold.
void MixStage::SyncSourcePositionFromClocks(AudioClock& source_clock, AudioClock& dest_clock,
                                            Mixer::SourceInfo& info,
                                            Mixer::Bookkeeping& bookkeeping, int64_t dest_frame,
                                            zx::time mono_now_from_dest, bool timeline_changed) {
  auto prev_running_dest_frame = info.next_dest_frame;
  auto prev_running_source_frame = info.next_source_frame;
  double prev_source_pos_error = static_cast<double>(info.source_pos_error.get());

  info.ResetPositions(dest_frame, bookkeeping);

  // Reset accumulated rate adjustment feedback, in the relevant clocks.
  AudioClock::ResetRateAdjustments(source_clock, dest_clock, mono_now_from_dest);

  if constexpr (kLogJamSyncs) {
    if (timeline_changed && !kLogInitialPositionSync) {
      return;
    }

    std::stringstream common_stream, dest_stream, source_stream;
    common_stream << "; MixStage " << static_cast<void*>(this) << ", SourceInfo "
                  << static_cast<void*>(&info) << "; "
                  << AudioClock::SyncInfo(source_clock, dest_clock);
    dest_stream << "dest " << (dest_clock.is_client_clock() ? "Client" : "Device")
                << (dest_clock.is_adjustable() ? "Adjustable" : "Fixed") << "["
                << static_cast<void*>(&dest_clock) << "]: " << ffl::String::DecRational
                << info.next_dest_frame;
    source_stream << "; src " << (source_clock.is_client_clock() ? "Client" : "Device")
                  << (source_clock.is_adjustable() ? "Adjustable" : "Fixed") << "["
                  << static_cast<void*>(&source_clock) << "]: " << ffl::String::DecRational
                  << info.next_source_frame;

    std::stringstream complete_log_msg;
    if (timeline_changed) {
      complete_log_msg << "JamSync(pos timeline changed): " << dest_stream.str()
                       << source_stream.str() << common_stream.str();
      // Log these at lowest level, but reset the count so we always log the next jam-sync
      jam_sync_count_ = -1;
    } else if (prev_running_dest_frame != dest_frame) {
      complete_log_msg << "JamSync(dest discontinuity)  : " << dest_frame - prev_running_dest_frame
                       << " frames; " << dest_stream.str() << " (expect " << prev_running_dest_frame
                       << ")" << source_stream.str() << " (was " << ffl::String::DecRational
                       << prev_running_source_frame << ") at dest " << mono_now_from_dest.get()
                       << common_stream.str();
    } else {
      complete_log_msg << "JamSync(source discontinuity): "
                       << static_cast<float>(prev_source_pos_error / ZX_USEC(1)) << " us (limit "
                       << static_cast<float>(static_cast<float>(kMaxErrorThresholdDuration.get()) /
                                             ZX_USEC(1))
                       << " us) at dest " << mono_now_from_dest.get() << "; " << dest_stream.str()
                       << source_stream.str() << " (expect " << ffl::String::DecRational
                       << prev_running_source_frame << ")" << common_stream.str();
    }
    if (kJamSyncWarningInterval && (jam_sync_count_ % kJamSyncWarningInterval == 0)) {
      FX_LOGS(WARNING) << complete_log_msg.str() << " (1/" << kJamSyncWarningInterval << ")";
    } else if (kJamSyncInfoInterval && (jam_sync_count_ % kJamSyncInfoInterval == 0)) {
      FX_LOGS(INFO) << complete_log_msg.str() << " (1/" << kJamSyncInfoInterval << ")";
    } else if (kJamSyncTraceInterval && (jam_sync_count_ % kJamSyncTraceInterval == 0)) {
      FX_LOGS(TRACE) << complete_log_msg.str() << " (1/" << kJamSyncTraceInterval << ")";
    }
    ++jam_sync_count_;
  }
}

// From a TimelineRate, calculate the [step_size, denominator, rate_modulo] used by Mixer::Mix()
void MixStage::SetStepSize(Mixer::SourceInfo& info, Mixer::Bookkeeping& bookkeeping,
                           const TimelineRate& frac_source_frames_per_dest_frame) {
  bookkeeping.step_size = Fixed::FromRaw(frac_source_frames_per_dest_frame.Scale(1));

  // Now that we have a new step_size, generate new rate_modulo and denominator values to
  // account for step_size's limitations.
  auto new_rate_modulo =
      frac_source_frames_per_dest_frame.subject_delta() -
      (frac_source_frames_per_dest_frame.reference_delta() * bookkeeping.step_size.raw_value());
  auto new_denominator = frac_source_frames_per_dest_frame.reference_delta();

  info.next_source_frame = bookkeeping.SetRateModuloAndDenominator(new_rate_modulo, new_denominator,
                                                                   info.next_source_frame);
}

}  // namespace media::audio
