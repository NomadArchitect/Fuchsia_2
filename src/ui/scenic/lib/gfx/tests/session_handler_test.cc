// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/gfx/tests/session_handler_test.h"

#include <lib/sys/cpp/testing/component_context_provider.h>

#include "src/ui/scenic/lib/scheduling/constant_frame_predictor.h"
#include "src/ui/scenic/lib/scheduling/default_frame_scheduler.h"

namespace scenic_impl {
namespace gfx {
namespace test {

SessionHandlerTest::SessionHandlerTest() {}

void SessionHandlerTest::SetUp() {
  ErrorReportingTest::SetUp();

  InitializeScenic();
  InitializeEngine();

  InitializeCommandDispatcher();

  RunLoopUntilIdle();  // Reset loop state; some tests are sensitive to dirty loop state.
}

void SessionHandlerTest::TearDown() {
  session_updater_.reset();
  command_dispatcher_.reset();
  engine_.reset();
  frame_scheduler_.reset();
  scenic_.reset();
  session_manager_.reset();

  ErrorReportingTest::TearDown();
}

void SessionHandlerTest::InitializeScenic() {
  sys::testing::ComponentContextProvider context_provider;
  scenic_ = std::make_unique<Scenic>(app_context_.context(), inspect::Node(), [] {});
}

void SessionHandlerTest::InitializeCommandDispatcher() {
  auto session_context = engine_->session_context();
  auto session_id = SessionId(1);

  InitializeScenicSession(session_id);

  command_dispatcher_ = session_manager_->CreateCommandDispatcher(
      scenic_session_->id(), std::move(session_context), this->shared_event_reporter(),
      this->shared_error_reporter());
}

void SessionHandlerTest::InitializeEngine() {
  frame_scheduler_ = std::make_shared<scheduling::DefaultFrameScheduler>(
      std::make_shared<scheduling::VsyncTiming>(),
      std::make_unique<scheduling::ConstantFramePredictor>(/* static_vsync_offset */ zx::msec(5)));
  engine_ = std::make_shared<Engine>(app_context_.context(), frame_scheduler_, GetEscherWeakPtr());
  frame_scheduler_->SetFrameRenderer(engine_);

  session_manager_ = std::make_unique<SessionManager>();
  session_updater_ = std::make_shared<TestSessionUpdater>(engine_.get(), session_manager_.get(),
                                                          &view_tree_updater_);
  frame_scheduler_->AddSessionUpdater(session_updater_);
}

void SessionHandlerTest::InitializeScenicSession(SessionId session_id) {
  fidl::InterfaceHandle<fuchsia::ui::scenic::SessionListener> listener;
  scenic_session_ = std::make_unique<scenic_impl::Session>(
      session_id, /*session_request=*/nullptr, std::move(listener), [this, session_id]() {
        scenic_->CloseSession(session_id);
        scenic_session_.reset();
      });
}

escher::EscherWeakPtr SessionHandlerTest::GetEscherWeakPtr() { return escher::EscherWeakPtr(); }

scheduling::SessionUpdater::UpdateResults SessionHandlerTest::TestSessionUpdater::UpdateSessions(
    const std::unordered_map<scheduling::SessionId, scheduling::PresentId>& sessions_to_update,
    uint64_t trace_id) {
  UpdateResults update_results;
  CommandContext command_context = {.scene_graph = engine_->scene_graph()->GetWeakPtr(),
                                    .view_tree_updater = view_tree_updater_};

  for (auto [session_id, present_id] : sessions_to_update) {
    auto session = session_manager_->FindSession(session_id);
    if (session) {
      session->ApplyScheduledUpdates(&command_context, present_id);
    }
  }

  return update_results;
}

}  // namespace test
}  // namespace gfx
}  // namespace scenic_impl
