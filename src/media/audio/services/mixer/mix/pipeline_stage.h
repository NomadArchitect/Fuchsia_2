// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_AUDIO_SERVICES_MIXER_MIX_PIPELINE_STAGE_H_
#define SRC_MEDIA_AUDIO_SERVICES_MIXER_MIX_PIPELINE_STAGE_H_

#include <lib/syslog/cpp/macros.h>
#include <lib/zx/time.h>

#include <atomic>
#include <optional>
#include <string>
#include <string_view>
#include <unordered_set>

#include <ffl/string.h>

#include "src/media/audio/lib/format2/fixed.h"
#include "src/media/audio/lib/format2/format.h"
#include "src/media/audio/services/mixer/common/basic_types.h"
#include "src/media/audio/services/mixer/mix/mix_job_context.h"
#include "src/media/audio/services/mixer/mix/packet_view.h"
#include "src/media/audio/services/mixer/mix/ptr_decls.h"
#include "src/media/audio/services/mixer/mix/thread.h"

namespace media_audio {

// A stage in a pipeline tree.
//
// Each `PipelineStage` consumes zero or more source streams and produces at most one destination
// stream. This abstract class provides functionality common to all pipeline stages.
class PipelineStage {
 public:
  class Packet : public PacketView {
   public:
    ~Packet() {
      if (destructor_) {
        destructor_(frames_consumed_);
      }
    }

    Packet(Packet&& rhs) = default;
    Packet& operator=(Packet&& rhs) = default;

    Packet(const Packet& rhs) = delete;
    Packet& operator=(const Packet& rhs) = delete;

    // Call this to indicate that packet frames of `[start(), start() + frames_consumed)` have been
    // consumed. If this is not set, by default, we assume that the entire packet is consumed.
    void set_frames_consumed(int64_t frames_consumed) {
      FX_CHECK(frames_consumed <= length())
          << ffl::String::DecRational << frames_consumed << " > " << length();
      frames_consumed_ = frames_consumed;
    }

   private:
    friend class PipelineStage;
    using DestructorType = fit::callback<void(int64_t frames_consumed)>;

    Packet(Args args, bool is_cached, DestructorType destructor)
        : PacketView(args),
          destructor_(std::move(destructor)),
          frames_consumed_(length()),
          is_cached_(is_cached) {}

    DestructorType destructor_;
    int64_t frames_consumed_;
    bool is_cached_;
  };

  virtual ~PipelineStage() = default;

  // Adds a source stream.
  //
  // Required: caller must verify that `source` produces a stream with a compatible format.
  virtual void AddSource(PipelineStagePtr source, std::unordered_set<GainControlId> gain_ids)
      TA_REQ(thread()->checker()) = 0;

  // Removes a source stream.
  //
  // Required: caller must verify that `source` is currently a source for this stage.
  virtual void RemoveSource(PipelineStagePtr source) TA_REQ(thread()->checker()) = 0;

  // Advances the destination stream by releasing any frames before the given `frame`. This is a
  // declaration that the caller will not attempt to `Read` any frame before the given `frame`. If
  // the stage has allocated packets for frames before `frame`, it can free those packets now. After
  // the destination stream is advanced, the source streams are advanced, recursively.
  //
  // This must *not* be called while the stage is _locked_, i.e., until an acquired packet by a
  // `Read` call is destroyed.
  void Advance(MixJobContext& ctx, Fixed frame);

  // Reads the destination stream of this stage, and returns the acquired packet. The parameters
  // `start_frame` and `frame_count` represent a range of frames on the destination stream's frame
  // timeline.
  //
  // ## Returned Packet
  //
  // Returns `std::nullopt` if no data is available for the requested frame range. Otherwise,
  // returns a packet representing all or part of the requested range. If the start frame on the
  // returned packet is greater than `start_frame`, then the stream has no data for those initial
  // frames, which may be treated as silence. Conversely, if the end frame of the returned packet is
  // less than `start_frame + frame_count`, this indicates the full frame range is not available on
  // a single contiguous packet. Clients should call `Read` again, with `start_frame` set to the end
  // of the previous packet, to see if the stream has more frames.
  //
  // The returned packet contains an integral number of frames satisfying the following conditions:
  //
  // * `packet.start() > start_frame - Fixed(1)`
  //
  // * `packet.end() <= start_frame + Fixed(frame_count)`
  //
  // * `packet.length() <= frame_count`
  //
  // The start frame of the returned packet is the position of the left edge of the first frame in
  // the packet. For example, given `Read(Fixed(10), 5)`, if the stream's frames happen to be
  // aligned on positions 9.1, 10.1, 11.1, etc., then `Read` will return a packet with the start
  // frame of 9.1, and the length of 5.
  //
  // The stage will remain _locked_ until the returned packet is destroyed.
  //
  // ## The Passage of Time
  //
  // Each stage maintains a current frame position, which always moves forward. The position is
  // explicitly advanced to a destination `frame` via `Advance(frame)` call. Similarly, a `Read`
  // call advances the position as follows:
  //
  // * If `std::nullopt` is returned, the position is advanced to `start_frame + frame_count`.
  //
  // * Otherwise, the position is advanced to `packet.start() + packet.frames_consumed_` when the
  //   returned packet is destroyed.
  //
  // Put differently, time advances when `Read` is called, when a packet is consumed, and on
  // explicit calls to `Advance`. Time does not go backwards, hence, each call to `Read` must have
  // `start_frame` that is not lesser than the last advanced frame.
  [[nodiscard]] std::optional<Packet> Read(MixJobContext& ctx, Fixed start_frame,
                                           int64_t frame_count);

  // Returns corresponding frame for a given `presentation_time`.
  //
  // Required: caller must verify that `presentation_time_to_frac_frame` is valid.
  [[nodiscard]] Fixed FrameFromPresentationTime(zx::time presentation_time) const {
    FX_CHECK(presentation_time_to_frac_frame_.has_value());
    return Fixed::FromRaw(presentation_time_to_frac_frame_->Apply(presentation_time.get()));
  }

  // Returns corresponding presentation time for a given `frame`.
  //
  // Required: caller must verify that `presentation_time_to_frac_frame` is valid.
  [[nodiscard]] zx::time PresentationTimeFromFrame(Fixed frame) const {
    FX_CHECK(presentation_time_to_frac_frame_.has_value());
    return zx::time(presentation_time_to_frac_frame_->ApplyInverse(frame.raw_value()));
  }

  // Returns the stage's name. This is used for diagnostics only.
  // The name may not be a unique identifier.
  [[nodiscard]] std::string_view name() const { return name_; }

  // Returns the stage's format.
  [[nodiscard]] const Format& format() const { return format_; }

  // Returns the stage's next readable frame.
  [[nodiscard]] std::optional<Fixed> next_readable_frame() { return next_readable_frame_; }

  // Returns the thread which currently controls this stage.
  // It is safe to call this method on any thread, but if not called from thread(),
  // the returned value may change concurrently.
  [[nodiscard]] ThreadPtr thread() const { return std::atomic_load(&thread_); }

  // Returns the koid of the clock used by the stage's destination stream.
  // The source streams may use different clocks.
  [[nodiscard]] zx_koid_t reference_clock_koid() const { return reference_clock_koid_; }

  // Returns a function that translates from presentation time to frame time, where frame time is
  // represented by a `Fixed::raw_value()` while presentation time is represented by a `zx::time`.
  [[nodiscard]] std::optional<TimelineFunction> presentation_time_to_frac_frame() const {
    return presentation_time_to_frac_frame_;
  }

  // Sets the stage's thread.
  void set_thread(ThreadPtr thread) { std::atomic_store(&thread_, std::move(thread)); }

  // TODO(fxbug.dev/87651): Add functionality to set presentation delay.

 protected:
  PipelineStage(std::string_view name, Format format, zx_koid_t reference_clock_koid)
      : name_(name),
        format_(format),
        reference_clock_koid_(reference_clock_koid),
        advance_trace_name_(name_ + std::string("::Advance")),
        read_trace_name_(name_ + std::string("::Read")) {}

  PipelineStage(const PipelineStage&) = delete;
  PipelineStage& operator=(const PipelineStage&) = delete;

  PipelineStage(PipelineStage&&) = delete;
  PipelineStage& operator=(PipelineStage&&) = delete;

  // Implements `Advance` by an internal `AdvanceSelf` function, which advances this stage, the
  // `AdvanceSelfImpl` function, which is the stage-specific implementation of `AdvanceSelf`, and
  // the `AdvanceSourcesImpl` function, which advances all connected source streams accordingly.
  virtual void AdvanceSelfImpl(Fixed frame) = 0;
  virtual void AdvanceSourcesImpl(MixJobContext& ctx, Fixed frame) = 0;

  // Implements stage-specific `Read`.
  virtual std::optional<Packet> ReadImpl(MixJobContext& ctx, Fixed start_frame,
                                         int64_t frame_count) = 0;

  // `ReadImpl` should use this to create a cached packet. If the packet is not fully consumed after
  // one `Read`, the next `Read` call will return the same packet without asking `ReadImpl` to
  // recreate the same data. `PipelineStage` will hold onto this packet until the packet is fully
  // consumed or the stream position is advanced beyond the end of the packet.
  //
  // This is useful for pipeline stages that compute buffers dynamically. Examples include mixers
  // and effects.
  //
  // Required:
  //
  // * The `start_frame` must obey the packet constraints described by `Read`, however the
  //   `frame_count` can be arbitrarily large. This is useful for pipeline stages that generate data
  //   in fixed-sized blocks, as they may cache the entire block for future `Read` calls.
  //
  // * The `payload` must remain valid until the packet is fully consumed, i.e., until an the stage
  //   is advanced past the end of the packet.
  [[nodiscard]] Packet MakeCachedPacket(Fixed start_frame, int64_t frame_count, void* payload);

  // `ReadImpl` should use this to create an uncached packet. If the packet is not fully consumed
  // after one `Read`, the next `Read` call will ask `ReadImpl` to recreate the packet.
  //
  // This is useful for pipeline stages that don't need caching or that want precise control over
  // packet lifetimes. Examples include ring buffers and packet queues.
  //
  // Required:
  //
  // * The `start_frame` and the `frame_count` must obey the packet constraints described by `Read`.
  //
  // * The `payload` must remain valid until the packet is destroyed.
  [[nodiscard]] Packet MakeUncachedPacket(Fixed start_frame, int64_t frame_count, void* payload);

  // `ReadImpl` should use this when forwarding a `Packet` from an upstream source. This may be used
  // by no-op pipeline stages. It is necessary to call `ForwardPacket`, rather than simply returning
  // a packet from an upstream source, so that `AdvanceSelf` is called when the packet is destroyed.
  //
  // If `start_frame` is specified, the start frame of the returned packet is set to the given
  // value, while the length of the packet is unchanged. This is useful when doing SampleAndHold on
  // a source stream. For example:
  //
  //   ```
  //   auto packet = source->Read(frame, frame_count);
  //   auto start_frame = packet->start().Ceiling();
  //   return ForwardPacketWithModifiedStart(std::move(packet), start_frame);
  //   ```
  //
  // If `start_frame` is not specified, the packet is forwarded unchanged.
  [[nodiscard]] std::optional<Packet> ForwardPacket(
      std::optional<Packet>&& packet, std::optional<Fixed> start_frame = std::nullopt);

 private:
  // Advances this stage, and returns whether it's needed to advance sources or not.
  bool AdvanceSelf(Fixed frame);

  // Returns cached packet intersection at `start_frame` and `frame_count`.
  [[nodiscard]] std::optional<Packet> ReadFromCachedPacket(Fixed start_frame, int64_t frame_count);

  const std::string name_;
  const Format format_;
  const zx_koid_t reference_clock_koid_;

  const std::string advance_trace_name_;
  const std::string read_trace_name_;

  // Cached packet from the last call to `ReadImpl`. It remains valid until `next_dest_frame_`
  // reaches the end of the packet.
  std::optional<Packet> cached_packet_ = std::nullopt;

  // Next readable frame.
  std::optional<Fixed> next_readable_frame_ = std::nullopt;

  // Denotes whether the stage stream is currently _locked_ or not.
  bool is_locked_ = false;

  // This is accessed with atomic instructions (std::atomic_load and std::atomic_store) so that any
  // thread can call thread()->checker(). This can't be a std::atomic<ThreadPtr> until C++20.
  ThreadPtr thread_;

  // Current translation from frame numbers to presentation timestamps.
  // This is nullopt iff the stage is stopped. Otherwise the stage is started.
  std::optional<TimelineFunction> presentation_time_to_frac_frame_;
};

}  // namespace media_audio

#endif  // SRC_MEDIA_AUDIO_SERVICES_MIXER_MIX_PIPELINE_STAGE_H_
