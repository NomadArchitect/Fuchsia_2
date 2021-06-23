// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/flatland/flatland_manager.h"

#include <lib/syslog/cpp/macros.h>

#include <gtest/gtest.h>

#include "fuchsia/ui/scenic/internal/cpp/fidl.h"
#include "lib/gtest/real_loop_fixture.h"
#include "src/ui/scenic/lib/allocation/mock_buffer_collection_importer.h"
#include "src/ui/scenic/lib/flatland/tests/mock_flatland_presenter.h"
#include "src/ui/scenic/lib/scheduling/frame_scheduler.h"
#include "src/ui/scenic/lib/scheduling/id.h"

using ::testing::_;
using ::testing::Return;

using flatland::FlatlandManager;
using flatland::FlatlandPresenter;
using flatland::LinkSystem;
using flatland::MockFlatlandPresenter;
using flatland::UberStructSystem;
using fuchsia::ui::scenic::internal::Error;
using fuchsia::ui::scenic::internal::Flatland;
using fuchsia::ui::scenic::internal::OnPresentProcessedValues;

// These macros works like functions that check a variety of conditions, but if those conditions
// fail, the line number for the failure will appear in-line rather than in a function.

// This macro calls Present() on a Flatland object and immediately triggers the session update
// for all sessions so that changes from that Present() are visible in global systems. This is
// primarily useful for testing the user-facing Flatland API.
//
// This macro must be used within a test using the FlatlandManagerTest harness.
//
// |flatland| is a Flatland object constructed with the MockFlatlandPresenter owned by the
// FlatlandManagerTest harness. |session_id| is the SessionId for |flatland|. |expect_success|
// should be false if the call to Present() is expected to trigger an error.
#define PRESENT(flatland, session_id, expect_success)                                  \
  {                                                                                    \
    const auto num_pending_sessions = GetNumPendingSessionUpdates(session_id);         \
    if (expect_success) {                                                              \
      EXPECT_CALL(*mock_flatland_presenter_, RegisterPresent(session_id, _));          \
      EXPECT_CALL(*mock_flatland_presenter_, ScheduleUpdateForSession(_, _, _));       \
    }                                                                                  \
    fuchsia::ui::scenic::internal::PresentArgs present_args;                           \
    present_args.set_requested_presentation_time(0);                                   \
    present_args.set_acquire_fences({});                                               \
    present_args.set_release_fences({});                                               \
    present_args.set_squashable(true);                                                 \
    flatland->Present(std::move(present_args));                                        \
    /* If expecting success, wait for the worker thread to process the request. */     \
    if (expect_success) {                                                              \
      EXPECT_TRUE(RunLoopWithTimeoutOrUntil([this, session_id, num_pending_sessions] { \
        return GetNumPendingSessionUpdates(session_id) > num_pending_sessions;         \
      }));                                                                             \
    }                                                                                  \
  }

namespace {

class FlatlandManagerTest : public gtest::RealLoopFixture {
 public:
  FlatlandManagerTest()
      : uber_struct_system_(std::make_shared<UberStructSystem>()),
        link_system_(std::make_shared<LinkSystem>(uber_struct_system_->GetNextInstanceId())) {}

  void SetUp() override {
    gtest::RealLoopFixture::SetUp();

    mock_flatland_presenter_ = new ::testing::StrictMock<MockFlatlandPresenter>();

    ON_CALL(*mock_flatland_presenter_, RegisterPresent(_, _))
        .WillByDefault(::testing::Invoke(
            [&](scheduling::SessionId session_id, std::vector<zx::event> release_fences) {
              EXPECT_TRUE(release_fences.empty());

              const auto next_present_id = scheduling::GetNextPresentId();

              pending_presents_.insert({session_id, next_present_id});

              return next_present_id;
            }));

    ON_CALL(*mock_flatland_presenter_, ScheduleUpdateForSession(_, _, _))
        .WillByDefault(
            ::testing::Invoke([&](zx::time requested_presentation_time,
                                  scheduling::SchedulingIdPair id_pair, bool squashable) {
              // The ID pair must be already registered.
              EXPECT_TRUE(pending_presents_.count(id_pair));

              // Ensure present IDs are strictly increasing.
              auto& queue = pending_session_updates_[id_pair.session_id];
              EXPECT_TRUE(queue.empty() || queue.back() < id_pair.present_id);

              // Save the pending present ID.
              queue.push(id_pair.present_id);
            }));

    ON_CALL(*mock_flatland_presenter_, GetFuturePresentationInfos(_))
        .WillByDefault(::testing::Invoke(
            [&](scheduling::FrameScheduler::GetFuturePresentationInfosCallback callback) {
              // The requested_prediction_span should be at least one frame.

              // Give back at least one info.
              std::vector<scheduling::FuturePresentationInfo> presentation_infos;
              auto& info = presentation_infos.emplace_back();
              info.latch_point = zx::time(5);
              info.presentation_time = zx::time(10);

              callback(std::move(presentation_infos));
            }));

    ON_CALL(*mock_flatland_presenter_, RemoveSession(_))
        .WillByDefault(::testing::Invoke(
            [&](scheduling::SessionId session_id) { removed_sessions_.insert(session_id); }));

    flatland_presenter_ = std::shared_ptr<FlatlandPresenter>(mock_flatland_presenter_);

    uint64_t kDisplayId = 1;
    uint32_t kDisplayWidth = 640;
    uint32_t kDisplayHeight = 480;
    std::vector<std::shared_ptr<allocation::BufferCollectionImporter>> importers;
    manager_ = std::make_unique<FlatlandManager>(
        dispatcher(), flatland_presenter_, uber_struct_system_, link_system_,
        std::make_shared<scenic_impl::display::Display>(kDisplayId, kDisplayWidth, kDisplayHeight),
        importers);
  }

  void TearDown() override {
    // Expect RemoveSession() calls for each Flatland instance that was active. |manager_| may have
    // been reset during the test.
    removed_sessions_.clear();
    size_t session_count = 0;
    if (manager_) {
      session_count = manager_->GetSessionCount();
      EXPECT_CALL(*mock_flatland_presenter_, RemoveSession(_)).Times(session_count);
    }

    // Triggers cleanup of manager resources for Flatland instances that have exited.
    RunLoopUntilIdle();

    // |manager_| may have been reset during the test. If not, run until all sessions have closed,
    // which depends on the worker threads receiving "peer closed" for the clients created in
    // the tests.
    if (manager_) {
      EXPECT_TRUE(RunLoopWithTimeoutOrUntil([this]() { return manager_->GetSessionCount() == 0; }));
      EXPECT_EQ(removed_sessions_.size(), session_count);
    }

    auto snapshot = uber_struct_system_->Snapshot();
    EXPECT_TRUE(snapshot.empty());

    manager_.reset();

    EXPECT_EQ(uber_struct_system_->GetSessionCount(), 0ul);

    pending_presents_.clear();
    pending_session_updates_.clear();
    removed_sessions_.clear();
    flatland_presenter_.reset();

    gtest::RealLoopFixture::TearDown();
  }

  fidl::InterfacePtr<fuchsia::ui::scenic::internal::Flatland> CreateFlatland() {
    fidl::InterfacePtr<fuchsia::ui::scenic::internal::Flatland> flatland;
    // Since GetFuturePresentationInfos() happens when a Flatland instance is created,
    // EXPECT it here.
    EXPECT_CALL(*mock_flatland_presenter_, GetFuturePresentationInfos(_));
    manager_->CreateFlatland(flatland.NewRequest(dispatcher()));
    return flatland;
  }

  // Returns the number of currently pending session updates for |session_id|.
  size_t GetNumPendingSessionUpdates(scheduling::SessionId session_id) {
    const auto& queue = pending_session_updates_[session_id];
    return queue.size();
  }

  // Returns the next pending PresentId for |session_id| and removes it from the list of pending
  // session updates. Fails if |session_id| has no pending presents.
  scheduling::PresentId PopPendingPresent(scheduling::SessionId session_id) {
    auto& queue = pending_session_updates_[session_id];
    EXPECT_FALSE(queue.empty());

    auto next_present_id = queue.front();
    queue.pop();
    return next_present_id;
  }

 protected:
  ::testing::StrictMock<MockFlatlandPresenter>* mock_flatland_presenter_;
  const std::shared_ptr<UberStructSystem> uber_struct_system_;

  std::unique_ptr<FlatlandManager> manager_;

  // Storage for |mock_flatland_presenter_|.
  std::set<scheduling::SchedulingIdPair> pending_presents_;
  std::unordered_map<scheduling::SessionId, std::queue<scheduling::PresentId>>
      pending_session_updates_;
  std::unordered_set<scheduling::SessionId> removed_sessions_;

 private:
  std::shared_ptr<FlatlandPresenter> flatland_presenter_;
  const std::shared_ptr<LinkSystem> link_system_;
};

}  // namespace

namespace flatland {
namespace test {

TEST_F(FlatlandManagerTest, CreateFlatlands) {
  fidl::InterfacePtr<fuchsia::ui::scenic::internal::Flatland> flatland1 = CreateFlatland();

  fidl::InterfacePtr<fuchsia::ui::scenic::internal::Flatland> flatland2 = CreateFlatland();

  RunLoopUntilIdle();

  EXPECT_TRUE(flatland1.is_bound());
  EXPECT_TRUE(flatland2.is_bound());
  EXPECT_EQ(manager_->GetSessionCount(), 2ul);
}

TEST_F(FlatlandManagerTest, ClientDiesBeforeManager) {
  scheduling::SessionId id;
  {
    fidl::InterfacePtr<fuchsia::ui::scenic::internal::Flatland> flatland = CreateFlatland();
    id = uber_struct_system_->GetLatestInstanceId();

    RunLoopUntilIdle();

    EXPECT_TRUE(flatland.is_bound());

    // |flatland| falls out of scope, killing the session.
    EXPECT_CALL(*mock_flatland_presenter_, RemoveSession(id));
  }

  // The session should show up in the set of removed sessions.
  EXPECT_TRUE(RunLoopWithTimeoutOrUntil([this]() { return manager_->GetSessionCount() == 0; }));

  EXPECT_EQ(removed_sessions_.size(), 1ul);
  EXPECT_TRUE(removed_sessions_.count(id));
}

TEST_F(FlatlandManagerTest, ManagerDiesBeforeClients) {
  fidl::InterfacePtr<fuchsia::ui::scenic::internal::Flatland> flatland = CreateFlatland();
  const scheduling::SessionId id = uber_struct_system_->GetLatestInstanceId();

  RunLoopUntilIdle();

  EXPECT_TRUE(flatland.is_bound());
  EXPECT_EQ(manager_->GetSessionCount(), 1ul);

  // Explicitly kill the server.
  EXPECT_CALL(*mock_flatland_presenter_, RemoveSession(id));
  manager_.reset();

  EXPECT_EQ(uber_struct_system_->GetSessionCount(), 0ul);
  EXPECT_EQ(removed_sessions_.size(), 1ul);
  EXPECT_TRUE(removed_sessions_.count(id));

  // Wait until unbound.
  EXPECT_TRUE(RunLoopWithTimeoutOrUntil([&flatland]() { return !flatland.is_bound(); }, zx::sec(10),
                                        zx::msec(100)));
}

TEST_F(FlatlandManagerTest, ManagerImmediatelySendsPresentTokens) {
  // Set up a Flatland instance with an OnPresentProcessed() callback.
  fidl::InterfacePtr<fuchsia::ui::scenic::internal::Flatland> flatland = CreateFlatland();
  const scheduling::SessionId id = uber_struct_system_->GetLatestInstanceId();

  uint32_t returned_tokens = 0;
  flatland.events().OnPresentProcessed = [&returned_tokens](OnPresentProcessedValues values,
                                                            Error error) {
    returned_tokens = values.num_presents_returned();
  };

  // Run until the instance receives the initial allotment of tokens.
  EXPECT_TRUE(RunLoopWithTimeoutOrUntil([&returned_tokens]() { return returned_tokens != 0; }));

  EXPECT_EQ(returned_tokens, scheduling::FrameScheduler::kMaxPresentsInFlight - 1u);
}

TEST_F(FlatlandManagerTest, UpdateSessionsReturnsPresentTokens) {
  // Setup two Flatland instances with OnPresentProcessed() callbacks.
  fidl::InterfacePtr<fuchsia::ui::scenic::internal::Flatland> flatland1 = CreateFlatland();
  const scheduling::SessionId id1 = uber_struct_system_->GetLatestInstanceId();

  uint32_t returned_tokens1 = 0;
  flatland1.events().OnPresentProcessed = [&returned_tokens1](OnPresentProcessedValues values,
                                                              Error error) {
    returned_tokens1 = values.num_presents_returned();
    EXPECT_FALSE(values.future_presentation_infos().empty());
  };

  fidl::InterfacePtr<fuchsia::ui::scenic::internal::Flatland> flatland2 = CreateFlatland();
  const scheduling::SessionId id2 = uber_struct_system_->GetLatestInstanceId();

  uint32_t returned_tokens2 = 0;
  flatland2.events().OnPresentProcessed = [&returned_tokens2](OnPresentProcessedValues values,
                                                              Error) {
    returned_tokens2 = values.num_presents_returned();
    EXPECT_FALSE(values.future_presentation_infos().empty());
  };

  // Both instances receive their initial allotment of tokens, then forget those tokens.
  EXPECT_TRUE(RunLoopWithTimeoutOrUntil([&returned_tokens1]() { return returned_tokens1 != 0; }));
  returned_tokens1 = 0;

  EXPECT_TRUE(RunLoopWithTimeoutOrUntil([&returned_tokens2]() { return returned_tokens2 != 0; }));
  returned_tokens2 = 0;

  // Present both instances twice, but don't update sessions.
  PRESENT(flatland1, id1, true);
  PRESENT(flatland1, id1, true);

  PRESENT(flatland2, id2, true);
  PRESENT(flatland2, id2, true);

  auto snapshot = uber_struct_system_->Snapshot();
  EXPECT_TRUE(snapshot.empty());

  EXPECT_EQ(GetNumPendingSessionUpdates(id1), 2ul);
  EXPECT_EQ(GetNumPendingSessionUpdates(id2), 2ul);

  // Update the first session, but only with the first PresentId, which should push an UberStruct
  // and return one token to the first instance.
  auto next_present_id1 = PopPendingPresent(id1);
  manager_->UpdateSessions({{id1, next_present_id1}}, /*trace_id=*/0);

  EXPECT_CALL(*mock_flatland_presenter_, GetFuturePresentationInfos(_));
  manager_->OnCpuWorkDone();

  snapshot = uber_struct_system_->Snapshot();
  EXPECT_EQ(snapshot.size(), 1u);
  EXPECT_TRUE(snapshot.count(id1));
  EXPECT_FALSE(snapshot.count(id2));

  EXPECT_TRUE(RunLoopWithTimeoutOrUntil([&returned_tokens1]() { return returned_tokens1 != 0; }));

  EXPECT_EQ(returned_tokens1, 1u);
  EXPECT_EQ(returned_tokens2, 0u);

  EXPECT_EQ(GetNumPendingSessionUpdates(id1), 1ul);
  EXPECT_EQ(GetNumPendingSessionUpdates(id2), 2ul);

  returned_tokens1 = 0;

  // Update only the second session and consume both PresentIds, which should push an UberStruct
  // and return two tokens to the second instance.
  auto next_present_id2 = PopPendingPresent(id2);
  next_present_id2 = PopPendingPresent(id2);

  manager_->UpdateSessions({{id2, next_present_id2}}, /*trace_id=*/0);

  EXPECT_CALL(*mock_flatland_presenter_, GetFuturePresentationInfos(_));
  manager_->OnCpuWorkDone();

  snapshot = uber_struct_system_->Snapshot();
  EXPECT_EQ(snapshot.size(), 2u);
  EXPECT_TRUE(snapshot.count(id1));
  EXPECT_TRUE(snapshot.count(id2));

  EXPECT_TRUE(RunLoopWithTimeoutOrUntil([&returned_tokens2]() { return returned_tokens2 != 0; }));

  EXPECT_EQ(returned_tokens1, 0u);
  EXPECT_EQ(returned_tokens2, 2u);

  EXPECT_EQ(GetNumPendingSessionUpdates(id1), 1ul);
  EXPECT_EQ(GetNumPendingSessionUpdates(id2), 0ul);
}

// It is possible for the session to update multiple times in a row before OnCpuWorkDone() is
// called. If that's the case, we need to ensure that present tokens returned from the first
// update are not lost.
TEST_F(FlatlandManagerTest, ConsecutiveUpdateSessions_ReturnsCorrectPresentTokens) {
  fidl::InterfacePtr<fuchsia::ui::scenic::internal::Flatland> flatland = CreateFlatland();
  const scheduling::SessionId id = uber_struct_system_->GetLatestInstanceId();

  uint32_t returned_tokens = 0;
  flatland.events().OnPresentProcessed = [&returned_tokens](OnPresentProcessedValues values,
                                                            Error error) {
    returned_tokens = values.num_presents_returned();
    EXPECT_FALSE(values.future_presentation_infos().empty());
  };

  // Receive the initial allotment of tokens, then forget those tokens.
  EXPECT_TRUE(RunLoopWithTimeoutOrUntil([&returned_tokens]() { return returned_tokens != 0; }));
  returned_tokens = 0;

  // Present twice, but don't update the session yet.
  PRESENT(flatland, id, true);
  PRESENT(flatland, id, true);

  auto snapshot = uber_struct_system_->Snapshot();
  EXPECT_TRUE(snapshot.empty());

  EXPECT_EQ(GetNumPendingSessionUpdates(id), 2ul);

  // Update the session, but only with the first PresentId, which should push an UberStruct
  // and return one token to the first instance.
  auto next_present_id = PopPendingPresent(id);
  manager_->UpdateSessions({{id, next_present_id}}, /*trace_id=*/0);

  // Update again.
  next_present_id = PopPendingPresent(id);
  manager_->UpdateSessions({{id, next_present_id}}, /*trace_id=*/0);

  // Finally, the work is done according to the frame scheduler.
  EXPECT_CALL(*mock_flatland_presenter_, GetFuturePresentationInfos(_));
  manager_->OnCpuWorkDone();

  snapshot = uber_struct_system_->Snapshot();
  EXPECT_EQ(snapshot.size(), 1u);
  EXPECT_TRUE(snapshot.count(id));

  EXPECT_TRUE(RunLoopWithTimeoutOrUntil([&returned_tokens]() { return returned_tokens != 0; }));

  EXPECT_EQ(returned_tokens, 2u);

  EXPECT_EQ(GetNumPendingSessionUpdates(id), 0ul);
}

TEST_F(FlatlandManagerTest, PresentWithoutTokensClosesSession) {
  // Setup a Flatland instance with an OnPresentProcessed() callback.
  fidl::InterfacePtr<fuchsia::ui::scenic::internal::Flatland> flatland = CreateFlatland();
  const scheduling::SessionId id = uber_struct_system_->GetLatestInstanceId();

  Error error_returned = Error::NO_ERROR;
  uint32_t tokens_remaining = 1;
  flatland.events().OnPresentProcessed = [&error_returned, &tokens_remaining](
                                             OnPresentProcessedValues values, Error error) {
    error_returned = error;
    tokens_remaining += (error == Error::NO_ERROR) ? values.num_presents_returned() : 0;
  };

  // Run until the instance receives the initial allotment of tokens.
  EXPECT_TRUE(RunLoopWithTimeoutOrUntil([&tokens_remaining]() { return tokens_remaining > 1; }));

  // Present until no tokens remain.
  while (tokens_remaining > 0) {
    PRESENT(flatland, id, true);
    --tokens_remaining;
  }

  EXPECT_TRUE(flatland.is_bound());

  // Present one more time and ensure the session is closed.
  EXPECT_CALL(*mock_flatland_presenter_, RemoveSession(id));
  PRESENT(flatland, id, false);

  // The instance will eventually be unbound, but it takes a pair of thread hops to complete since
  // the destroy_instance_function() posts a task from the worker to the main and that task
  // ultimately posts the destruction back onto the worker.
  EXPECT_TRUE(RunLoopWithTimeoutOrUntil([&flatland]() { return !flatland.is_bound(); }));
  EXPECT_EQ(error_returned, Error::NO_PRESENTS_REMAINING);
}

TEST_F(FlatlandManagerTest, ErrorClosesSession) {
  // Setup a Flatland instance with an OnPresentProcessed() callback.
  fidl::InterfacePtr<fuchsia::ui::scenic::internal::Flatland> flatland = CreateFlatland();
  const scheduling::SessionId id = uber_struct_system_->GetLatestInstanceId();

  Error error_returned = Error::NO_ERROR;
  uint32_t tokens_remaining = 1;
  flatland.events().OnPresentProcessed = [&error_returned, &tokens_remaining](
                                             OnPresentProcessedValues values, Error error) {
    error_returned = error;
    tokens_remaining += (error == Error::NO_ERROR) ? values.num_presents_returned() : 0;
  };

  // Run until the instance receives the initial allotment of tokens.
  EXPECT_TRUE(RunLoopWithTimeoutOrUntil([&tokens_remaining]() { return tokens_remaining > 1; }));
  EXPECT_TRUE(flatland.is_bound());

  // Queue a bad SetRootTransform call ensure the session is closed.
  EXPECT_CALL(*mock_flatland_presenter_, RemoveSession(id));
  flatland->SetRootTransform({2});
  PRESENT(flatland, id, false);

  // The instance will eventually be unbound, but it takes a pair of thread hops to complete since
  // the destroy_instance_function() posts a task from the worker to the main and that task
  // ultimately posts the destruction back onto the worker.
  EXPECT_TRUE(RunLoopWithTimeoutOrUntil([&flatland]() { return !flatland.is_bound(); }));
  EXPECT_EQ(error_returned, Error::BAD_OPERATION);
}

TEST_F(FlatlandManagerTest, TokensAreReplenishedAfterRunningOut) {
  // Setup a Flatland instance with an OnPresentProcessed() callback.
  fidl::InterfacePtr<fuchsia::ui::scenic::internal::Flatland> flatland = CreateFlatland();
  const scheduling::SessionId id = uber_struct_system_->GetLatestInstanceId();

  uint32_t tokens_remaining = 1;
  flatland.events().OnPresentProcessed = [&tokens_remaining](OnPresentProcessedValues values,
                                                             Error error) {
    tokens_remaining += values.num_presents_returned();
  };

  // Run until the instance receives the initial allotment of tokens.
  EXPECT_TRUE(RunLoopWithTimeoutOrUntil([&tokens_remaining]() { return tokens_remaining > 1; }));

  // Present until no tokens remain.
  while (tokens_remaining > 0) {
    PRESENT(flatland, id, true);
    --tokens_remaining;
  }

  // Process the first present.
  auto next_present_id = PopPendingPresent(id);
  manager_->UpdateSessions({{id, next_present_id}}, /*trace_id=*/0);

  // Signal that the work is done, which should return present tokens to the client.
  EXPECT_CALL(*mock_flatland_presenter_, GetFuturePresentationInfos(_));
  manager_->OnCpuWorkDone();

  EXPECT_TRUE(RunLoopWithTimeoutOrUntil([&tokens_remaining]() { return tokens_remaining != 0; }));

  // Present once more which should succeed.
  PRESENT(flatland, id, true);
  EXPECT_TRUE(flatland.is_bound());
}

TEST_F(FlatlandManagerTest, OnFramePresentedEvent) {
  // Setup two Flatland instances with OnFramePresented() callbacks.
  fidl::InterfacePtr<fuchsia::ui::scenic::internal::Flatland> flatland1 = CreateFlatland();
  const scheduling::SessionId id1 = uber_struct_system_->GetLatestInstanceId();

  std::optional<fuchsia::scenic::scheduling::FramePresentedInfo> info1;
  flatland1.events().OnFramePresented =
      [&info1](fuchsia::scenic::scheduling::FramePresentedInfo info) { info1 = std::move(info); };

  fidl::InterfacePtr<fuchsia::ui::scenic::internal::Flatland> flatland2 = CreateFlatland();
  const scheduling::SessionId id2 = uber_struct_system_->GetLatestInstanceId();

  std::optional<fuchsia::scenic::scheduling::FramePresentedInfo> info2;
  flatland2.events().OnFramePresented =
      [&info2](fuchsia::scenic::scheduling::FramePresentedInfo info) { info2 = std::move(info); };

  // Present both instances twice, but don't update sessions.
  PRESENT(flatland1, id1, true);
  PRESENT(flatland1, id1, true);

  PRESENT(flatland2, id2, true);
  PRESENT(flatland2, id2, true);

  // Call OnFramePresented() with a PresentId for the first session and ensure the event fires.
  scheduling::PresentTimestamps timestamps{
      .presented_time = zx::time(111),
      .vsync_interval = zx::duration(11),
  };
  zx::time latch_time1 = zx::time(123);
  auto next_present_id1 = PopPendingPresent(id1);

  std::unordered_map<scheduling::SessionId,
                     std::map<scheduling::PresentId, /*latched_time*/ zx::time>>
      latch_times;
  latch_times[id1] = {{next_present_id1, latch_time1}};

  manager_->OnFramePresented(latch_times, timestamps);

  // Wait until the event has fired.
  EXPECT_TRUE(RunLoopWithTimeoutOrUntil([&info1]() { return info1.has_value(); }));

  // Verify that info1 contains the expected data.
  EXPECT_EQ(zx::time(info1->actual_presentation_time), timestamps.presented_time);
  EXPECT_EQ(info1->num_presents_allowed, 0ul);
  EXPECT_EQ(info1->presentation_infos.size(), 1ul);
  EXPECT_EQ(zx::time(info1->presentation_infos[0].latched_time()), latch_time1);

  // Run the loop again to show that info2 hasn't been populated.
  RunLoopUntilIdle();
  EXPECT_FALSE(info2.has_value());

  // Call OnFramePresented with all the remaining PresentIds and ensure an event fires for both.
  info1.reset();
  latch_times.clear();

  timestamps = scheduling::PresentTimestamps({
      .presented_time = zx::time(222),
      .vsync_interval = zx::duration(22),
  });
  latch_time1 = zx::time(234);
  auto latch_time2_1 = zx::time(345);
  auto latch_time2_2 = zx::time(456);
  next_present_id1 = PopPendingPresent(id1);
  auto next_present_id2_1 = PopPendingPresent(id2);
  auto next_present_id2_2 = PopPendingPresent(id2);

  latch_times[id1] = {{next_present_id1, latch_time1}};
  latch_times[id2] = {{next_present_id2_1, latch_time2_1}, {next_present_id2_2, latch_time2_2}};

  manager_->OnFramePresented(latch_times, timestamps);

  // Wait until both events have fired.
  EXPECT_TRUE(RunLoopWithTimeoutOrUntil([&info1]() { return info1.has_value(); }));
  EXPECT_TRUE(RunLoopWithTimeoutOrUntil([&info2]() { return info2.has_value(); }));

  // Verify that both infos contain the expected data.
  EXPECT_EQ(zx::time(info1->actual_presentation_time), timestamps.presented_time);
  EXPECT_EQ(info1->num_presents_allowed, 0ul);
  EXPECT_EQ(info1->presentation_infos.size(), 1ul);
  EXPECT_EQ(zx::time(info1->presentation_infos[0].latched_time()), latch_time1);

  EXPECT_EQ(zx::time(info2->actual_presentation_time), timestamps.presented_time);
  EXPECT_EQ(info2->num_presents_allowed, 0ul);
  EXPECT_EQ(info2->presentation_infos.size(), 2ul);
  EXPECT_EQ(zx::time(info2->presentation_infos[0].latched_time()), latch_time2_1);
  EXPECT_EQ(zx::time(info2->presentation_infos[1].latched_time()), latch_time2_2);
}

#undef PRESENT

}  // namespace test
}  // namespace flatland
