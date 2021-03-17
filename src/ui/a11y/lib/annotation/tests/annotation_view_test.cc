// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/a11y/lib/annotation/annotation_view.h"

#include <fuchsia/ui/annotation/cpp/fidl.h>
#include <fuchsia/ui/scenic/cpp/fidl.h>
#include <fuchsia/ui/scenic/cpp/fidl_test_base.h>
#include <lib/gtest/test_loop_fixture.h>
#include <lib/sys/cpp/testing/component_context_provider.h>
#include <lib/sys/cpp/testing/fake_component.h>

#include <set>
#include <unordered_map>
#include <vector>

#include <gtest/gtest.h>

#include "src/ui/a11y/lib/util/util.h"

namespace a11y {
namespace {

static constexpr fuchsia::ui::gfx::ViewProperties kViewProperties = {
    .bounding_box = {.min = {.x = 10, .y = 5, .z = -100}, .max = {.x = 100, .y = 50, .z = 0}}};

struct ViewAttributes {
  uint32_t id;
  std::set<uint32_t> children;
  bool operator==(const ViewAttributes& rhs) const {
    return this->id == rhs.id && this->children == rhs.children;
  }
};

struct EntityNodeAttributes {
  uint32_t id;
  uint32_t parent_id;
  std::array<float, 3> scale_vector;
  std::array<float, 3> translation_vector;
  std::set<uint32_t> children;
  bool operator==(const EntityNodeAttributes& rhs) const {
    return this->id == rhs.id && this->parent_id == rhs.parent_id &&
           this->scale_vector == rhs.scale_vector &&
           this->translation_vector == rhs.translation_vector && this->children == rhs.children;
  }
};

struct RectangleNodeAttributes {
  uint32_t id;
  uint32_t parent_id;
  uint32_t rectangle_id;
  uint32_t material_id;
  bool operator==(const RectangleNodeAttributes& rhs) const {
    return this->id == rhs.id && this->parent_id == rhs.parent_id &&
           this->rectangle_id == rhs.rectangle_id && this->material_id == rhs.material_id;
  }
};

struct RectangleAttributes {
  uint32_t id;
  uint32_t parent_id;
  float width;
  float height;
  float elevation;
  float center_x;
  float center_y;
  bool operator==(const RectangleAttributes& rhs) const {
    return this->id == rhs.id && this->parent_id == rhs.parent_id && this->width == rhs.width &&
           this->height == rhs.height && this->elevation == rhs.elevation &&
           this->center_x == rhs.center_x && this->center_y == rhs.center_y;
  }
};

class MockAnnotationRegistry : public fuchsia::ui::annotation::Registry {
 public:
  MockAnnotationRegistry() = default;
  ~MockAnnotationRegistry() override = default;

  void CreateAnnotationViewHolder(
      fuchsia::ui::views::ViewRef client_view,
      fuchsia::ui::views::ViewHolderToken view_holder_token,
      fuchsia::ui::annotation::Registry::CreateAnnotationViewHolderCallback callback) override {
    create_annotation_view_holder_called_ = true;
    callback();
  }

  fidl::InterfaceRequestHandler<fuchsia::ui::annotation::Registry> GetHandler(
      async_dispatcher_t* dispatcher = nullptr) {
    return [this, dispatcher](fidl::InterfaceRequest<fuchsia::ui::annotation::Registry> request) {
      bindings_.AddBinding(this, std::move(request), dispatcher);
    };
  }

  bool create_annotation_view_holder_called() { return create_annotation_view_holder_called_; }

 private:
  fidl::BindingSet<fuchsia::ui::annotation::Registry> bindings_;
  bool create_annotation_view_holder_called_;
};

class MockSession : public fuchsia::ui::scenic::testing::Session_TestBase {
 public:
  MockSession() : binding_(this) {}
  ~MockSession() override = default;

  void NotImplemented_(const std::string& name) override {}

  void Enqueue(std::vector<fuchsia::ui::scenic::Command> cmds) override {
    cmd_queue_.insert(cmd_queue_.end(), std::make_move_iterator(cmds.begin()),
                      std::make_move_iterator(cmds.end()));
  }

  void ApplyCreateResourceCommand(const fuchsia::ui::gfx::CreateResourceCmd& command) {
    const uint32_t id = command.id;
    switch (command.resource.Which()) {
      case fuchsia::ui::gfx::ResourceArgs::Tag::kView3:
        views_[id].id = id;
        break;

      case fuchsia::ui::gfx::ResourceArgs::Tag::kEntityNode:
        entity_nodes_[id].id = id;
        break;

      case fuchsia::ui::gfx::ResourceArgs::Tag::kShapeNode:
        rectangle_nodes_[id].id = id;
        break;

      case fuchsia::ui::gfx::ResourceArgs::Tag::kMaterial:
        materials_.emplace(id);
        break;

      case fuchsia::ui::gfx::ResourceArgs::Tag::kRectangle:
        EXPECT_GE(id, 8u);
        rectangles_[id].id = id;
        rectangles_[id].width = command.resource.rectangle().width.vector1();
        rectangles_[id].height = command.resource.rectangle().height.vector1();
        break;

      default:
        break;
    }
  }

  void ApplyAddChildCommand(const fuchsia::ui::gfx::AddChildCmd& command) {
    const uint32_t parent_id = command.node_id;
    const uint32_t child_id = command.child_id;

    // Update parent's children. Only views and entity nodes will have children. Also, resource ids
    // are unique globally across all resource types, so only one of views_ and entity_nodes_ will
    // contain parent_id as a key.
    if (views_.find(parent_id) != views_.end()) {
      views_[parent_id].children.insert(child_id);
    } else if (entity_nodes_.find(parent_id) != entity_nodes_.end()) {
      entity_nodes_[parent_id].children.insert(child_id);
    }

    // Update child's parent. Only entity nodes and shape nodes will have parents.
    if (entity_nodes_.find(child_id) != entity_nodes_.end()) {
      entity_nodes_[child_id].parent_id = parent_id;
    } else if (rectangle_nodes_.find(child_id) != rectangle_nodes_.end()) {
      rectangle_nodes_[child_id].parent_id = parent_id;
    }
  }

  void ApplySetMaterialCommand(const fuchsia::ui::gfx::SetMaterialCmd& command) {
    rectangle_nodes_[command.node_id].material_id = command.material_id;
  }

  void ApplySetShapeCommand(const fuchsia::ui::gfx::SetShapeCmd& command) {
    const uint32_t node_id = command.node_id;
    const uint32_t rectangle_id = command.shape_id;

    rectangle_nodes_[node_id].rectangle_id = rectangle_id;
    rectangles_[rectangle_id].parent_id = node_id;
  }

  void ApplySetTranslationCommand(const fuchsia::ui::gfx::SetTranslationCmd& command) {
    if (command.id == AnnotationView::kFocusHighlightContentNodeId) {
      entity_nodes_[command.id].translation_vector[0] = command.value.value.x;
      entity_nodes_[command.id].translation_vector[1] = command.value.value.y;
      entity_nodes_[command.id].translation_vector[2] = command.value.value.z;
    } else {
      const uint32_t parent_id = command.id;
      const uint32_t rectangle_id = rectangle_nodes_[parent_id].rectangle_id;
      const auto& translation = command.value.value;
      rectangles_[rectangle_id].center_x = translation.x;
      rectangles_[rectangle_id].center_y = translation.y;
      rectangles_[rectangle_id].elevation = translation.z;
    }
  }

  void ApplySetScaleCommand(const fuchsia::ui::gfx::SetScaleCmd& command) {
    if (entity_nodes_.find(command.id) != entity_nodes_.end()) {
      entity_nodes_[command.id].scale_vector[0] = command.value.value.x;
      entity_nodes_[command.id].scale_vector[1] = command.value.value.y;
      entity_nodes_[command.id].scale_vector[2] = command.value.value.z;
    }
  }

  void ApplyDetachCommand(const fuchsia::ui::gfx::DetachCmd& command) {
    const uint32_t id = command.id;

    // The annotation view only ever detaches the content entity node from the view node.
    auto& entity_node = entity_nodes_[id];

    if (entity_node.parent_id != 0) {
      views_[entity_node.parent_id].children.erase(id);
    }

    entity_node.parent_id = 0u;
  }

  void Present(uint64_t presentation_time, ::std::vector<::zx::event> acquire_fences,
               ::std::vector<::zx::event> release_fences, PresentCallback callback) override {
    EXPECT_FALSE(cmd_queue_.empty());

    for (const auto& command : cmd_queue_) {
      if (command.Which() != fuchsia::ui::scenic::Command::Tag::kGfx) {
        continue;
      }

      const auto& gfx_command = command.gfx();

      switch (gfx_command.Which()) {
        case fuchsia::ui::gfx::Command::Tag::kCreateResource:
          ApplyCreateResourceCommand(gfx_command.create_resource());
          break;

        case fuchsia::ui::gfx::Command::Tag::kAddChild:
          ApplyAddChildCommand(gfx_command.add_child());
          break;

        case fuchsia::ui::gfx::Command::Tag::kSetMaterial:
          ApplySetMaterialCommand(gfx_command.set_material());
          break;

        case fuchsia::ui::gfx::Command::Tag::kSetShape:
          ApplySetShapeCommand(gfx_command.set_shape());
          break;

        case fuchsia::ui::gfx::Command::Tag::kSetTranslation:
          ApplySetTranslationCommand(gfx_command.set_translation());
          break;

        case fuchsia::ui::gfx::Command::Tag::kSetScale:
          ApplySetScaleCommand(gfx_command.set_scale());
          break;

        case fuchsia::ui::gfx::Command::Tag::kDetach:
          ApplyDetachCommand(gfx_command.detach());
          break;

        default:
          break;
      }
    }

    callback(fuchsia::images::PresentationInfo());
  }

  void SendGfxEvent(fuchsia::ui::gfx::Event event) {
    fuchsia::ui::scenic::Event scenic_event;
    scenic_event.set_gfx(std::move(event));

    std::vector<fuchsia::ui::scenic::Event> events;
    events.emplace_back(std::move(scenic_event));

    listener_->OnScenicEvent(std::move(events));
  }

  void SendViewPropertiesChangedEvent() {
    fuchsia::ui::gfx::ViewPropertiesChangedEvent view_properties_changed_event = {
        .view_id = 1u,
        .properties = kViewProperties,
    };
    fuchsia::ui::gfx::Event event;
    event.set_view_properties_changed(view_properties_changed_event);

    SendGfxEvent(std::move(event));
  }

  void SendViewDetachedFromSceneEvent() {
    fuchsia::ui::gfx::ViewDetachedFromSceneEvent view_detached_from_scene_event = {.view_id = 1u};
    fuchsia::ui::gfx::Event event;
    event.set_view_detached_from_scene(view_detached_from_scene_event);

    SendGfxEvent(std::move(event));
  }

  void SendViewAttachedToSceneEvent() {
    fuchsia::ui::gfx::ViewAttachedToSceneEvent view_attached_to_scene_event = {.view_id = 1u};
    fuchsia::ui::gfx::Event event;
    event.set_view_attached_to_scene(view_attached_to_scene_event);

    SendGfxEvent(std::move(event));
  }

  void Bind(fidl::InterfaceRequest<::fuchsia::ui::scenic::Session> request,
            ::fuchsia::ui::scenic::SessionListenerPtr listener) {
    binding_.Bind(std::move(request));
    listener_ = std::move(listener);
  }

  const std::set<uint32_t>& materials() { return materials_; }
  const std::unordered_map<uint32_t, ViewAttributes>& views() { return views_; }
  const std::unordered_map<uint32_t, EntityNodeAttributes>& entity_nodes() { return entity_nodes_; }
  const std::unordered_map<uint32_t, RectangleNodeAttributes>& rectangle_nodes() {
    return rectangle_nodes_;
  }
  const std::unordered_map<uint32_t, RectangleAttributes>& rectangles() { return rectangles_; }

 private:
  fidl::Binding<fuchsia::ui::scenic::Session> binding_;
  fuchsia::ui::scenic::SessionListenerPtr listener_;
  std::vector<fuchsia::ui::scenic::Command> cmd_queue_;

  std::set<uint32_t> materials_;
  std::unordered_map<uint32_t, ViewAttributes> views_;
  std::unordered_map<uint32_t, EntityNodeAttributes> entity_nodes_;
  std::unordered_map<uint32_t, RectangleNodeAttributes> rectangle_nodes_;
  std::unordered_map<uint32_t, RectangleAttributes> rectangles_;
};

class FakeScenic : public fuchsia::ui::scenic::testing::Scenic_TestBase {
 public:
  explicit FakeScenic(MockSession* mock_session) : mock_session_(mock_session) {}
  ~FakeScenic() override = default;

  void NotImplemented_(const std::string& name) override {}

  void CreateSession(
      fidl::InterfaceRequest<fuchsia::ui::scenic::Session> session,
      fidl::InterfaceHandle<fuchsia::ui::scenic::SessionListener> listener) override {
    mock_session_->Bind(std::move(session), listener.Bind());
    create_session_called_ = true;
  }

  fidl::InterfaceRequestHandler<fuchsia::ui::scenic::Scenic> GetHandler(
      async_dispatcher_t* dispatcher = nullptr) {
    return [this, dispatcher](fidl::InterfaceRequest<fuchsia::ui::scenic::Scenic> request) {
      bindings_.AddBinding(this, std::move(request), dispatcher);
    };
  }

  bool create_session_called() { return create_session_called_; }

 private:
  fidl::BindingSet<fuchsia::ui::scenic::Scenic> bindings_;
  MockSession* mock_session_;
  bool create_session_called_;
};

class AnnotationViewTest : public gtest::TestLoopFixture {
 public:
  AnnotationViewTest() = default;
  ~AnnotationViewTest() override = default;

  void SetUp() override {
    gtest::TestLoopFixture::SetUp();

    mock_session_ = std::make_unique<MockSession>();
    fake_scenic_ = std::make_unique<FakeScenic>(mock_session_.get());
    mock_annotation_registry_ = std::make_unique<MockAnnotationRegistry>();

    context_provider_.service_directory_provider()->AddService(fake_scenic_->GetHandler());
    context_provider_.service_directory_provider()->AddService(
        mock_annotation_registry_->GetHandler());

    properties_changed_ = false;
    view_attached_ = false;
    view_detached_ = false;

    annotation_view_factory_ = std::make_unique<AnnotationViewFactory>();

    annotation_view_ = annotation_view_factory_->CreateAndInitAnnotationView(
        CreateOrphanViewRef(), context_provider_.context(),
        [this]() { properties_changed_ = true; }, [this]() { view_attached_ = true; },
        [this]() { view_detached_ = true; });

    RunLoopUntilIdle();
  }

  fuchsia::ui::views::ViewRef CreateOrphanViewRef() {
    fuchsia::ui::views::ViewRef view_ref;

    zx::eventpair::create(0u, &view_ref.reference, &eventpair_peer_);
    return view_ref;
  }

  void ExpectView(ViewAttributes expected) {
    const auto& views = mock_session_->views();
    EXPECT_EQ(views.at(expected.id), expected);
  }

  void ExpectMaterial(uint32_t expected) {
    const auto& materials = mock_session_->materials();
    EXPECT_NE(materials.find(expected), materials.end());
  }

  void ExpectEntityNode(EntityNodeAttributes expected) {
    const auto& entity_nodes = mock_session_->entity_nodes();
    EXPECT_EQ(entity_nodes.at(expected.id), expected);
  }

  void ExpectRectangleNode(RectangleNodeAttributes expected) {
    const auto& rectangle_nodes = mock_session_->rectangle_nodes();
    EXPECT_EQ(rectangle_nodes.at(expected.id), expected);
  }

  void ExpectRectangle(RectangleAttributes expected) {
    const auto& rectangles = mock_session_->rectangles();
    EXPECT_EQ(rectangles.at(expected.id), expected);
  }

  void ExpectHighlightEdge(uint32_t id, uint32_t parent_id, float width, float height,
                           float center_x, float center_y, float elevation,
                           uint32_t content_node_id = AnnotationView::kFocusHighlightContentNodeId,
                           uint32_t material_id = AnnotationView::kFocusHighlightMaterialId) {
    // Check properties for rectangle shape.
    RectangleAttributes rectangle;
    rectangle.id = id;
    rectangle.parent_id = parent_id;
    rectangle.width = width;
    rectangle.height = height;
    rectangle.center_x = center_x;
    rectangle.center_y = center_y;
    rectangle.elevation = elevation;
    ExpectRectangle(rectangle);

    // Check that rectangle was set as shape of parent node.
    ExpectRectangleNode({parent_id, content_node_id, id, material_id});
  }

 protected:
  sys::testing::ComponentContextProvider context_provider_;
  std::unique_ptr<MockSession> mock_session_;
  std::unique_ptr<FakeScenic> fake_scenic_;
  std::unique_ptr<MockAnnotationRegistry> mock_annotation_registry_;
  zx::eventpair eventpair_peer_;
  std::unique_ptr<AnnotationViewFactory> annotation_view_factory_;
  std::unique_ptr<AnnotationViewInterface> annotation_view_;
  bool properties_changed_;
  bool view_attached_;
  bool view_detached_;
};

TEST_F(AnnotationViewTest, TestInit) {
  EXPECT_TRUE(mock_annotation_registry_->create_annotation_view_holder_called());

  // Verify that annotation view was created.
  ExpectView({AnnotationView::kAnnotationViewId, {}});

  // Verify that top-level content node (used to attach/detach annotations from view) was created.
  ExpectEntityNode({AnnotationView::kFocusHighlightContentNodeId,
                    0u,
                    {}, /* scale vector */
                    {}, /* translation vector */
                    {AnnotationView::kFocusHighlightLeftEdgeNodeId,
                     AnnotationView::kFocusHighlightRightEdgeNodeId,
                     AnnotationView::kFocusHighlightTopEdgeNodeId,
                     AnnotationView::kFocusHighlightBottomEdgeNodeId}});

  // Verify that drawing material was created.
  ExpectMaterial(AnnotationView::kFocusHighlightMaterialId);

  // Verify that four shape nodes that will hold respective edge rectangles are created and added as
  // children of top-level content node. Also verify material of each.
  ExpectRectangleNode({AnnotationView::kFocusHighlightLeftEdgeNodeId,
                       AnnotationView::kFocusHighlightContentNodeId, 0,
                       AnnotationView::kFocusHighlightMaterialId});
  ExpectRectangleNode({AnnotationView::kFocusHighlightRightEdgeNodeId,
                       AnnotationView::kFocusHighlightContentNodeId, 0,
                       AnnotationView::kFocusHighlightMaterialId});
  ExpectRectangleNode({AnnotationView::kFocusHighlightTopEdgeNodeId,
                       AnnotationView::kFocusHighlightContentNodeId, 0,
                       AnnotationView::kFocusHighlightMaterialId});
  ExpectRectangleNode({AnnotationView::kFocusHighlightBottomEdgeNodeId,
                       AnnotationView::kFocusHighlightContentNodeId, 0,
                       AnnotationView::kFocusHighlightMaterialId});
}

TEST_F(AnnotationViewTest, TestDrawFocusHighlight) {
  fuchsia::ui::gfx::BoundingBox bounding_box = {.min = {.x = 0, .y = 0, .z = 0},
                                                .max = {.x = 1.0, .y = 2.0, .z = 3.0}};

  annotation_view_->DrawHighlight(bounding_box, {1, 1, 1}, {0, 0, 0}, false);

  RunLoopUntilIdle();

  // Verify that all four expected edges are present.
  // Resource IDs 1-7 are used for the resources created in InitializeView(), so the next available
  // id is 8. Since resource ids are generated incrementally, we expect the four edge rectangles to
  // have ids 8-11.

  // Before we set up the parent View bounding box, the z value of default
  // bounding box is 0.
  constexpr float kHighlightElevation = 0.0f;

  ExpectHighlightEdge(
      14u, AnnotationView::kFocusHighlightLeftEdgeNodeId, AnnotationView::kHighlightEdgeThickness,
      bounding_box.max.y + AnnotationView::kHighlightEdgeThickness, bounding_box.min.x,
      (bounding_box.min.y + bounding_box.max.y) / 2, kHighlightElevation);

  ExpectHighlightEdge(
      15u, AnnotationView::kFocusHighlightRightEdgeNodeId, AnnotationView::kHighlightEdgeThickness,
      bounding_box.max.y + AnnotationView::kHighlightEdgeThickness, bounding_box.max.x,
      (bounding_box.min.y + bounding_box.max.y) / 2.f, kHighlightElevation);

  ExpectHighlightEdge(16u, AnnotationView::kFocusHighlightTopEdgeNodeId,
                      bounding_box.max.x + AnnotationView::kHighlightEdgeThickness,
                      AnnotationView::kHighlightEdgeThickness,
                      (bounding_box.min.x + bounding_box.max.x) / 2.f, bounding_box.max.y,
                      kHighlightElevation);

  ExpectHighlightEdge(17u, AnnotationView::kFocusHighlightBottomEdgeNodeId,
                      bounding_box.max.x + AnnotationView::kHighlightEdgeThickness,
                      AnnotationView::kHighlightEdgeThickness,
                      (bounding_box.min.x + bounding_box.max.x) / 2.f, bounding_box.min.y,
                      kHighlightElevation);

  // Verify that top-level content node (used to attach/detach annotations from view) was attached
  // to view.
  ExpectEntityNode({AnnotationView::kFocusHighlightContentNodeId,
                    AnnotationView::kAnnotationViewId,
                    {1, 1, 1}, /* scale vector */
                    {0, 0, 0}, /* translation vector */
                    {AnnotationView::kFocusHighlightLeftEdgeNodeId,
                     AnnotationView::kFocusHighlightRightEdgeNodeId,
                     AnnotationView::kFocusHighlightTopEdgeNodeId,
                     AnnotationView::kFocusHighlightBottomEdgeNodeId}});
}

TEST_F(AnnotationViewTest, TestDrawFocusHighlightAndClearMagnificationHighlight) {
  fuchsia::ui::gfx::BoundingBox bounding_box = {.min = {.x = 0, .y = 0, .z = 0},
                                                .max = {.x = 1.0, .y = 2.0, .z = 3.0}};

  annotation_view_->DrawHighlight(bounding_box, {1, 1, 1}, {0, 0, 0}, false);

  RunLoopUntilIdle();

  // This operation should not affect the focus highlight.
  annotation_view_->ClearMagnificationHighlights();

  RunLoopUntilIdle();

  // Verify that all four expected edges are present.
  // Resource IDs 1-7 are used for the resources created in InitializeView(), so the next available
  // id is 8. Since resource ids are generated incrementally, we expect the four edge rectangles to
  // have ids 8-11.

  // Before we set up the parent View bounding box, the z value of default
  // bounding box is 0.
  constexpr float kHighlightElevation = 0.0f;

  ExpectHighlightEdge(
      14u, AnnotationView::kFocusHighlightLeftEdgeNodeId, AnnotationView::kHighlightEdgeThickness,
      bounding_box.max.y + AnnotationView::kHighlightEdgeThickness, bounding_box.min.x,
      (bounding_box.min.y + bounding_box.max.y) / 2, kHighlightElevation);

  ExpectHighlightEdge(
      15u, AnnotationView::kFocusHighlightRightEdgeNodeId, AnnotationView::kHighlightEdgeThickness,
      bounding_box.max.y + AnnotationView::kHighlightEdgeThickness, bounding_box.max.x,
      (bounding_box.min.y + bounding_box.max.y) / 2.f, kHighlightElevation);

  ExpectHighlightEdge(16u, AnnotationView::kFocusHighlightTopEdgeNodeId,
                      bounding_box.max.x + AnnotationView::kHighlightEdgeThickness,
                      AnnotationView::kHighlightEdgeThickness,
                      (bounding_box.min.x + bounding_box.max.x) / 2.f, bounding_box.max.y,
                      kHighlightElevation);

  ExpectHighlightEdge(17u, AnnotationView::kFocusHighlightBottomEdgeNodeId,
                      bounding_box.max.x + AnnotationView::kHighlightEdgeThickness,
                      AnnotationView::kHighlightEdgeThickness,
                      (bounding_box.min.x + bounding_box.max.x) / 2.f, bounding_box.min.y,
                      kHighlightElevation);

  // Verify that top-level content node (used to attach/detach annotations from view) was attached
  // to view.
  ExpectEntityNode({AnnotationView::kFocusHighlightContentNodeId,
                    AnnotationView::kAnnotationViewId,
                    {1, 1, 1}, /* scale vector */
                    {0, 0, 0}, /* translation vector */
                    {AnnotationView::kFocusHighlightLeftEdgeNodeId,
                     AnnotationView::kFocusHighlightRightEdgeNodeId,
                     AnnotationView::kFocusHighlightTopEdgeNodeId,
                     AnnotationView::kFocusHighlightBottomEdgeNodeId}});
}

TEST_F(AnnotationViewTest, TestDrawMagnificationHighlight) {
  fuchsia::ui::gfx::BoundingBox bounding_box = {.min = {.x = 0, .y = 0, .z = 0},
                                                .max = {.x = 1.0, .y = 2.0, .z = 3.0}};

  annotation_view_->DrawHighlight(bounding_box, {1, 1, 1}, {0, 0, 0}, true);

  RunLoopUntilIdle();

  // Verify that all four expected edges are present.
  // Resource IDs 1-7 are used for the resources created in InitializeView(), so the next available
  // id is 8. Since resource ids are generated incrementally, we expect the four edge rectangles to
  // have ids 8-11.

  // Before we set up the parent View bounding box, the z value of default
  // bounding box is 0.
  constexpr float kHighlightElevation = 0.0f;

  ExpectHighlightEdge(14u, AnnotationView::kMagnificationHighlightLeftEdgeNodeId,
                      AnnotationView::kHighlightEdgeThickness,
                      bounding_box.max.y + AnnotationView::kHighlightEdgeThickness,
                      bounding_box.min.x, (bounding_box.min.y + bounding_box.max.y) / 2,
                      kHighlightElevation, AnnotationView::kMagnificationHighlightContentNodeId,
                      AnnotationView::kMagnificationHighlightMaterialId);

  ExpectHighlightEdge(15u, AnnotationView::kMagnificationHighlightRightEdgeNodeId,
                      AnnotationView::kHighlightEdgeThickness,
                      bounding_box.max.y + AnnotationView::kHighlightEdgeThickness,
                      bounding_box.max.x, (bounding_box.min.y + bounding_box.max.y) / 2.f,
                      kHighlightElevation, AnnotationView::kMagnificationHighlightContentNodeId,
                      AnnotationView::kMagnificationHighlightMaterialId);

  ExpectHighlightEdge(16u, AnnotationView::kMagnificationHighlightTopEdgeNodeId,
                      bounding_box.max.x + AnnotationView::kHighlightEdgeThickness,
                      AnnotationView::kHighlightEdgeThickness,
                      (bounding_box.min.x + bounding_box.max.x) / 2.f, bounding_box.max.y,
                      kHighlightElevation, AnnotationView::kMagnificationHighlightContentNodeId,
                      AnnotationView::kMagnificationHighlightMaterialId);

  ExpectHighlightEdge(17u, AnnotationView::kMagnificationHighlightBottomEdgeNodeId,
                      bounding_box.max.x + AnnotationView::kHighlightEdgeThickness,
                      AnnotationView::kHighlightEdgeThickness,
                      (bounding_box.min.x + bounding_box.max.x) / 2.f, bounding_box.min.y,
                      kHighlightElevation, AnnotationView::kMagnificationHighlightContentNodeId,
                      AnnotationView::kMagnificationHighlightMaterialId);

  // Verify that top-level content node (used to attach/detach annotations from view) was attached
  // to view.
  ExpectEntityNode({AnnotationView::kMagnificationHighlightContentNodeId,
                    AnnotationView::kAnnotationViewId,
                    {1, 1, 1}, /* scale vector */
                    {0, 0, 0}, /* translation vector */
                    {AnnotationView::kMagnificationHighlightLeftEdgeNodeId,
                     AnnotationView::kMagnificationHighlightRightEdgeNodeId,
                     AnnotationView::kMagnificationHighlightTopEdgeNodeId,
                     AnnotationView::kMagnificationHighlightBottomEdgeNodeId}});
}

TEST_F(AnnotationViewTest, TestDrawMagnificationHighlightAndClearFocusHighlight) {
  fuchsia::ui::gfx::BoundingBox bounding_box = {.min = {.x = 0, .y = 0, .z = 0},
                                                .max = {.x = 1.0, .y = 2.0, .z = 3.0}};

  annotation_view_->DrawHighlight(bounding_box, {1, 1, 1}, {0, 0, 0}, true);

  RunLoopUntilIdle();

  // Attempt to clear focus highlight. This operation should not affect the
  // magnification highlight.
  annotation_view_->ClearFocusHighlights();

  RunLoopUntilIdle();

  // Verify that all four expected edges are present.
  // Resource IDs 1-7 are used for the resources created in InitializeView(), so the next available
  // id is 8. Since resource ids are generated incrementally, we expect the four edge rectangles to
  // have ids 8-11.

  // Before we set up the parent View bounding box, the z value of default
  // bounding box is 0.
  constexpr float kHighlightElevation = 0.0f;

  ExpectHighlightEdge(14u, AnnotationView::kMagnificationHighlightLeftEdgeNodeId,
                      AnnotationView::kHighlightEdgeThickness,
                      bounding_box.max.y + AnnotationView::kHighlightEdgeThickness,
                      bounding_box.min.x, (bounding_box.min.y + bounding_box.max.y) / 2,
                      kHighlightElevation, AnnotationView::kMagnificationHighlightContentNodeId,
                      AnnotationView::kMagnificationHighlightMaterialId);

  ExpectHighlightEdge(15u, AnnotationView::kMagnificationHighlightRightEdgeNodeId,
                      AnnotationView::kHighlightEdgeThickness,
                      bounding_box.max.y + AnnotationView::kHighlightEdgeThickness,
                      bounding_box.max.x, (bounding_box.min.y + bounding_box.max.y) / 2.f,
                      kHighlightElevation, AnnotationView::kMagnificationHighlightContentNodeId,
                      AnnotationView::kMagnificationHighlightMaterialId);

  ExpectHighlightEdge(16u, AnnotationView::kMagnificationHighlightTopEdgeNodeId,
                      bounding_box.max.x + AnnotationView::kHighlightEdgeThickness,
                      AnnotationView::kHighlightEdgeThickness,
                      (bounding_box.min.x + bounding_box.max.x) / 2.f, bounding_box.max.y,
                      kHighlightElevation, AnnotationView::kMagnificationHighlightContentNodeId,
                      AnnotationView::kMagnificationHighlightMaterialId);

  ExpectHighlightEdge(17u, AnnotationView::kMagnificationHighlightBottomEdgeNodeId,
                      bounding_box.max.x + AnnotationView::kHighlightEdgeThickness,
                      AnnotationView::kHighlightEdgeThickness,
                      (bounding_box.min.x + bounding_box.max.x) / 2.f, bounding_box.min.y,
                      kHighlightElevation, AnnotationView::kMagnificationHighlightContentNodeId,
                      AnnotationView::kMagnificationHighlightMaterialId);

  // Verify that top-level content node (used to attach/detach annotations from view) was attached
  // to view.
  ExpectEntityNode({AnnotationView::kMagnificationHighlightContentNodeId,
                    AnnotationView::kAnnotationViewId,
                    {1, 1, 1}, /* scale vector */
                    {0, 0, 0}, /* translation vector */
                    {AnnotationView::kMagnificationHighlightLeftEdgeNodeId,
                     AnnotationView::kMagnificationHighlightRightEdgeNodeId,
                     AnnotationView::kMagnificationHighlightTopEdgeNodeId,
                     AnnotationView::kMagnificationHighlightBottomEdgeNodeId}});
}

TEST_F(AnnotationViewTest, TestClearFocusHighlights) {
  fuchsia::ui::gfx::BoundingBox bounding_box = {.min = {.x = 0, .y = 0, .z = 0},
                                                .max = {.x = 1.0, .y = 2.0, .z = 3.0}};

  annotation_view_->DrawHighlight(bounding_box, {1, 1, 1}, {0, 0, 0}, false);

  RunLoopUntilIdle();

  // Verify that top-level content node (used to attach/detach annotations from view) was attached
  // to view.
  ExpectEntityNode({AnnotationView::kFocusHighlightContentNodeId,
                    AnnotationView::kAnnotationViewId,
                    {1, 1, 1}, /* scale vector */
                    {0, 0, 0}, /* translation vector */
                    {AnnotationView::kFocusHighlightLeftEdgeNodeId,
                     AnnotationView::kFocusHighlightRightEdgeNodeId,
                     AnnotationView::kFocusHighlightTopEdgeNodeId,
                     AnnotationView::kFocusHighlightBottomEdgeNodeId}});

  annotation_view_->ClearFocusHighlights();

  RunLoopUntilIdle();

  // Verify that top-level content node (used to attach/detach annotations from view) was detached
  // from view.
  ExpectEntityNode({AnnotationView::kFocusHighlightContentNodeId,
                    0u,
                    {1, 1, 1}, /* scale vector */
                    {0, 0, 0}, /* translation vector */
                    {AnnotationView::kFocusHighlightLeftEdgeNodeId,
                     AnnotationView::kFocusHighlightRightEdgeNodeId,
                     AnnotationView::kFocusHighlightTopEdgeNodeId,
                     AnnotationView::kFocusHighlightBottomEdgeNodeId}});
}

TEST_F(AnnotationViewTest, TestClearMagnificationHighlights) {
  fuchsia::ui::gfx::BoundingBox bounding_box = {.min = {.x = 0, .y = 0, .z = 0},
                                                .max = {.x = 1.0, .y = 2.0, .z = 3.0}};

  annotation_view_->DrawHighlight(bounding_box, {1, 1, 1}, {0, 0, 0}, true);

  RunLoopUntilIdle();

  // Verify that top-level content node (used to attach/detach annotations from view) was attached
  // to view.
  ExpectEntityNode({AnnotationView::kMagnificationHighlightContentNodeId,
                    AnnotationView::kAnnotationViewId,
                    {1, 1, 1}, /* scale vector */
                    {0, 0, 0}, /* translation vector */
                    {AnnotationView::kMagnificationHighlightLeftEdgeNodeId,
                     AnnotationView::kMagnificationHighlightRightEdgeNodeId,
                     AnnotationView::kMagnificationHighlightTopEdgeNodeId,
                     AnnotationView::kMagnificationHighlightBottomEdgeNodeId}});

  annotation_view_->ClearMagnificationHighlights();

  RunLoopUntilIdle();

  // Verify that top-level content node (used to attach/detach annotations from view) was detached
  // from view.
  ExpectEntityNode({AnnotationView::kMagnificationHighlightContentNodeId,
                    0u,
                    {1, 1, 1}, /* scale vector */
                    {0, 0, 0}, /* translation vector */
                    {AnnotationView::kMagnificationHighlightLeftEdgeNodeId,
                     AnnotationView::kMagnificationHighlightRightEdgeNodeId,
                     AnnotationView::kMagnificationHighlightTopEdgeNodeId,
                     AnnotationView::kMagnificationHighlightBottomEdgeNodeId}});
}

TEST_F(AnnotationViewTest, TestViewPropertiesChangedEvent) {
  fuchsia::ui::gfx::BoundingBox bounding_box = {.min = {.x = 0, .y = 0, .z = 0},
                                                .max = {.x = 1.0, .y = 2.0, .z = 3.0}};

  annotation_view_->DrawHighlight(bounding_box, {1, 1, 1}, {0, 0, 0}, false);

  RunLoopUntilIdle();

  // Update test node bounding box to reflect change in view properties.
  bounding_box = {.min = {.x = 0, .y = 0, .z = 0}, .max = {.x = 2.0, .y = 4.0, .z = 6.0}};

  mock_session_->SendViewPropertiesChangedEvent();
  RunLoopUntilIdle();

  EXPECT_TRUE(properties_changed_);
}

TEST_F(AnnotationViewTest, TestViewPropertiesChangedElevation) {
  mock_session_->SendViewPropertiesChangedEvent();
  RunLoopUntilIdle();

  fuchsia::ui::gfx::BoundingBox bounding_box = {.min = {.x = 0, .y = 0, .z = 0},
                                                .max = {.x = 1.0, .y = 2.0, .z = 3.0}};
  annotation_view_->DrawHighlight(bounding_box, {1, 1, 1}, {0, 0, 0}, false);
  RunLoopUntilIdle();

  // Same as the value defined in annotation_view.cc.
  const float kEpsilon = 0.950f;
  const float kExpectedElevation = kViewProperties.bounding_box.min.z * kEpsilon;

  const auto& rectangles = mock_session_->rectangles();
  EXPECT_FLOAT_EQ(rectangles.at(14u).elevation, kExpectedElevation);
  EXPECT_FLOAT_EQ(rectangles.at(15u).elevation, kExpectedElevation);
  EXPECT_FLOAT_EQ(rectangles.at(16u).elevation, kExpectedElevation);
  EXPECT_FLOAT_EQ(rectangles.at(17u).elevation, kExpectedElevation);

  EXPECT_TRUE(properties_changed_);
}

TEST_F(AnnotationViewTest, TestViewDetachAndReattachEvents) {
  fuchsia::ui::gfx::BoundingBox bounding_box = {.min = {.x = 0, .y = 0, .z = 0},
                                                .max = {.x = 1.0, .y = 2.0, .z = 3.0}};
  annotation_view_->DrawHighlight(bounding_box, {1, 1, 1}, {0, 0, 0}, false);

  // ViewAttachedToSceneEvent() should have no effect before any highlights are drawn.
  mock_session_->SendViewDetachedFromSceneEvent();
  RunLoopUntilIdle();

  EXPECT_TRUE(view_detached_);

  mock_session_->SendViewAttachedToSceneEvent();
  RunLoopUntilIdle();

  EXPECT_TRUE(view_attached_);
}

}  // namespace
}  // namespace a11y
