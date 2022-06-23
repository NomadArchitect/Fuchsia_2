// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_CODEC_CODECS_VAAPI_CODEC_ADAPTER_VAAPI_DECODER_H_
#define SRC_MEDIA_CODEC_CODECS_VAAPI_CODEC_ADAPTER_VAAPI_DECODER_H_

#include <fidl/fuchsia.sysmem/cpp/wire.h>
#include <fuchsia/sysmem/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async/cpp/task.h>
#include <lib/media/codec_impl/codec_adapter.h>
#include <lib/media/codec_impl/codec_buffer.h>
#include <lib/media/codec_impl/codec_diagnostics.h>
#include <lib/media/codec_impl/codec_input_item.h>
#include <lib/media/codec_impl/codec_packet.h>
#include <lib/media/codec_impl/fourcc.h>
#include <lib/trace/event.h>
#include <threads.h>

#include <condition_variable>
#include <memory>
#include <mutex>
#include <optional>
#include <queue>

#include <fbl/algorithm.h>
#include <va/va.h>

#include "buffer_pool.h"
#include "media/base/decoder_buffer.h"
#include "media/gpu/accelerated_video_decoder.h"
#include "src/lib/fxl/macros.h"
#include "src/lib/fxl/synchronization/thread_annotations.h"
#include "src/media/codec/codecs/vaapi/avcc_processor.h"
#include "src/media/lib/mpsc_queue/mpsc_queue.h"
#include "vaapi_utils.h"

class CodecAdapterVaApiDecoder;

// Used for friend declarations in CodecAdapterVaApiDecoder
namespace test {
class Vp9VaapiTestFixture;
}  // namespace test

// Interface used to manage output buffer, DPB surfaces and their relationship to each other. The
// goal of this class is to abstract away the implementation details on how linear and tiled
// surfaces are handled differently.
class SurfaceBufferManager {
 public:
  virtual ~SurfaceBufferManager() = default;

  // Adds a output CodecBuffer under the management of the class
  virtual void AddBuffer(const CodecBuffer* buffer) = 0;

  // Called when an output buffer that was shared with the client is no longer by that client and
  // now can be used again.
  virtual void RecycleBuffer(const CodecBuffer* buffer) = 0;

  // Deconfigures all output buffers under the manager's control
  virtual void DeconfigureBuffers() = 0;

  // Get a surface that will be used as a DPB for the codec. If no current surfaces are available
  // this function will block until either a DPB surfaces becomes available or Reset() is called
  virtual scoped_refptr<VASurface> GetDPBSurface() = 0;

  // This function returns an output CodecBuffer to be sent to the client for the given DPB surface
  virtual std::optional<std::pair<const CodecBuffer*, uint32_t>> ProcessOutputSurface(
      scoped_refptr<VASurface> dpb_surface) = 0;

  // Resets any underlying blocking data structures after a call to StopAllWaits(). This allows the
  // data structures to block again.
  virtual void Reset() = 0;

  // Stops all blocking calls, specially the potentially blocking call of GetDPBSurface() or
  // ProcessOutputSurface(). Will cause blocking calls to immediately return with default
  // constructed objects as their return values.
  virtual void StopAllWaits() = 0;

  // Increments the surface generation tracker to signal to subclasses that a resize event has
  // happened mid stream.
  void IncrementSurfaceGeneration(const gfx::Size& new_surface_size, size_t num_of_surfaces,
                                  uint32_t output_stride) {
    std::lock_guard<std::mutex> guard(surface_lock_);
    surface_generation_ += 1;
    surface_size_ = new_surface_size;

    // Signal to subclass that new surface generation has occurred. Called under lock
    OnSurfaceGenerationUpdatedLocked(num_of_surfaces, output_stride);
  }

 protected:
  explicit SurfaceBufferManager(std::mutex& codec_lock) : codec_lock_(codec_lock) {}

  // Event method subclass must implement. Called when the surface generation has been incremented
  // and a new surface size is available. This function is guaranteed to be called with the
  // surface_lock_ locked.
  virtual void OnSurfaceGenerationUpdatedLocked(size_t num_of_surfaces, uint32_t output_stride)
      FXL_REQUIRE(surface_lock_) = 0;

  // The lock is owned by the VAAPI decoder and hence the decoder class will always outlive this
  // class.
  std::mutex& codec_lock_;

  // Lock that must be used when modifying any surface data.
  std::mutex surface_lock_{};

  // Holds the current version of surface generation. If incremented DPB surfaces will have to be
  // destroyed and recreated with the new the new surface_size_ dimensions
  uint64_t surface_generation_ FXL_GUARDED_BY(surface_lock_) = {};
  gfx::Size surface_size_ FXL_GUARDED_BY(surface_lock_) = {};

  // The order of output_buffer_pool_ and in_use_by_client_ matters, so that
  // destruction of in_use_by_client_ happens first, because those destructing
  // will return buffers to output_buffer_pool_.
  BufferPool output_buffer_pool_{};
};

class CodecAdapterVaApiDecoder : public CodecAdapter {
 public:
  CodecAdapterVaApiDecoder(std::mutex& lock, CodecAdapterEvents* codec_adapter_events)
      : CodecAdapter(lock, codec_adapter_events),
        avcc_processor_(fit::bind_member<&CodecAdapterVaApiDecoder::DecodeAnnexBBuffer>(this),
                        codec_adapter_events) {
    ZX_DEBUG_ASSERT(events_);
  }

  ~CodecAdapterVaApiDecoder() override {
    input_processing_loop_.Shutdown();
    // Tear down first to make sure the accelerator doesn't reference other variables in this
    // class later.
    media_decoder_.reset();
  }

  void SetCodecDiagnostics(CodecDiagnostics* codec_diagnostics) override {
    codec_diagnostics_ = codec_diagnostics;
  }

  bool IsCoreCodecRequiringOutputConfigForFormatDetection() override { return false; }

  bool IsCoreCodecMappedBufferUseful(CodecPort port) override { return true; }

  bool IsCoreCodecHwBased(CodecPort port) override { return true; }

  void CoreCodecInit(const fuchsia::media::FormatDetails& initial_input_format_details) override;

  void CoreCodecAddBuffer(CodecPort port, const CodecBuffer* buffer) override {
    if (port != kOutputPort) {
      return;
    }

    staged_output_buffers_.push_back(buffer);
  }

  void CoreCodecConfigureBuffers(
      CodecPort port, const std::vector<std::unique_ptr<CodecPacket>>& packets) override {
    if (port != kOutputPort) {
      return;
    }

    std::vector<CodecPacket*> all_packets;
    for (auto& packet : packets) {
      all_packets.push_back(packet.get());
    }

    std::shuffle(all_packets.begin(), all_packets.end(), not_for_security_prng_);
    for (CodecPacket* packet : all_packets) {
      free_output_packets_.Push(packet);
    }
  }

  void CoreCodecStartStream() override;

  void CoreCodecQueueInputFormatDetails(
      const fuchsia::media::FormatDetails& per_stream_override_format_details) override {
    // TODO(turnage): Accept midstream and interstream input format changes.
    // For now these should always be 0, so assert to notice if anything
    // changes.
    ZX_ASSERT(per_stream_override_format_details.has_format_details_version_ordinal() &&
              per_stream_override_format_details.format_details_version_ordinal() ==
                  input_format_details_version_ordinal_);
    input_queue_.Push(CodecInputItem::FormatDetails(per_stream_override_format_details));
  }

  void CoreCodecQueueInputPacket(CodecPacket* packet) override {
    TRACE_INSTANT("codec_runner", "Media:PacketReceived", TRACE_SCOPE_THREAD);
    input_queue_.Push(CodecInputItem::Packet(packet));
  }

  void CoreCodecQueueInputEndOfStream() override {
    input_queue_.Push(CodecInputItem::EndOfStream());
  }

  void CoreCodecStopStream() override {
    input_queue_.StopAllWaits();
    free_output_packets_.StopAllWaits();

    // If we are waiting for a mid stream output buffer reconfiguration, stop.
    // CoreCodecMidStreamOutputBufferReConfigFinish() will not be called.
    {
      std::lock_guard<std::mutex> guard(lock_);
      is_stream_stopped_ = true;
    }
    surface_buffer_manager_cv_.notify_all();

    // It is possible a stream was started by no input packets were provided which means that the
    // surface buffer manager was never constructed.
    if (surface_buffer_manager_) {
      surface_buffer_manager_->StopAllWaits();
    }

    WaitForInputProcessingLoopToEnd();
    CleanUpAfterStream();

    auto queued_input_items = BlockingMpscQueue<CodecInputItem>::Extract(std::move(input_queue_));
    while (!queued_input_items.empty()) {
      CodecInputItem input_item = std::move(queued_input_items.front());
      queued_input_items.pop();
      if (input_item.is_packet()) {
        events_->onCoreCodecInputPacketDone(input_item.packet());
      }
    }

    TRACE_INSTANT("codec_runner", "Media:Stop", TRACE_SCOPE_THREAD);
  }

  void CoreCodecResetStreamAfterCurrentFrame() override;

  void CoreCodecRecycleOutputPacket(CodecPacket* packet) override {
    if (packet->is_new()) {
      // CoreCodecConfigureBuffers() took care of initially populating
      // free_output_packets_ (in shuffled order), so ignore new packets.
      ZX_DEBUG_ASSERT(!packet->buffer());
      packet->SetIsNew(false);
      return;
    }

    if (packet->buffer()) {
      ZX_ASSERT(surface_buffer_manager_);
      surface_buffer_manager_->RecycleBuffer(packet->buffer());
    }

    free_output_packets_.Push(packet);
  }

  void CoreCodecEnsureBuffersNotConfigured(CodecPort port) override {
    buffer_settings_[port] = std::nullopt;
    if (port != kOutputPort) {
      // We don't do anything with input buffers.
      return;
    }

    // The first time this function is called before CodecStartStream() which means that
    // surface_buffer_manager_ will not be configured yet. If this is the case then by default our
    // surface buffer manager is not configured and no action is needed
    if (surface_buffer_manager_) {
      surface_buffer_manager_->DeconfigureBuffers();
      surface_buffer_manager_->Reset();
    }

    // VMO handles for the old output buffers may still exist, but the SW
    // decoder doesn't know about those, and buffer_lifetime_ordinal will
    // prevent us calling output_buffer_pool_.FreeBuffer() for any of the old
    // buffers.  So forget about the old buffers here.
    staged_output_buffers_.clear();
    free_output_packets_.Reset();
  }

  void CoreCodecMidStreamOutputBufferReConfigPrepare() override {
    // Nothing to do here.
  }

  void CoreCodecMidStreamOutputBufferReConfigFinish() override;

  std::string CoreCodecGetName() override { return "VAAPI"; }

  std::unique_ptr<const fuchsia::media::StreamOutputConstraints> CoreCodecBuildNewOutputConstraints(
      uint64_t stream_lifetime_ordinal, uint64_t new_output_buffer_constraints_version_ordinal,
      bool buffer_constraints_action_required) override {
    auto config = std::make_unique<fuchsia::media::StreamOutputConstraints>();

    config->set_stream_lifetime_ordinal(stream_lifetime_ordinal);

    // For the moment, there will be only one StreamOutputConstraints, and it'll
    // need output buffers configured for it.
    ZX_DEBUG_ASSERT(buffer_constraints_action_required);
    config->set_buffer_constraints_action_required(buffer_constraints_action_required);
    auto* constraints = config->mutable_buffer_constraints();
    constraints->set_buffer_constraints_version_ordinal(
        new_output_buffer_constraints_version_ordinal);

    return config;
  }

  fuchsia::media::StreamOutputFormat CoreCodecGetOutputFormat(
      uint64_t stream_lifetime_ordinal,
      uint64_t new_output_format_details_version_ordinal) override {
    std::lock_guard<std::mutex> lock(lock_);
    fuchsia::media::StreamOutputFormat result;
    fuchsia::sysmem::ImageFormat_2 image_format;
    gfx::Size pic_size = media_decoder_->GetPicSize();
    gfx::Rect visible_rect = media_decoder_->GetVisibleRect();
    image_format.pixel_format.type = fuchsia::sysmem::PixelFormatType::NV12;

    bool is_output_tiled = IsOutputTiled();
    image_format.pixel_format.has_format_modifier = is_output_tiled;
    if (is_output_tiled) {
      image_format.pixel_format.format_modifier.value =
          fuchsia_sysmem::wire::kFormatModifierIntelI915YTiled;
    }

    image_format.coded_width = pic_size.width();
    image_format.coded_height = pic_size.height();
    image_format.bytes_per_row = GetOutputStride();
    image_format.display_width = visible_rect.width();
    image_format.display_height = visible_rect.height();
    image_format.layers = 1;
    image_format.color_space.type = fuchsia::sysmem::ColorSpaceType::REC709;
    image_format.has_pixel_aspect_ratio = false;

    fuchsia::media::FormatDetails format_details;

    format_details.set_mime_type("video/raw");

    fuchsia::media::VideoFormat video_format;
    video_format.set_uncompressed(GetUncompressedFormat(image_format));

    format_details.mutable_domain()->set_video(std::move(video_format));

    result.set_stream_lifetime_ordinal(stream_lifetime_ordinal);
    result.set_format_details(std::move(format_details));
    result.mutable_format_details()->set_format_details_version_ordinal(
        new_output_format_details_version_ordinal);
    return result;
  }

  fuchsia::sysmem::BufferCollectionConstraints CoreCodecGetBufferCollectionConstraints(
      CodecPort port, const fuchsia::media::StreamBufferConstraints& stream_buffer_constraints,
      const fuchsia::media::StreamBufferPartialSettings& partial_settings) override {
    if (port == kInputPort) {
      fuchsia::sysmem::BufferCollectionConstraints constraints;
      constraints.min_buffer_count_for_camping = 1;
      constraints.has_buffer_memory_constraints = true;
      constraints.buffer_memory_constraints.cpu_domain_supported = true;
      // Must be big enough to hold an entire NAL unit, since the H264Decoder doesn't support
      // split NAL units.
      constraints.buffer_memory_constraints.min_size_bytes = 8192 * 512;
      return constraints;
    } else if (port == kOutputPort) {
      fuchsia::sysmem::BufferCollectionConstraints constraints;
      constraints.min_buffer_count_for_camping =
          static_cast<uint32_t>(media_decoder_->GetRequiredNumOfPictures());
      constraints.has_buffer_memory_constraints = true;
      // TODO(fxbug.dev/94140): Add RAM domain support.
      constraints.buffer_memory_constraints.cpu_domain_supported = true;

      // Two image format constraints
      // 1) Linear format
      // 2) Y-Tiled format
      constraints.image_format_constraints_count = 2;

      // Linear Format
      auto& linear_constraints = constraints.image_format_constraints[0];
      linear_constraints.pixel_format.has_format_modifier = false;
      linear_constraints.bytes_per_row_divisor = 16;

      // Y-Tiled format
      auto& tiled_constraints = constraints.image_format_constraints[1];
      tiled_constraints.pixel_format.has_format_modifier = true;
      tiled_constraints.pixel_format.format_modifier.value =
          fuchsia::sysmem::FORMAT_MODIFIER_INTEL_I915_Y_TILED;
      tiled_constraints.bytes_per_row_divisor = 0;

      // Common Settings
      linear_constraints.pixel_format.type = tiled_constraints.pixel_format.type =
          fuchsia::sysmem::PixelFormatType::NV12;

      // TODO(fix)
      linear_constraints.color_spaces_count = tiled_constraints.color_spaces_count = 1;
      linear_constraints.color_space[0].type = tiled_constraints.color_space[0].type =
          fuchsia::sysmem::ColorSpaceType::REC709;

      // The non-"required_" fields indicate the decoder's ability to potentially
      // output frames at various dimensions as coded in the stream.  Aside from
      // the current stream being somewhere in these bounds, these have nothing to
      // do with the current stream in particular.
      linear_constraints.min_coded_width = tiled_constraints.min_coded_width = 16;
      linear_constraints.max_coded_width = tiled_constraints.max_coded_width = max_picture_width_;
      linear_constraints.min_coded_height = tiled_constraints.min_coded_height = 16;
      linear_constraints.max_coded_height = tiled_constraints.max_coded_height =
          max_picture_height_;

      // This intentionally isn't the height of a 4k frame.  See
      // max_coded_width_times_coded_height.  We intentionally constrain the max
      // dimension in width or height to the width of a 4k frame.  While the HW
      // might be able to go bigger than that as long as the other dimension is
      // smaller to compensate, we don't really need to enable any larger than
      // 4k's width in either dimension, so we don't.
      linear_constraints.min_bytes_per_row = tiled_constraints.min_bytes_per_row = 16;

      // no hard-coded max stride, at least for now
      linear_constraints.max_bytes_per_row = tiled_constraints.max_bytes_per_row = 0xFFFFFFFF;
      linear_constraints.max_coded_width_times_coded_height =
          tiled_constraints.max_coded_width_times_coded_height = 3840 * 2160;
      linear_constraints.layers = tiled_constraints.layers = 1;
      linear_constraints.coded_width_divisor = tiled_constraints.coded_width_divisor = 16;
      linear_constraints.coded_height_divisor = tiled_constraints.coded_height_divisor = 16;
      linear_constraints.start_offset_divisor = tiled_constraints.start_offset_divisor = 1;

      // Odd display dimensions are permitted, but these don't imply odd YV12
      // dimensions - those are constrainted by coded_width_divisor and
      // coded_height_divisor which are both 16.
      linear_constraints.display_width_divisor = tiled_constraints.display_width_divisor = 1;
      linear_constraints.display_height_divisor = tiled_constraints.display_height_divisor = 1;

      // The decoder is producing frames and the decoder has no choice but to
      // produce frames at their coded size.  The decoder wants to potentially be
      // able to support a stream with dynamic resolution, potentially including
      // dimensions both less than and greater than the dimensions that led to the
      // current need to allocate a BufferCollection.  For this reason, the
      // required_ fields are set to the exact current dimensions, and the
      // permitted (non-required_) fields is set to the full potential range that
      // the decoder could potentially output.  If an initiator wants to require a
      // larger range of dimensions that includes the required range indicated
      // here (via a-priori knowledge of the potential stream dimensions), an
      // initiator is free to do so.
      gfx::Size pic_size = media_decoder_->GetPicSize();
      linear_constraints.required_min_coded_width = tiled_constraints.required_min_coded_width =
          pic_size.width();
      linear_constraints.required_max_coded_width = tiled_constraints.required_max_coded_width =
          pic_size.width();
      linear_constraints.required_min_coded_height = tiled_constraints.required_min_coded_height =
          pic_size.height();
      linear_constraints.required_max_coded_height = tiled_constraints.required_max_coded_height =
          pic_size.height();

      return constraints;
    }

    return fuchsia::sysmem::BufferCollectionConstraints{};
  }

  void CoreCodecSetBufferCollectionInfo(
      CodecPort port,
      const fuchsia::sysmem::BufferCollectionInfo_2& buffer_collection_info) override {
    buffer_settings_[port] = buffer_collection_info.settings;
  }

  bool ProcessOutput(scoped_refptr<VASurface> surface, int bitstream_id);

  VAContextID context_id() { return context_id_->id(); }

  scoped_refptr<VASurface> GetVASurface();

  // Intel Y-Tiling alignment
  static constexpr uint32_t kTileWidthAlignment = 128u;
  static constexpr uint32_t kTileHeightAlignment = 32u;

 private:
  friend class VaApiOutput;
  friend class test::Vp9VaapiTestFixture;

  // Used from trace events
  enum DecoderState { kIdle, kDecoding, kError };

  static const char* DecoderStateName(DecoderState state);

  template <class... Args>
  void SetCodecFailure(const char* format, Args&&... args);

  void LaunchInputProcessingLoop() {
    zx_status_t post_result =
        async::PostTask(input_processing_loop_.dispatcher(), [this] { ProcessInputLoop(); });
    ZX_ASSERT_MSG(post_result == ZX_OK,
                  "async::PostTask() failed to post input processing loop - result: %d\n",
                  post_result);
  }

  void WaitForInputProcessingLoopToEnd() {
    ZX_DEBUG_ASSERT(thrd_current() != input_processing_thread_);

    std::condition_variable stream_stopped_condition;
    bool stream_stopped = false;
    zx_status_t post_result = async::PostTask(input_processing_loop_.dispatcher(),
                                              [this, &stream_stopped, &stream_stopped_condition] {
                                                {
                                                  std::lock_guard<std::mutex> lock(lock_);
                                                  stream_stopped = true;
                                                  // Under lock since
                                                  // WaitForInputProcessingLoopToEnd()
                                                  // may otherwise return too soon deleting
                                                  // stream_stopped_condition too soon.
                                                  stream_stopped_condition.notify_all();
                                                }
                                              });
    ZX_ASSERT_MSG(post_result == ZX_OK,
                  "async::PostTask() failed to post input processing loop - result: %d\n",
                  post_result);

    std::unique_lock<std::mutex> lock(lock_);
    stream_stopped_condition.wait(lock, [&stream_stopped] { return stream_stopped; });
  }

  // We don't give the codec any buffers in its output pool until
  // configuration is finished or a stream starts. Until finishing
  // configuration we stage all the buffers. Here we load all the staged
  // buffers so the codec can make output.
  void LoadStagedOutputBuffers() {
    ZX_ASSERT(surface_buffer_manager_);
    std::vector<const CodecBuffer*> to_add = std::move(staged_output_buffers_);
    for (auto buffer : to_add) {
      surface_buffer_manager_->AddBuffer(buffer);
    }
  }

  bool IsOutputTiled() const {
    ZX_ASSERT(buffer_settings_[kOutputPort]);
    ZX_ASSERT(buffer_settings_[kOutputPort]->has_image_format_constraints);

    auto& format_constraints = buffer_settings_[kOutputPort]->image_format_constraints;

    return (format_constraints.pixel_format.has_format_modifier) &&
           (format_constraints.pixel_format.format_modifier.value !=
            fuchsia_sysmem::wire::kFormatModifierLinear);
  }

  // Processes input in a loop. Should only execute on input_processing_thread_.
  // Loops for the lifetime of a stream.
  void ProcessInputLoop();

  // Releases any resources from the just-ended stream.
  void CleanUpAfterStream();

  void DecodeAnnexBBuffer(media::DecoderBuffer buffer);

  uint32_t GetOutputStride() {
    auto pic_size = media_decoder_->GetPicSize();

    uint32_t alignment;
    if (IsOutputTiled()) {
      alignment = kTileWidthAlignment;
    } else {
      // bytes_per_row_divisor must be a multiple of the size from in the output constraints.
      auto& bytes_per_row_divisor =
          buffer_settings_[kOutputPort]->image_format_constraints.bytes_per_row_divisor;
      ZX_ASSERT(bytes_per_row_divisor >= 16);
      alignment = bytes_per_row_divisor;
    }

    uint64_t stride = fbl::round_up(static_cast<uint64_t>(pic_size.width()), alignment);
    auto checked_stride = safemath::MakeCheckedNum(stride).Cast<uint32_t>();

    if (!checked_stride.IsValid()) {
      FX_LOGS(FATAL) << "Stride could not be represented as a 32 bit integer";
    }

    return checked_stride.ValueOrDie();
  }

  fuchsia::media::VideoUncompressedFormat GetUncompressedFormat(
      const fuchsia::sysmem::ImageFormat_2& image_format) {
    ZX_DEBUG_ASSERT(image_format.pixel_format.type == fuchsia::sysmem::PixelFormatType::NV12);

    fuchsia::media::VideoUncompressedFormat video_uncompressed;

    // Common Settings
    video_uncompressed.image_format = image_format;
    video_uncompressed.fourcc = make_fourcc('N', 'V', '1', '2');
    video_uncompressed.primary_width_pixels = image_format.coded_width;
    video_uncompressed.primary_height_pixels = image_format.coded_height;
    video_uncompressed.planar = true;
    video_uncompressed.primary_line_stride_bytes = image_format.bytes_per_row;
    video_uncompressed.secondary_line_stride_bytes = image_format.bytes_per_row;
    video_uncompressed.primary_start_offset = 0;
    video_uncompressed.primary_pixel_stride = 1;
    video_uncompressed.secondary_pixel_stride = 2;
    video_uncompressed.has_pixel_aspect_ratio = image_format.has_pixel_aspect_ratio;
    video_uncompressed.pixel_aspect_ratio_height = image_format.pixel_aspect_ratio_height;
    video_uncompressed.pixel_aspect_ratio_width = image_format.pixel_aspect_ratio_width;
    video_uncompressed.primary_display_width_pixels = image_format.display_width;
    video_uncompressed.primary_display_height_pixels = image_format.display_height;

    video_uncompressed.secondary_width_pixels = image_format.coded_width / 2;
    video_uncompressed.secondary_height_pixels = image_format.coded_height / 2;

    // Tile dependant settings
    if (IsOutputTiled()) {
      video_uncompressed.swizzled = true;
      video_uncompressed.secondary_start_offset =
          image_format.bytes_per_row *
          fbl::round_up(image_format.coded_height, kTileHeightAlignment);
      video_uncompressed.tertiary_start_offset = video_uncompressed.secondary_start_offset + 1;

    } else {
      video_uncompressed.swizzled = false;
      video_uncompressed.secondary_start_offset =
          image_format.bytes_per_row * image_format.coded_height;
      video_uncompressed.tertiary_start_offset = video_uncompressed.secondary_start_offset + 1;
    }

    return video_uncompressed;
  }

  // Allow up to 240 frames (8 seconds @ 30 fps) between keyframes.
  static constexpr uint32_t kMaxDecoderFailures = 240u;

  BlockingMpscQueue<CodecInputItem> input_queue_{};
  BlockingMpscQueue<CodecPacket*> free_output_packets_{};

  std::optional<ScopedConfigID> config_;

  // DPB surfaces.
  std::mutex surfaces_lock_;

  // The order of output_buffer_pool_ and in_use_by_client_ matters, so that
  // destruction of in_use_by_client_ happens first, because those destructing
  // will return buffers to output_buffer_pool_.
  std::unique_ptr<SurfaceBufferManager> surface_buffer_manager_;
  std::condition_variable surface_buffer_manager_cv_;
  bool mid_stream_output_buffer_reconfig_finish_ FXL_GUARDED_BY(lock_) = false;
  bool is_stream_stopped_ /* FXL_GUARDED_BY(lock_) */ = false;

  // Buffers the client has added but that we cannot use until configuration is
  // complete.
  std::vector<const CodecBuffer*> staged_output_buffers_;

  uint64_t input_format_details_version_ordinal_;

  AvccProcessor avcc_processor_;

  std::optional<fuchsia::sysmem::SingleBufferSettings> buffer_settings_[kPortCount];

  // Since CoreCodecInit() is called after SetDriverDiagnostics() we need to save a pointer to the
  // codec diagnostics object so that we can create the codec diagnotcis when we construct the
  // codec.
  CodecDiagnostics* codec_diagnostics_{nullptr};
  std::optional<ComponentCodecDiagnostics> codec_instance_diagnostics_;

  std::optional<ScopedContextID> context_id_;

  // Will be accessed from the input processing thread if that's active, or the main thread
  // otherwise.
  std::unique_ptr<media::AcceleratedVideoDecoder> media_decoder_;
  bool is_h264_{false};  // TODO(stefanbossbaly): Remove in favor abstraction in VAAPI layer
  uint32_t decoder_failures_{0};  // The amount of failures the decoder has encountered
  DiagnosticStateWrapper<DecoderState> state_{
      []() {}, DecoderState::kIdle, &DecoderStateName};  // Used for trace events to show when we
                                                         // are waiting on the iGPU for data

  // These are set in CoreCodecInit() by querying the underlying hardware. If the hardware query
  // returns no results the current value is not overwritten.
  uint32_t max_picture_height_{3840};
  uint32_t max_picture_width_{3840};

  std::deque<std::pair<int32_t, uint64_t>> stream_to_pts_map_;
  int32_t next_stream_id_{};

  async::Loop input_processing_loop_{&kAsyncLoopConfigNoAttachToCurrentThread};
  thrd_t input_processing_thread_;
};

#endif  // SRC_MEDIA_CODEC_CODECS_VAAPI_CODEC_ADAPTER_VAAPI_DECODER_H_
