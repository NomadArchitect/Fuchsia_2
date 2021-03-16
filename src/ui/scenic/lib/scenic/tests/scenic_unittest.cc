// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <gtest/gtest.h>

#include "src/ui/scenic/lib/gfx/tests/mocks/mocks.h"
#include "src/ui/scenic/lib/scenic/take_screenshot_delegate_deprecated.h"
#include "src/ui/scenic/lib/scenic/tests/dummy_system.h"
#include "src/ui/scenic/lib/scenic/tests/scenic_test.h"
#include "src/ui/scenic/lib/scheduling/tests/mocks/frame_scheduler_mocks.h"

namespace {

class DisplayInfoDelegate : public scenic_impl::Scenic::GetDisplayInfoDelegateDeprecated {
 public:
  void GetDisplayInfo(fuchsia::ui::scenic::Scenic::GetDisplayInfoCallback callback) override {
    auto info = fuchsia::ui::gfx::DisplayInfo();
    callback(std::move(info));
  };

  void GetDisplayOwnershipEvent(
      fuchsia::ui::scenic::Scenic::GetDisplayOwnershipEventCallback callback) override {
    zx::event event;
    callback(std::move(event));
  };
};

class TakeScreenshotDelegate : public scenic_impl::TakeScreenshotDelegateDeprecated {
 public:
  void TakeScreenshot(fuchsia::ui::scenic::Scenic::TakeScreenshotCallback callback) override {
    fuchsia::ui::scenic::ScreenshotData data;
    callback(std::move(data), true);
  }
};

}  // namespace

namespace scenic_impl {
namespace test {

TEST_F(ScenicTest, CreateAndDestroySession) {
  auto mock_system = scenic()->RegisterSystem<DummySystem>();
  const auto frame_scheduler = std::make_shared<scheduling::test::MockFrameScheduler>();
  scenic()->SetFrameScheduler(frame_scheduler);
  scenic()->SetInitialized();
  EXPECT_EQ(scenic()->num_sessions(), 0U);

  auto session = CreateSession();
  EXPECT_EQ(scenic()->num_sessions(), 1U);
  EXPECT_EQ(mock_system->GetNumDispatchers(), 1U);
  EXPECT_NE(mock_system->GetLastSessionId(), -1);

  // Closing the session should cause another update to be scheduled.
  bool update_scheduled = false;
  frame_scheduler->set_schedule_update_for_session_callback(
      [&update_scheduled](auto...) { update_scheduled = true; });
  scenic()->CloseSession(mock_system->GetLastSessionId());
  EXPECT_EQ(scenic()->num_sessions(), 0U);
  EXPECT_TRUE(update_scheduled);
}

TEST_F(ScenicTest, CreateAndDestroyMultipleSessions) {
  auto mock_system = scenic()->RegisterSystem<DummySystem>();
  scenic()->SetInitialized();
  EXPECT_EQ(scenic()->num_sessions(), 0U);

  auto session1 = CreateSession();
  EXPECT_EQ(scenic()->num_sessions(), 1U);
  EXPECT_EQ(mock_system->GetNumDispatchers(), 1U);
  auto session1_id = mock_system->GetLastSessionId();
  EXPECT_NE(session1_id, -1);

  auto session2 = CreateSession();
  EXPECT_EQ(scenic()->num_sessions(), 2U);
  EXPECT_EQ(mock_system->GetNumDispatchers(), 2U);
  auto session2_id = mock_system->GetLastSessionId();
  EXPECT_NE(session2_id, -1);

  auto session3 = CreateSession();
  EXPECT_EQ(scenic()->num_sessions(), 3U);
  EXPECT_EQ(mock_system->GetNumDispatchers(), 3U);
  auto session3_id = mock_system->GetLastSessionId();
  EXPECT_TRUE(session3_id);

  scenic()->CloseSession(session2_id);
  EXPECT_EQ(scenic()->num_sessions(), 2U);

  scenic()->CloseSession(session3_id);
  EXPECT_EQ(scenic()->num_sessions(), 1U);

  scenic()->CloseSession(session1_id);
  EXPECT_EQ(scenic()->num_sessions(), 0U);
}

TEST_F(ScenicTest, SessionCreatedAfterInitialization) {
  EXPECT_EQ(scenic()->num_sessions(), 0U);

  // Request session creation, which doesn't occur yet because system isn't
  // initialized.
  auto session = CreateSession();
  EXPECT_EQ(scenic()->num_sessions(), 0U);

  // Initializing Scenic allows the session to be created.
  scenic()->SetInitialized();
  EXPECT_EQ(scenic()->num_sessions(), 1U);
}

TEST_F(ScenicTest, InvalidPresentCall_ShouldDestroySession) {
  scenic()->SetInitialized();
  EXPECT_EQ(scenic()->num_sessions(), 0U);
  auto session = CreateSession();
  EXPECT_EQ(scenic()->num_sessions(), 1U);

  session->Present(/*Presentation Time*/ 10,
                   /*Present Callback*/ [](auto) {});

  // Trigger error by making a present call with an earlier presentation time
  // than the previous call to present
  session->Present(/*Presentation Time*/ 0,
                   /*Present Callback*/ [](auto) {});

  RunLoopUntilIdle();

  EXPECT_EQ(scenic()->num_sessions(), 0U);
}

TEST_F(ScenicTest, InvalidPresent2Call_ShouldDestroySession) {
  scenic()->SetInitialized();
  EXPECT_EQ(scenic()->num_sessions(), 0U);
  auto session = CreateSession();
  EXPECT_EQ(scenic()->num_sessions(), 1U);

  session->Present2(/*requested_presentation_time=*/10, /*requested_prediction_span=*/0,
                    /*immediate_callback=*/[](auto) {});

  // Trigger error by making a Present2 call with an earlier presentation time
  // than the previous call to Present2.
  session->Present2(/*requested_presentation_time=*/0, /*requested_prediction_span=*/0,
                    /*immediate_callback=*/[](auto) {});

  RunLoopUntilIdle();

  EXPECT_EQ(scenic()->num_sessions(), 0U);
}

TEST_F(ScenicTest, FailedUpdate_ShouldDestroySession) {
  auto mock_system = scenic()->RegisterSystem<DummySystem>();
  scenic()->SetInitialized();
  EXPECT_EQ(scenic()->num_sessions(), 0U);
  auto session = CreateSession();
  EXPECT_EQ(scenic()->num_sessions(), 1U);

  // Mark the session as having failed an update next time DummySystem runs UpdateSessions().
  auto id = mock_system->GetLastSessionId();
  ASSERT_GE(id, 0);
  scheduling::SessionId session_id = static_cast<scheduling::SessionId>(id);
  mock_system->SetUpdateSessionsReturnValue({.sessions_with_failed_updates = {session_id}});

  // Check that the next update causes Session destruction.
  EXPECT_EQ(scenic()->num_sessions(), 1U);
  auto update_result = scenic()->UpdateSessions(/*sessions_to_update*/ {}, /*frame_trace_id*/ 23);
  EXPECT_EQ(scenic()->num_sessions(), 0U);

  // Returned |update_result| should contain the same sessions returned from the system.
  ASSERT_EQ(update_result.sessions_with_failed_updates.size(), 1u);
  EXPECT_EQ(*update_result.sessions_with_failed_updates.begin(), session_id);
}

TEST_F(ScenicTest, ScenicApiRaceBeforeSystemRegistration) {
  DisplayInfoDelegate display_info_delegate;
  TakeScreenshotDelegate screenshot_delegate;

  bool display_info = false;
  auto display_info_callback = [&](fuchsia::ui::gfx::DisplayInfo info) { display_info = true; };

  bool screenshot = false;
  auto screenshot_callback = [&](fuchsia::ui::scenic::ScreenshotData data, bool status) {
    screenshot = true;
  };

  bool display_ownership = false;
  auto display_ownership_callback = [&](zx::event event) { display_ownership = true; };

  scenic()->GetDisplayInfo(display_info_callback);
  scenic()->TakeScreenshot(screenshot_callback);
  scenic()->GetDisplayOwnershipEvent(display_ownership_callback);

  EXPECT_FALSE(display_info);
  EXPECT_FALSE(screenshot);
  EXPECT_FALSE(display_ownership);

  auto mock_system = scenic()->RegisterSystem<DummySystem>();
  scenic()->SetDisplayInfoDelegate(&display_info_delegate);
  scenic()->SetScreenshotDelegate(&screenshot_delegate);

  EXPECT_FALSE(display_info);
  EXPECT_FALSE(screenshot);
  EXPECT_FALSE(display_ownership);

  scenic()->SetInitialized();

  EXPECT_TRUE(display_info);
  EXPECT_TRUE(screenshot);
  EXPECT_TRUE(display_ownership);
}

TEST_F(ScenicTest, ScenicApiRaceAfterSystemRegistration) {
  DisplayInfoDelegate display_info_delegate;
  TakeScreenshotDelegate screenshot_delegate;

  bool display_info = false;
  auto display_info_callback = [&](fuchsia::ui::gfx::DisplayInfo info) { display_info = true; };

  bool screenshot = false;
  auto screenshot_callback = [&](fuchsia::ui::scenic::ScreenshotData data, bool status) {
    screenshot = true;
  };

  bool display_ownership = false;
  auto display_ownership_callback = [&](zx::event event) { display_ownership = true; };

  auto mock_system = scenic()->RegisterSystem<DummySystem>();

  scenic()->GetDisplayInfo(display_info_callback);
  scenic()->TakeScreenshot(screenshot_callback);
  scenic()->GetDisplayOwnershipEvent(display_ownership_callback);

  EXPECT_FALSE(display_info);
  EXPECT_FALSE(screenshot);
  EXPECT_FALSE(display_ownership);

  scenic()->SetDisplayInfoDelegate(&display_info_delegate);
  scenic()->SetScreenshotDelegate(&screenshot_delegate);

  EXPECT_FALSE(display_info);
  EXPECT_FALSE(screenshot);
  EXPECT_FALSE(display_ownership);

  scenic()->SetInitialized();

  EXPECT_TRUE(display_info);
  EXPECT_TRUE(screenshot);
  EXPECT_TRUE(display_ownership);
}

TEST_F(ScenicTest, ScenicApiAfterDelegate) {
  DisplayInfoDelegate display_info_delegate;
  TakeScreenshotDelegate screenshot_delegate;

  bool display_info = false;
  auto display_info_callback = [&](fuchsia::ui::gfx::DisplayInfo info) { display_info = true; };

  bool screenshot = false;
  auto screenshot_callback = [&](fuchsia::ui::scenic::ScreenshotData data, bool status) {
    screenshot = true;
  };

  bool display_ownership = false;
  auto display_ownership_callback = [&](zx::event event) { display_ownership = true; };

  auto mock_system = scenic()->RegisterSystem<DummySystem>();
  scenic()->SetDisplayInfoDelegate(&display_info_delegate);
  scenic()->SetScreenshotDelegate(&screenshot_delegate);

  scenic()->GetDisplayInfo(display_info_callback);
  scenic()->TakeScreenshot(screenshot_callback);
  scenic()->GetDisplayOwnershipEvent(display_ownership_callback);

  EXPECT_FALSE(display_info);
  EXPECT_FALSE(screenshot);
  EXPECT_FALSE(display_ownership);

  scenic()->SetInitialized();

  EXPECT_TRUE(display_info);
  EXPECT_TRUE(screenshot);
  EXPECT_TRUE(display_ownership);
}

}  // namespace test
}  // namespace scenic_impl
