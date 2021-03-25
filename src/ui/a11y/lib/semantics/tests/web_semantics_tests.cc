// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/syslog/cpp/macros.h>

#include <gtest/gtest.h>

#include "src/chromium/web_runner_tests/mock_get.h"
#include "src/chromium/web_runner_tests/test_server.h"
#include "src/lib/fxl/strings/string_printf.h"
#include "src/lib/ui/base_view/embedded_view_utils.h"
#include "src/ui/a11y/lib/semantics/tests/semantics_integration_test_fixture.h"
#include "src/ui/testing/views/embedder_view.h"

namespace accessibility_test {
namespace {

constexpr zx::duration kTimeout = zx::sec(60);

class WebSemanticsTest : public SemanticsIntegrationTest {
 public:
  WebSemanticsTest() : SemanticsIntegrationTest("web_semantics_test"), view_ref_koid_(0) {}

  // |SemanticsIntegrationTest|
  void CreateServices(std::unique_ptr<sys::testing::EnvironmentServices>& services) override {
    // Additional services to inject into the test environment that we want to re-create for each
    // test case
    constexpr size_t kNumInjectedServices = 2;
    constexpr std::array<std::pair<const char*, const char*>, kNumInjectedServices>
        kInjectedServices = {{
            // clang-format off
            {
              "fuchsia.memorypressure.Provider",
              "fuchsia-pkg://fuchsia.com/memory_monitor#meta/memory_monitor.cmx"
            }, {
              "fuchsia.web.ContextProvider",
              "fuchsia-pkg://fuchsia.com/web_engine#meta/context_provider.cmx"
            },
            // clang-format on
        }};
    // Add test-specific services.
    for (const auto& service_info : kInjectedServices) {
      zx_status_t status =
          services->AddServiceWithLaunchInfo({.url = service_info.second}, service_info.first);
      ASSERT_EQ(status, ZX_OK) << service_info.first;
    }

    services->AllowParentService("fuchsia.netstack.Netstack");
    services->AllowParentService("fuchsia.net.interfaces.State");
    services->AllowParentService("fuchsia.sysmem.Allocator");
    services->AllowParentService("fuchsia.vulkan.loader.Loader");
  }

  // Render the given page.  Can only be called once per test case.
  void RenderPage(std::string page) {
    ASSERT_FALSE(embedder_view_.has_value());

    web_runner_tests::TestServer server;
    FX_CHECK(server.FindAndBindPort());

    auto serve = server.ServeAsync([&server, page] {
      while (server.Accept()) {
        web_runner_tests::MockHttpGetResponse(&server, page.c_str());
      }
    });

    view_manager()->SetSemanticsEnabled(true);

    scenic::EmbeddedViewInfo web_runner = scenic::LaunchComponentAndCreateView(
        environment()->launcher_ptr(),
        fxl::StringPrintf("http://localhost:%d/%s", server.port(), page.c_str()), {});

    web_runner.controller.events().OnTerminated = [](auto...) { FAIL(); };

    view_ref_koid_ = fsl::GetKoid(web_runner.view_ref.reference.get());

    // Present the view.
    embedder_view_.emplace(scenic::ViewContext{
        .session_and_listener_request = scenic::CreateScenicSessionPtrAndListenerRequest(scenic()),
        .view_token = CreatePresentationViewToken(),
    });

    // Embed the view.
    bool is_rendering = false;
    embedder_view_->EmbedView(std::move(web_runner),
                              [&is_rendering](fuchsia::ui::gfx::ViewState view_state) {
                                is_rendering = view_state.is_rendering;
                              });
    ASSERT_TRUE(RunLoopWithTimeoutOrUntil([&is_rendering] { return is_rendering; }, kTimeout));

    EXPECT_TRUE(RunLoopWithTimeoutOrUntil(
        [this] {
          auto node = view_manager()->GetSemanticNode(view_ref_koid_, 0u);
          return node != nullptr && node->has_attributes() && node->attributes().has_label();
        },
        kTimeout))
        << "No root node found.";
  }

  zx_koid_t view_ref_koid() const { return view_ref_koid_; }

 private:
  // Wrapped in optional since the view is not created until the middle of SetUp
  std::optional<scenic::EmbedderView> embedder_view_;

  zx_koid_t view_ref_koid_;
};

// Loads a static page via the component framework and verifies its semantic tree.
TEST_F(WebSemanticsTest, StaticSemantics) {
  ASSERT_NO_FATAL_FAILURE(RenderPage("static.html"));

  /* The semantic tree for static.html:
   * ID: 0 Label:Say something. Anything.
   *     ID: 5 Label:no label
   *         ID: 7 Label:Test 1 2 3...
   *             ID: 13 Label:Test 1 2 3...
   *         ID: 11 Label:Click here
   *             ID: 14 Label:Click here
   *                 ID: 15 Label:Click here
   */
  auto root = view_manager()->GetSemanticNode(view_ref_koid(), 0u);
  auto node = FindNodeWithLabel(root, view_ref_koid(), "Say something. Anything.");
  ASSERT_TRUE(node);

  node = FindNodeWithLabel(root, view_ref_koid(), "Test 1 2 3... ");
  ASSERT_TRUE(node);

  node = FindNodeWithLabel(root, view_ref_koid(), "Click here");
  ASSERT_TRUE(node);
}

// BUG(fxb.dev/60002): Disable this test until the flakes are resolved.
TEST_F(WebSemanticsTest, DISABLED_HitTesting) {
  ASSERT_NO_FATAL_FAILURE(RenderPage("static.html"));
  auto root = view_manager()->GetSemanticNode(view_ref_koid(), 0u);

  // When performing hit tests, aim for just inside the node's bounding box.  Note
  // that for nodes from Chrome, the min corner has a larger y value than the max.
  fuchsia::math::PointF offset = {1., -1.};

  // Hit test the plain text
  auto node = FindNodeWithLabel(root, view_ref_koid(), "Test 1 2 3... ");
  ASSERT_TRUE(node);
  auto hit_node = HitTest(view_ref_koid(), CalculateViewTargetPoint(view_ref_koid(), node, offset));
  ASSERT_TRUE(hit_node.has_value());
  ASSERT_EQ(*hit_node, node->node_id());

  // Hit test the button
  node = FindNodeWithLabel(root, view_ref_koid(), "Click here");
  ASSERT_TRUE(node);
  hit_node = HitTest(view_ref_koid(), CalculateViewTargetPoint(view_ref_koid(), node, offset));
  ASSERT_TRUE(hit_node.has_value());
  ASSERT_EQ(*hit_node, node->node_id());
}

TEST_F(WebSemanticsTest, PerformAction) {
  ASSERT_NO_FATAL_FAILURE(RenderPage("dynamic_button.html"));

  auto root = view_manager()->GetSemanticNode(view_ref_koid(), 0u);

  // Find the node with the counter to make sure it still reads 0
  auto node = FindNodeWithLabel(root, view_ref_koid(), "0");
  EXPECT_TRUE(node);
  // There shouldn't be a node labeled 1 yet
  node = FindNodeWithLabel(root, view_ref_koid(), "1");
  EXPECT_FALSE(node);

  // Trigger the button's default action
  node = FindNodeWithLabel(root, view_ref_koid(), "Increment");
  ASSERT_TRUE(node);
  EXPECT_TRUE(node->has_role() && node->role() == fuchsia::accessibility::semantics::Role::BUTTON);
  bool callback_handled = PerformAccessibilityAction(
      view_ref_koid(), node->node_id(), fuchsia::accessibility::semantics::Action::DEFAULT);
  EXPECT_TRUE(callback_handled);

  // Find the node with the counter to make sure it now reads 1
  // TODO(fxb.dev/58276): Once we have the Semantic Event Updates work done, this logic can be
  // more clearly written as waiting for notification of an update then checking the tree.
  EXPECT_TRUE(RunLoopWithTimeoutOrUntil(
      [this, root] {
        auto node = FindNodeWithLabel(root, view_ref_koid(), "1");
        return node != nullptr;
      },
      kTimeout));
}

// BUG(fxb.dev/60002): Disable this test until the flakes are resolved.
TEST_F(WebSemanticsTest, DISABLED_ScrollToMakeVisible) {
  ASSERT_NO_FATAL_FAILURE(RenderPage("big_list.html"));

  auto root = view_manager()->GetSemanticNode(view_ref_koid(), 0u);

  // The "Entry 999" node should be off-screen
  auto node = FindNodeWithLabel(root, view_ref_koid(), "Entry 999");
  ASSERT_TRUE(node);

  // Record the location of a corner of the node's bounding box.  We record this rather than the
  // transform or the location fields since the runtime could change either when an element is
  // moved.
  auto node_corner =
      GetTransformForNode(view_ref_koid(), node->node_id()).Apply(node->location().min);

  bool callback_handled = PerformAccessibilityAction(
      view_ref_koid(), node->node_id(), fuchsia::accessibility::semantics::Action::SHOW_ON_SCREEN);
  EXPECT_TRUE(callback_handled);

  // Verify the "Entry 999" node has moved.  Note that this does not verify that it's now on screen,
  // since the semantics API does not encode enough information to be able to answer that
  // definitively.
  // TODO(fxb.dev/58276): Once we have the Semantic Event Updates work done, this logic can be
  // more clearly written as waiting for notification of an update then checking the tree.
  EXPECT_TRUE(RunLoopWithTimeoutOrUntil(
      [this, root, &node_corner] {
        auto node = FindNodeWithLabel(root, view_ref_koid(), "Entry 999");
        if (node == nullptr) {
          return false;
        }

        auto new_node_corner =
            GetTransformForNode(view_ref_koid(), node->node_id()).Apply(node->location().min);
        return node_corner.x != new_node_corner.x || node_corner.y != new_node_corner.y ||
               node_corner.z != new_node_corner.z;
      },
      kTimeout));
}

}  // namespace
}  // namespace accessibility_test
