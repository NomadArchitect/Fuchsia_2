// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/input/touch_source.h"

namespace scenic_impl::input {

TouchSource::TouchSource(zx_koid_t view_ref_koid,
                         fidl::InterfaceRequest<fuchsia::ui::pointer::TouchSource> event_provider,
                         fit::function<void(StreamId, const std::vector<GestureResponse>&)> respond,
                         fit::function<void()> error_handler, GestureContenderInspector& inspector)
    : TouchSourceBase(
          view_ref_koid, std::move(respond), [this](zx_status_t epitaph) { CloseChannel(epitaph); },
          inspector),
      binding_(this, std::move(event_provider)),
      error_handler_(std::move(error_handler)) {
  binding_.set_error_handler([this](zx_status_t epitaph) { error_handler_(); });
}

void TouchSource::CloseChannel(zx_status_t epitaph) {
  binding_.Close(epitaph);
  // NOTE: Triggers destruction of this object.
  error_handler_();
}

}  // namespace scenic_impl::input
