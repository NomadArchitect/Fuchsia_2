// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async-loop/testing/cpp/real_loop.h>
#include <lib/sys/component/cpp/testing/realm_builder.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/ui/scenic/cpp/resources.h>
#include <lib/ui/scenic/cpp/session.h>
#include <lib/ui/scenic/cpp/view_ref_pair.h>
#include <lib/ui/scenic/cpp/view_token_pair.h>
#include <zircon/status.h>

#include <map>
#include <string>
#include <vector>

#include <zxtest/zxtest.h>

#include "src/ui/scenic/tests/utils/scenic_realm_builder.h"
#include "src/ui/scenic/tests/utils/utils.h"

// These tests exercise InputSystem logic during startup, e.g. potential race conditions.

namespace integration_tests {

using RealmRoot = component_testing::RealmRoot;

scenic::Session CreateSession(fuchsia::ui::scenic::Scenic* scenic,
                              fuchsia::ui::scenic::SessionEndpoints endpoints) {
  FX_DCHECK(scenic);
  FX_DCHECK(!endpoints.has_session());
  FX_DCHECK(!endpoints.has_session_listener());

  fuchsia::ui::scenic::SessionPtr session_ptr;
  fuchsia::ui::scenic::SessionListenerHandle listener_handle;
  auto listener_request = listener_handle.NewRequest();

  endpoints.set_session(session_ptr.NewRequest());
  endpoints.set_session_listener(std::move(listener_handle));
  scenic->CreateSessionT(std::move(endpoints), [] {});

  return scenic::Session(std::move(session_ptr), std::move(listener_request));
}

// Test fixture that sets up an environment with a Scenic we can connect to.
class GfxStartupInputTest : public zxtest::Test, public loop_fixture::RealLoop {
 protected:
  fuchsia::ui::scenic::Scenic* scenic() { return scenic_.get(); }

  void SetUp() override {
    // Build the realm topology and route the protocols required by this test fixture from the
    // scenic subrealm.
    realm_ = std::make_unique<RealmRoot>(
        ScenicRealmBuilder().AddRealmProtocol(fuchsia::ui::scenic::Scenic::Name_).Build());

    scenic_ = realm_->Connect<fuchsia::ui::scenic::Scenic>();
    scenic_.set_error_handler([](zx_status_t status) {
      FAIL("Lost connection to Scenic: %s", zx_status_get_string(status));
    });
  }

  void BlockingPresent(scenic::Session& session) {
    bool presented = false;
    session.set_on_frame_presented_handler([&presented](auto) { presented = true; });
    session.Present2(0, 0, [](auto) {});
    RunLoopUntil([&presented] { return presented; });
    session.set_on_frame_presented_handler([](auto) {});
  }

  // Injects an arbitrary input event using the legacy injection API.
  // Uses a new pointer on each injection to minimize conteracting between different injections.
  void InjectFreshEvent(scenic::Session& session, uint32_t compositor_id) {
    PointerCommandGenerator pointer(compositor_id, /*device id*/ 1,
                                    /*pointer id*/ ++last_pointer_id_,
                                    fuchsia::ui::input::PointerEventType::TOUCH);
    session.Enqueue(pointer.Add(2.5, 2.5));
    BlockingPresent(session);
  }

 private:
  fuchsia::ui::scenic::ScenicPtr scenic_;
  std::unique_ptr<RealmRoot> realm_;

  uint32_t last_pointer_id_ = 0;
};

// This test builds up a scene piece by piece, injecting input at every point to confirm
// that there is no crash.
TEST_F(GfxStartupInputTest, LegacyInjectBeforeSceneSetupComplete_ShouldNotCrash) {
  constexpr uint32_t kFakeCompositorId = 321241;
  scenic::Session session = CreateSession(scenic(), {});
  std::vector<fuchsia::ui::input::InputEvent> received_input_events;
  session.set_event_handler([&received_input_events](auto events) {
    for (auto& event : events) {
      if (event.is_input() && !event.input().is_focus())
        received_input_events.emplace_back(std::move(event.input()));
    }
  });

  // Set up a view to receive input in.
  auto [v, vh] = scenic::ViewTokenPair::New();
  scenic::ViewHolder holder(&session, std::move(vh), "holder");
  holder.SetViewProperties({.bounding_box = {.max = {5, 5, 1}}});
  scenic::View view(&session, std::move(v), "view");
  scenic::ShapeNode shape(&session);
  scenic::Rectangle rec(&session, 5, 5);
  shape.SetShape(rec);
  shape.SetTranslation(2.5f, 2.5f, 0);  // Center the shape within the View.
  view.AddChild(shape);

  // Empty.
  BlockingPresent(session);
  InjectFreshEvent(session, kFakeCompositorId);
  EXPECT_TRUE(received_input_events.empty());

  // Only a Scene object.
  scenic::Scene scene(&session);
  BlockingPresent(session);
  InjectFreshEvent(session, kFakeCompositorId);
  EXPECT_TRUE(received_input_events.empty());

  // Attach the view to the scene now that we have a scene.
  scene.AddChild(holder);

  scenic::Camera camera(scene);
  BlockingPresent(session);
  InjectFreshEvent(session, kFakeCompositorId);
  EXPECT_TRUE(received_input_events.empty());

  scenic::Renderer renderer(&session);
  BlockingPresent(session);
  InjectFreshEvent(session, kFakeCompositorId);
  EXPECT_TRUE(received_input_events.empty());

  renderer.SetCamera(camera);
  BlockingPresent(session);
  InjectFreshEvent(session, kFakeCompositorId);
  EXPECT_TRUE(received_input_events.empty());

  scenic::Compositor compositor(&session);
  BlockingPresent(session);
  const uint32_t compositor_id = compositor.id();
  InjectFreshEvent(session, kFakeCompositorId);  // With fake compositor id.
  InjectFreshEvent(session, compositor_id);      // With real compositor id.

  scenic::LayerStack layer_stack(&session);
  BlockingPresent(session);
  InjectFreshEvent(session, compositor_id);
  EXPECT_TRUE(received_input_events.empty());

  compositor.SetLayerStack(layer_stack);
  BlockingPresent(session);
  InjectFreshEvent(session, compositor_id);
  EXPECT_TRUE(received_input_events.empty());

  scenic::Layer layer(&session);
  BlockingPresent(session);
  InjectFreshEvent(session, compositor_id);
  EXPECT_TRUE(received_input_events.empty());

  layer_stack.AddLayer(layer);
  BlockingPresent(session);
  InjectFreshEvent(session, compositor_id);
  EXPECT_TRUE(received_input_events.empty());

  layer.SetRenderer(renderer);
  BlockingPresent(session);
  InjectFreshEvent(session, compositor_id);
  EXPECT_TRUE(received_input_events.empty());

  layer.SetSize(10, 10);
  BlockingPresent(session);
  InjectFreshEvent(session, compositor_id);

  // Should now have received the final event.
  EXPECT_FALSE(received_input_events.empty());
}

}  // namespace integration_tests
