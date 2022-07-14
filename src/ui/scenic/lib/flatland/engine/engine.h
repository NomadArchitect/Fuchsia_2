// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_FLATLAND_ENGINE_ENGINE_H_
#define SRC_UI_SCENIC_LIB_FLATLAND_ENGINE_ENGINE_H_

#include <fuchsia/ui/display/color/cpp/fidl.h>
#include <lib/sys/inspect/cpp/component.h>
#include <lib/zx/eventpair.h>

// TODO(fxbug.dev/76640): delete when we delete hack_seen_display_ids_.
#include <lib/fit/function.h>

#include <optional>
#include <set>
#include <utility>

#include "src/ui/scenic/lib/flatland/default_flatland_presenter.h"
#include "src/ui/scenic/lib/flatland/engine/display_compositor.h"
#include "src/ui/scenic/lib/flatland/flatland_manager.h"
#include "src/ui/scenic/lib/flatland/global_matrix_data.h"
#include "src/ui/scenic/lib/flatland/link_system.h"
#include "src/ui/scenic/lib/flatland/uber_struct_system.h"
#include "src/ui/scenic/lib/scheduling/frame_scheduler.h"
#include "src/ui/scenic/lib/view_tree/snapshot_types.h"

namespace flatland {

using GetRootTransformFunc = fit::function<std::optional<TransformHandle>()>;
using ImageRectangles = std::vector<Rectangle2D>;
using ImageMetadatas = std::vector<allocation::ImageMetadata>;
using Renderables = std::pair<ImageRectangles, ImageMetadatas>;

// Engine is responsible for building a display list for DisplayCompositor, to insulate it from
// needing to know anything about the Flatland scene graph.
class Engine {
 public:
  Engine(std::shared_ptr<flatland::DisplayCompositor> flatland_compositor,
         std::shared_ptr<flatland::DefaultFlatlandPresenter> flatland_presenter,
         std::shared_ptr<flatland::UberStructSystem> uber_struct_system,
         std::shared_ptr<flatland::LinkSystem> link_system, inspect::Node inspect_node,
         GetRootTransformFunc get_root_transform);
  ~Engine() = default;

  // Builds a display list for the Flatland content tree rooted at |display|.
  void RenderScheduledFrame(uint64_t frame_number, zx::time presentation_time,
                            const FlatlandDisplay& display,
                            scheduling::FrameRenderer::FramePresentedCallback callback);

  // Snapshots the current Flatland content tree rooted at |root_transform|. |root_transform| is set
  // from the root transform of the display returned from
  // |FlatlandManager::GetPrimaryFlatlandDisplayForRendering|.
  view_tree::SubtreeSnapshot GenerateViewTreeSnapshot(const TransformHandle& root_transform) const;

  // Returns all renderables reachable from the display's root transform.
  Renderables GetRenderables(const FlatlandDisplay& display);

  // Binds the engine as the backend to the color correction service.
  void SetColorConversionInterface(
      fidl::InterfaceRequest<fuchsia::ui::display::color::Converter> request) {
    if (color_conversion_impl_ != nullptr) {
      FX_LOGS(WARNING) << "Color correction Implementation already exists.";
    }
    color_conversion_impl_ =
        std::make_shared<ColorConversionImpl>(std::move(request), flatland_compositor_.get());
  }

 private:
  // Initialize all inspect::Nodes, so that the Engine state can be observed.
  void InitializeInspectObjects();

  // Separate out the color correction implementation as a nested class within the engine,
  // so that we can lazily create it after allocating the engine.
  class ColorConversionImpl : public fuchsia::ui::display::color::Converter {
   public:
    ColorConversionImpl(fidl::InterfaceRequest<fuchsia::ui::display::color::Converter> request,
                        DisplayCompositor* flatland_compositor)
        : binding_(this, std::move(request)), flatland_compositor_(flatland_compositor) {}

    // |fuchsia::ui::display::color::Converter|
    void SetValues(fuchsia::ui::display::color::ConversionProperties properties,
                   SetValuesCallback callback) override;

    // |fuchsia::ui::display::color::Converter|
    void SetMinimumRgb(uint8_t minimum_rgb, SetMinimumRgbCallback callback) override;

   private:
    // The FIDL binding for the color correction api, which references |this| as the implementation.
    fidl::Binding<fuchsia::ui::display::color::Converter> binding_;
    flatland::DisplayCompositor* flatland_compositor_ = nullptr;
  };

  struct SceneState {
    UberStruct::InstanceMap snapshot;
    flatland::GlobalTopologyData topology_data;
    flatland::GlobalMatrixVector global_matrices;
    flatland::GlobalImageVector images;
    flatland::GlobalIndexVector image_indices;
    flatland::GlobalRectangleVector image_rectangles;

    SceneState(Engine& engine, TransformHandle root_transform);
  };

  std::shared_ptr<ColorConversionImpl> color_conversion_impl_;
  std::shared_ptr<flatland::DisplayCompositor> flatland_compositor_;
  std::shared_ptr<flatland::DefaultFlatlandPresenter> flatland_presenter_;
  std::shared_ptr<flatland::UberStructSystem> uber_struct_system_;
  std::shared_ptr<flatland::LinkSystem> link_system_;

  uint64_t last_rendered_frame_ = 0;

  // TODO(fxbug.dev/76640): hack so that we can call DisplayCompositor::AddDisplay() when we first
  // encounter a new display.  Need a more straightforward way to call AddDisplay().
  std::set<uint64_t> hack_seen_display_ids_;

  inspect::Node inspect_node_;
  inspect::LazyNode inspect_scene_dump_;
  GetRootTransformFunc get_root_transform_;
};

}  // namespace flatland

#endif  // SRC_UI_SCENIC_LIB_FLATLAND_ENGINE_ENGINE_H_
