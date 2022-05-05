// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "vaapi_utils.h"

#include <lib/fdio/directory.h>
#include <lib/zx/channel.h>
#include <zircon/assert.h>

#include <filesystem>

#include <va/va_magma.h>

namespace {
std::unique_ptr<VADisplayWrapper> display_wrapper;
}

// static
bool VADisplayWrapper::InitializeSingleton(uint64_t required_vendor_id) {
  ZX_ASSERT(!display_wrapper);

  auto new_display_wrapper = std::make_unique<VADisplayWrapper>();

  for (auto& p : std::filesystem::directory_iterator("/dev/class/gpu")) {
    {
      zx::channel local, remote;
      zx_status_t zx_status = zx::channel::create(0 /*flags*/, &local, &remote);
      ZX_ASSERT(zx_status == ZX_OK);
      zx_status = fdio_service_connect(p.path().c_str(), remote.release());
      ZX_ASSERT(zx_status == ZX_OK);
      magma_status_t status =
          magma_device_import(local.release(), &new_display_wrapper->magma_device_);
      ZX_ASSERT(status == MAGMA_STATUS_OK);
      if (status != MAGMA_STATUS_OK)
        continue;
    }
    {
      uint64_t vendor_id;
      magma_status_t magma_status = magma_query(new_display_wrapper->magma_device_,
                                                MAGMA_QUERY_VENDOR_ID, nullptr, &vendor_id);
      if (magma_status == MAGMA_STATUS_OK && vendor_id == required_vendor_id) {
        break;
      }
    }

    magma_device_release(new_display_wrapper->magma_device_);
    new_display_wrapper->magma_device_ = {};
  }

  if (!new_display_wrapper->magma_device_)
    return false;

  if (!new_display_wrapper->Initialize())
    return false;
  display_wrapper = std::move(new_display_wrapper);
  return true;
}

// static
bool VADisplayWrapper::InitializeSingletonForTesting() {
  auto new_display_wrapper = std::make_unique<VADisplayWrapper>();
  if (!new_display_wrapper->Initialize())
    return false;
  display_wrapper = std::move(new_display_wrapper);
  return true;
}

bool VADisplayWrapper::Initialize() {
  display_ = vaGetDisplayMagma(magma_device_);
  if (!display_)
    return false;

  int major_ver, minor_ver;

  VAStatus va_status = vaInitialize(display_, &major_ver, &minor_ver);
  if (va_status != VA_STATUS_SUCCESS) {
    return false;
  }
  return true;
}

// static
VADisplayWrapper* VADisplayWrapper::GetSingleton() { return display_wrapper.get(); }

VASurface::VASurface(VASurfaceID va_surface_id, const gfx::Size& size, unsigned int format,
                     ReleaseCB release_cb)
    : va_surface_id_(va_surface_id),
      size_(size),
      format_(format),
      release_cb_(std::move(release_cb)) {
  DCHECK(release_cb_);
}

VASurface::~VASurface() { std::move(release_cb_)(va_surface_id_); }

static bool SupportsProfile(const VAProfile& profile, uint32_t format_mask) {
  VADisplay display = VADisplayWrapper::GetSingleton()->display();
  std::vector<VAEntrypoint> entrypoints(vaMaxNumEntrypoints(display));
  int num_entrypoints, vld_entrypoint;
  VAStatus va_status =
      vaQueryConfigEntrypoints(display, profile, entrypoints.data(), &num_entrypoints);
  if (va_status != VA_STATUS_SUCCESS)
    return false;

  for (vld_entrypoint = 0; vld_entrypoint < num_entrypoints; vld_entrypoint++) {
    if (entrypoints[vld_entrypoint] == VAEntrypointVLD)
      break;
  }
  if (vld_entrypoint == num_entrypoints) {
    return false;
  }

  VAConfigAttrib attrib;
  attrib.type = VAConfigAttribRTFormat;
  va_status = vaGetConfigAttributes(display, profile, VAEntrypointVLD, &attrib, 1);
  if (va_status != VA_STATUS_SUCCESS)
    return false;
  if ((attrib.value & format_mask) == 0) {
    return false;
  }
  return true;
}

static bool SupportsH264() { return SupportsProfile(VAProfileH264High, VA_RT_FORMAT_YUV420); }

static bool SupportsVP9() { return SupportsProfile(VAProfileVP9Profile0, VA_RT_FORMAT_YUV420); }

std::vector<fuchsia::mediacodec::CodecDescription> GetCodecList() {
  std::vector<fuchsia::mediacodec::CodecDescription> descriptions;
  if (SupportsH264()) {
    fuchsia::mediacodec::CodecDescription description;
    description.codec_type = fuchsia::mediacodec::CodecType::DECODER;
    description.mime_type = "video/h264";
    descriptions.push_back(description);
    description.codec_type = fuchsia::mediacodec::CodecType::DECODER;
    description.mime_type = "video/h264-multi";
    descriptions.push_back(description);
  }

  if (SupportsVP9()) {
    fuchsia::mediacodec::CodecDescription description;
    description.codec_type = fuchsia::mediacodec::CodecType::DECODER;
    description.mime_type = "video/vp9";
    descriptions.push_back(description);
  }

  return descriptions;
}
