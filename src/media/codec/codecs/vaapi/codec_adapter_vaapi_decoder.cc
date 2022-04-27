// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "codec_adapter_vaapi_decoder.h"

#include <zircon/status.h>

#include <condition_variable>
#include <mutex>

#include <va/va_drmcommon.h>

#include "h264_accelerator.h"
#include "lib/async/cpp/task.h"
#include "media/gpu/h264_decoder.h"
#include "media/gpu/vp9_decoder.h"
#include "vp9_accelerator.h"

#define LOG(x, ...) fprintf(stderr, __VA_ARGS__)

#define DRM_FORMAT_MOD_LINEAR 0
#define DRM_FORMAT_NV12 'NV12'

void CodecAdapterVaApiDecoder::CoreCodecInit(
    const fuchsia::media::FormatDetails& initial_input_format_details) {
  if (!initial_input_format_details.has_format_details_version_ordinal()) {
    events_->onCoreCodecFailCodec(
        "CoreCodecInit(): Initial input format details missing version "
        "ordinal.");
    return;
  }
  // Will always be 0 for now.
  input_format_details_version_ordinal_ =
      initial_input_format_details.format_details_version_ordinal();

  const std::string& mime_type = initial_input_format_details.mime_type();
  if (mime_type == "video/h264-multi" || mime_type == "video/h264") {
    media_decoder_ = std::make_unique<media::H264Decoder>(std::make_unique<H264Accelerator>(this),
                                                          media::H264PROFILE_HIGH);
    is_h264_ = true;
  } else if (mime_type == "video/vp9") {
    media_decoder_ = std::make_unique<media::VP9Decoder>(std::make_unique<VP9Accelerator>(this),
                                                         media::VP9PROFILE_PROFILE0);
  } else {
    events_->onCoreCodecFailCodec("CodecCodecInit(): Unknown mime_type %s\n", mime_type.c_str());
    return;
  }
  zx_status_t result =
      input_processing_loop_.StartThread("input_processing_thread_", &input_processing_thread_);
  if (result != ZX_OK) {
    events_->onCoreCodecFailCodec(
        "CodecCodecInit(): Failed to start input processing thread with "
        "zx_status_t: %d",
        result);
    return;
  }
}

void CodecAdapterVaApiDecoder::CoreCodecResetStreamAfterCurrentFrame() {
  // Before we reset the decoder we must ensure that ProcessInputLoop() has exited and has no
  // outstanding tasks
  WaitForInputProcessingLoopToEnd();

  media_decoder_.reset();

  if (is_h264_) {
    media_decoder_ = std::make_unique<media::H264Decoder>(std::make_unique<H264Accelerator>(this),
                                                          media::H264PROFILE_HIGH);
  } else {
    media_decoder_ = std::make_unique<media::VP9Decoder>(std::make_unique<VP9Accelerator>(this),
                                                         media::VP9PROFILE_PROFILE0);
  }

  CoreCodecStartStream();
}

void CodecAdapterVaApiDecoder::DecodeAnnexBBuffer(std::vector<uint8_t> data) {
  media::DecoderBuffer buffer(std::move(data), nullptr, 0, {});

  media_decoder_->SetStream(next_stream_id_++, buffer);

  while (true) {
    auto result = media_decoder_->Decode();

    if (result == media::AcceleratedVideoDecoder::kConfigChange) {
      events_->onCoreCodecMidStreamOutputConstraintsChange(true);
      gfx::Size pic_size = media_decoder_->GetPicSize();
      VAContextID context_id;
      VAStatus va_res = vaCreateContext(VADisplayWrapper::GetSingleton()->display(), config_->id(),
                                        pic_size.width(), pic_size.height(), VA_PROGRESSIVE,
                                        nullptr, 0, &context_id);
      if (va_res != VA_STATUS_SUCCESS) {
        events_->onCoreCodecFailCodec("vaCreateContext failed: %s", vaErrorStr(va_res));
        return;
      }
      context_id_.emplace(context_id);
      std::vector<VASurfaceID> va_surfaces;
      va_surfaces.resize(media_decoder_->GetRequiredNumOfPictures());

      std::lock_guard lock(surfaces_lock_);
      // Increment surface generation so all existing surfaces will be freed
      // when they're released instead of being returned to the pool.
      surface_generation_++;
      surfaces_.clear();
      surface_size_ = pic_size;

      va_res = vaCreateSurfaces(VADisplayWrapper::GetSingleton()->display(), VA_RT_FORMAT_YUV420,
                                pic_size.width(), pic_size.height(), va_surfaces.data(),
                                static_cast<uint32_t>(va_surfaces.size()), nullptr, 0);
      if (va_res != VA_STATUS_SUCCESS) {
        events_->onCoreCodecFailCodec("vaCreateSurfaces failed: %s", vaErrorStr(va_res));
        return;
      }

      for (VASurfaceID id : va_surfaces) {
        surfaces_.emplace_back(id);
      }
      continue;
    } else if (result == media::AcceleratedVideoDecoder::kRanOutOfStreamData) {
      // Reset decoder failures on successful decode
      decoder_failures_ = 0;
      break;
    } else {
      decoder_failures_ += 1;
      if (decoder_failures_ >= kMaxDecoderFailures) {
        events_->onCoreCodecFailCodec(
            "Decoder exceeded the number of allowed failures. media_decoder::Decode result: "
            "%d",
            result);
      } else {
        // We allow the decoder to fail a set amount of times, reset the decoder after the current
        // frame. We need to stop the input_queue_ from processing any further items before the
        // stream reset. The stream control thread is responsible starting the stream once is has
        // been successfully reset.
        input_queue_.StopAllWaits();
        events_->onCoreCodecResetStreamAfterCurrentFrame();
      }

      return;
    }
  }
}

void CodecAdapterVaApiDecoder::ProcessInputLoop() {
  std::optional<CodecInputItem> maybe_input_item;
  while ((maybe_input_item = input_queue_.WaitForElement())) {
    CodecInputItem input_item = std::move(maybe_input_item.value());
    if (input_item.is_format_details()) {
      const std::string& mime_type = input_item.format_details().mime_type();
      if (!config_) {
        VAConfigAttrib attribs[1];
        attribs[0].type = VAConfigAttribRTFormat;
        attribs[0].value = VA_RT_FORMAT_YUV420;
        VAConfigID config_id;
        VAStatus va_status;

        if ((!is_h264_ && (mime_type == "video/h264-multi" || mime_type == "video/h264")) ||
            (is_h264_ && mime_type == "video/vp9")) {
          events_->onCoreCodecFailCodec(
              "CodecCodecInit(): Can not switch codec type after setting it in CoreCodecInit(). "
              "Attempting to switch it to %s\n",
              mime_type.c_str());
          return;
        }

        if (mime_type == "video/h264-multi" || mime_type == "video/h264") {
          va_status = vaCreateConfig(VADisplayWrapper::GetSingleton()->display(), VAProfileH264High,
                                     VAEntrypointVLD, attribs, std::size(attribs), &config_id);
        } else if (mime_type == "video/vp9") {
          va_status =
              vaCreateConfig(VADisplayWrapper::GetSingleton()->display(), VAProfileVP9Profile0,
                             VAEntrypointVLD, attribs, std::size(attribs), &config_id);
        } else {
          events_->onCoreCodecFailCodec("CodecCodecInit(): Unknown mime_type %s\n",
                                        mime_type.c_str());
          return;
        }

        if (va_status != VA_STATUS_SUCCESS) {
          events_->onCoreCodecFailCodec("Failed to create config.");
          return;
        }
        config_.emplace(config_id);
      }

      if (mime_type == "video/h264-multi" || mime_type == "video/h264") {
        avcc_processor_.ProcessOobBytes(input_item.format_details());
      }
    } else if (input_item.is_end_of_stream()) {
      // TODO(stefanbossbaly): Encapsulate in abstraction
      if (is_h264_) {
        constexpr uint8_t kEndOfStreamNalUnitType = 11;
        // Force frames to be processed.
        std::vector<uint8_t> end_of_stream_delimiter{0, 0, 1, kEndOfStreamNalUnitType};

        media::DecoderBuffer buffer(std::move(end_of_stream_delimiter), nullptr, 0, {});
        media_decoder_->SetStream(next_stream_id_++, buffer);
        auto result = media_decoder_->Decode();
        if (result != media::AcceleratedVideoDecoder::kRanOutOfStreamData) {
          events_->onCoreCodecFailCodec(
              "Unexpected media_decoder::Decode result for end of stream: %d", result);
          return;
        }
      }

      bool res = media_decoder_->Flush();
      if (!res) {
        FX_LOGS(WARNING) << "media decoder flush failed";
      }
      events_->onCoreCodecOutputEndOfStream(/*error_detected_before=*/!res);
    } else if (input_item.is_packet()) {
      auto* packet = input_item.packet();
      ZX_DEBUG_ASSERT(packet->has_start_offset());
      uint8_t* buffer_start = packet->buffer()->base() + packet->start_offset();
      // TODO(fxbug.dev/94139): Remove this copy.
      std::vector<uint8_t> data(buffer_start, buffer_start + packet->valid_length_bytes());
      if (packet->has_timestamp_ish()) {
        stream_to_pts_map_.emplace_back(next_stream_id_, packet->timestamp_ish());
        constexpr size_t kMaxPtsMapSize = 64;
        if (stream_to_pts_map_.size() > kMaxPtsMapSize)
          stream_to_pts_map_.pop_front();
      }
      events_->onCoreCodecInputPacketDone(input_item.packet());

      if (is_h264_ && avcc_processor_.is_avcc()) {
        DecodeAnnexBBuffer(avcc_processor_.ParseVideoAvcc(std::move(data)));
      } else {
        DecodeAnnexBBuffer(std::move(data));
      }

      // TODO(stefanbossbaly): Encapsulate in abstraction
      if (is_h264_) {
        constexpr uint8_t kAccessUnitDelimiterNalUnitType = 9;
        constexpr uint8_t kPrimaryPicType = 1 << (7 - 3);
        // Force frames to be processed. TODO(jbauman): Key on known_end_access_unit.
        std::vector<uint8_t> access_unit_delimiter{0, 0, 1, kAccessUnitDelimiterNalUnitType,
                                                   kPrimaryPicType};

        media::DecoderBuffer buffer(std::move(access_unit_delimiter), nullptr, 0, {});
        media_decoder_->SetStream(next_stream_id_++, buffer);
        auto result = media_decoder_->Decode();
        if (result != media::AcceleratedVideoDecoder::kRanOutOfStreamData) {
          events_->onCoreCodecFailCodec("Unexpected media_decoder::Decode result for delimiter: %d",
                                        result);
          return;
        }
      }
    }
  }
}

void CodecAdapterVaApiDecoder::CleanUpAfterStream() {
  {
    // TODO(stefanbossbaly): Encapsulate in abstraction
    if (is_h264_) {
      // Force frames to be processed.
      std::vector<uint8_t> end_of_stream_delimiter{0, 0, 1, 11};

      media::DecoderBuffer buffer(std::move(end_of_stream_delimiter), nullptr, 0, {});
      media_decoder_->SetStream(next_stream_id_++, buffer);
      auto result = media_decoder_->Decode();
      if (result != media::AcceleratedVideoDecoder::kRanOutOfStreamData) {
        events_->onCoreCodecFailCodec(
            "Unexpected media_decoder::Decode result for end of stream: %d", result);
        return;
      }
    }
  }

  bool res = media_decoder_->Flush();
  if (!res) {
    FX_LOGS(WARNING) << "media decoder flush failed";
  }
}

bool CodecAdapterVaApiDecoder::ProcessOutput(scoped_refptr<VASurface> va_surface,
                                             int bitstream_id) {
  gfx::Size pic_size = va_surface->size();

  // NV12 texture
  auto main_plane_size = safemath::CheckMul(GetOutputStride(), pic_size.height());
  auto uv_plane_size = main_plane_size / 2;
  auto pic_size_checked = main_plane_size + uv_plane_size;
  if (!pic_size_checked.IsValid()) {
    FX_LOGS(WARNING) << "Output picture size overflowed";
    return false;
  }
  size_t pic_size_bytes = pic_size_checked.ValueOrDie();
  const CodecBuffer* buffer = output_buffer_pool_.AllocateBuffer(pic_size_bytes);
  if (!buffer) {
    // Wait will succeed unless we're dropping all remaining frames of a stream.
    return true;
  }

  // For the moment we use DRM_PRIME_2 to represent VMOs.
  // To specify the destination VMO, we need two VASurfaceAttrib, one for the
  // DRM_PRIME_2 and one for the ext_attrib.
  VASurfaceAttrib attrib[2];
  attrib[0].type = VASurfaceAttribMemoryType;
  attrib[0].flags = VA_SURFACE_ATTRIB_SETTABLE;
  attrib[0].value.type = VAGenericValueTypeInteger;
  attrib[0].value.value.i = VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2;
  VADRMPRIMESurfaceDescriptor ext_attrib{};
  ext_attrib.width = pic_size.width();
  ext_attrib.height = pic_size.height();
  ext_attrib.fourcc = VA_FOURCC_NV12;

  attrib[1].type = VASurfaceAttribExternalBufferDescriptor;
  attrib[1].flags = VA_SURFACE_ATTRIB_SETTABLE;
  attrib[1].value.type = VAGenericValueTypePointer;
  attrib[1].value.value.p = &ext_attrib;
  zx::vmo vmo_dup;
  zx_status_t zx_status = buffer->vmo().duplicate(ZX_RIGHT_SAME_RIGHTS, &vmo_dup);
  if (zx_status != ZX_OK) {
    FX_LOGS(WARNING) << "Failed to duplicate vmo " << zx_status_get_string(zx_status);
    return false;
  }
  ext_attrib.num_objects = 1;
  ext_attrib.objects[0].fd = vmo_dup.release();
  ext_attrib.objects[0].drm_format_modifier = DRM_FORMAT_MOD_LINEAR;
  ext_attrib.num_layers = 1;
  ext_attrib.layers[0].drm_format = DRM_FORMAT_NV12;
  ext_attrib.layers[0].num_planes = 2;
  ext_attrib.layers[0].object_index[0] = 0;
  ext_attrib.layers[0].object_index[1] = 0;
  ext_attrib.layers[0].pitch[0] = GetOutputStride();
  ext_attrib.layers[0].pitch[1] = GetOutputStride();
  ext_attrib.layers[0].offset[1] = GetOutputStride() * pic_size.height();

  VASurfaceID processed_surface_id;
  // Create one surface backed by the destination VMO.
  VAStatus status =
      vaCreateSurfaces(VADisplayWrapper::GetSingleton()->display(), VA_RT_FORMAT_YUV420,
                       pic_size.width(), pic_size.height(), &processed_surface_id, 1, attrib, 2);
  if (status != VA_STATUS_SUCCESS) {
    FX_LOGS(WARNING) << "CreateSurface failed: " << vaErrorStr(status);
    return false;
  }
  ScopedSurfaceID processed_surface(processed_surface_id);
  VAImage image;
  // Set up a VAImage for the destination VMO.
  status =
      vaDeriveImage(VADisplayWrapper::GetSingleton()->display(), processed_surface.id(), &image);
  if (status != VA_STATUS_SUCCESS) {
    FX_LOGS(WARNING) << "DeriveImage failed: " << vaErrorStr(status);
    return false;
  }
  ScopedImageID scoped_image(image.image_id);

  // Copy from potentially-tiled surface to output surface. Intel decoders only
  // support writing to Y-tiled textures, so this copy is necessary for linear
  // output.
  // TODO(jbauman): Use VPP.
  status = vaGetImage(VADisplayWrapper::GetSingleton()->display(), va_surface->id(), 0, 0,
                      pic_size.width(), pic_size.height(), scoped_image.id());
  if (status != VA_STATUS_SUCCESS) {
    FX_LOGS(WARNING) << "GetImage failed: " << vaErrorStr(status);
    return false;
  }

  // Clean up the image; the data was already copied to the destination VMO
  // above.  The surface is cleaned up by ~processed_surface.
  scoped_image = ScopedImageID();

  std::optional<CodecPacket*> maybe_output_packet = free_output_packets_.WaitForElement();
  if (!maybe_output_packet) {
    // Wait will succeed unless we're dropping all remaining frames of a stream.
    return true;
  }
  auto output_packet = *maybe_output_packet;
  output_packet->SetBuffer(buffer);
  output_packet->SetStartOffset(0);
  output_packet->SetValidLengthBytes(static_cast<uint32_t>(pic_size_bytes));
  {
    auto pts_it =
        std::find_if(stream_to_pts_map_.begin(), stream_to_pts_map_.end(),
                     [bitstream_id](const auto& pair) { return pair.first == bitstream_id; });
    if (pts_it != stream_to_pts_map_.end()) {
      output_packet->SetTimstampIsh(pts_it->second);
    } else {
      output_packet->ClearTimestampIsh();
    }
  }

  {
    std::lock_guard<std::mutex> lock(lock_);
    ZX_DEBUG_ASSERT(in_use_by_client_.find(output_packet) == in_use_by_client_.end());

    in_use_by_client_.emplace(output_packet, VaApiOutput(buffer->base(), this));
  }
  events_->onCoreCodecOutputPacket(output_packet,
                                   /*error_detected_before=*/false,
                                   /*error_detected_during=*/false);
  return true;
}

scoped_refptr<VASurface> CodecAdapterVaApiDecoder::GetVASurface() {
  uint64_t surface_generation;
  VASurfaceID surface_id;
  gfx::Size pic_size;
  {
    std::lock_guard lock(surfaces_lock_);
    if (surfaces_.empty())
      return {};
    surface_id = surfaces_.back().release();
    surfaces_.pop_back();
    surface_generation = surface_generation_;
    pic_size = surface_size_;
  }
  return std::make_shared<VASurface>(
      surface_id, pic_size, VA_RT_FORMAT_YUV420,
      fit::function<void(VASurfaceID)>([this, surface_generation](VASurfaceID surface_id) {
        std::lock_guard lock(surfaces_lock_);
        if (surface_generation_ == surface_generation) {
          surfaces_.emplace_back(surface_id);
        } else {
          vaDestroySurfaces(VADisplayWrapper::GetSingleton()->display(), &surface_id, 1);
        }
      }));
}

VaApiOutput::~VaApiOutput() {
  if (adapter_)
    adapter_->output_buffer_pool_.FreeBuffer(base_address_);
}

VaApiOutput::VaApiOutput(VaApiOutput&& other) noexcept {
  adapter_ = other.adapter_;
  base_address_ = other.base_address_;
  other.adapter_ = nullptr;
}

VaApiOutput& VaApiOutput::operator=(VaApiOutput&& other) noexcept {
  adapter_ = other.adapter_;
  base_address_ = other.base_address_;
  other.adapter_ = nullptr;
  return *this;
}
