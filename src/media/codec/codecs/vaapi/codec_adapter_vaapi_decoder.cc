// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "codec_adapter_vaapi_decoder.h"

#include <lib/async/cpp/task.h>
#include <lib/fit/defer.h>
#include <lib/stdcompat/span.h>
#include <zircon/assert.h>
#include <zircon/status.h>

#include <condition_variable>
#include <mutex>
#include <optional>
#include <unordered_map>

#include <fbl/algorithm.h>
#include <safemath/checked_math.h>
#include <va/va_drmcommon.h>

#include "geometry.h"
#include "h264_accelerator.h"
#include "media/gpu/h264_decoder.h"
#include "media/gpu/vp9_decoder.h"
#include "vp9_accelerator.h"

#define LOG(x, ...) fprintf(stderr, __VA_ARGS__)

// This class manages output buffers when the client selects a linear buffer output. Since the
// output is linear the client will have to deswizzle the output from the decoded picture buffer
// (DPB) meaning that we can't directly share the output with the client. The manager will be
// responsible for creating the DPB surfaces used by the decoder and reconstructing them when a mid
// stream configuration change is required. This buffer manager will also be responsible for copying
// the output from the DBPs to the CodecBuffers the client provides us.
class LinearBufferManager : public SurfaceBufferManager {
 public:
  LinearBufferManager(std::mutex& codec_lock) : SurfaceBufferManager(codec_lock) {}
  ~LinearBufferManager() override = default;

  void AddBuffer(const CodecBuffer* buffer) override { output_buffer_pool_.AddBuffer(buffer); }

  void RecycleBuffer(const CodecBuffer* buffer) override {
    LinearOutput local_output;
    {
      std::lock_guard<std::mutex> guard(codec_lock_);
      ZX_DEBUG_ASSERT(in_use_by_client_.find(buffer) != in_use_by_client_.end());
      local_output = std::move(in_use_by_client_[buffer]);
      in_use_by_client_.erase(buffer);
    }
    // ~ local_output, which may trigger a buffer free callback.
  }

  void DeconfigureBuffers() override {
    {
      std::map<const CodecBuffer*, LinearOutput> to_drop;
      {
        std::lock_guard<std::mutex> lock(codec_lock_);
        std::swap(to_drop, in_use_by_client_);
      }
    }
    // ~to_drop

    ZX_DEBUG_ASSERT(!output_buffer_pool_.has_buffers_in_use());
  }

  scoped_refptr<VASurface> GetDPBSurface() override {
    uint64_t surface_generation;
    VASurfaceID surface_id;
    gfx::Size pic_size;

    {
      std::lock_guard<std::mutex> guard(surface_lock_);
      if (surfaces_.empty()) {
        return {};
      }
      surface_id = surfaces_.back().release();
      surfaces_.pop_back();
      surface_generation = surface_generation_;
      pic_size = surface_size_;
    }

    VASurface::ReleaseCB release_cb = [this, surface_generation](VASurfaceID surface_id) {
      std::lock_guard lock(surface_lock_);
      if (surface_generation_ == surface_generation) {
        surfaces_.emplace_back(surface_id);
      } else {
        auto status =
            vaDestroySurfaces(VADisplayWrapper::GetSingleton()->display(), &surface_id, 1);

        if (status != VA_STATUS_SUCCESS) {
          FX_LOGS(WARNING) << "vaDestroySurfaces failed: " << vaErrorStr(status);
        }
      }
    };

    return std::make_shared<VASurface>(surface_id, pic_size, VA_RT_FORMAT_YUV420,
                                       std::move(release_cb));
  }

  std::optional<std::pair<const CodecBuffer*, uint32_t>> ProcessOutputSurface(
      scoped_refptr<VASurface> va_surface) override {
    const CodecBuffer* buffer = output_buffer_pool_.AllocateBuffer();

    if (!buffer) {
      return std::nullopt;
    }

    // If any errors happen, release the buffer back into the pool
    auto release_buffer = fit::defer([&]() { output_buffer_pool_.FreeBuffer(buffer->base()); });

    const auto surface_size = va_surface->size();

    const auto aligned_stride_checked = GetAlignedStride(surface_size);
    const auto& [y_plane_checked, uv_plane_checked] = GetSurfacePlaneSizes(surface_size);
    const auto pic_size_checked = (y_plane_checked + uv_plane_checked).Cast<uint32_t>();

    if (!pic_size_checked.IsValid()) {
      FX_LOGS(WARNING) << "Output picture size overflowed";
      return std::nullopt;
    }

    size_t pic_size_bytes = static_cast<size_t>(pic_size_checked.ValueOrDie());
    ZX_ASSERT(buffer->size() >= pic_size_bytes);

    zx::vmo vmo_dup;
    zx_status_t zx_status = buffer->vmo().duplicate(ZX_RIGHT_SAME_RIGHTS, &vmo_dup);
    if (zx_status != ZX_OK) {
      FX_LOGS(WARNING) << "Failed to duplicate vmo " << zx_status_get_string(zx_status);
      return std::nullopt;
    }

    // For the moment we use DRM_PRIME_2 to represent VMOs.
    // To specify the destination VMO, we need two VASurfaceAttrib, one to set the
    // VASurfaceAttribMemoryType to VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2 and one for the
    // VADRMPRIMESurfaceDescriptor.
    VADRMPRIMESurfaceDescriptor ext_attrib{};
    VASurfaceAttrib attrib[2] = {
        {.type = VASurfaceAttribMemoryType,
         .flags = VA_SURFACE_ATTRIB_SETTABLE,
         .value = {.type = VAGenericValueTypeInteger,
                   .value = {.i = VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2}}},
        {.type = VASurfaceAttribExternalBufferDescriptor,
         .flags = VA_SURFACE_ATTRIB_SETTABLE,
         .value = {.type = VAGenericValueTypePointer, .value = {.p = &ext_attrib}}},
    };

    // VADRMPRIMESurfaceDescriptor
    ext_attrib.width = surface_size.width();
    ext_attrib.height = surface_size.height();
    ext_attrib.fourcc = VA_FOURCC_NV12;  // 2 plane YCbCr
    ext_attrib.num_objects = 1;
    ext_attrib.objects[0].fd = vmo_dup.release();
    ext_attrib.objects[0].drm_format_modifier = fuchsia::sysmem::FORMAT_MODIFIER_LINEAR;
    ext_attrib.objects[0].size = pic_size_checked.ValueOrDie();
    ext_attrib.num_layers = 1;
    ext_attrib.layers[0].drm_format = make_fourcc('N', 'V', '1', '2');
    ext_attrib.layers[0].num_planes = 2;

    // Y plane
    ext_attrib.layers[0].object_index[0] = 0;
    ext_attrib.layers[0].pitch[0] = aligned_stride_checked.ValueOrDie();
    ext_attrib.layers[0].offset[0] = 0;

    // UV Plane
    ext_attrib.layers[0].object_index[1] = 0;
    ext_attrib.layers[0].pitch[1] = aligned_stride_checked.ValueOrDie();
    ext_attrib.layers[0].offset[1] = y_plane_checked.ValueOrDie();

    VASurfaceID processed_surface_id;
    // Create one surface backed by the destination VMO.
    VAStatus status = vaCreateSurfaces(VADisplayWrapper::GetSingleton()->display(),
                                       VA_RT_FORMAT_YUV420, surface_size.width(),
                                       surface_size.height(), &processed_surface_id, 1, attrib, 2);
    if (status != VA_STATUS_SUCCESS) {
      FX_LOGS(WARNING) << "CreateSurface failed: " << vaErrorStr(status);
      return std::nullopt;
    }

    ScopedSurfaceID processed_surface(processed_surface_id);

    // Set up a VAImage for the destination VMO.
    VAImage image;
    status =
        vaDeriveImage(VADisplayWrapper::GetSingleton()->display(), processed_surface.id(), &image);
    if (status != VA_STATUS_SUCCESS) {
      FX_LOGS(WARNING) << "DeriveImage failed: " << vaErrorStr(status);
      return std::nullopt;
    }

    {
      ScopedImageID scoped_image(image.image_id);

      // Copy from potentially-tiled surface to output surface. Intel decoders only
      // support writing to Y-tiled textures, so this copy is necessary for linear
      // output.
      status = vaGetImage(VADisplayWrapper::GetSingleton()->display(), va_surface->id(), 0, 0,
                          surface_size.width(), surface_size.height(), scoped_image.id());
      if (status != VA_STATUS_SUCCESS) {
        FX_LOGS(WARNING) << "GetImage failed: " << vaErrorStr(status);
        return std::nullopt;
      }
    }
    // ~processed_surface: Clean up the image; the data was already copied to the destination VMO
    // above.

    {
      std::lock_guard<std::mutex> guard(codec_lock_);
      ZX_DEBUG_ASSERT(in_use_by_client_.count(buffer) == 0);

      in_use_by_client_.emplace(buffer, LinearOutput(buffer, this));
    }
    // ~guard

    // LinearOutput has taken ownership of the buffer.
    release_buffer.cancel();

    return std::make_pair(buffer, pic_size_checked.ValueOrDie());
  }

  void Reset() override { output_buffer_pool_.Reset(true); }

  void StopAllWaits() override { output_buffer_pool_.StopAllWaits(); }

 protected:
  void OnSurfaceGenerationUpdatedLocked(size_t num_of_surfaces, uint32_t output_stride)
      FXL_REQUIRE(surface_lock_) override {
    // Clear all existing DPB surfaces
    surfaces_.clear();

    std::vector<VASurfaceID> va_surfaces(num_of_surfaces, 0);
    VAStatus va_res =
        vaCreateSurfaces(VADisplayWrapper::GetSingleton()->display(), VA_RT_FORMAT_YUV420,
                         surface_size_.width(), surface_size_.height(), va_surfaces.data(),
                         static_cast<uint32_t>(va_surfaces.size()), nullptr, 0);

    if (va_res != VA_STATUS_SUCCESS) {
      // TODO(stefanbossbaly): Fix this
#if 0
      SetCodecFailure("vaCreateSurfaces failed: %s", vaErrorStr(va_res));
#endif
      return;
    }

    for (VASurfaceID id : va_surfaces) {
      surfaces_.emplace_back(id);
    }

    output_stride_ = output_stride;
  }

 private:
  safemath::internal::CheckedNumeric<uint32_t> GetAlignedStride(const gfx::Size& size) const {
    ZX_DEBUG_ASSERT(output_stride_.has_value());
    uint32_t output_stride = output_stride_.value();

    auto aligned_stride = fbl::round_up(static_cast<uint64_t>(size.width()), output_stride);
    return safemath::MakeCheckedNum(aligned_stride).Cast<uint32_t>();
  }

  std::pair<safemath::internal::CheckedNumeric<uint32_t>,
            safemath::internal::CheckedNumeric<uint32_t>>
  GetSurfacePlaneSizes(const gfx::Size& size) {
    // Depending on if the output is tiled or not we have to align our planes on tile boundaries
    // for both width and height
    auto aligned_stride = GetAlignedStride(size);
    auto aligned_y_height = static_cast<uint32_t>(size.height());
    auto aligned_uv_height = static_cast<uint32_t>(size.height()) / 2u;

    auto y_plane_size = safemath::CheckMul(aligned_stride, aligned_y_height);
    auto uv_plane_size = safemath::CheckMul(aligned_stride, aligned_uv_height);

    return std::make_pair(y_plane_size, uv_plane_size);
  }

  // VA-API outputs are distinct from the DPB and are stored in a regular
  // BufferPool, since the hardware doesn't necessarily support decoding to a
  // linear format like downstream consumers might need.
  class LinearOutput {
   public:
    LinearOutput() = default;
    LinearOutput(const CodecBuffer* buffer, LinearBufferManager* buffer_manager)
        : codec_buffer_(buffer), buffer_manager_(buffer_manager) {}
    ~LinearOutput() {
      if (buffer_manager_) {
        buffer_manager_->output_buffer_pool_.FreeBuffer(codec_buffer_->base());
      }
    }

    // Delete copying
    LinearOutput(const LinearOutput&) noexcept = delete;
    LinearOutput& operator=(const LinearOutput&) noexcept = delete;

    // Allow moving
    LinearOutput(LinearOutput&& other) noexcept {
      codec_buffer_ = other.codec_buffer_;
      buffer_manager_ = other.buffer_manager_;
      other.buffer_manager_ = nullptr;
    }

    LinearOutput& operator=(LinearOutput&& other) noexcept {
      codec_buffer_ = other.codec_buffer_;
      buffer_manager_ = other.buffer_manager_;
      other.buffer_manager_ = nullptr;
      return *this;
    }

   private:
    const CodecBuffer* codec_buffer_ = nullptr;
    LinearBufferManager* buffer_manager_ = nullptr;
  };

  // The order of output_buffer_pool_ and in_use_by_client_ matters, so that
  // destruction of in_use_by_client_ happens first, because those destructing
  // will return buffers to output_buffer_pool_.
  BufferPool output_buffer_pool_;
  std::map<const CodecBuffer*, LinearOutput> in_use_by_client_ FXL_GUARDED_BY(codec_lock_);

  // Holds the DPB surfaces
  std::vector<ScopedSurfaceID> surfaces_ FXL_GUARDED_BY(surface_lock_) = {};

  // Output stride
  std::optional<uint32_t> output_stride_;
};

// This class manages output buffers when the client selects a tiled buffer output. Since the output
// is tiled the client will directly share the output from the decoded picture buffer (DPB). The
// manager will be responsible for creating the DPB surfaces that are backed by CodecBuffers the
// client provides us. The manager is also responsible for reconfiguring surfaces when a mid stream
// configuration change is required.
class TiledBufferManager : public SurfaceBufferManager {
 public:
  TiledBufferManager(std::mutex& codec_lock) : SurfaceBufferManager(codec_lock) {}
  ~TiledBufferManager() override = default;

  void AddBuffer(const CodecBuffer* buffer) override { output_buffer_pool_.AddBuffer(buffer); }

  void RecycleBuffer(const CodecBuffer* buffer) override {
    scoped_refptr<VASurface> to_drop;
    {
      std::lock_guard<std::mutex> guard(codec_lock_);
      ZX_DEBUG_ASSERT(in_use_by_client_.count(buffer) != 0);
      auto map_itr = in_use_by_client_.find(buffer);
      to_drop = std::move(map_itr->second);
      in_use_by_client_.erase(map_itr);
    }
    // ~ to_drop, which may trigger a buffer free callback if the decoder is no longer referencing
    // the frame
  }

  void DeconfigureBuffers() override {
    // Drop all references to buffers referenced by the client but keep the ones referenced by the
    // decoder
    {
      std::unordered_multimap<const CodecBuffer*, scoped_refptr<VASurface>> to_drop;
      {
        std::lock_guard<std::mutex> lock(codec_lock_);
        std::swap(to_drop, in_use_by_client_);
      }
    }
    // ~to_drop

    ZX_DEBUG_ASSERT(!output_buffer_pool_.has_buffers_in_use());
  }

  // Getting a DPB requires that the surface is not in use by the client. This differs from the
  // linear version where DPB were not backed by a VMO. This function will block until a buffer is
  // recycled by the client or the manager is reset by the codec.
  scoped_refptr<VASurface> GetDPBSurface() override {
    const CodecBuffer* buffer = output_buffer_pool_.AllocateBuffer();

    if (!buffer) {
      return {};
    }

    // If any errors happen, release the buffer back into the pool
    auto release_buffer = fit::defer([&]() { output_buffer_pool_.FreeBuffer(buffer->base()); });

    std::lock_guard<std::mutex> guard(surface_lock_);
    VASurfaceID vmo_surface_id;

    // Check to see if there already is a surface allocated for this buffer
    auto map_itr = allocated_free_surfaces_.find(buffer);
    if (map_itr != allocated_free_surfaces_.end()) {
      vmo_surface_id = map_itr->second.release();
      allocated_free_surfaces_.erase(map_itr);
    } else {
      zx::vmo vmo_dup;
      zx_status_t zx_status = buffer->vmo().duplicate(ZX_RIGHT_SAME_RIGHTS, &vmo_dup);
      if (zx_status != ZX_OK) {
        FX_LOGS(WARNING) << "Failed to duplicate vmo " << zx_status_get_string(zx_status);
        return {};
      }

      const auto aligned_stride_checked = GetAlignedStride(surface_size_);
      const auto& [y_plane_checked, uv_plane_checked] = GetSurfacePlaneSizes(surface_size_);
      const auto pic_size_checked = (y_plane_checked + uv_plane_checked).Cast<uint32_t>();

      if (!aligned_stride_checked.IsValid()) {
        FX_LOGS(WARNING) << "Aligned stride overflowed";
        return {};
      }

      if (!pic_size_checked.IsValid()) {
        FX_LOGS(WARNING) << "Output picture size overflowed";
        return {};
      }

      size_t pic_size_bytes = static_cast<size_t>(pic_size_checked.ValueOrDie());
      ZX_ASSERT(buffer->size() >= pic_size_bytes);

      // For the moment we use DRM_PRIME_2 to represent VMOs.
      // To specify the destination VMO, we need two VASurfaceAttrib, one to set the
      // VASurfaceAttribMemoryType to VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2 and one for the
      // VADRMPRIMESurfaceDescriptor.
      VADRMPRIMESurfaceDescriptor ext_attrib{};
      VASurfaceAttrib attrib[2] = {
          {.type = VASurfaceAttribMemoryType,
           .flags = VA_SURFACE_ATTRIB_SETTABLE,
           .value = {.type = VAGenericValueTypeInteger,
                     .value = {.i = VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2}}},
          {.type = VASurfaceAttribExternalBufferDescriptor,
           .flags = VA_SURFACE_ATTRIB_SETTABLE,
           .value = {.type = VAGenericValueTypePointer, .value = {.p = &ext_attrib}}},
      };

      // VADRMPRIMESurfaceDescriptor
      ext_attrib.width = surface_size_.width();
      ext_attrib.height = surface_size_.height();
      ext_attrib.fourcc = VA_FOURCC_NV12;  // 2 plane YCbCr
      ext_attrib.num_objects = 1;
      ext_attrib.objects[0].fd = vmo_dup.release();
      ext_attrib.objects[0].drm_format_modifier =
          fuchsia::sysmem::FORMAT_MODIFIER_INTEL_I915_Y_TILED;
      ext_attrib.objects[0].size = pic_size_checked.ValueOrDie();
      ext_attrib.num_layers = 1;
      ext_attrib.layers[0].drm_format = make_fourcc('N', 'V', '1', '2');
      ext_attrib.layers[0].num_planes = 2;

      // Y plane
      ext_attrib.layers[0].object_index[0] = 0;
      ext_attrib.layers[0].pitch[0] = aligned_stride_checked.ValueOrDie();
      ext_attrib.layers[0].offset[0] = 0;

      // UV Plane
      ext_attrib.layers[0].object_index[1] = 0;
      ext_attrib.layers[0].pitch[1] = aligned_stride_checked.ValueOrDie();
      ext_attrib.layers[0].offset[1] = y_plane_checked.ValueOrDie();

      // Create one surface backed by the destination VMO.
      VAStatus status = vaCreateSurfaces(VADisplayWrapper::GetSingleton()->display(),
                                         VA_RT_FORMAT_YUV420, surface_size_.width(),
                                         surface_size_.height(), &vmo_surface_id, 1, attrib, 2);
      if (status != VA_STATUS_SUCCESS) {
        FX_LOGS(WARNING) << "CreateSurface failed: " << vaErrorStr(status);
        return {};
      }
    }

    gfx::Size pic_size = surface_size_;
    uint64_t surface_generation = surface_generation_;

    // Callback that is called when the ref_count of this new constructed surface hits 0, This
    // occurs when the surface is no longer being used in the decoder (aka a new frame has replaced
    // us) and is no longer in use by the client (surface has been removed from in_use_by_client_).
    // Therefore once the VASurface release callback is called we can return this surface (and
    // therefore the VMO backing the surface) back into the pool of available surfaces.
    VASurface::ReleaseCB release_cb = [this, buffer, surface_generation](VASurfaceID surface_id) {
      {
        std::lock_guard<std::mutex> guard(surface_lock_);
        ZX_ASSERT(surface_to_buffer_.erase(surface_id) == 1);

        if (surface_generation_ == surface_generation) {
          allocated_free_surfaces_.emplace(buffer, surface_id);
        } else {
          auto status =
              vaDestroySurfaces(VADisplayWrapper::GetSingleton()->display(), &surface_id, 1);

          if (status != VA_STATUS_SUCCESS) {
            FX_LOGS(ERROR) << "vaDestroySurfaces failed: " << vaErrorStr(status);
          }
        }
      }
      // ~guard

      output_buffer_pool_.FreeBuffer(buffer->base());
    };

    ZX_DEBUG_ASSERT(surface_to_buffer_.count(vmo_surface_id) == 0);
    surface_to_buffer_.emplace(vmo_surface_id, buffer);

    release_buffer.cancel();
    return std::make_shared<VASurface>(vmo_surface_id, pic_size, VA_RT_FORMAT_YUV420,
                                       std::move(release_cb));
  }

  std::optional<std::pair<const CodecBuffer*, uint32_t>> ProcessOutputSurface(
      scoped_refptr<VASurface> va_surface) override {
    const CodecBuffer* buffer = nullptr;

    {
      std::lock_guard<std::mutex> guard(surface_lock_);
      ZX_DEBUG_ASSERT(surface_to_buffer_.count(va_surface->id()) != 0);
      buffer = surface_to_buffer_[va_surface->id()];
    }

    if (!buffer) {
      return {};
    }

    const auto& [y_plane_checked, uv_plane_checked] = GetSurfacePlaneSizes(va_surface->size());
    const auto pic_size_checked = (y_plane_checked + uv_plane_checked).Cast<uint32_t>();
    if (!pic_size_checked.IsValid()) {
      FX_LOGS(WARNING) << "Output picture size overflowed";
      return {};
    }

    // We are about to lend out the surface to the client so store the surface in in_use_by_client_
    // multimap so it increments the refcount until the client recycles it
    {
      std::lock_guard<std::mutex> guard(codec_lock_);
      in_use_by_client_.insert(std::make_pair(buffer, va_surface));
    }

    return std::make_pair(buffer, pic_size_checked.ValueOrDie());
  }

  void Reset() override { output_buffer_pool_.Reset(true); }

  void StopAllWaits() override { output_buffer_pool_.StopAllWaits(); }

 protected:
  void OnSurfaceGenerationUpdatedLocked(size_t num_of_surfaces, uint32_t output_stride)
      FXL_REQUIRE(surface_lock_) override {
    // This will call vaDestroySurface on all surfaces held by this data structure. Don't need to
    // reconstruct the surfaces here. They will be reconstructed once GetDPBSurface() is called and
    // the buffer has no linked surface.
    allocated_free_surfaces_.clear();
  }

 private:
  static safemath::internal::CheckedNumeric<uint32_t> GetAlignedStride(const gfx::Size& size) {
    auto aligned_stride = fbl::round_up(static_cast<uint64_t>(size.width()),
                                        CodecAdapterVaApiDecoder::kTileWidthAlignment);
    return safemath::MakeCheckedNum(aligned_stride).Cast<uint32_t>();
  }

  static std::pair<safemath::internal::CheckedNumeric<uint32_t>,
                   safemath::internal::CheckedNumeric<uint32_t>>
  GetSurfacePlaneSizes(const gfx::Size& size) {
    // Depending on if the output is tiled or not we have to align our planes on tile boundaries
    // for both width and height
    auto aligned_stride = GetAlignedStride(size);
    auto aligned_y_height = static_cast<uint32_t>(size.height());
    auto aligned_uv_height = static_cast<uint32_t>(size.height()) / 2u;

    aligned_y_height =
        fbl::round_up(aligned_y_height, CodecAdapterVaApiDecoder::kTileHeightAlignment);
    aligned_uv_height =
        fbl::round_up(aligned_uv_height, CodecAdapterVaApiDecoder::kTileHeightAlignment);

    auto y_plane_size = safemath::CheckMul(aligned_stride, aligned_y_height);
    auto uv_plane_size = safemath::CheckMul(aligned_stride, aligned_uv_height);

    return std::make_pair(y_plane_size, uv_plane_size);
  }

  // Structure that maps allocated buffers shared with the client. Once the buffer is no longer in
  // use by the client and the decoder it should be removed from this map and marked as free in the
  // output_buffer_pool_.
  std::unordered_map<VASurfaceID, const CodecBuffer*> surface_to_buffer_
      FXL_GUARDED_BY(surface_lock_);

  // Once a surface is allocated it is stored in this map which maps the codec buffer that backs
  // the surface. If a resize event happens this structure will have to be invalidated and the
  // surfaces will have to be regenerated to match the new surface_size_
  std::unordered_map<const CodecBuffer*, ScopedSurfaceID> allocated_free_surfaces_
      FXL_GUARDED_BY(surface_lock_);

  // Maps the codec buffer to the VA surface being shared to the client. In addition to the
  // mapping this data structure holds a reference to the surface being used by the client,
  // preventing it from being destructed prior to it being recycled.
  // This has to be a multimap because it is possible to lend out the same surface concurrently to
  // the client and we don't want the destructor of the VASurface to be called when only one of the
  // lent out surfaces is recycled. For example on VP9 if show_existing_frame is marked true, we can
  // lend out the same surface concurrently.
  std::unordered_multimap<const CodecBuffer*, scoped_refptr<VASurface>> in_use_by_client_
      FXL_GUARDED_BY(codec_lock_);
};

void CodecAdapterVaApiDecoder::CoreCodecInit(
    const fuchsia::media::FormatDetails& initial_input_format_details) {
  if (!initial_input_format_details.has_format_details_version_ordinal()) {
    SetCodecFailure("CoreCodecInit(): Initial input format details missing version ordinal.");
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
    SetCodecFailure("CodecCodecInit(): Unknown mime_type %s\n", mime_type.c_str());
    return;
  }

  if (codec_diagnostics_) {
    std::string codec_name = is_h264_ ? "H264" : "VP9";
    codec_instance_diagnostics_ = codec_diagnostics_->CreateComponentCodec(codec_name);
  }

  VAConfigAttrib attribs[1] = {{.type = VAConfigAttribRTFormat, .value = VA_RT_FORMAT_YUV420}};
  VAConfigID config_id;
  VAEntrypoint va_entrypoint = VAEntrypointVLD;
  VAStatus va_status;
  VAProfile va_profile;

  if (mime_type == "video/h264-multi" || mime_type == "video/h264") {
    va_profile = VAProfileH264High;
  } else if (mime_type == "video/vp9") {
    va_profile = VAProfileVP9Profile0;
  } else {
    SetCodecFailure("CodecCodecInit(): Unknown mime_type %s\n", mime_type.c_str());
    return;
  }

  va_status = vaCreateConfig(VADisplayWrapper::GetSingleton()->display(), va_profile, va_entrypoint,
                             attribs, std::size(attribs), &config_id);
  if (va_status != VA_STATUS_SUCCESS) {
    SetCodecFailure("CodecCodecInit(): Failed to create config: %s", vaErrorStr(va_status));
    return;
  }
  config_.emplace(config_id);

  int max_config_attributes = vaMaxNumConfigAttributes(VADisplayWrapper::GetSingleton()->display());
  std::vector<VAConfigAttrib> config_attributes(max_config_attributes);

  int num_config_attributes;
  va_status = vaQueryConfigAttributes(VADisplayWrapper::GetSingleton()->display(), config_->id(),
                                      &va_profile, &va_entrypoint, config_attributes.data(),
                                      &num_config_attributes);

  if (va_status != VA_STATUS_SUCCESS) {
    SetCodecFailure("CodecCodecInit(): Failed to query attributes: %s", vaErrorStr(va_status));
    return;
  }

  std::optional<uint32_t> max_height = std::nullopt;
  std::optional<uint32_t> max_width = std::nullopt;

  for (int i = 0; i < num_config_attributes; i += 1) {
    const VAConfigAttrib& attrib = config_attributes[i];
    switch (attrib.type) {
      case VAConfigAttribMaxPictureHeight:
        max_height = attrib.value;
        break;
      case VAConfigAttribMaxPictureWidth:
        max_width = attrib.value;
        break;
      default:
        break;
    }
  }

  if (!max_height) {
    FX_LOGS(WARNING)
        << "Could not query hardware for max picture height supported. Setting default";
  } else {
    max_picture_height_ = max_height.value();
  }

  if (!max_width) {
    FX_LOGS(WARNING) << "Could not query hardware for max picture width supported. Setting default";
  } else {
    max_picture_width_ = max_width.value();
  }

  zx_status_t result =
      input_processing_loop_.StartThread("input_processing_thread_", &input_processing_thread_);
  if (result != ZX_OK) {
    SetCodecFailure(
        "CodecCodecInit(): Failed to start input processing thread with "
        "zx_status_t: %d",
        result);
    return;
  }
}

void CodecAdapterVaApiDecoder::CoreCodecStartStream() {
  // It's ok for RecycleInputPacket to make a packet free anywhere in this
  // sequence. Nothing else ought to be happening during CoreCodecStartStream
  // (in this or any other thread).
  input_queue_.Reset();
  free_output_packets_.Reset(/*keep_data=*/true);

  // If the stream has initialized then reset
  if (surface_buffer_manager_) {
    surface_buffer_manager_->Reset();
  }

  LaunchInputProcessingLoop();

  TRACE_INSTANT("codec_runner", "Media:Start", TRACE_SCOPE_THREAD);
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

  input_queue_.Reset(/*keep_data=*/true);

  LaunchInputProcessingLoop();
}

void CodecAdapterVaApiDecoder::DecodeAnnexBBuffer(media::DecoderBuffer buffer) {
  media_decoder_->SetStream(next_stream_id_++, buffer);

  while (true) {
    state_ = DecoderState::kDecoding;
    auto result = media_decoder_->Decode();
    state_ = DecoderState::kIdle;

    if (result == media::AcceleratedVideoDecoder::kConfigChange) {
      {
        std::lock_guard<std::mutex> guard(lock_);
        mid_stream_output_buffer_reconfig_finish_ = false;
      }

      // Trigger a mid stream output constraints change
      // TODO(fxbug.dev/102737): We always request a output reconfiguration. This may or may not be
      // needed.
      events_->onCoreCodecMidStreamOutputConstraintsChange(true);

      gfx::Size pic_size = media_decoder_->GetPicSize();
      VAContextID context_id;
      VAStatus va_res = vaCreateContext(VADisplayWrapper::GetSingleton()->display(), config_->id(),
                                        pic_size.width(), pic_size.height(), VA_PROGRESSIVE,
                                        nullptr, 0, &context_id);
      if (va_res != VA_STATUS_SUCCESS) {
        SetCodecFailure("vaCreateContext failed: %s", vaErrorStr(va_res));
        break;
      }
      context_id_.emplace(context_id);

      // Wait for the stream reconfiguration to finish before continuing to increment the surface
      // generation value
      {
        std::unique_lock<std::mutex> lock(lock_);
        surface_buffer_manager_cv_.wait(lock, [this]() FXL_REQUIRE(lock_) {
          return mid_stream_output_buffer_reconfig_finish_;
        });
      }

      // Increment surface generation so all existing surfaces will be freed
      // when they're released instead of being returned to the pool.
      surface_buffer_manager_->IncrementSurfaceGeneration(
          pic_size, media_decoder_->GetRequiredNumOfPictures(), GetOutputStride());

      continue;
    } else if (result == media::AcceleratedVideoDecoder::kRanOutOfStreamData) {
      // Reset decoder failures on successful decode
      decoder_failures_ = 0;
      break;
    } else {
      decoder_failures_ += 1;
      if (decoder_failures_ >= kMaxDecoderFailures) {
        SetCodecFailure(
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

      break;
    }
  }
}  // ~buffer

const char* CodecAdapterVaApiDecoder::DecoderStateName(DecoderState state) {
  switch (state) {
    case DecoderState::kIdle:
      return "Idle";
    case DecoderState::kDecoding:
      return "Decoding";
    case DecoderState::kError:
      return "Error";
    default:
      return "UNKNOWN";
  }
}

template <class... Args>
void CodecAdapterVaApiDecoder::SetCodecFailure(const char* format, Args&&... args) {
  state_ = DecoderState::kError;
  events_->onCoreCodecFailCodec(format, std::forward<Args>(args)...);
}

void CodecAdapterVaApiDecoder::ProcessInputLoop() {
  std::optional<CodecInputItem> maybe_input_item;
  while ((maybe_input_item = input_queue_.WaitForElement())) {
    CodecInputItem input_item = std::move(maybe_input_item.value());
    if (input_item.is_format_details()) {
      const std::string& mime_type = input_item.format_details().mime_type();

      if ((!is_h264_ && (mime_type == "video/h264-multi" || mime_type == "video/h264")) ||
          (is_h264_ && mime_type == "video/vp9")) {
        SetCodecFailure(
            "CodecCodecInit(): Can not switch codec type after setting it in CoreCodecInit(). "
            "Attempting to switch it to %s\n",
            mime_type.c_str());
        return;
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

        media::DecoderBuffer buffer(end_of_stream_delimiter);
        media_decoder_->SetStream(next_stream_id_++, buffer);
        state_ = DecoderState::kDecoding;
        auto result = media_decoder_->Decode();
        state_ = DecoderState::kIdle;
        if (result != media::AcceleratedVideoDecoder::kRanOutOfStreamData) {
          SetCodecFailure("Unexpected media_decoder::Decode result for end of stream: %d", result);
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
      if (packet->has_timestamp_ish()) {
        stream_to_pts_map_.emplace_back(next_stream_id_, packet->timestamp_ish());
        constexpr size_t kMaxPtsMapSize = 64;
        if (stream_to_pts_map_.size() > kMaxPtsMapSize)
          stream_to_pts_map_.pop_front();
      }

      const uint8_t* buffer_start = packet->buffer()->base() + packet->start_offset();
      size_t buffer_size = packet->valid_length_bytes();

      bool returned_buffer = false;
      auto return_input_packet =
          fit::defer_callback(fit::closure([this, &input_item, &returned_buffer] {
            events_->onCoreCodecInputPacketDone(input_item.packet());
            returned_buffer = true;
          }));

      if (is_h264_ && avcc_processor_.is_avcc()) {
        // TODO(fxbug.dev/94139): Remove this copy.
        auto output_avcc_vec = avcc_processor_.ParseVideoAvcc(buffer_start, buffer_size);
        media::DecoderBuffer buffer(output_avcc_vec, packet->buffer(), packet->start_offset(),
                                    std::move(return_input_packet));
        DecodeAnnexBBuffer(std::move(buffer));
      } else {
        media::DecoderBuffer buffer({buffer_start, buffer_size}, packet->buffer(),
                                    packet->start_offset(), std::move(return_input_packet));
        DecodeAnnexBBuffer(std::move(buffer));
      }

      // Ensure that the decode buffer has been destroyed and the input packet has been returned
      ZX_ASSERT(returned_buffer);

      // TODO(stefanbossbaly): Encapsulate in abstraction
      if (is_h264_) {
        constexpr uint8_t kAccessUnitDelimiterNalUnitType = 9;
        constexpr uint8_t kPrimaryPicType = 1 << (7 - 3);
        // Force frames to be processed. TODO(jbauman): Key on known_end_access_unit.
        std::vector<uint8_t> access_unit_delimiter{0, 0, 1, kAccessUnitDelimiterNalUnitType,
                                                   kPrimaryPicType};

        media::DecoderBuffer buffer(access_unit_delimiter);
        media_decoder_->SetStream(next_stream_id_++, buffer);
        state_ = DecoderState::kDecoding;
        auto result = media_decoder_->Decode();
        state_ = DecoderState::kIdle;
        if (result != media::AcceleratedVideoDecoder::kRanOutOfStreamData) {
          SetCodecFailure("Unexpected media_decoder::Decode result for delimiter: %d", result);
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

      media::DecoderBuffer buffer(end_of_stream_delimiter);
      media_decoder_->SetStream(next_stream_id_++, buffer);
      auto result = media_decoder_->Decode();
      if (result != media::AcceleratedVideoDecoder::kRanOutOfStreamData) {
        SetCodecFailure("Unexpected media_decoder::Decode result for end of stream: %d", result);
        return;
      }
    }
  }

  bool res = media_decoder_->Flush();
  if (!res) {
    FX_LOGS(WARNING) << "media decoder flush failed";
  }
}

void CodecAdapterVaApiDecoder::CoreCodecMidStreamOutputBufferReConfigFinish() {
  surface_buffer_manager_.reset();

  if (IsOutputTiled()) {
    surface_buffer_manager_ = std::make_unique<TiledBufferManager>(lock_);
  } else {
    surface_buffer_manager_ = std::make_unique<LinearBufferManager>(lock_);
  }

  LoadStagedOutputBuffers();

  // Signal that we are done with the mid stream output buffer configuration to other threads
  {
    std::lock_guard<std::mutex> guard(lock_);
    mid_stream_output_buffer_reconfig_finish_ = true;
  }
  surface_buffer_manager_cv_.notify_all();
}

bool CodecAdapterVaApiDecoder::ProcessOutput(scoped_refptr<VASurface> va_surface,
                                             int bitstream_id) {
  auto maybe_processed_surface = surface_buffer_manager_->ProcessOutputSurface(va_surface);

  if (!maybe_processed_surface) {
    return true;
  }

  auto& [codec_buffer, pic_size_bytes] = maybe_processed_surface.value();

  auto release_buffer = fit::defer([this, codec_buffer = codec_buffer]() {
    surface_buffer_manager_->RecycleBuffer(codec_buffer);
  });

  std::optional<CodecPacket*> maybe_output_packet = free_output_packets_.WaitForElement();
  if (!maybe_output_packet) {
    // Wait will succeed unless we're dropping all remaining frames of a stream.
    return true;
  }

  auto output_packet = maybe_output_packet.value();
  output_packet->SetBuffer(codec_buffer);
  output_packet->SetStartOffset(0);
  output_packet->SetValidLengthBytes(pic_size_bytes);
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

  release_buffer.cancel();
  events_->onCoreCodecOutputPacket(output_packet,
                                   /*error_detected_before=*/false,
                                   /*error_detected_during=*/false);
  return true;
}

scoped_refptr<VASurface> CodecAdapterVaApiDecoder::GetVASurface() {
  return surface_buffer_manager_->GetDPBSurface();
}
