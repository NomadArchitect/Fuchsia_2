// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_FLATLAND_FLATLAND_H_
#define SRC_UI_SCENIC_LIB_FLATLAND_FLATLAND_H_

#include <fuchsia/scenic/allocation/cpp/fidl.h>
#include <fuchsia/ui/gfx/cpp/fidl.h>
#include <fuchsia/ui/scenic/internal/cpp/fidl.h>
#include <lib/async/cpp/wait.h>
#include <lib/fidl/cpp/binding.h>
#include <lib/fit/function.h>

#include <map>
#include <memory>
#include <unordered_map>
#include <unordered_set>
#include <vector>

// clang-format off
#include "src/ui/lib/glm_workaround/glm_workaround.h"
#include <glm/vec2.hpp>
#include <glm/mat3x3.hpp>
// clang-format on

#include "src/ui/lib/escher/flib/fence_queue.h"
#include "src/ui/scenic/lib/allocation/buffer_collection_importer.h"
#include "src/ui/scenic/lib/flatland/flatland_presenter.h"
#include "src/ui/scenic/lib/flatland/link_system.h"
#include "src/ui/scenic/lib/flatland/transform_graph.h"
#include "src/ui/scenic/lib/flatland/transform_handle.h"
#include "src/ui/scenic/lib/flatland/uber_struct_system.h"
#include "src/ui/scenic/lib/gfx/engine/object_linker.h"
#include "src/ui/scenic/lib/scheduling/id.h"
#include "src/ui/scenic/lib/scheduling/present2_helper.h"

namespace flatland {

// This is a WIP implementation of the 2D Layer API. It currently exists to run unit tests, and to
// provide a platform for features to be iterated and implemented over time.
class Flatland : public fuchsia::ui::scenic::internal::Flatland {
 public:
  using BufferCollectionId = uint64_t;
  using ContentId = fuchsia::ui::scenic::internal::ContentId;
  using FuturePresentationInfos = std::vector<fuchsia::scenic::scheduling::PresentationInfo>;
  using TransformId = fuchsia::ui::scenic::internal::TransformId;

  // Binds this Flatland object to serve |request| on |dispatcher|. The |destroy_instance_function|
  // will be invoked from the Looper that owns |dispatcher| when this object is ready to be cleaned
  // up (e.g. when the client closes their side of the channel or encounters makes an unrecoverable
  // API call error).
  //
  // |flatland_presenter|, |link_system|, |uber_struct_queue|, and |buffer_collection_importers|
  // allow this Flatland object to access resources shared by all Flatland instances for actions
  // like frame scheduling, linking, buffer allocation, and presentation to the global scene graph.
  explicit Flatland(async_dispatcher_t* dispatcher,
                    fidl::InterfaceRequest<fuchsia::ui::scenic::internal::Flatland> request,
                    scheduling::SessionId session_id,
                    std::function<void()> destroy_instance_function,
                    const std::shared_ptr<FlatlandPresenter>& flatland_presenter,
                    const std::shared_ptr<LinkSystem>& link_system,
                    const std::shared_ptr<UberStructSystem::UberStructQueue>& uber_struct_queue,
                    const std::vector<std::shared_ptr<allocation::BufferCollectionImporter>>&
                        buffer_collection_importers);
  ~Flatland();

  // Because this object captures its "this" pointer in internal closures, it is unsafe to copy or
  // move it. Disable all copy and move operations.
  Flatland(const Flatland&) = delete;
  Flatland& operator=(const Flatland&) = delete;
  Flatland(Flatland&&) = delete;
  Flatland& operator=(Flatland&&) = delete;

  // |fuchsia::ui::scenic::internal::Flatland|
  void Present(fuchsia::ui::scenic::internal::PresentArgs args, PresentCallback callback) override;
  // |fuchsia::ui::scenic::internal::Flatland|
  void LinkToParent(
      fuchsia::ui::scenic::internal::GraphLinkToken token,
      fidl::InterfaceRequest<fuchsia::ui::scenic::internal::GraphLink> graph_link) override;
  // |fuchsia::ui::scenic::internal::Flatland|
  void UnlinkFromParent(
      fuchsia::ui::scenic::internal::Flatland::UnlinkFromParentCallback callback) override;
  // |fuchsia::ui::scenic::internal::Flatland|
  void ClearGraph() override;
  // |fuchsia::ui::scenic::internal::Flatland|
  void CreateTransform(TransformId transform_id) override;
  // |fuchsia::ui::scenic::internal::Flatland|
  void SetTranslation(TransformId transform_id,
                      fuchsia::ui::scenic::internal::Vec2 translation) override;
  // |fuchsia::ui::scenic::internal::Flatland|
  void SetOrientation(TransformId transform_id,
                      fuchsia::ui::scenic::internal::Orientation orientation) override;
  // |fuchsia::ui::scenic::internal::Flatland|
  void SetScale(TransformId transform_id, fuchsia::ui::scenic::internal::Vec2 scale) override;
  // |fuchsia::ui::scenic::internal::Flatland|
  void AddChild(TransformId parent_transform_id, TransformId child_transform_id) override;
  // |fuchsia::ui::scenic::internal::Flatland|
  void RemoveChild(TransformId parent_transform_id, TransformId child_transform_id) override;
  // |fuchsia::ui::scenic::internal::Flatland|
  void SetRootTransform(TransformId transform_id) override;
  // |fuchsia::ui::scenic::internal::Flatland|
  void CreateLink(
      ContentId link_id, fuchsia::ui::scenic::internal::ContentLinkToken token,
      fuchsia::ui::scenic::internal::LinkProperties properties,
      fidl::InterfaceRequest<fuchsia::ui::scenic::internal::ContentLink> content_link) override;
  // |fuchsia::ui::scenic::internal::Flatland|
  void CreateImage(ContentId image_id,
                   fuchsia::scenic::allocation::BufferCollectionImportToken import_token,
                   uint32_t vmo_index,
                   fuchsia::ui::scenic::internal::ImageProperties properties) override;
  // |fuchsia::ui::scenic::internal::Flatland|
  void SetOpacity(TransformId transform_id, float val) override;
  // |fuchsia::ui::scenic::internal::Flatland|
  void SetContentOnTransform(TransformId transform_id, ContentId content_id) override;
  // |fuchsia::ui::scenic::internal::Flatland|
  void SetLinkProperties(ContentId link_id,
                         fuchsia::ui::scenic::internal::LinkProperties properties) override;
  // |fuchsia::ui::scenic::internal::Flatland|
  void SetLinkSize(ContentId link_id, fuchsia::ui::scenic::internal::Vec2 size) override;
  // |fuchsia::ui::scenic::internal::Flatland|
  void ReleaseTransform(TransformId transform_id) override;
  // |fuchsia::ui::scenic::internal::Flatland|
  void ReleaseLink(ContentId link_id,
                   fuchsia::ui::scenic::internal::Flatland::ReleaseLinkCallback callback) override;
  // |fuchsia::ui::scenic::internal::Flatland|
  void ReleaseImage(ContentId image_id) override;

  // Called just before the FIDL client receives the event of the same name, indicating that this
  // Flatland instance should allow an additional |num_present_tokens| calls to Present().
  void OnPresentProcessed(uint32_t num_present_tokens, FuturePresentationInfos presentation_infos);

  // Called when this Flatland instance should send the OnFramePresented() event to the FIDL
  // client.
  void OnFramePresented(const std::map<scheduling::PresentId, zx::time>& latched_times,
                        scheduling::PresentTimestamps present_times);

  // For validating the transform hierarchy in tests only. For the sake of testing, the "root" will
  // always be the top-most TransformHandle from the TransformGraph owned by this Flatland. If
  // currently linked to a parent, that means the link_origin. If not, that means the local_root_.
  TransformHandle GetRoot() const;

  // For validating properties associated with content in tests only. If |content_id| does not
  // exist for this Flatland instance, returns std::nullopt.
  std::optional<TransformHandle> GetContentHandle(ContentId content_id) const;

 private:
  void ReportError();
  void CloseConnection();

  // The dispatcher this Flatland instance is running on.
  async_dispatcher_t* dispatcher_;

  // The FIDL bindings for this Flatland instance, which reference |this| as the implementation and
  // run on |dispatcher_|.
  fidl::Binding<fuchsia::ui::scenic::internal::Flatland> binding_;

  // Users are not allowed to use zero as a TransformId or ContentId.
  static constexpr uint64_t kInvalidId = 0;

  // The unique SessionId for this Flatland instance. Used to schedule Presents and register
  // UberStructs with the UberStructSystem.
  const scheduling::SessionId session_id_;

  // A function that, when called, will destroy this instance. Necessary because an async::Wait can
  // only wait on peer channel destruction, not "this" channel destruction, so the FlatlandManager
  // cannot detect if this instance closes |binding_|.
  std::function<void()> destroy_instance_function_;

  // Waits for the invalidation of the bound channel, then triggers the destruction of this client.
  // Uses WaitOnce since calling the handler will result in the destruction of this object.
  async::WaitOnce peer_closed_waiter_;

  // A Present2Helper to facilitate sendng the appropriate OnFramePresented() callback to FIDL
  // clients when frames are presented to the display.
  scheduling::Present2Helper present2_helper_;

  // A FlatlandPresenter shared between Flatland instances. Flatland uses this interface to get
  // PresentIds when publishing to the UberStructSystem.
  std::shared_ptr<FlatlandPresenter> flatland_presenter_;

  // A link system shared between Flatland instances, so that links can be made between them.
  std::shared_ptr<LinkSystem> link_system_;

  // An UberStructSystem shared between Flatland instances. Flatland publishes local data to the
  // UberStructSystem in order to have it seen by the global render loop.
  std::shared_ptr<UberStructSystem::UberStructQueue> uber_struct_queue_;

  // Used to import Flatland images to external services that Flatland does not have knowledge of.
  // Each importer is used for a different service.
  std::vector<std::shared_ptr<allocation::BufferCollectionImporter>> buffer_collection_importers_;

  // A Sysmem allocator to facilitate buffer allocation with the Renderer.
  fuchsia::sysmem::AllocatorSyncPtr sysmem_allocator_;

  // True if any function has failed since the previous call to Present(), false otherwise.
  bool failure_since_previous_present_ = false;

  // The number of Present() calls remaining before the client runs out. Incremented when
  // OnPresentProcessed() is called, decremented by 1 for each Present() call.
  uint32_t present_tokens_remaining_ = 1;

  // Must be managed by a shared_ptr because the implementation uses weak_from_this().
  std::shared_ptr<escher::FenceQueue> fence_queue_ = std::make_shared<escher::FenceQueue>();

  // A map from user-generated ID to global handle. This map constitutes the set of transforms that
  // can be referenced by the user through method calls. Keep in mind that additional transforms may
  // be kept alive through child references.
  std::unordered_map<uint64_t, TransformHandle> transforms_;

  // A graph representing this flatland instance's local transforms and their relationships.
  TransformGraph transform_graph_;

  // A unique transform for this instance, the local_root_, is part of the transform_graph_,
  // and will never be released or changed during the course of the instance's lifetime. This makes
  // it a fixed attachment point for cross-instance Links.
  const TransformHandle local_root_;

  // A mapping from user-generated ID to the TransformHandle that owns that piece of Content.
  // Attaching Content to a Transform consists of setting one of these "Content Handles" as the
  // priority child of the Transform.
  std::unordered_map<uint64_t, TransformHandle> content_handles_;

  // The set of link operations that are pending a call to Present(). Unlike other operations,
  // whose effects are only visible when a new UberStruct is published, Link destruction operations
  // result in immediate changes in the LinkSystem. To avoid having these changes visible before
  // Present() is called, the actual destruction of Links happens in the following Present().
  std::vector<fit::function<void()>> pending_link_operations_;

  // Wraps a LinkSystem::ChildLink and the properties currently associated with that link.
  struct ChildLinkData {
    LinkSystem::ChildLink link;
    fuchsia::ui::scenic::internal::LinkProperties properties;
    fuchsia::ui::scenic::internal::Vec2 size;
  };

  // Recomputes the scale matrix responsible for fitting a Link's logical size into the actual size
  // designated for it.
  void UpdateLinkScale(const ChildLinkData& link_data);

  // A mapping from Flatland-generated TransformHandle to the ChildLinkData it represents.
  std::unordered_map<TransformHandle, ChildLinkData> child_links_;

  // The link from this Flatland instance to our parent.
  std::optional<LinkSystem::ParentLink> parent_link_;

  // Represents a geometric transformation as three separate components applied in the following
  // order: translation (relative to the parent's coordinate space), orientation (around the new
  // origin as defined by the translation), and scale (relative to the new rotated origin).
  class MatrixData {
   public:
    void SetTranslation(fuchsia::ui::scenic::internal::Vec2 translation);
    void SetOrientation(fuchsia::ui::scenic::internal::Orientation orientation);
    void SetScale(fuchsia::ui::scenic::internal::Vec2 scale);

    // Returns this geometric transformation as a single 3x3 matrix using the order of operations
    // above: translation, orientation, then scale.
    glm::mat3 GetMatrix() const;

    static float GetOrientationAngle(fuchsia::ui::scenic::internal::Orientation orientation);

   private:
    // Applies the translation, then orientation, then scale to the identity matrix.
    void RecomputeMatrix();

    glm::vec2 translation_ = glm::vec2(0.f, 0.f);
    glm::vec2 scale_ = glm::vec2(1.f, 1.f);

    // Counterclockwise rotation angle, in radians.
    float angle_ = 0.f;

    // Recompute and cache the local matrix each time a component is changed to avoid recomputing
    // the matrix for each frame. We expect GetMatrix() to be called far more frequently (roughly
    // once per rendered frame) than the setters are called.
    glm::mat3 matrix_ = glm::mat3(1.f);
  };

  // A geometric transform for each TransformHandle. If not present, that TransformHandle has the
  // identity matrix for its transform.
  std::unordered_map<TransformHandle, MatrixData> matrices_;

  // A map of transform handles to opacity values where the values are strictly in the range
  // [0.f,1.f). 0.f is completely transparent and 1.f, which is completely opaque, is stored
  // implicitly as a transform handle with no entry in this map will default to 1.0.
  std::unordered_map<TransformHandle, float> opacity_values_;

  // A mapping from Flatland-generated TransformHandle to the ImageMetadata it represents.
  std::unordered_map<TransformHandle, allocation::ImageMetadata> image_metadatas_;
};

}  // namespace flatland

#endif  // SRC_UI_SCENIC_LIB_FLATLAND_FLATLAND_H_
