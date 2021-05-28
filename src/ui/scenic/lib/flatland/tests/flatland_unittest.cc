// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/flatland/flatland.h"

#include <lib/async/time.h>
#include <lib/sys/cpp/testing/component_context_provider.h>
#include <lib/syslog/cpp/macros.h>

#include <limits>

#include <gtest/gtest.h>

#include "fuchsia/ui/scenic/internal/cpp/fidl.h"
#include "lib/gtest/test_loop_fixture.h"
#include "src/lib/fsl/handles/object_info.h"
#include "src/ui/scenic/lib/allocation/allocator.h"
#include "src/ui/scenic/lib/allocation/buffer_collection_import_export_tokens.h"
#include "src/ui/scenic/lib/allocation/mock_buffer_collection_importer.h"
#include "src/ui/scenic/lib/flatland/global_matrix_data.h"
#include "src/ui/scenic/lib/flatland/global_topology_data.h"
#include "src/ui/scenic/lib/flatland/tests/mock_flatland_presenter.h"
#include "src/ui/scenic/lib/scheduling/frame_scheduler.h"
#include "src/ui/scenic/lib/scheduling/id.h"
#include "src/ui/scenic/lib/utils/dispatcher_holder.h"
#include "src/ui/scenic/lib/utils/helpers.h"

#include <glm/gtx/matrix_transform_2d.hpp>

using ::testing::_;
using ::testing::Return;

using BufferCollectionId = flatland::Flatland::BufferCollectionId;
using allocation::Allocator;
using allocation::BufferCollectionImporter;
using allocation::BufferCollectionImportExportTokens;
using allocation::ImageMetadata;
using allocation::MockBufferCollectionImporter;
using flatland::Flatland;
using flatland::FlatlandPresenter;
using flatland::GlobalMatrixVector;
using flatland::GlobalTopologyData;
using flatland::LinkSystem;
using flatland::MockFlatlandPresenter;
using flatland::TransformGraph;
using flatland::TransformHandle;
using flatland::UberStruct;
using flatland::UberStructSystem;
using fuchsia::scenic::allocation::Allocator_RegisterBufferCollection_Result;
using fuchsia::scenic::allocation::BufferCollectionExportToken;
using fuchsia::scenic::allocation::BufferCollectionImportToken;
using fuchsia::ui::scenic::internal::ContentId;
using fuchsia::ui::scenic::internal::ContentLink;
using fuchsia::ui::scenic::internal::ContentLinkStatus;
using fuchsia::ui::scenic::internal::ContentLinkToken;
using fuchsia::ui::scenic::internal::Flatland_Present_Result;
using fuchsia::ui::scenic::internal::GraphLink;
using fuchsia::ui::scenic::internal::GraphLinkStatus;
using fuchsia::ui::scenic::internal::GraphLinkToken;
using fuchsia::ui::scenic::internal::ImageProperties;
using fuchsia::ui::scenic::internal::LayoutInfo;
using fuchsia::ui::scenic::internal::LinkProperties;
using fuchsia::ui::scenic::internal::Orientation;
using fuchsia::ui::scenic::internal::TransformId;
using fuchsia::ui::scenic::internal::Vec2;

namespace {

// Convenience struct for the PRESENT_WITH_ARGS macro to avoid having to update it every time
// a new argument is added to Flatland::Present(). This struct also includes additional flags
// to PRESENT_WITH_ARGS itself for testing timing-related Present() functionality.
struct PresentArgs {
  // Arguments to Flatland::Present().
  zx::time requested_presentation_time;
  std::vector<zx::event> acquire_fences;
  std::vector<zx::event> release_fences;
  bool squashable = true;

  // Arguments to the PRESENT_WITH_ARGS macro.

  // If true, skips the session update associated with the Present(), meaning the new UberStruct
  // will not be in the snapshot and the release fences will not be signaled.
  bool skip_session_update_and_release_fences = false;

  // The number of present tokens that should be returned to the client.
  uint32_t present_tokens_returned = 1;

  // The future presentation infos that should be returned to the client.
  flatland::Flatland::FuturePresentationInfos presentation_infos = {};

  // If PRESENT_WITH_ARGS is called with |expect_success| = false, the error that should be
  // expected as the return value from Present().
  fuchsia::ui::scenic::internal::Error expected_error =
      fuchsia::ui::scenic::internal::Error::BAD_OPERATION;
};

struct GlobalIdPair {
  allocation::GlobalBufferCollectionId collection_id;
  allocation::GlobalImageId image_id;
};

// These macros works like functions that check a variety of conditions, but if those conditions
// fail, the line number for the failure will appear in-line rather than in a function.

// This macro calls Present() on a Flatland object and immediately triggers the session update
// for all sessions so that changes from that Present() are visible in global systems. This is
// primarily useful for testing the user-facing Flatland API.
//
// This macro must be used within a test using the FlatlandTest harness.
//
// |flatland| is a Flatland object constructed with the MockFlatlandPresenter owned by the
// FlatlandTest harness. |expect_success| should be false if the call to Present() is expected to
// trigger an error.
#define PRESENT_WITH_ARGS(flatland, args, expect_success)                                    \
  {                                                                                          \
    bool had_acquire_fences = !args.acquire_fences.empty();                                  \
    if (expect_success) {                                                                    \
      EXPECT_CALL(*mock_flatland_presenter_,                                                 \
                  RegisterPresent(flatland->GetRoot().GetInstanceId(), _));                  \
    }                                                                                        \
    bool processed_callback = false;                                                         \
    fuchsia::ui::scenic::internal::PresentArgs present_args;                                 \
    present_args.set_requested_presentation_time(args.requested_presentation_time.get());    \
    present_args.set_acquire_fences(std::move(args.acquire_fences));                         \
    present_args.set_release_fences(std::move(args.release_fences));                         \
    present_args.set_squashable(args.squashable);                                            \
    flatland->Present(std::move(present_args), [&](Flatland_Present_Result result) {         \
      EXPECT_EQ(!expect_success, result.is_err());                                           \
      if (!expect_success) {                                                                 \
        EXPECT_EQ(args.expected_error, result.err());                                        \
      }                                                                                      \
      processed_callback = true;                                                             \
    });                                                                                      \
    EXPECT_TRUE(processed_callback);                                                         \
    if (expect_success) {                                                                    \
      /* Even with no acquire_fences, UberStruct updates queue on the dispatcher. */         \
      if (!had_acquire_fences) {                                                             \
        EXPECT_CALL(                                                                         \
            *mock_flatland_presenter_,                                                       \
            ScheduleUpdateForSession(args.requested_presentation_time, _, args.squashable)); \
      }                                                                                      \
      RunLoopUntilIdle();                                                                    \
      if (!args.skip_session_update_and_release_fences) {                                    \
        ApplySessionUpdatesAndSignalFences();                                                \
      }                                                                                      \
    }                                                                                        \
    flatland->OnPresentProcessed(args.present_tokens_returned,                               \
                                 std::move(args.presentation_infos));                        \
  }

// Identical to PRESENT_WITH_ARGS, but supplies an empty PresentArgs to the Present() call.
#define PRESENT(flatland, expect_success) \
  { PRESENT_WITH_ARGS(flatland, PresentArgs(), expect_success); }

#define REGISTER_BUFFER_COLLECTION(allocator, export_token, token, expect_success)                \
  if (expect_success) {                                                                           \
    EXPECT_CALL(*mock_buffer_collection_importer_,                                                \
                ImportBufferCollection(fsl::GetKoid(export_token.value.get()), _, _))             \
        .WillOnce(testing::Invoke(                                                                \
            [](allocation::GlobalBufferCollectionId, fuchsia::sysmem::Allocator_Sync*,            \
               fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken>) { return true; })); \
  }                                                                                               \
  bool processed_callback = false;                                                                \
  fuchsia::scenic::allocation::RegisterBufferCollectionArgs args;                                 \
  args.set_export_token(std::move(export_token));                                                 \
  args.set_buffer_collection_token(token);                                                        \
  allocator->RegisterBufferCollection(                                                            \
      std::move(args), [&processed_callback](Allocator_RegisterBufferCollection_Result result) {  \
        EXPECT_EQ(!expect_success, result.is_err());                                              \
        processed_callback = true;                                                                \
      });                                                                                         \
  EXPECT_TRUE(processed_callback);

// This macro searches for a local matrix associated with a specific TransformHandle.
//
// |uber_struct| is the UberStruct to search to find the matrix. |target_handle| is the
// TransformHandle of the matrix to compare. |expected_matrix| is the expected value of that
// matrix.
#define EXPECT_MATRIX(uber_struct, target_handle, expected_matrix)                               \
  {                                                                                              \
    glm::mat3 matrix = glm::mat3();                                                              \
    auto matrix_kv = uber_struct->local_matrices.find(target_handle);                            \
    if (matrix_kv != uber_struct->local_matrices.end()) {                                        \
      matrix = matrix_kv->second;                                                                \
    }                                                                                            \
    for (size_t i = 0; i < 3; ++i) {                                                             \
      for (size_t j = 0; j < 3; ++j) {                                                           \
        EXPECT_FLOAT_EQ(matrix[i][j], expected_matrix[i][j]) << " row " << j << " column " << i; \
      }                                                                                          \
    }                                                                                            \
  }

const float kDefaultSize = 1.f;
const glm::vec2 kDefaultPixelScale = {1.f, 1.f};

float GetOrientationAngle(fuchsia::ui::scenic::internal::Orientation orientation) {
  switch (orientation) {
    case Orientation::CCW_0_DEGREES:
      return 0.f;
    case Orientation::CCW_90_DEGREES:
      return glm::half_pi<float>();
    case Orientation::CCW_180_DEGREES:
      return glm::pi<float>();
    case Orientation::CCW_270_DEGREES:
      return glm::three_over_two_pi<float>();
  }
}

class FlatlandTest : public gtest::TestLoopFixture {
 public:
  FlatlandTest()
      : uber_struct_system_(std::make_shared<UberStructSystem>()),
        link_system_(std::make_shared<LinkSystem>(uber_struct_system_->GetNextInstanceId())) {}

  void SetUp() override {
    mock_flatland_presenter_ = new ::testing::StrictMock<MockFlatlandPresenter>();

    ON_CALL(*mock_flatland_presenter_, RegisterPresent(_, _))
        .WillByDefault(::testing::Invoke(
            [&](scheduling::SessionId session_id, std::vector<zx::event> release_fences) {
              const auto next_present_id = scheduling::GetNextPresentId();

              // Store all release fences.
              pending_release_fences_[{session_id, next_present_id}] = std::move(release_fences);

              return next_present_id;
            }));

    ON_CALL(*mock_flatland_presenter_, ScheduleUpdateForSession(_, _, _))
        .WillByDefault(
            ::testing::Invoke([&](zx::time requested_presentation_time,
                                  scheduling::SchedulingIdPair id_pair, bool squashable) {
              // The ID must be already registered.
              EXPECT_TRUE(pending_release_fences_.find(id_pair) != pending_release_fences_.end());

              // Ensure IDs are strictly increasing.
              auto current_id_kv = pending_session_updates_.find(id_pair.session_id);
              EXPECT_TRUE(current_id_kv == pending_session_updates_.end() ||
                          current_id_kv->second < id_pair.present_id);

              // Only save the latest PresentId: the UberStructSystem will flush all Presents prior
              // to it.
              pending_session_updates_[id_pair.session_id] = id_pair.present_id;

              // Store all requested presentation times to verify in test.
              requested_presentation_times_[id_pair] = requested_presentation_time;
            }));

    sysmem_allocator_ = utils::CreateSysmemAllocatorSyncPtr();

    flatland_presenter_ = std::shared_ptr<FlatlandPresenter>(mock_flatland_presenter_);

    mock_buffer_collection_importer_ = new MockBufferCollectionImporter();
    buffer_collection_importer_ =
        std::shared_ptr<allocation::BufferCollectionImporter>(mock_buffer_collection_importer_);

    // Capture uninteresting cleanup calls from Allocator dtor.
    EXPECT_CALL(*mock_buffer_collection_importer_, ReleaseBufferCollection(_))
        .Times(::testing::AtLeast(0));
  }

  void TearDown() override {
    RunLoopUntilIdle();

    auto link_topologies = link_system_->GetResolvedTopologyLinks();
    EXPECT_TRUE(link_topologies.empty());

    buffer_collection_importer_.reset();
    flatland_presenter_.reset();
    flatlands_.clear();
  }

  std::shared_ptr<Allocator> CreateAllocator() {
    std::vector<std::shared_ptr<BufferCollectionImporter>> importers;
    std::vector<std::shared_ptr<BufferCollectionImporter>> screenshot_importers;
    importers.push_back(buffer_collection_importer_);
    return std::make_shared<Allocator>(context_provider_.context(), importers, screenshot_importers,
                                       utils::CreateSysmemAllocatorSyncPtr("-allocator"));
  }

  std::shared_ptr<Flatland> CreateFlatland() {
    auto session_id = scheduling::GetNextSessionId();
    flatlands_.push_back({});
    std::vector<std::shared_ptr<BufferCollectionImporter>> importers;
    importers.push_back(buffer_collection_importer_);
    return Flatland::New(
        std::make_shared<utils::UnownedDispatcherHolder>(dispatcher()),
        flatlands_.back().NewRequest(), session_id,
        /*destroy_instance_functon=*/[]() {}, flatland_presenter_, link_system_,
        uber_struct_system_->AllocateQueueForSession(session_id), importers);
  }

  fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken> CreateToken() {
    fuchsia::sysmem::BufferCollectionTokenSyncPtr token;
    zx_status_t status = sysmem_allocator_->AllocateSharedCollection(token.NewRequest());
    EXPECT_EQ(status, ZX_OK);
    status = token->Sync();
    EXPECT_EQ(status, ZX_OK);
    return token;
  }

  // Applies the most recently scheduled session update for each session and signals the release
  // fences of all Presents up to and including that update.
  void ApplySessionUpdatesAndSignalFences() {
    uber_struct_system_->UpdateSessions(pending_session_updates_);

    // Signal all release fences up to and including the PresentId in |pending_session_updates_|.
    for (const auto& [session_id, present_id] : pending_session_updates_) {
      auto begin = pending_release_fences_.lower_bound({session_id, 0});
      auto end = pending_release_fences_.upper_bound({session_id, present_id});
      for (auto fences_kv = begin; fences_kv != end; ++fences_kv) {
        for (auto& event : fences_kv->second) {
          event.signal(0, ZX_EVENT_SIGNALED);
        }
      }
      pending_release_fences_.erase(begin, end);
    }

    pending_session_updates_.clear();
    requested_presentation_times_.clear();
  }

  // Gets the list of registered PresentIds for a particular |session_id|.
  std::vector<scheduling::PresentId> GetRegisteredPresents(scheduling::SessionId session_id) const {
    std::vector<scheduling::PresentId> present_ids;

    auto begin = pending_release_fences_.lower_bound({session_id, 0});
    auto end = pending_release_fences_.upper_bound({session_id + 1, 0});
    for (auto fence_kv = begin; fence_kv != end; ++fence_kv) {
      present_ids.push_back(fence_kv->first.present_id);
    }

    return present_ids;
  }

  // Returns true if |session_id| currently has a session update pending.
  bool HasSessionUpdate(scheduling::SessionId session_id) const {
    return pending_session_updates_.count(session_id);
  }

  // Returns the requested presentation time for a particular |id_pair|, or std::nullopt if that
  // pair has not had a presentation scheduled for it.
  std::optional<zx::time> GetRequestedPresentationTime(scheduling::SchedulingIdPair id_pair) {
    auto iter = requested_presentation_times_.find(id_pair);
    if (iter == requested_presentation_times_.end()) {
      return std::nullopt;
    }
    return iter->second;
  }

  void SetDisplayPixelScale(const glm::vec2& pixel_scale) { display_pixel_scale_ = pixel_scale; }

  // The parent transform must be a topology root or ComputeGlobalTopologyData() will crash.
  bool IsDescendantOf(TransformHandle parent, TransformHandle child) {
    auto snapshot = uber_struct_system_->Snapshot();
    auto links = link_system_->GetResolvedTopologyLinks();
    auto data = GlobalTopologyData::ComputeGlobalTopologyData(
        snapshot, links, link_system_->GetInstanceId(), parent);
    for (auto handle : data.topology_vector) {
      if (handle == child) {
        return true;
      }
    }
    return false;
  }

  // Snapshots the UberStructSystem and fetches the UberStruct associated with |flatland|. If no
  // UberStruct exists for |flatland|, returns nullptr.
  std::shared_ptr<UberStruct> GetUberStruct(Flatland* flatland) {
    auto snapshot = uber_struct_system_->Snapshot();

    auto root = flatland->GetRoot();
    auto uber_struct_kv = snapshot.find(root.GetInstanceId());
    if (uber_struct_kv == snapshot.end()) {
      return nullptr;
    }

    auto uber_struct = uber_struct_kv->second;
    EXPECT_FALSE(uber_struct->local_topology.empty());
    EXPECT_EQ(uber_struct->local_topology[0].handle, root);

    return uber_struct;
  }

  // Updates all Links reachable from |root_transform|, which must be the root transform of one of
  // the active Flatland instances.
  //
  // Tests that call this function are testing both Flatland and LinkSystem::UpdateLinks().
  void UpdateLinks(TransformHandle root_transform) {
    // Run the looper in case there are queued commands in, e.g., ObjectLinker.
    RunLoopUntilIdle();

    // This is a replica of the core render loop.
    const auto snapshot = uber_struct_system_->Snapshot();
    const auto links = link_system_->GetResolvedTopologyLinks();
    const auto data = GlobalTopologyData::ComputeGlobalTopologyData(
        snapshot, links, link_system_->GetInstanceId(), root_transform);
    const auto matrices =
        flatland::ComputeGlobalMatrices(data.topology_vector, data.parent_indices, snapshot);

    link_system_->UpdateLinks(data.topology_vector, data.live_handles, matrices,
                              display_pixel_scale_, snapshot);

    // Run the looper again to process any queued FIDL events (i.e., Link callbacks).
    RunLoopUntilIdle();
  }

  void CreateLink(Flatland* parent, Flatland* child, ContentId id,
                  fidl::InterfacePtr<ContentLink>* content_link,
                  fidl::InterfacePtr<GraphLink>* graph_link) {
    ContentLinkToken parent_token;
    GraphLinkToken child_token;
    ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token.value, &child_token.value));

    LinkProperties properties;
    properties.set_logical_size({kDefaultSize, kDefaultSize});
    parent->CreateLink(id, std::move(parent_token), std::move(properties),
                       content_link->NewRequest());
    child->LinkToParent(std::move(child_token), graph_link->NewRequest());
    PRESENT(parent, true);
    PRESENT(child, true);
  }

  // Creates an image in |flatland| with the specified |image_id| and backing properties.
  // This function also returns the GlobalBufferCollectionId that will be in the ImageMetadata
  // struct for that Image.
  GlobalIdPair CreateImage(
      Flatland* flatland, Allocator* allocator, ContentId image_id,
      BufferCollectionImportExportTokens buffer_collection_import_export_tokens,
      ImageProperties properties) {
    const auto koid = fsl::GetKoid(buffer_collection_import_export_tokens.export_token.value.get());
    REGISTER_BUFFER_COLLECTION(allocator, buffer_collection_import_export_tokens.export_token,
                               CreateToken(), true);

    FX_DCHECK(properties.has_width());
    FX_DCHECK(properties.has_height());

    allocation::GlobalImageId global_image_id;
    EXPECT_CALL(*mock_buffer_collection_importer_, ImportBufferImage(_))
        .WillOnce(testing::Invoke([&global_image_id](const ImageMetadata& metadata) {
          global_image_id = metadata.identifier;
          return true;
        }));

    flatland->CreateImage(image_id, std::move(buffer_collection_import_export_tokens.import_token),
                          0, std::move(properties));
    PRESENT(flatland, true);
    return {.collection_id = koid, .image_id = global_image_id};
  }

 protected:
  ::testing::StrictMock<MockFlatlandPresenter>* mock_flatland_presenter_;
  MockBufferCollectionImporter* mock_buffer_collection_importer_;
  std::shared_ptr<allocation::BufferCollectionImporter> buffer_collection_importer_;
  const std::shared_ptr<UberStructSystem> uber_struct_system_;
  std::shared_ptr<FlatlandPresenter> flatland_presenter_;
  const std::shared_ptr<LinkSystem> link_system_;
  sys::testing::ComponentContextProvider context_provider_;

 private:
  std::vector<fuchsia::ui::scenic::internal::FlatlandPtr> flatlands_;
  glm::vec2 display_pixel_scale_ = kDefaultPixelScale;

  // Storage for |mock_flatland_presenter_|.
  std::map<scheduling::SchedulingIdPair, std::vector<zx::event>> pending_release_fences_;
  std::map<scheduling::SchedulingIdPair, zx::time> requested_presentation_times_;
  std::unordered_map<scheduling::SessionId, scheduling::PresentId> pending_session_updates_;
  fuchsia::sysmem::AllocatorSyncPtr sysmem_allocator_;
};

}  // namespace

namespace flatland {
namespace test {

TEST_F(FlatlandTest, PresentShouldReturnSuccess) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();
  PRESENT(flatland, true);
}

TEST_F(FlatlandTest, PresentErrorNoTokens) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  // Present, but return no tokens so the client has none left.
  {
    PresentArgs args;
    args.present_tokens_returned = 0;
    PRESENT_WITH_ARGS(flatland, std::move(args), true);
  }

  // Present again, which should fail because the client has no tokens.
  {
    PresentArgs args;
    args.expected_error = fuchsia::ui::scenic::internal::Error::NO_PRESENTS_REMAINING;
    PRESENT_WITH_ARGS(flatland, std::move(args), false);
  }
}

TEST_F(FlatlandTest, MultiplePresentTokensAvailable) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  // Return one extra present token, meaning the instance now has two.
  flatland->OnPresentProcessed(1, {});

  // Present, but return no tokens so the client has only one left.
  {
    PresentArgs args;
    args.present_tokens_returned = 0;
    PRESENT_WITH_ARGS(flatland, std::move(args), true);
  }

  // Present again, which should succeed because the client already has an extra token even though
  // the previous PRESENT_WITH_ARGS returned none.
  {
    PresentArgs args;
    args.present_tokens_returned = 0;
    PRESENT_WITH_ARGS(flatland, std::move(args), true);
  }

  // A third Present() will fail since the previous two calls consumed the two tokens.
  {
    PresentArgs args;
    args.expected_error = fuchsia::ui::scenic::internal::Error::NO_PRESENTS_REMAINING;
    PRESENT_WITH_ARGS(flatland, std::move(args), false);
  }
}

TEST_F(FlatlandTest, PresentWithNoFieldsSet) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  const bool kDefaultSquashable = true;
  const zx::time kDefaultRequestedPresentationTime = zx::time(0);

  EXPECT_CALL(*mock_flatland_presenter_, RegisterPresent(flatland->GetRoot().GetInstanceId(), _));
  bool processed_callback = false;
  fuchsia::ui::scenic::internal::PresentArgs present_args;
  flatland->Present(std::move(present_args), [&](Flatland_Present_Result result) {
    EXPECT_FALSE(result.is_err());
    processed_callback = true;
  });
  EXPECT_TRUE(processed_callback);
  EXPECT_CALL(*mock_flatland_presenter_,
              ScheduleUpdateForSession(kDefaultRequestedPresentationTime, _, kDefaultSquashable));
  RunLoopUntilIdle();
}

TEST_F(FlatlandTest, PresentWaitsForAcquireFences) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  // Create two events to serve as acquire fences.
  PresentArgs args;
  args.acquire_fences = utils::CreateEventArray(2);
  auto acquire1_copy = utils::CopyEvent(args.acquire_fences[0]);
  auto acquire2_copy = utils::CopyEvent(args.acquire_fences[1]);

  // Create an event to serve as a release fence.
  args.release_fences = utils::CreateEventArray(1);
  auto release_copy = utils::CopyEvent(args.release_fences[0]);

  // Because the Present includes acquire fences, it should only be registered with the
  // FlatlandPresenter. The UberStructSystem shouldn't have any entries and applying session
  // updates shouldn't signal the release fence.
  PRESENT_WITH_ARGS(flatland, std::move(args), true);

  auto registered_presents = GetRegisteredPresents(flatland->GetRoot().GetInstanceId());
  EXPECT_EQ(registered_presents.size(), 1ul);

  EXPECT_EQ(GetUberStruct(flatland.get()), nullptr);

  EXPECT_FALSE(utils::IsEventSignalled(release_copy, ZX_EVENT_SIGNALED));

  // Signal the second fence and ensure the Present is still registered, the UberStructSystem
  // doesn't update, and the release fence isn't signaled.
  acquire2_copy.signal(0, ZX_EVENT_SIGNALED);
  RunLoopUntilIdle();
  ApplySessionUpdatesAndSignalFences();

  registered_presents = GetRegisteredPresents(flatland->GetRoot().GetInstanceId());
  EXPECT_EQ(registered_presents.size(), 1ul);

  EXPECT_EQ(GetUberStruct(flatland.get()), nullptr);

  EXPECT_FALSE(utils::IsEventSignalled(release_copy, ZX_EVENT_SIGNALED));

  // Signal the first fence and ensure the Present is no longer registered (because it has been
  // applied), the UberStructSystem contains an UberStruct for the instance, and the release fence
  // is signaled.
  acquire1_copy.signal(0, ZX_EVENT_SIGNALED);

  EXPECT_CALL(*mock_flatland_presenter_, ScheduleUpdateForSession(_, _, _));
  RunLoopUntilIdle();

  ApplySessionUpdatesAndSignalFences();

  registered_presents = GetRegisteredPresents(flatland->GetRoot().GetInstanceId());
  EXPECT_TRUE(registered_presents.empty());

  EXPECT_NE(GetUberStruct(flatland.get()), nullptr);

  EXPECT_TRUE(utils::IsEventSignalled(release_copy, ZX_EVENT_SIGNALED));
}

TEST_F(FlatlandTest, PresentForwardsRequestedPresentationTime) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  // Create an event to serve as an acquire fence.
  const zx::time requested_presentation_time = zx::time(123);

  PresentArgs args;
  args.requested_presentation_time = requested_presentation_time;
  args.acquire_fences = utils::CreateEventArray(1);
  auto acquire_copy = utils::CopyEvent(args.acquire_fences[0]);

  // Because the Present includes acquire fences, it should only be registered with the
  // FlatlandPresenter. There should be no requested presentation time.
  PRESENT_WITH_ARGS(flatland, std::move(args), true);

  auto registered_presents = GetRegisteredPresents(flatland->GetRoot().GetInstanceId());
  EXPECT_EQ(registered_presents.size(), 1ul);

  const auto id_pair = scheduling::SchedulingIdPair({
      .session_id = flatland->GetRoot().GetInstanceId(),
      .present_id = registered_presents[0],
  });

  auto maybe_presentation_time = GetRequestedPresentationTime(id_pair);
  EXPECT_FALSE(maybe_presentation_time.has_value());

  // Signal the fence and ensure the Present is still registered, but now with a requested
  // presentation time.
  acquire_copy.signal(0, ZX_EVENT_SIGNALED);

  EXPECT_CALL(*mock_flatland_presenter_, ScheduleUpdateForSession(_, _, _));
  RunLoopUntilIdle();

  registered_presents = GetRegisteredPresents(flatland->GetRoot().GetInstanceId());
  EXPECT_EQ(registered_presents.size(), 1ul);

  maybe_presentation_time = GetRequestedPresentationTime(id_pair);
  EXPECT_TRUE(maybe_presentation_time.has_value());
  EXPECT_EQ(maybe_presentation_time.value(), requested_presentation_time);
}

TEST_F(FlatlandTest, PresentWithSignaledFencesUpdatesImmediately) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  // Create an event to serve as the acquire fence.
  PresentArgs args;
  args.acquire_fences = utils::CreateEventArray(1);
  auto acquire_copy = utils::CopyEvent(args.acquire_fences[0]);

  // Create an event to serve as a release fence.
  args.release_fences = utils::CreateEventArray(1);
  auto release_copy = utils::CopyEvent(args.release_fences[0]);

  // Signal the event before the Present() call.
  acquire_copy.signal(0, ZX_EVENT_SIGNALED);

  // The PresentId is no longer registered because it has been applied, the UberStructSystem should
  // update immediately, and the release fence should be signaled. The PRESENT macro only expects
  // the ScheduleUpdateForSession() call when no acquire fences are present, but since this test
  // specifically tests pre-signaled fences, the EXPECT_CALL must be added here.
  EXPECT_CALL(*mock_flatland_presenter_, ScheduleUpdateForSession(_, _, _));
  PRESENT_WITH_ARGS(flatland, std::move(args), true);

  auto registered_presents = GetRegisteredPresents(flatland->GetRoot().GetInstanceId());
  EXPECT_TRUE(registered_presents.empty());

  EXPECT_NE(GetUberStruct(flatland.get()), nullptr);

  EXPECT_TRUE(utils::IsEventSignalled(release_copy, ZX_EVENT_SIGNALED));
}

TEST_F(FlatlandTest, PresentsUpdateInCallOrder) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  // Create an event to serve as the acquire fence for the first Present().
  PresentArgs args1;
  args1.acquire_fences = utils::CreateEventArray(1);
  auto acquire1_copy = utils::CopyEvent(args1.acquire_fences[0]);

  // Create an event to serve as a release fence.
  args1.release_fences = utils::CreateEventArray(1);
  auto release1_copy = utils::CopyEvent(args1.release_fences[0]);

  // Present, but do not signal the fence, and ensure Present is registered, the UberStructSystem is
  // empty, and the release fence is unsignaled.
  PRESENT_WITH_ARGS(flatland, std::move(args1), true);

  auto registered_presents = GetRegisteredPresents(flatland->GetRoot().GetInstanceId());
  EXPECT_EQ(registered_presents.size(), 1ul);

  EXPECT_EQ(GetUberStruct(flatland.get()), nullptr);

  EXPECT_FALSE(utils::IsEventSignalled(release1_copy, ZX_EVENT_SIGNALED));

  // Create a transform and make it the root.
  const TransformId kId = {1};

  flatland->CreateTransform(kId);
  flatland->SetRootTransform(kId);

  // Create another event to serve as the acquire fence for the second Present().
  PresentArgs args2;
  args2.acquire_fences = utils::CreateEventArray(1);
  auto acquire2_copy = utils::CopyEvent(args2.acquire_fences[0]);

  // Create an event to serve as a release fence.
  args2.release_fences = utils::CreateEventArray(1);
  auto release2_copy = utils::CopyEvent(args2.release_fences[0]);

  // Present, but do not signal the fence, and ensure there are two Presents registered, but the
  // UberStructSystem is still empty and both release fences are unsignaled.
  PRESENT_WITH_ARGS(flatland, std::move(args2), true);

  registered_presents = GetRegisteredPresents(flatland->GetRoot().GetInstanceId());
  EXPECT_EQ(registered_presents.size(), 2ul);

  EXPECT_EQ(GetUberStruct(flatland.get()), nullptr);

  EXPECT_FALSE(utils::IsEventSignalled(release1_copy, ZX_EVENT_SIGNALED));
  EXPECT_FALSE(utils::IsEventSignalled(release2_copy, ZX_EVENT_SIGNALED));

  // Signal the fence for the second Present(). Since the first one is not done, there should still
  // be two Presents registered, no UberStruct for the instance, and neither fence should be
  // signaled.
  acquire2_copy.signal(0, ZX_EVENT_SIGNALED);
  RunLoopUntilIdle();
  ApplySessionUpdatesAndSignalFences();

  registered_presents = GetRegisteredPresents(flatland->GetRoot().GetInstanceId());
  EXPECT_EQ(registered_presents.size(), 2ul);

  EXPECT_EQ(GetUberStruct(flatland.get()), nullptr);

  EXPECT_FALSE(utils::IsEventSignalled(release1_copy, ZX_EVENT_SIGNALED));
  EXPECT_FALSE(utils::IsEventSignalled(release2_copy, ZX_EVENT_SIGNALED));

  // Signal the fence for the first Present(). This should trigger both Presents(), resulting no
  // registered Presents and an UberStruct with a 2-element topology: the local root, and kId.
  acquire1_copy.signal(0, ZX_EVENT_SIGNALED);

  EXPECT_CALL(*mock_flatland_presenter_, ScheduleUpdateForSession(_, _, _)).Times(2);
  RunLoopUntilIdle();

  ApplySessionUpdatesAndSignalFences();

  registered_presents = GetRegisteredPresents(flatland->GetRoot().GetInstanceId());
  EXPECT_TRUE(registered_presents.empty());

  auto uber_struct = GetUberStruct(flatland.get());
  EXPECT_NE(uber_struct, nullptr);
  EXPECT_EQ(uber_struct->local_topology.size(), 2ul);

  EXPECT_TRUE(utils::IsEventSignalled(release1_copy, ZX_EVENT_SIGNALED));
  EXPECT_TRUE(utils::IsEventSignalled(release2_copy, ZX_EVENT_SIGNALED));
}

TEST_F(FlatlandTest, CreateAndReleaseTransformValidCases) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  const TransformId kId1 = {1};
  const TransformId kId2 = {2};

  // Create two transforms.
  flatland->CreateTransform(kId1);
  flatland->CreateTransform(kId2);
  PRESENT(flatland, true);

  // Clear, then create two transforms in the other order.
  flatland->ClearGraph();
  flatland->CreateTransform(kId2);
  flatland->CreateTransform(kId1);
  PRESENT(flatland, true);

  // Clear, create and release transforms, non-overlapping.
  flatland->ClearGraph();
  flatland->CreateTransform(kId1);
  flatland->ReleaseTransform(kId1);
  flatland->CreateTransform(kId2);
  flatland->ReleaseTransform(kId2);
  PRESENT(flatland, true);

  // Clear, create and release transforms, nested.
  flatland->ClearGraph();
  flatland->CreateTransform(kId2);
  flatland->CreateTransform(kId1);
  flatland->ReleaseTransform(kId1);
  flatland->ReleaseTransform(kId2);
  PRESENT(flatland, true);

  // Reuse the same id, legally, in a single present call.
  flatland->CreateTransform(kId1);
  flatland->ReleaseTransform(kId1);
  flatland->CreateTransform(kId1);
  flatland->ClearGraph();
  flatland->CreateTransform(kId1);
  PRESENT(flatland, true);

  // Create and clear, overlapping, with multiple present calls.
  flatland->ClearGraph();
  flatland->CreateTransform(kId2);
  PRESENT(flatland, true);
  flatland->CreateTransform(kId1);
  flatland->ReleaseTransform(kId2);
  PRESENT(flatland, true);
  flatland->ReleaseTransform(kId1);
  PRESENT(flatland, true);
}

TEST_F(FlatlandTest, CreateAndReleaseTransformErrorCases) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  const TransformId kId1 = {1};
  const TransformId kId2 = {2};

  // Zero is not a valid transform id.
  flatland->CreateTransform({0});
  PRESENT(flatland, false);
  flatland->ReleaseTransform({0});
  PRESENT(flatland, false);

  // Double creation is an error.
  flatland->CreateTransform(kId1);
  flatland->CreateTransform(kId1);
  PRESENT(flatland, false);

  // Releasing a non-existent transform is an error.
  flatland->ReleaseTransform(kId2);
  PRESENT(flatland, false);
}

TEST_F(FlatlandTest, AddAndRemoveChildValidCases) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  const TransformId kIdParent = {1};
  const TransformId kIdChild1 = {2};
  const TransformId kIdChild2 = {3};
  const TransformId kIdGrandchild = {4};

  flatland->CreateTransform(kIdParent);
  flatland->CreateTransform(kIdChild1);
  flatland->CreateTransform(kIdChild2);
  flatland->CreateTransform(kIdGrandchild);
  PRESENT(flatland, true);

  // Add and remove.
  flatland->AddChild(kIdParent, kIdChild1);
  flatland->RemoveChild(kIdParent, kIdChild1);
  PRESENT(flatland, true);

  // Add two children.
  flatland->AddChild(kIdParent, kIdChild1);
  flatland->AddChild(kIdParent, kIdChild2);
  PRESENT(flatland, true);

  // Remove two children.
  flatland->RemoveChild(kIdParent, kIdChild1);
  flatland->RemoveChild(kIdParent, kIdChild2);
  PRESENT(flatland, true);

  // Add two-deep hierarchy.
  flatland->AddChild(kIdParent, kIdChild1);
  flatland->AddChild(kIdChild1, kIdGrandchild);
  PRESENT(flatland, true);

  // Add sibling.
  flatland->AddChild(kIdParent, kIdChild2);
  PRESENT(flatland, true);

  // Add shared grandchild (deadly diamond dependency).
  flatland->AddChild(kIdChild2, kIdGrandchild);
  PRESENT(flatland, true);

  // Remove original deep-hierarchy.
  flatland->RemoveChild(kIdChild1, kIdGrandchild);
  PRESENT(flatland, true);
}

TEST_F(FlatlandTest, AddAndRemoveChildErrorCases) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  const TransformId kIdParent = {1};
  const TransformId kIdChild = {2};
  const TransformId kIdNotCreated = {3};

  // Setup.
  flatland->CreateTransform(kIdParent);
  flatland->CreateTransform(kIdChild);
  flatland->AddChild(kIdParent, kIdChild);
  PRESENT(flatland, true);

  // Zero is not a valid transform id.
  flatland->AddChild({0}, {0});
  PRESENT(flatland, false);
  flatland->AddChild(kIdParent, {0});
  PRESENT(flatland, false);
  flatland->AddChild({0}, kIdChild);
  PRESENT(flatland, false);

  // Child does not exist.
  flatland->AddChild(kIdParent, kIdNotCreated);
  PRESENT(flatland, false);
  flatland->RemoveChild(kIdParent, kIdNotCreated);
  PRESENT(flatland, false);

  // Parent does not exist.
  flatland->AddChild(kIdNotCreated, kIdChild);
  PRESENT(flatland, false);
  flatland->RemoveChild(kIdNotCreated, kIdChild);
  PRESENT(flatland, false);

  // Child is already a child of parent->
  flatland->AddChild(kIdParent, kIdChild);
  PRESENT(flatland, false);

  // Both nodes exist, but not in the correct relationship.
  flatland->RemoveChild(kIdChild, kIdParent);
  PRESENT(flatland, false);
}

// Test that Transforms can be children to multiple different parents.
TEST_F(FlatlandTest, MultichildUsecase) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  const TransformId kIdParent1 = {1};
  const TransformId kIdParent2 = {2};
  const TransformId kIdChild1 = {3};
  const TransformId kIdChild2 = {4};
  const TransformId kIdChild3 = {5};

  // Setup
  flatland->CreateTransform(kIdParent1);
  flatland->CreateTransform(kIdParent2);
  flatland->CreateTransform(kIdChild1);
  flatland->CreateTransform(kIdChild2);
  flatland->CreateTransform(kIdChild3);
  PRESENT(flatland, true);

  // Add all children to first parent->
  flatland->AddChild(kIdParent1, kIdChild1);
  flatland->AddChild(kIdParent1, kIdChild2);
  flatland->AddChild(kIdParent1, kIdChild3);
  PRESENT(flatland, true);

  // Add all children to second parent->
  flatland->AddChild(kIdParent2, kIdChild1);
  flatland->AddChild(kIdParent2, kIdChild2);
  flatland->AddChild(kIdParent2, kIdChild3);
  PRESENT(flatland, true);
}

// Test that Present() fails if it detects a graph cycle.
TEST_F(FlatlandTest, CycleDetector) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  const TransformId kId1 = {1};
  const TransformId kId2 = {2};
  const TransformId kId3 = {3};
  const TransformId kId4 = {4};

  // Create an immediate cycle.
  {
    flatland->CreateTransform(kId1);
    flatland->AddChild(kId1, kId1);
    PRESENT(flatland, false);
  }

  // Create a legal chain of depth one.
  // Then, create a cycle of length 2.
  {
    flatland->ClearGraph();
    flatland->CreateTransform(kId1);
    flatland->CreateTransform(kId2);
    flatland->AddChild(kId1, kId2);
    PRESENT(flatland, true);

    flatland->AddChild(kId2, kId1);
    PRESENT(flatland, false);
  }

  // Create two legal chains of length one.
  // Then, connect each chain into a cycle of length four.
  {
    flatland->ClearGraph();
    flatland->CreateTransform(kId1);
    flatland->CreateTransform(kId2);
    flatland->CreateTransform(kId3);
    flatland->CreateTransform(kId4);
    flatland->AddChild(kId1, kId2);
    flatland->AddChild(kId3, kId4);
    PRESENT(flatland, true);

    flatland->AddChild(kId2, kId3);
    flatland->AddChild(kId4, kId1);
    PRESENT(flatland, false);
  }

  // Create a cycle, where the root is not involved in the cycle.
  {
    flatland->ClearGraph();
    flatland->CreateTransform(kId1);
    flatland->CreateTransform(kId2);
    flatland->CreateTransform(kId3);
    flatland->CreateTransform(kId4);

    flatland->AddChild(kId1, kId2);
    flatland->AddChild(kId2, kId3);
    flatland->AddChild(kId3, kId2);
    flatland->AddChild(kId3, kId4);

    flatland->SetRootTransform(kId1);
    flatland->ReleaseTransform(kId1);
    flatland->ReleaseTransform(kId2);
    flatland->ReleaseTransform(kId3);
    flatland->ReleaseTransform(kId4);
    PRESENT(flatland, false);
  }
}

TEST_F(FlatlandTest, SetRootTransform) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  const TransformId kId1 = {1};
  const TransformId kIdNotCreated = {2};

  flatland->CreateTransform(kId1);
  PRESENT(flatland, true);

  // Even with no root transform, so clearing it is not an error.
  flatland->SetRootTransform({0});
  PRESENT(flatland, true);

  // Setting the root to an unknown transform is an error.
  flatland->SetRootTransform(kIdNotCreated);
  PRESENT(flatland, false);

  flatland->SetRootTransform(kId1);
  PRESENT(flatland, true);

  // Setting the root to a non-existent transform does not clear the root, which means the local
  // topology will contain two handles: the "local root" and kId1.
  auto uber_struct = GetUberStruct(flatland.get());
  EXPECT_EQ(uber_struct->local_topology.size(), 2ul);

  flatland->SetRootTransform(kIdNotCreated);
  PRESENT(flatland, false);

  // The previous Present() fails, so we Present() again to ensure the UberStruct is updated,
  // even though we expect no changes.
  PRESENT(flatland, true);

  uber_struct = GetUberStruct(flatland.get());
  EXPECT_EQ(uber_struct->local_topology.size(), 2ul);

  // Releasing the root is allowed, though it will remain in the hierarchy until reset.
  flatland->ReleaseTransform(kId1);
  PRESENT(flatland, true);

  uber_struct = GetUberStruct(flatland.get());
  EXPECT_EQ(uber_struct->local_topology.size(), 2ul);

  // Clearing the root after release is also allowed.
  flatland->SetRootTransform({0});
  PRESENT(flatland, true);

  // Setting the root to a released transform is not allowed.
  flatland->SetRootTransform(kId1);
  PRESENT(flatland, false);
}

TEST_F(FlatlandTest, SetTranslationErrorCases) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  const TransformId kIdNotCreated = {1};

  // Zero is not a valid transform ID.
  flatland->SetTranslation({0}, {1.f, 2.f});
  PRESENT(flatland, false);

  // Transform does not exist.
  flatland->SetTranslation(kIdNotCreated, {1.f, 2.f});
  PRESENT(flatland, false);
}

TEST_F(FlatlandTest, SetOrientationErrorCases) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  const TransformId kIdNotCreated = {1};

  // Zero is not a valid transform ID.
  flatland->SetOrientation({0}, Orientation::CCW_90_DEGREES);
  PRESENT(flatland, false);

  // Transform does not exist.
  flatland->SetOrientation(kIdNotCreated, Orientation::CCW_90_DEGREES);
  PRESENT(flatland, false);
}

TEST_F(FlatlandTest, SetScaleErrorCases) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  const TransformId kIdNotCreated = {1};

  // Zero is not a valid transform ID.
  flatland->SetScale({0}, {1.f, 2.f});
  PRESENT(flatland, false);

  // Transform does not exist.
  flatland->SetScale(kIdNotCreated, {1.f, 2.f});
  PRESENT(flatland, false);
}

// Test that changing geometric transform properties affects the local matrix of Transforms.
TEST_F(FlatlandTest, SetGeometricTransformProperties) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  // Create two Transforms to ensure properties are local to individual Transforms.
  const TransformId kId1 = {1};
  const TransformId kId2 = {2};

  flatland->CreateTransform(kId1);
  flatland->CreateTransform(kId2);

  flatland->SetRootTransform(kId1);
  flatland->AddChild(kId1, kId2);

  PRESENT(flatland, true);

  // Get the TransformHandles for kId1 and kId2.
  auto uber_struct = GetUberStruct(flatland.get());
  ASSERT_EQ(uber_struct->local_topology.size(), 3ul);
  ASSERT_EQ(uber_struct->local_topology[0].handle, flatland->GetRoot());

  const auto handle1 = uber_struct->local_topology[1].handle;
  const auto handle2 = uber_struct->local_topology[2].handle;

  // The local topology will always have 3 transforms: the local root, kId1, and kId2. With no
  // properties set, there will be no local matrices.
  uber_struct = GetUberStruct(flatland.get());
  EXPECT_TRUE(uber_struct->local_matrices.empty());

  // Set up one property per transform.
  flatland->SetTranslation(kId1, {1.f, 2.f});
  flatland->SetScale(kId2, {2.f, 3.f});
  PRESENT(flatland, true);

  // The two handles should have the expected matrices.
  uber_struct = GetUberStruct(flatland.get());
  EXPECT_MATRIX(uber_struct, handle1, glm::translate(glm::mat3(), {1.f, 2.f}));
  EXPECT_MATRIX(uber_struct, handle2, glm::scale(glm::mat3(), {2.f, 3.f}));

  // Fill out the remaining properties on both transforms.
  flatland->SetOrientation(kId1, Orientation::CCW_90_DEGREES);
  flatland->SetScale(kId1, {4.f, 5.f});

  flatland->SetTranslation(kId2, {6.f, 7.f});
  flatland->SetOrientation(kId2, Orientation::CCW_270_DEGREES);

  PRESENT(flatland, true);

  // Verify the new properties were applied in the correct orders.
  uber_struct = GetUberStruct(flatland.get());

  glm::mat3 matrix1 = glm::mat3();
  matrix1 = glm::translate(matrix1, {1.f, 2.f});
  matrix1 = glm::rotate(matrix1, GetOrientationAngle(Orientation::CCW_90_DEGREES));
  matrix1 = glm::scale(matrix1, {4.f, 5.f});
  EXPECT_MATRIX(uber_struct, handle1, matrix1);

  glm::mat3 matrix2 = glm::mat3();
  matrix2 = glm::translate(matrix2, {6.f, 7.f});
  matrix2 = glm::rotate(matrix2, GetOrientationAngle(Orientation::CCW_270_DEGREES));
  matrix2 = glm::scale(matrix2, {2.f, 3.f});
  EXPECT_MATRIX(uber_struct, handle2, matrix2);
}

// Ensure that local matrix data is only cleaned up when a Transform is completely unreferenced,
// meaning no Transforms reference it as a child->
TEST_F(FlatlandTest, MatrixReleasesWhenTransformNotReferenced) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  // Create two Transforms to ensure properties are local to individual Transforms.
  const TransformId kId1 = {1};
  const TransformId kId2 = {2};

  flatland->CreateTransform(kId1);
  flatland->CreateTransform(kId2);

  flatland->SetRootTransform(kId1);
  flatland->AddChild(kId1, kId2);

  PRESENT(flatland, true);

  // Get the TransformHandles for kId1 and kId2.
  auto uber_struct = GetUberStruct(flatland.get());
  ASSERT_EQ(uber_struct->local_topology.size(), 3ul);
  ASSERT_EQ(uber_struct->local_topology[0].handle, flatland->GetRoot());

  const auto handle1 = uber_struct->local_topology[1].handle;
  const auto handle2 = uber_struct->local_topology[2].handle;

  // Set a geometric property on kId1.
  flatland->SetTranslation(kId1, {1.f, 2.f});
  PRESENT(flatland, true);

  // Only handle1 should have a local matrix.
  uber_struct = GetUberStruct(flatland.get());
  EXPECT_MATRIX(uber_struct, handle1, glm::translate(glm::mat3(), {1.f, 2.f}));

  // Release kId1, but ensure its matrix stays around.
  flatland->ReleaseTransform(kId1);
  PRESENT(flatland, true);

  uber_struct = GetUberStruct(flatland.get());
  EXPECT_MATRIX(uber_struct, handle1, glm::translate(glm::mat3(), {1.f, 2.f}));

  // Clear kId1 as the root transform, which should clear the matrix.
  flatland->SetRootTransform({0});
  PRESENT(flatland, true);

  uber_struct = GetUberStruct(flatland.get());
  EXPECT_TRUE(uber_struct->local_matrices.empty());
}

TEST_F(FlatlandTest, GraphLinkReplaceWithoutConnection) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  ContentLinkToken parent_token;
  GraphLinkToken child_token;
  ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token.value, &child_token.value));

  fidl::InterfacePtr<GraphLink> graph_link;
  flatland->LinkToParent(std::move(child_token), graph_link.NewRequest());
  PRESENT(flatland, true);

  ContentLinkToken parent_token2;
  GraphLinkToken child_token2;
  ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token2.value, &child_token2.value));

  fidl::InterfacePtr<GraphLink> graph_link2;
  flatland->LinkToParent(std::move(child_token2), graph_link2.NewRequest());

  RunLoopUntilIdle();

  // Until Present() is called, the previous GraphLink is not unbound.
  EXPECT_TRUE(graph_link.is_bound());
  EXPECT_TRUE(graph_link2.is_bound());

  PRESENT(flatland, true);

  EXPECT_FALSE(graph_link.is_bound());
  EXPECT_TRUE(graph_link2.is_bound());
}

TEST_F(FlatlandTest, GraphLinkReplaceWithConnection) {
  std::shared_ptr<Flatland> parent = CreateFlatland();
  std::shared_ptr<Flatland> child = CreateFlatland();

  const ContentId kLinkId1 = {1};

  fidl::InterfacePtr<ContentLink> content_link;
  fidl::InterfacePtr<GraphLink> graph_link;
  CreateLink(parent.get(), child.get(), kLinkId1, &content_link, &graph_link);

  fidl::InterfacePtr<GraphLink> graph_link2;

  // Don't use the helper function for the second link to test when the previous links are closed.
  ContentLinkToken parent_token;
  GraphLinkToken child_token;
  ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token.value, &child_token.value));

  // Creating the new GraphLink doesn't invalidate either of the old links until Present() is
  // called on the child->
  child->LinkToParent(std::move(child_token), graph_link2.NewRequest());

  RunLoopUntilIdle();

  EXPECT_TRUE(content_link.is_bound());
  EXPECT_TRUE(graph_link.is_bound());
  EXPECT_TRUE(graph_link2.is_bound());

  // Present() replaces the original GraphLink, which also results in the invalidation of both ends
  // of the original link.
  PRESENT(child, true);

  EXPECT_FALSE(content_link.is_bound());
  EXPECT_FALSE(graph_link.is_bound());
  EXPECT_TRUE(graph_link2.is_bound());
}

TEST_F(FlatlandTest, GraphLinkUnbindsOnParentDeath) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  ContentLinkToken parent_token;
  GraphLinkToken child_token;
  ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token.value, &child_token.value));

  fidl::InterfacePtr<GraphLink> graph_link;
  flatland->LinkToParent(std::move(child_token), graph_link.NewRequest());
  PRESENT(flatland, true);

  parent_token.value.reset();
  RunLoopUntilIdle();

  EXPECT_FALSE(graph_link.is_bound());
}

TEST_F(FlatlandTest, GraphLinkUnbindsImmediatelyWithInvalidToken) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  GraphLinkToken child_token;

  fidl::InterfacePtr<GraphLink> graph_link;
  flatland->LinkToParent(std::move(child_token), graph_link.NewRequest());

  // The link will be unbound even before Present() is called.
  RunLoopUntilIdle();
  EXPECT_FALSE(graph_link.is_bound());

  PRESENT(flatland, false);
}

TEST_F(FlatlandTest, GraphUnlinkFailsWithoutLink) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  flatland->UnlinkFromParent([](GraphLinkToken token) { EXPECT_TRUE(false); });

  PRESENT(flatland, false);
}

TEST_F(FlatlandTest, GraphUnlinkReturnsOrphanedTokenOnParentDeath) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  ContentLinkToken parent_token;
  GraphLinkToken child_token;
  ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token.value, &child_token.value));

  fidl::InterfacePtr<GraphLink> graph_link;
  flatland->LinkToParent(std::move(child_token), graph_link.NewRequest());
  PRESENT(flatland, true);

  // Killing the peer token does not prevent the instance from returning a valid token.
  parent_token.value.reset();
  RunLoopUntilIdle();

  GraphLinkToken graph_token;
  flatland->UnlinkFromParent(
      [&graph_token](GraphLinkToken token) { graph_token = std::move(token); });
  PRESENT(flatland, true);

  EXPECT_TRUE(graph_token.value.is_valid());

  // But trying to link with that token will immediately fail because it is already orphaned.
  fidl::InterfacePtr<GraphLink> graph_link2;
  flatland->LinkToParent(std::move(graph_token), graph_link2.NewRequest());
  PRESENT(flatland, true);

  EXPECT_FALSE(graph_link2.is_bound());
}

TEST_F(FlatlandTest, GraphUnlinkReturnsOriginalToken) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  ContentLinkToken parent_token;
  GraphLinkToken child_token;
  ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token.value, &child_token.value));

  const zx_koid_t expected_koid = fsl::GetKoid(child_token.value.get());

  fidl::InterfacePtr<GraphLink> graph_link;
  flatland->LinkToParent(std::move(child_token), graph_link.NewRequest());
  PRESENT(flatland, true);

  GraphLinkToken graph_token;
  flatland->UnlinkFromParent(
      [&graph_token](GraphLinkToken token) { graph_token = std::move(token); });

  RunLoopUntilIdle();

  // Until Present() is called and the acquire fence is signaled, the previous GraphLink is not
  // unbound.
  EXPECT_TRUE(graph_link.is_bound());
  EXPECT_FALSE(graph_token.value.is_valid());

  PresentArgs args;
  args.acquire_fences = utils::CreateEventArray(1);
  auto event_copy = utils::CopyEvent(args.acquire_fences[0]);

  PRESENT_WITH_ARGS(flatland, std::move(args), true);

  EXPECT_TRUE(graph_link.is_bound());
  EXPECT_FALSE(graph_token.value.is_valid());

  // Signal the acquire fence to unbind the link.
  event_copy.signal(0, ZX_EVENT_SIGNALED);

  EXPECT_CALL(*mock_flatland_presenter_, ScheduleUpdateForSession(_, _, _));
  RunLoopUntilIdle();

  EXPECT_FALSE(graph_link.is_bound());
  EXPECT_TRUE(graph_token.value.is_valid());
  EXPECT_EQ(fsl::GetKoid(graph_token.value.get()), expected_koid);
}

TEST_F(FlatlandTest, ContentLinkUnbindsOnChildDeath) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  ContentLinkToken parent_token;
  GraphLinkToken child_token;
  ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token.value, &child_token.value));

  const ContentId kLinkId1 = {1};

  fidl::InterfacePtr<ContentLink> content_link;
  LinkProperties properties;
  properties.set_logical_size({kDefaultSize, kDefaultSize});
  flatland->CreateLink(kLinkId1, std::move(parent_token), std::move(properties),
                       content_link.NewRequest());
  PRESENT(flatland, true);

  child_token.value.reset();
  RunLoopUntilIdle();

  EXPECT_FALSE(content_link.is_bound());
}

TEST_F(FlatlandTest, ContentLinkUnbindsImmediatelyWithInvalidToken) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  ContentLinkToken parent_token;

  const ContentId kLinkId1 = {1};

  fidl::InterfacePtr<ContentLink> content_link;
  flatland->CreateLink(kLinkId1, std::move(parent_token), {}, content_link.NewRequest());

  // The link will be unbound even before Present() is called.
  RunLoopUntilIdle();
  EXPECT_FALSE(content_link.is_bound());

  PRESENT(flatland, false);
}

TEST_F(FlatlandTest, ContentLinkFailsIdIsZero) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  ContentLinkToken parent_token;
  GraphLinkToken child_token;
  ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token.value, &child_token.value));

  fidl::InterfacePtr<ContentLink> content_link;
  LinkProperties properties;
  properties.set_logical_size({kDefaultSize, kDefaultSize});
  flatland->CreateLink({0}, std::move(parent_token), std::move(properties),
                       content_link.NewRequest());
  PRESENT(flatland, false);
}

TEST_F(FlatlandTest, ContentLinkFailsNoLogicalSize) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  ContentLinkToken parent_token;
  GraphLinkToken child_token;
  ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token.value, &child_token.value));

  fidl::InterfacePtr<ContentLink> content_link;
  LinkProperties properties;
  flatland->CreateLink({0}, std::move(parent_token), std::move(properties),
                       content_link.NewRequest());
  PRESENT(flatland, false);
}

TEST_F(FlatlandTest, ContentLinkFailsInvalidLogicalSize) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  ContentLinkToken parent_token;
  GraphLinkToken child_token;
  ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token.value, &child_token.value));

  fidl::InterfacePtr<ContentLink> content_link;

  // The X value must be positive.
  LinkProperties properties;
  properties.set_logical_size({0.f, kDefaultSize});
  flatland->CreateLink({0}, std::move(parent_token), std::move(properties),
                       content_link.NewRequest());
  PRESENT(flatland, false);

  ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token.value, &child_token.value));

  // The Y value must be positive.
  LinkProperties properties2;
  properties2.set_logical_size({kDefaultSize, 0.f});
  flatland->CreateLink({0}, std::move(parent_token), std::move(properties2),
                       content_link.NewRequest());
  PRESENT(flatland, false);
}

TEST_F(FlatlandTest, ContentLinkFailsIdCollision) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  ContentLinkToken parent_token;
  GraphLinkToken child_token;
  ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token.value, &child_token.value));

  const ContentId kId1 = {1};

  fidl::InterfacePtr<ContentLink> content_link;
  LinkProperties properties;
  properties.set_logical_size({kDefaultSize, kDefaultSize});
  flatland->CreateLink(kId1, std::move(parent_token), std::move(properties),
                       content_link.NewRequest());
  PRESENT(flatland, true);

  ContentLinkToken parent_token2;
  GraphLinkToken child_token2;
  ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token2.value, &child_token2.value));

  flatland->CreateLink(kId1, std::move(parent_token2), std::move(properties),
                       content_link.NewRequest());
  PRESENT(flatland, false);
}

TEST_F(FlatlandTest, ClearGraphDelaysLinkDestructionUntilPresent) {
  std::shared_ptr<Flatland> parent = CreateFlatland();
  std::shared_ptr<Flatland> child = CreateFlatland();

  const ContentId kLinkId1 = {1};

  fidl::InterfacePtr<ContentLink> content_link;
  fidl::InterfacePtr<GraphLink> graph_link;
  CreateLink(parent.get(), child.get(), kLinkId1, &content_link, &graph_link);

  EXPECT_TRUE(content_link.is_bound());
  EXPECT_TRUE(graph_link.is_bound());

  // Clearing the parent graph should not unbind the interfaces until Present() is called and the
  // acquire fence is signaled.
  parent->ClearGraph();
  RunLoopUntilIdle();

  EXPECT_TRUE(content_link.is_bound());
  EXPECT_TRUE(graph_link.is_bound());

  PresentArgs args;
  args.acquire_fences = utils::CreateEventArray(1);
  auto event_copy = utils::CopyEvent(args.acquire_fences[0]);

  PRESENT_WITH_ARGS(parent, std::move(args), true);

  EXPECT_TRUE(content_link.is_bound());
  EXPECT_TRUE(graph_link.is_bound());

  // Signal the acquire fence to unbind the links.
  event_copy.signal(0, ZX_EVENT_SIGNALED);

  EXPECT_CALL(*mock_flatland_presenter_, ScheduleUpdateForSession(_, _, _));
  RunLoopUntilIdle();

  EXPECT_FALSE(content_link.is_bound());
  EXPECT_FALSE(graph_link.is_bound());

  // Recreate the Link. The parent graph was cleared so we can reuse the LinkId.
  CreateLink(parent.get(), child.get(), kLinkId1, &content_link, &graph_link);

  EXPECT_TRUE(content_link.is_bound());
  EXPECT_TRUE(graph_link.is_bound());

  // Clearing the child graph should not unbind the interfaces until Present() is called and the
  // acquire fence is signaled.
  child->ClearGraph();
  RunLoopUntilIdle();

  EXPECT_TRUE(content_link.is_bound());
  EXPECT_TRUE(graph_link.is_bound());

  PresentArgs args2;
  args2.acquire_fences = utils::CreateEventArray(1);
  event_copy = utils::CopyEvent(args2.acquire_fences[0]);

  PRESENT_WITH_ARGS(child, std::move(args2), true);

  EXPECT_TRUE(content_link.is_bound());
  EXPECT_TRUE(graph_link.is_bound());

  // Signal the acquire fence to unbind the links.
  event_copy.signal(0, ZX_EVENT_SIGNALED);

  EXPECT_CALL(*mock_flatland_presenter_, ScheduleUpdateForSession(_, _, _));
  RunLoopUntilIdle();

  EXPECT_FALSE(content_link.is_bound());
  EXPECT_FALSE(graph_link.is_bound());
}

// This test doesn't use the helper function to create a link, because it tests intermediate steps
// and timing corner cases.
TEST_F(FlatlandTest, ChildGetsLayoutUpdateWithoutPresenting) {
  std::shared_ptr<Flatland> parent = CreateFlatland();
  std::shared_ptr<Flatland> child = CreateFlatland();

  // Set up a link, but don't call Present() on either instance.
  ContentLinkToken parent_token;
  GraphLinkToken child_token;
  ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token.value, &child_token.value));

  const ContentId kLinkId = {1};

  fidl::InterfacePtr<ContentLink> content_link;
  LinkProperties properties;
  properties.set_logical_size({1.0f, 2.0f});
  parent->CreateLink(kLinkId, std::move(parent_token), std::move(properties),
                     content_link.NewRequest());

  fidl::InterfacePtr<GraphLink> graph_link;
  child->LinkToParent(std::move(child_token), graph_link.NewRequest());

  // Request a layout update.
  bool layout_updated = false;
  graph_link->GetLayout([&](LayoutInfo info) {
    EXPECT_EQ(1.0f, info.logical_size().x);
    EXPECT_EQ(2.0f, info.logical_size().y);
    layout_updated = true;
  });

  // Without even presenting, the child is able to get the initial properties from the parent->
  UpdateLinks(parent->GetRoot());
  EXPECT_TRUE(layout_updated);
}

TEST_F(FlatlandTest, OverwrittenHangingGetsReturnError) {
  std::shared_ptr<Flatland> parent = CreateFlatland();
  std::shared_ptr<Flatland> child = CreateFlatland();

  // Set up a link, but don't call Present() on either instance.
  ContentLinkToken parent_token;
  GraphLinkToken child_token;
  ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token.value, &child_token.value));

  const ContentId kLinkId = {1};
  fidl::InterfacePtr<ContentLink> content_link;
  LinkProperties properties;
  properties.set_logical_size({1.0f, 2.0f});
  parent->CreateLink(kLinkId, std::move(parent_token), std::move(properties),
                     content_link.NewRequest());

  fidl::InterfacePtr<GraphLink> graph_link;
  child->LinkToParent(std::move(child_token), graph_link.NewRequest());
  UpdateLinks(parent->GetRoot());

  // First layout request should succeed immediately.
  bool layout_updated = false;
  graph_link->GetLayout([&](auto) { layout_updated = true; });
  RunLoopUntilIdle();
  EXPECT_TRUE(layout_updated);

  // Queue overwriting hanging gets.
  layout_updated = false;
  graph_link->GetLayout([&](auto) { layout_updated = true; });
  graph_link->GetLayout([&](auto) { layout_updated = true; });
  RunLoopUntilIdle();
  EXPECT_FALSE(layout_updated);

  // Present should fail on child because the client has broken flow control.
  PresentArgs args;
  args.expected_error = fuchsia::ui::scenic::internal::Error::BAD_HANGING_GET;
  PRESENT_WITH_ARGS(child, std::move(args), false);
}

TEST_F(FlatlandTest, HangingGetsReturnOnCorrectDispatcher) {
  ContentLinkToken parent_token;
  GraphLinkToken child_token;
  ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token.value, &child_token.value));

  // Create the parent Flatland session using another loop.
  async::TestLoop parent_loop;
  auto session_id = scheduling::GetNextSessionId();
  std::vector<std::shared_ptr<BufferCollectionImporter>> importers;
  importers.push_back(buffer_collection_importer_);
  fuchsia::ui::scenic::internal::FlatlandPtr parent_ptr;
  std::shared_ptr<Flatland> parent = Flatland::New(
      std::make_shared<utils::UnownedDispatcherHolder>(parent_loop.dispatcher()),
      parent_ptr.NewRequest(), session_id,
      /*destroy_instance_functon=*/[]() {}, flatland_presenter_, link_system_,
      uber_struct_system_->AllocateQueueForSession(session_id), importers);

  // Create parent link.
  const ContentId kLinkId = {1};
  fidl::InterfacePtr<ContentLink> content_link;
  LinkProperties properties;
  properties.set_logical_size({1.0f, 2.0f});
  parent_ptr->CreateLink(kLinkId, std::move(parent_token), std::move(properties),
                         content_link.NewRequest());
  EXPECT_TRUE(parent_loop.RunUntilIdle());

  // Create the child Flatland session using another loop.
  async::TestLoop child_loop;
  session_id = scheduling::GetNextSessionId();
  fuchsia::ui::scenic::internal::FlatlandPtr child_ptr;
  std::shared_ptr<Flatland> child = Flatland::New(
      std::make_shared<utils::UnownedDispatcherHolder>(child_loop.dispatcher()),
      child_ptr.NewRequest(), session_id,
      /*destroy_instance_functon=*/[]() {}, flatland_presenter_, link_system_,
      uber_struct_system_->AllocateQueueForSession(session_id), importers);

  // Create child link.
  fidl::InterfacePtr<GraphLink> graph_link;
  child_ptr->LinkToParent(std::move(child_token), graph_link.NewRequest());
  EXPECT_TRUE(child_loop.RunUntilIdle());

  // Complete linking sessions.
  UpdateLinks(parent->GetRoot());

  // Send the first GetLayout hanging get which should have an immediate answer.
  bool layout_updated = false;
  graph_link->GetLayout([&](auto) { layout_updated = true; });

  // Process the request on child's loop.
  EXPECT_TRUE(child_loop.RunUntilIdle());

  // Process the response on parent's loop. Response should not run yet because it is posted on
  // child's loop.
  EXPECT_TRUE(parent_loop.RunUntilIdle());
  EXPECT_FALSE(layout_updated);

  // Run the response on child's loop.
  EXPECT_TRUE(child_loop.RunUntilIdle());
  EXPECT_TRUE(layout_updated);

  // Send overwriting hanging gets that will cause an error.
  layout_updated = false;
  graph_link->GetLayout([&](auto) { layout_updated = true; });
  graph_link->GetLayout([&](auto) { layout_updated = true; });

  // Overwriting hanging gets should cause an error on child's loop as we process the request.
  EXPECT_TRUE(child_loop.RunUntilIdle());
  PresentArgs args;
  args.expected_error = fuchsia::ui::scenic::internal::Error::BAD_HANGING_GET;
  PRESENT_WITH_ARGS(child, std::move(args), false);
}

// This test doesn't use the helper function to create a link, because it tests intermediate steps
// and timing corner cases.
TEST_F(FlatlandTest, ConnectedToDisplayParentPresentsBeforeChild) {
  std::shared_ptr<Flatland> parent = CreateFlatland();
  std::shared_ptr<Flatland> child = CreateFlatland();

  // Set up a link and attach it to the parent's root, but don't call Present() on either instance.
  ContentLinkToken parent_token;
  GraphLinkToken child_token;
  ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token.value, &child_token.value));

  const TransformId kTransformId = {1};

  parent->CreateTransform(kTransformId);
  parent->SetRootTransform(kTransformId);

  const ContentId kLinkId = {2};

  fidl::InterfacePtr<ContentLink> content_link;
  LinkProperties properties;
  properties.set_logical_size({1.0f, 2.0f});
  parent->CreateLink(kLinkId, std::move(parent_token), std::move(properties),
                     content_link.NewRequest());
  parent->SetContent(kTransformId, kLinkId);

  fidl::InterfacePtr<GraphLink> graph_link;
  child->LinkToParent(std::move(child_token), graph_link.NewRequest());

  // Request a status update.
  bool status_updated = false;
  graph_link->GetStatus([&](GraphLinkStatus status) {
    EXPECT_EQ(status, GraphLinkStatus::DISCONNECTED_FROM_DISPLAY);
    status_updated = true;
  });

  // The child begins disconnected from the display.
  UpdateLinks(parent->GetRoot());
  EXPECT_TRUE(status_updated);

  // The GraphLinkStatus will update when both the parent and child Present().
  status_updated = false;
  graph_link->GetStatus([&](GraphLinkStatus status) {
    EXPECT_EQ(status, GraphLinkStatus::CONNECTED_TO_DISPLAY);
    status_updated = true;
  });

  // The parent presents first, no update.
  PRESENT(parent, true);
  UpdateLinks(parent->GetRoot());
  EXPECT_FALSE(status_updated);

  // The child presents second and the status updates.
  PRESENT(child, true);
  UpdateLinks(parent->GetRoot());
  EXPECT_TRUE(status_updated);
}

// This test doesn't use the helper function to create a link, because it tests intermediate steps
// and timing corner cases.
TEST_F(FlatlandTest, ConnectedToDisplayChildPresentsBeforeParent) {
  std::shared_ptr<Flatland> parent = CreateFlatland();
  std::shared_ptr<Flatland> child = CreateFlatland();

  // Set up a link and attach it to the parent's root, but don't call Present() on either instance.
  ContentLinkToken parent_token;
  GraphLinkToken child_token;
  ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token.value, &child_token.value));

  const TransformId kTransformId = {1};

  parent->CreateTransform(kTransformId);
  parent->SetRootTransform(kTransformId);

  const ContentId kLinkId = {2};

  fidl::InterfacePtr<ContentLink> content_link;
  LinkProperties properties;
  properties.set_logical_size({1.0f, 2.0f});
  parent->CreateLink(kLinkId, std::move(parent_token), std::move(properties),
                     content_link.NewRequest());
  parent->SetContent(kTransformId, kLinkId);

  fidl::InterfacePtr<GraphLink> graph_link;
  child->LinkToParent(std::move(child_token), graph_link.NewRequest());

  // Request a status update.
  bool status_updated = false;
  graph_link->GetStatus([&](GraphLinkStatus status) {
    EXPECT_EQ(status, GraphLinkStatus::DISCONNECTED_FROM_DISPLAY);
    status_updated = true;
  });

  // The child begins disconnected from the display.
  UpdateLinks(parent->GetRoot());
  EXPECT_TRUE(status_updated);

  // The GraphLinkStatus will update when both the parent and child Present().
  status_updated = false;
  graph_link->GetStatus([&](GraphLinkStatus status) {
    EXPECT_EQ(status, GraphLinkStatus::CONNECTED_TO_DISPLAY);
    status_updated = true;
  });

  // The child presents first, no update.
  PRESENT(child, true);
  UpdateLinks(parent->GetRoot());
  EXPECT_FALSE(status_updated);

  // The parent presents second and the status updates.
  PRESENT(parent, true);
  UpdateLinks(parent->GetRoot());
  EXPECT_TRUE(status_updated);
}

// This test doesn't use the helper function to create a link, because it tests intermediate steps
// and timing corner cases.
TEST_F(FlatlandTest, ChildReceivesDisconnectedFromDisplay) {
  std::shared_ptr<Flatland> parent = CreateFlatland();
  std::shared_ptr<Flatland> child = CreateFlatland();

  // Set up a link and attach it to the parent's root, but don't call Present() on either instance.
  ContentLinkToken parent_token;
  GraphLinkToken child_token;
  ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token.value, &child_token.value));

  const TransformId kTransformId = {1};

  parent->CreateTransform(kTransformId);
  parent->SetRootTransform(kTransformId);

  const ContentId kLinkId = {2};

  fidl::InterfacePtr<ContentLink> content_link;
  LinkProperties properties;
  properties.set_logical_size({1.0f, 2.0f});
  parent->CreateLink(kLinkId, std::move(parent_token), std::move(properties),
                     content_link.NewRequest());
  parent->SetContent(kTransformId, kLinkId);

  fidl::InterfacePtr<GraphLink> graph_link;
  child->LinkToParent(std::move(child_token), graph_link.NewRequest());

  // The GraphLinkStatus will update when both the parent and child Present().
  bool status_updated = false;
  graph_link->GetStatus([&](GraphLinkStatus status) {
    EXPECT_EQ(status, GraphLinkStatus::CONNECTED_TO_DISPLAY);
    status_updated = true;
  });

  PRESENT(child, true);
  PRESENT(parent, true);
  UpdateLinks(parent->GetRoot());
  EXPECT_TRUE(status_updated);

  // The GraphLinkStatus will update again if the parent removes the child link from its topology.
  status_updated = false;
  graph_link->GetStatus([&](GraphLinkStatus status) {
    EXPECT_EQ(status, GraphLinkStatus::DISCONNECTED_FROM_DISPLAY);
    status_updated = true;
  });

  parent->SetContent(kTransformId, {0});
  PRESENT(parent, true);

  UpdateLinks(parent->GetRoot());
  EXPECT_TRUE(status_updated);
}

// This test doesn't use the helper function to create a link, because it tests intermediate steps
// and timing corner cases.
TEST_F(FlatlandTest, ValidChildToParentFlow) {
  std::shared_ptr<Flatland> parent = CreateFlatland();
  std::shared_ptr<Flatland> child = CreateFlatland();

  ContentLinkToken parent_token;
  GraphLinkToken child_token;
  ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token.value, &child_token.value));

  const ContentId kLinkId = {1};

  fidl::InterfacePtr<ContentLink> content_link;
  LinkProperties properties;
  properties.set_logical_size({1.0f, 2.0f});
  parent->CreateLink(kLinkId, std::move(parent_token), std::move(properties),
                     content_link.NewRequest());

  fidl::InterfacePtr<GraphLink> graph_link;
  child->LinkToParent(std::move(child_token), graph_link.NewRequest());

  bool status_updated = false;
  content_link->GetStatus([&](ContentLinkStatus status) {
    ASSERT_EQ(ContentLinkStatus::CONTENT_HAS_PRESENTED, status);
    status_updated = true;
  });

  // The content link status changes as soon as the child presents - the parent does not have to
  // present.
  EXPECT_FALSE(status_updated);

  PRESENT(child, true);
  UpdateLinks(parent->GetRoot());
  EXPECT_TRUE(status_updated);
}

TEST_F(FlatlandTest, LayoutOnlyUpdatesChildrenInGlobalTopology) {
  std::shared_ptr<Flatland> parent = CreateFlatland();
  std::shared_ptr<Flatland> child = CreateFlatland();

  const TransformId kTransformId = {1};
  const ContentId kLinkId = {2};

  fidl::InterfacePtr<ContentLink> content_link;
  fidl::InterfacePtr<GraphLink> graph_link;
  CreateLink(parent.get(), child.get(), kLinkId, &content_link, &graph_link);
  UpdateLinks(parent->GetRoot());

  // Confirm that the initial logical size is available immediately.
  {
    bool layout_updated = false;
    graph_link->GetLayout([&](LayoutInfo info) {
      EXPECT_EQ(kDefaultSize, info.logical_size().x);
      EXPECT_EQ(kDefaultSize, info.logical_size().y);
      layout_updated = true;
    });

    EXPECT_FALSE(layout_updated);
    UpdateLinks(parent->GetRoot());
    EXPECT_TRUE(layout_updated);
  }

  // Set the logical size to something new.
  {
    LinkProperties properties;
    properties.set_logical_size({2.0f, 3.0f});
    parent->SetLinkProperties(kLinkId, std::move(properties));
    PRESENT(parent, true);
  }

  {
    bool layout_updated = false;
    graph_link->GetLayout([&](LayoutInfo info) {
      EXPECT_EQ(2.0f, info.logical_size().x);
      EXPECT_EQ(3.0f, info.logical_size().y);
      layout_updated = true;
    });

    // Confirm that no update is triggered since the child is not in the global topology.
    EXPECT_FALSE(layout_updated);
    UpdateLinks(parent->GetRoot());
    EXPECT_FALSE(layout_updated);

    // Attach the child to the global topology.
    parent->CreateTransform(kTransformId);
    parent->SetRootTransform(kTransformId);
    parent->SetContent(kTransformId, kLinkId);
    PRESENT(parent, true);

    // Confirm that the new logical size is accessible.
    EXPECT_FALSE(layout_updated);
    UpdateLinks(parent->GetRoot());
    EXPECT_TRUE(layout_updated);
  }
}

TEST_F(FlatlandTest, SetLinkPropertiesDefaultBehavior) {
  std::shared_ptr<Flatland> parent = CreateFlatland();
  std::shared_ptr<Flatland> child = CreateFlatland();

  const TransformId kTransformId = {1};
  const ContentId kLinkId = {2};

  fidl::InterfacePtr<ContentLink> content_link;
  fidl::InterfacePtr<GraphLink> graph_link;
  CreateLink(parent.get(), child.get(), kLinkId, &content_link, &graph_link);

  parent->CreateTransform(kTransformId);
  parent->SetRootTransform(kTransformId);
  parent->SetContent(kTransformId, kLinkId);
  PRESENT(parent, true);

  UpdateLinks(parent->GetRoot());

  // Confirm that the initial layout is the default.
  {
    bool layout_updated = false;
    graph_link->GetLayout([&](LayoutInfo info) {
      EXPECT_EQ(kDefaultSize, info.logical_size().x);
      EXPECT_EQ(kDefaultSize, info.logical_size().y);
      layout_updated = true;
    });

    EXPECT_FALSE(layout_updated);
    UpdateLinks(parent->GetRoot());
    EXPECT_TRUE(layout_updated);
  }

  // Set the logical size to something new.
  {
    LinkProperties properties;
    properties.set_logical_size({2.0f, 3.0f});
    parent->SetLinkProperties(kLinkId, std::move(properties));
    PRESENT(parent, true);
  }

  // Confirm that the new logical size is accessible.
  {
    bool layout_updated = false;
    graph_link->GetLayout([&](LayoutInfo info) {
      EXPECT_EQ(2.0f, info.logical_size().x);
      EXPECT_EQ(3.0f, info.logical_size().y);
      layout_updated = true;
    });

    EXPECT_FALSE(layout_updated);
    UpdateLinks(parent->GetRoot());
    EXPECT_TRUE(layout_updated);
  }

  // Set link properties using a properties object with an unset size field.
  {
    LinkProperties default_properties;
    parent->SetLinkProperties(kLinkId, std::move(default_properties));
    PRESENT(parent, true);
  }

  // Confirm that no update has been triggered.
  {
    bool layout_updated = false;
    graph_link->GetLayout([&](LayoutInfo info) { layout_updated = true; });

    EXPECT_FALSE(layout_updated);
    UpdateLinks(parent->GetRoot());
    EXPECT_FALSE(layout_updated);
  }
}

TEST_F(FlatlandTest, SetLinkPropertiesMultisetBehavior) {
  std::shared_ptr<Flatland> parent = CreateFlatland();
  std::shared_ptr<Flatland> child = CreateFlatland();

  const TransformId kTransformId = {1};
  const ContentId kLinkId = {2};

  fidl::InterfacePtr<ContentLink> content_link;
  fidl::InterfacePtr<GraphLink> graph_link;
  CreateLink(parent.get(), child.get(), kLinkId, &content_link, &graph_link);

  // Our initial layout (from link creation) should be the default size.
  {
    int num_updates = 0;
    graph_link->GetLayout([&](LayoutInfo info) {
      EXPECT_EQ(kDefaultSize, info.logical_size().x);
      EXPECT_EQ(kDefaultSize, info.logical_size().y);
      ++num_updates;
    });

    EXPECT_EQ(0, num_updates);
    UpdateLinks(parent->GetRoot());
    EXPECT_EQ(1, num_updates);
  }

  // Create a full chain of transforms from parent root to child root.
  parent->CreateTransform(kTransformId);
  parent->SetRootTransform(kTransformId);
  parent->SetContent(kTransformId, kLinkId);
  PRESENT(parent, true);

  const float kInitialSize = 100.0f;

  // Set the logical size to something new multiple times.
  for (int i = 10; i >= 0; --i) {
    LinkProperties properties;
    properties.set_logical_size({kInitialSize + i + 1.0f, kInitialSize + i + 1.0f});
    parent->SetLinkProperties(kLinkId, std::move(properties));
    LinkProperties properties2;
    properties2.set_logical_size({kInitialSize + i, kInitialSize + i});
    parent->SetLinkProperties(kLinkId, std::move(properties2));
    PRESENT(parent, true);
  }

  // Confirm that the callback is fired once, and that it has the most up-to-date data.
  {
    int num_updates = 0;
    graph_link->GetLayout([&](LayoutInfo info) {
      EXPECT_EQ(kInitialSize, info.logical_size().x);
      EXPECT_EQ(kInitialSize, info.logical_size().y);
      ++num_updates;
    });

    EXPECT_EQ(0, num_updates);
    UpdateLinks(parent->GetRoot());
    EXPECT_EQ(1, num_updates);
  }

  const float kNewSize = 50.0f;

  // Confirm that calling GetLayout again results in a hung get.
  int num_updates = 0;
  graph_link->GetLayout([&](LayoutInfo info) {
    // When we receive the new layout information, confirm that we receive the last update in the
    // batch.
    EXPECT_EQ(kNewSize, info.logical_size().x);
    EXPECT_EQ(kNewSize, info.logical_size().y);
    ++num_updates;
  });

  EXPECT_EQ(0, num_updates);
  UpdateLinks(parent->GetRoot());
  EXPECT_EQ(0, num_updates);

  // Update the properties twice, once with the old value, once with the new value.
  {
    LinkProperties properties;
    properties.set_logical_size({kInitialSize, kInitialSize});
    parent->SetLinkProperties(kLinkId, std::move(properties));
    LinkProperties properties2;
    properties2.set_logical_size({kNewSize, kNewSize});
    parent->SetLinkProperties(kLinkId, std::move(properties2));
    PRESENT(parent, true);
  }

  // Confirm that we receive the update.
  EXPECT_EQ(0, num_updates);
  UpdateLinks(parent->GetRoot());
  EXPECT_EQ(1, num_updates);
}

TEST_F(FlatlandTest, SetLinkPropertiesOnMultipleChildren) {
  const int kNumChildren = 3;
  const TransformId kRootTransform = {1};
  const TransformId kTransformIds[kNumChildren] = {{2}, {3}, {4}};
  const ContentId kLinkIds[kNumChildren] = {{5}, {6}, {7}};

  std::shared_ptr<Flatland> parent = CreateFlatland();
  std::shared_ptr<Flatland> children[kNumChildren] = {CreateFlatland(), CreateFlatland(),
                                                      CreateFlatland()};
  fidl::InterfacePtr<ContentLink> content_link[kNumChildren];
  fidl::InterfacePtr<GraphLink> graph_link[kNumChildren];

  parent->CreateTransform(kRootTransform);
  parent->SetRootTransform(kRootTransform);

  for (int i = 0; i < kNumChildren; ++i) {
    parent->CreateTransform(kTransformIds[i]);
    parent->AddChild(kRootTransform, kTransformIds[i]);
    CreateLink(parent.get(), children[i].get(), kLinkIds[i], &content_link[i], &graph_link[i]);
    parent->SetContent(kTransformIds[i], kLinkIds[i]);
  }
  UpdateLinks(parent->GetRoot());

  const float kDefaultSize = 1.0f;

  // Confirm that all children are at the default value
  for (int i = 0; i < kNumChildren; ++i) {
    bool layout_updated = false;
    graph_link[i]->GetLayout([&](LayoutInfo info) {
      EXPECT_EQ(kDefaultSize, info.logical_size().x);
      EXPECT_EQ(kDefaultSize, info.logical_size().y);
      layout_updated = true;
    });

    EXPECT_FALSE(layout_updated);
    UpdateLinks(parent->GetRoot());
    EXPECT_TRUE(layout_updated);
  }

  // Resize the content on all children.
  for (auto id : kLinkIds) {
    LinkProperties properties;
    properties.set_logical_size({static_cast<float>(id.value), id.value * 2.0f});
    parent->SetLinkProperties(id, std::move(properties));
  }

  PRESENT(parent, true);

  for (int i = 0; i < kNumChildren; ++i) {
    bool layout_updated = false;
    graph_link[i]->GetLayout([&](LayoutInfo info) {
      EXPECT_EQ(kLinkIds[i].value, info.logical_size().x);
      EXPECT_EQ(kLinkIds[i].value * 2.0f, info.logical_size().y);
      layout_updated = true;
    });

    EXPECT_FALSE(layout_updated);
    UpdateLinks(parent->GetRoot());
    EXPECT_TRUE(layout_updated);
  }
}

TEST_F(FlatlandTest, DisplayPixelScaleAffectsPixelScale) {
  std::shared_ptr<Flatland> parent = CreateFlatland();
  std::shared_ptr<Flatland> child = CreateFlatland();

  const TransformId kTransformId = {1};
  const ContentId kLinkId = {2};

  fidl::InterfacePtr<ContentLink> content_link;
  fidl::InterfacePtr<GraphLink> graph_link;
  CreateLink(parent.get(), child.get(), kLinkId, &content_link, &graph_link);

  parent->CreateTransform(kTransformId);
  parent->SetRootTransform(kTransformId);
  parent->SetContent(kTransformId, kLinkId);
  PRESENT(parent, true);

  UpdateLinks(parent->GetRoot());

  // Change the display pixel scale.
  const glm::vec2 new_display_pixel_scale = {0.1f, 0.2f};
  SetDisplayPixelScale(new_display_pixel_scale);

  // Call and ignore GetLayout() to guarantee the next call hangs.
  graph_link->GetLayout([&](LayoutInfo info) {});

  // Confirm that the new pixel scale is (.1, .2).
  {
    bool layout_updated = false;
    graph_link->GetLayout([&](LayoutInfo info) {
      EXPECT_EQ(new_display_pixel_scale.x, info.pixel_scale().x);
      EXPECT_EQ(new_display_pixel_scale.y, info.pixel_scale().y);
      layout_updated = true;
    });

    EXPECT_FALSE(layout_updated);
    UpdateLinks(parent->GetRoot());
    EXPECT_TRUE(layout_updated);
  }
}

TEST_F(FlatlandTest, LinkSizesAffectPixelScale) {
  std::shared_ptr<Flatland> parent = CreateFlatland();
  std::shared_ptr<Flatland> child = CreateFlatland();

  const TransformId kTransformId = {1};
  const ContentId kLinkId = {2};

  fidl::InterfacePtr<ContentLink> content_link;
  fidl::InterfacePtr<GraphLink> graph_link;
  CreateLink(parent.get(), child.get(), kLinkId, &content_link, &graph_link);

  parent->CreateTransform(kTransformId);
  parent->SetRootTransform(kTransformId);
  parent->SetContent(kTransformId, kLinkId);
  PRESENT(parent, true);

  UpdateLinks(parent->GetRoot());

  // Change the link size and logical size of the link.
  const Vec2 kNewLinkSize = {2.f, 3.f};
  parent->SetLinkSize(kLinkId, kNewLinkSize);

  const Vec2 kNewLogicalSize = {5.f, 7.f};
  {
    LinkProperties properties;
    properties.set_logical_size(kNewLogicalSize);
    parent->SetLinkProperties(kLinkId, std::move(properties));
  }

  PRESENT(parent, true);

  // Call and ignore GetLayout() to guarantee the next call hangs.
  graph_link->GetLayout([&](LayoutInfo info) {});

  // Confirm that the new pixel scale is (2 / 5, 3 / 7).
  {
    bool layout_updated = false;
    graph_link->GetLayout([&](LayoutInfo info) {
      EXPECT_FLOAT_EQ(kNewLinkSize.x / kNewLogicalSize.x, info.pixel_scale().x);
      EXPECT_FLOAT_EQ(kNewLinkSize.y / kNewLogicalSize.y, info.pixel_scale().y);
      layout_updated = true;
    });

    EXPECT_FALSE(layout_updated);
    UpdateLinks(parent->GetRoot());
    EXPECT_TRUE(layout_updated);
  }
}

TEST_F(FlatlandTest, GeometricAttributesAffectPixelScale) {
  std::shared_ptr<Flatland> parent = CreateFlatland();
  std::shared_ptr<Flatland> child = CreateFlatland();

  const TransformId kTransformId = {1};
  const ContentId kLinkId = {2};

  fidl::InterfacePtr<ContentLink> content_link;
  fidl::InterfacePtr<GraphLink> graph_link;
  CreateLink(parent.get(), child.get(), kLinkId, &content_link, &graph_link);

  parent->CreateTransform(kTransformId);
  parent->SetRootTransform(kTransformId);
  parent->SetContent(kTransformId, kLinkId);
  PRESENT(parent, true);

  UpdateLinks(parent->GetRoot());

  // Set a scale on the parent transform.
  const Vec2 scale = {2.f, 3.f};
  parent->SetScale(kTransformId, scale);
  PRESENT(parent, true);

  // Call and ignore GetLayout() to guarantee the next call hangs.
  graph_link->GetLayout([&](LayoutInfo info) {});

  // Confirm that the new pixel scale is (2, 3).
  {
    bool layout_updated = false;
    graph_link->GetLayout([&](LayoutInfo info) {
      EXPECT_FLOAT_EQ(scale.x, info.pixel_scale().x);
      EXPECT_FLOAT_EQ(scale.y, info.pixel_scale().y);
      layout_updated = true;
    });

    EXPECT_FALSE(layout_updated);
    UpdateLinks(parent->GetRoot());
    EXPECT_TRUE(layout_updated);
  }

  // Set a negative scale, but confirm that pixel scale is still positive.
  parent->SetScale(kTransformId, {-scale.x, -scale.y});
  PRESENT(parent, true);

  // Call and ignore GetLayout() to guarantee the next call hangs.
  graph_link->GetLayout([&](LayoutInfo info) {});

  // Pixel scale is still (2, 3), so nothing changes.
  {
    bool layout_updated = false;
    graph_link->GetLayout([&](LayoutInfo info) { layout_updated = true; });

    EXPECT_FALSE(layout_updated);
    UpdateLinks(parent->GetRoot());
    EXPECT_FALSE(layout_updated);
  }

  // Set a rotation on the parent transform.
  parent->SetOrientation(kTransformId, Orientation::CCW_90_DEGREES);
  PRESENT(parent, true);

  // Call and ignore GetLayout() to guarantee the next call hangs.
  graph_link->GetLayout([&](LayoutInfo info) {});

  // This call hangs
  {
    bool layout_updated = false;
    graph_link->GetLayout([&](LayoutInfo info) {
      EXPECT_FLOAT_EQ(scale.y, info.pixel_scale().x);
      EXPECT_FLOAT_EQ(scale.x, info.pixel_scale().y);
      layout_updated = true;
    });

    EXPECT_FALSE(layout_updated);
    UpdateLinks(parent->GetRoot());
    EXPECT_FALSE(layout_updated);
  }
}

TEST_F(FlatlandTest, SetLinkOnTransformErrorCases) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  // Setup.

  const TransformId kId1 = {1};
  const TransformId kId2 = {2};

  flatland->CreateTransform(kId1);

  const ContentId kLinkId1 = {1};
  const ContentId kLinkId2 = {2};

  fidl::InterfacePtr<ContentLink> content_link;

  // Creating a link with an empty property object is an error. Logical size must be provided at
  // creation time.
  {
    ContentLinkToken parent_token;
    GraphLinkToken child_token;
    ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token.value, &child_token.value));
    LinkProperties empty_properties;
    flatland->CreateLink(kLinkId1, std::move(parent_token), std::move(empty_properties),
                         content_link.NewRequest());

    PRESENT(flatland, false);
  }

  // We have to recreate our tokens to get a valid link object.
  ContentLinkToken parent_token;
  GraphLinkToken child_token;
  ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token.value, &child_token.value));

  LinkProperties properties;
  properties.set_logical_size({kDefaultSize, kDefaultSize});
  flatland->CreateLink(kLinkId1, std::move(parent_token), std::move(properties),
                       content_link.NewRequest());

  PRESENT(flatland, true);

  // Zero is not a valid transform_id.
  flatland->SetContent({0}, kLinkId1);
  PRESENT(flatland, false);

  // Setting a valid link on an ivnalid transform is not valid.
  flatland->SetContent(kId2, kLinkId1);
  PRESENT(flatland, false);

  // Setting an invalid link on a valid transform is not valid.
  flatland->SetContent(kId1, kLinkId2);
  PRESENT(flatland, false);
}

TEST_F(FlatlandTest, ReleaseLinkErrorCases) {
  std::shared_ptr<Allocator> allocator = CreateAllocator();
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  // Zero is not a valid link_id.
  flatland->ReleaseLink({0}, [](ContentLinkToken token) { EXPECT_TRUE(false); });
  PRESENT(flatland, false);

  // Using a link_id that does not exist is not valid.
  const ContentId kLinkId1 = {1};
  flatland->ReleaseLink(kLinkId1, [](ContentLinkToken token) { EXPECT_TRUE(false); });
  PRESENT(flatland, false);

  // ContentId is not a Link.
  const ContentId kImageId = {2};
  BufferCollectionImportExportTokens ref_pair = BufferCollectionImportExportTokens::New();

  ImageProperties properties;
  properties.set_width(100);
  properties.set_height(200);

  CreateImage(flatland.get(), allocator.get(), kImageId, std::move(ref_pair),
              std::move(properties));

  flatland->ReleaseLink(kImageId, [](ContentLinkToken token) { EXPECT_TRUE(false); });
  PRESENT(flatland, false);
}

TEST_F(FlatlandTest, ReleaseLinkReturnsOriginalToken) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  ContentLinkToken parent_token;
  GraphLinkToken child_token;
  ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token.value, &child_token.value));

  const zx_koid_t expected_koid = fsl::GetKoid(parent_token.value.get());

  const ContentId kLinkId1 = {1};

  fidl::InterfacePtr<ContentLink> content_link;
  LinkProperties properties;
  properties.set_logical_size({kDefaultSize, kDefaultSize});
  flatland->CreateLink(kLinkId1, std::move(parent_token), std::move(properties),
                       content_link.NewRequest());
  PRESENT(flatland, true);

  ContentLinkToken content_token;
  flatland->ReleaseLink(
      kLinkId1, [&content_token](ContentLinkToken token) { content_token = std::move(token); });

  RunLoopUntilIdle();

  // Until Present() is called and the acquire fence is signaled, the previous ContentLink is not
  // unbound.
  EXPECT_TRUE(content_link.is_bound());
  EXPECT_FALSE(content_token.value.is_valid());

  PresentArgs args;
  args.acquire_fences = utils::CreateEventArray(1);
  auto event_copy = utils::CopyEvent(args.acquire_fences[0]);

  PRESENT_WITH_ARGS(flatland, std::move(args), true);

  EXPECT_TRUE(content_link.is_bound());
  EXPECT_FALSE(content_token.value.is_valid());

  // Signal the acquire fence to unbind the link.
  event_copy.signal(0, ZX_EVENT_SIGNALED);

  EXPECT_CALL(*mock_flatland_presenter_, ScheduleUpdateForSession(_, _, _));
  RunLoopUntilIdle();

  EXPECT_FALSE(content_link.is_bound());
  EXPECT_TRUE(content_token.value.is_valid());
  EXPECT_EQ(fsl::GetKoid(content_token.value.get()), expected_koid);
}

TEST_F(FlatlandTest, ReleaseLinkReturnsOrphanedTokenOnChildDeath) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  ContentLinkToken parent_token;
  GraphLinkToken child_token;
  ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token.value, &child_token.value));

  const ContentId kLinkId1 = {1};

  fidl::InterfacePtr<ContentLink> content_link;
  LinkProperties properties;
  properties.set_logical_size({kDefaultSize, kDefaultSize});
  flatland->CreateLink(kLinkId1, std::move(parent_token), std::move(properties),
                       content_link.NewRequest());
  PRESENT(flatland, true);

  // Killing the peer token does not prevent the instance from returning a valid token.
  child_token.value.reset();
  RunLoopUntilIdle();

  ContentLinkToken content_token;
  flatland->ReleaseLink(
      kLinkId1, [&content_token](ContentLinkToken token) { content_token = std::move(token); });
  PRESENT(flatland, true);

  EXPECT_TRUE(content_token.value.is_valid());

  // But trying to link with that token will immediately fail because it is already orphaned.
  const ContentId kLinkId2 = {2};

  fidl::InterfacePtr<ContentLink> content_link2;
  flatland->CreateLink(kLinkId2, std::move(content_token), std::move(properties),
                       content_link2.NewRequest());
  PRESENT(flatland, true);

  EXPECT_FALSE(content_link2.is_bound());
}

TEST_F(FlatlandTest, CreateLinkPresentedBeforeLinkToParent) {
  std::shared_ptr<Flatland> parent = CreateFlatland();
  std::shared_ptr<Flatland> child = CreateFlatland();

  ContentLinkToken parent_token;
  GraphLinkToken child_token;
  ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token.value, &child_token.value));

  // Create a transform, add it to the parent, then create a link and assign to the transform.
  const TransformId kId1 = {1};
  parent->CreateTransform(kId1);
  parent->SetRootTransform(kId1);

  const ContentId kLinkId = {1};

  fidl::InterfacePtr<ContentLink> parent_content_link;
  LinkProperties properties;
  properties.set_logical_size({kDefaultSize, kDefaultSize});
  parent->CreateLink(kLinkId, std::move(parent_token), std::move(properties),
                     parent_content_link.NewRequest());
  parent->SetContent(kId1, kLinkId);

  PRESENT(parent, true);

  // Link the child to the parent->
  fidl::InterfacePtr<GraphLink> child_graph_link;
  child->LinkToParent(std::move(child_token), child_graph_link.NewRequest());

  // The child should only be accessible from the parent when Present() is called on the child->
  EXPECT_FALSE(IsDescendantOf(parent->GetRoot(), child->GetRoot()));

  PRESENT(child, true);

  EXPECT_TRUE(IsDescendantOf(parent->GetRoot(), child->GetRoot()));
}

TEST_F(FlatlandTest, LinkToParentPresentedBeforeCreateLink) {
  std::shared_ptr<Flatland> parent = CreateFlatland();
  std::shared_ptr<Flatland> child = CreateFlatland();

  ContentLinkToken parent_token;
  GraphLinkToken child_token;
  ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token.value, &child_token.value));

  // Link the child to the parent
  fidl::InterfacePtr<GraphLink> child_graph_link;
  child->LinkToParent(std::move(child_token), child_graph_link.NewRequest());

  PRESENT(child, true);

  // Create a transform, add it to the parent, then create a link and assign to the transform.
  const TransformId kId1 = {1};
  parent->CreateTransform(kId1);
  parent->SetRootTransform(kId1);

  // Present the parent once so that it has a topology or else IsDescendantOf() will crash.
  PRESENT(parent, true);

  const ContentId kLinkId = {1};

  fidl::InterfacePtr<ContentLink> parent_content_link;
  LinkProperties properties;
  properties.set_logical_size({kDefaultSize, kDefaultSize});
  parent->CreateLink(kLinkId, std::move(parent_token), std::move(properties),
                     parent_content_link.NewRequest());
  parent->SetContent(kId1, kLinkId);

  // The child should only be accessible from the parent when Present() is called on the parent->
  EXPECT_FALSE(IsDescendantOf(parent->GetRoot(), child->GetRoot()));

  PRESENT(parent, true);

  EXPECT_TRUE(IsDescendantOf(parent->GetRoot(), child->GetRoot()));
}

TEST_F(FlatlandTest, LinkResolvedBeforeEitherPresent) {
  std::shared_ptr<Flatland> parent = CreateFlatland();
  std::shared_ptr<Flatland> child = CreateFlatland();

  ContentLinkToken parent_token;
  GraphLinkToken child_token;
  ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token.value, &child_token.value));

  // Create a transform, add it to the parent, then create a link and assign to the transform.
  const TransformId kId1 = {1};
  parent->CreateTransform(kId1);
  parent->SetRootTransform(kId1);

  // Present the parent once so that it has a topology or else IsDescendantOf() will crash.
  PRESENT(parent, true);

  const ContentId kLinkId = {1};

  fidl::InterfacePtr<ContentLink> parent_content_link;
  LinkProperties properties;
  properties.set_logical_size({kDefaultSize, kDefaultSize});
  parent->CreateLink(kLinkId, std::move(parent_token), std::move(properties),
                     parent_content_link.NewRequest());
  parent->SetContent(kId1, kLinkId);

  // Link the child to the parent->
  fidl::InterfacePtr<GraphLink> child_graph_link;
  child->LinkToParent(std::move(child_token), child_graph_link.NewRequest());

  // The child should only be accessible from the parent when Present() is called on both the parent
  // and the child->
  EXPECT_FALSE(IsDescendantOf(parent->GetRoot(), child->GetRoot()));

  PRESENT(parent, true);

  EXPECT_FALSE(IsDescendantOf(parent->GetRoot(), child->GetRoot()));

  PRESENT(child, true);

  EXPECT_TRUE(IsDescendantOf(parent->GetRoot(), child->GetRoot()));
}

TEST_F(FlatlandTest, ClearChildLink) {
  std::shared_ptr<Flatland> parent = CreateFlatland();
  std::shared_ptr<Flatland> child = CreateFlatland();

  ContentLinkToken parent_token;
  GraphLinkToken child_token;
  ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token.value, &child_token.value));

  // Create and link the two instances.
  const TransformId kId1 = {1};
  parent->CreateTransform(kId1);
  parent->SetRootTransform(kId1);

  const ContentId kLinkId = {1};

  fidl::InterfacePtr<ContentLink> parent_content_link;
  LinkProperties properties;
  properties.set_logical_size({kDefaultSize, kDefaultSize});
  parent->CreateLink(kLinkId, std::move(parent_token), std::move(properties),
                     parent_content_link.NewRequest());
  parent->SetContent(kId1, kLinkId);

  fidl::InterfacePtr<GraphLink> child_graph_link;
  child->LinkToParent(std::move(child_token), child_graph_link.NewRequest());

  PRESENT(parent, true);
  PRESENT(child, true);

  EXPECT_TRUE(IsDescendantOf(parent->GetRoot(), child->GetRoot()));

  // Reset the child link using zero as the link id.
  parent->SetContent(kId1, {0});

  PRESENT(parent, true);

  EXPECT_FALSE(IsDescendantOf(parent->GetRoot(), child->GetRoot()));
}

TEST_F(FlatlandTest, RelinkUnlinkedParentSameToken) {
  std::shared_ptr<Flatland> parent = CreateFlatland();
  std::shared_ptr<Flatland> child = CreateFlatland();

  const ContentId kLinkId1 = {1};

  fidl::InterfacePtr<ContentLink> content_link;
  fidl::InterfacePtr<GraphLink> graph_link;
  CreateLink(parent.get(), child.get(), kLinkId1, &content_link, &graph_link);
  RunLoopUntilIdle();

  const TransformId kId1 = {1};
  parent->CreateTransform(kId1);
  parent->SetRootTransform(kId1);
  parent->SetContent(kId1, kLinkId1);

  PRESENT(parent, true);

  EXPECT_TRUE(IsDescendantOf(parent->GetRoot(), child->GetRoot()));

  GraphLinkToken graph_token;
  child->UnlinkFromParent([&graph_token](GraphLinkToken token) { graph_token = std::move(token); });

  PRESENT(child, true);

  EXPECT_FALSE(IsDescendantOf(parent->GetRoot(), child->GetRoot()));

  // The same token can be used to link a different instance.
  std::shared_ptr<Flatland> child2 = CreateFlatland();
  child2->LinkToParent(std::move(graph_token), graph_link.NewRequest());

  PRESENT(child2, true);

  EXPECT_TRUE(IsDescendantOf(parent->GetRoot(), child2->GetRoot()));

  // The old instance is not re-linked.
  EXPECT_FALSE(IsDescendantOf(parent->GetRoot(), child->GetRoot()));
}

TEST_F(FlatlandTest, RecreateReleasedLinkSameToken) {
  std::shared_ptr<Flatland> parent = CreateFlatland();
  std::shared_ptr<Flatland> child = CreateFlatland();

  const ContentId kLinkId1 = {1};

  fidl::InterfacePtr<ContentLink> content_link;
  fidl::InterfacePtr<GraphLink> graph_link;
  CreateLink(parent.get(), child.get(), kLinkId1, &content_link, &graph_link);
  RunLoopUntilIdle();

  const TransformId kId1 = {1};
  parent->CreateTransform(kId1);
  parent->SetRootTransform(kId1);
  parent->SetContent(kId1, kLinkId1);

  PRESENT(parent, true);

  EXPECT_TRUE(IsDescendantOf(parent->GetRoot(), child->GetRoot()));

  ContentLinkToken content_token;
  parent->ReleaseLink(
      kLinkId1, [&content_token](ContentLinkToken token) { content_token = std::move(token); });

  PRESENT(parent, true);

  EXPECT_FALSE(IsDescendantOf(parent->GetRoot(), child->GetRoot()));

  // The same token can be used to create a different link to the same child with a different
  // parent->
  std::shared_ptr<Flatland> parent2 = CreateFlatland();

  const TransformId kId2 = {2};
  parent2->CreateTransform(kId2);
  parent2->SetRootTransform(kId2);

  const ContentId kLinkId2 = {2};
  LinkProperties properties;
  properties.set_logical_size({kDefaultSize, kDefaultSize});
  parent2->CreateLink(kLinkId2, std::move(content_token), std::move(properties),
                      content_link.NewRequest());
  parent2->SetContent(kId2, kLinkId2);

  PRESENT(parent2, true);

  EXPECT_TRUE(IsDescendantOf(parent2->GetRoot(), child->GetRoot()));

  // The old instance is not re-linked.
  EXPECT_FALSE(IsDescendantOf(parent->GetRoot(), child->GetRoot()));
}

TEST_F(FlatlandTest, SetLinkSizeErrorCases) {
  std::shared_ptr<Allocator> allocator = CreateAllocator();
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  const ContentId kIdNotCreated = {1};

  // Zero is not a valid transform ID.
  flatland->SetLinkSize({0}, {1.f, 2.f});
  PRESENT(flatland, false);

  // Size contains non-positive components.
  flatland->SetLinkSize({0}, {-1.f, 2.f});
  PRESENT(flatland, false);

  flatland->SetLinkSize({0}, {1.f, 0.f});
  PRESENT(flatland, false);

  // Link does not exist.
  flatland->SetLinkSize(kIdNotCreated, {1.f, 2.f});
  PRESENT(flatland, false);

  // ContentId is not a Link.
  const ContentId kImageId = {2};
  BufferCollectionImportExportTokens ref_pair = BufferCollectionImportExportTokens::New();

  ImageProperties properties;
  properties.set_width(100);
  properties.set_height(200);

  CreateImage(flatland.get(), allocator.get(), kImageId, std::move(ref_pair),
              std::move(properties));

  flatland->SetLinkSize(kImageId, {1.f, 2.f});
  PRESENT(flatland, false);
}

TEST_F(FlatlandTest, LinkSizeRatiosCreateScaleMatrix) {
  std::shared_ptr<Flatland> parent = CreateFlatland();
  std::shared_ptr<Flatland> child = CreateFlatland();

  const ContentId kLinkId1 = {1};

  fidl::InterfacePtr<ContentLink> content_link;
  fidl::InterfacePtr<GraphLink> graph_link;
  CreateLink(parent.get(), child.get(), kLinkId1, &content_link, &graph_link);

  const TransformId kId1 = {1};

  parent->CreateTransform(kId1);
  parent->SetRootTransform(kId1);
  parent->SetContent(kId1, kLinkId1);

  PRESENT(parent, true);

  const auto maybe_link_handle = parent->GetContentHandle(kLinkId1);
  ASSERT_TRUE(maybe_link_handle.has_value());
  const auto link_handle = maybe_link_handle.value();

  // The default size is the same as the logical size, so the link handle won't have a matrix.
  auto uber_struct = GetUberStruct(parent.get());
  EXPECT_MATRIX(uber_struct, link_handle, glm::mat3());

  // Change the link size to half the width and a quarter the height.
  const float kNewLinkWidth = 0.5f * kDefaultSize;
  const float kNewLinkHeight = 0.25f * kDefaultSize;
  parent->SetLinkSize(kLinkId1, {kNewLinkWidth, kNewLinkHeight});

  PRESENT(parent, true);

  // This should change the expected matrix to apply the same scales.
  const glm::mat3 expected_scale_matrix = glm::scale(glm::mat3(), {kNewLinkWidth, kNewLinkHeight});

  uber_struct = GetUberStruct(parent.get());
  EXPECT_MATRIX(uber_struct, link_handle, expected_scale_matrix);

  // Changing the logical size to the same values returns the matrix to the identity matrix.
  LinkProperties properties;
  properties.set_logical_size({kNewLinkWidth, kNewLinkHeight});
  parent->SetLinkProperties(kLinkId1, std::move(properties));

  PRESENT(parent, true);

  uber_struct = GetUberStruct(parent.get());
  EXPECT_MATRIX(uber_struct, link_handle, glm::mat3());

  // Change the logical size back to the default size.
  LinkProperties properties2;
  properties2.set_logical_size({kDefaultSize, kDefaultSize});
  parent->SetLinkProperties(kLinkId1, std::move(properties2));

  PRESENT(parent, true);

  // This should change the expected matrix back to applying the scales.
  uber_struct = GetUberStruct(parent.get());
  EXPECT_MATRIX(uber_struct, link_handle, expected_scale_matrix);
}

TEST_F(FlatlandTest, EmptyLogicalSizePreservesOldSize) {
  std::shared_ptr<Flatland> parent = CreateFlatland();
  std::shared_ptr<Flatland> child = CreateFlatland();

  const ContentId kLinkId1 = {1};

  fidl::InterfacePtr<ContentLink> content_link;
  fidl::InterfacePtr<GraphLink> graph_link;
  CreateLink(parent.get(), child.get(), kLinkId1, &content_link, &graph_link);

  const TransformId kId1 = {1};

  parent->CreateTransform(kId1);
  parent->SetRootTransform(kId1);
  parent->SetContent(kId1, kLinkId1);

  PRESENT(parent, true);

  const auto maybe_link_handle = parent->GetContentHandle(kLinkId1);
  ASSERT_TRUE(maybe_link_handle.has_value());
  const auto link_handle = maybe_link_handle.value();

  // Set the link size and logical size to new values
  const float kNewLinkWidth = 2.f * kDefaultSize;
  const float kNewLinkHeight = 3.f * kDefaultSize;
  parent->SetLinkSize(kLinkId1, {kNewLinkWidth, kNewLinkHeight});

  const float kNewLinkLogicalWidth = 5.f * kDefaultSize;
  const float kNewLinkLogicalHeight = 7.f * kDefaultSize;
  LinkProperties properties;
  properties.set_logical_size({kNewLinkLogicalWidth, kNewLinkLogicalHeight});
  parent->SetLinkProperties(kLinkId1, std::move(properties));

  PRESENT(parent, true);

  // This should result in an expected matrix that applies the ratio of the scales.
  glm::mat3 expected_scale_matrix = glm::scale(
      glm::mat3(), {kNewLinkWidth / kNewLinkLogicalWidth, kNewLinkHeight / kNewLinkLogicalHeight});

  auto uber_struct = GetUberStruct(parent.get());
  EXPECT_MATRIX(uber_struct, link_handle, expected_scale_matrix);

  // Setting a new LinkProperties with no logical size shouldn't change the matrix.
  LinkProperties properties2;
  parent->SetLinkProperties(kLinkId1, std::move(properties2));

  PRESENT(parent, true);

  uber_struct = GetUberStruct(parent.get());
  EXPECT_MATRIX(uber_struct, link_handle, expected_scale_matrix);

  // But it should still preserve the old logical size so that a subsequent link size update uses
  // the old logical size.
  const float kNewLinkWidth2 = 11.f * kDefaultSize;
  const float kNewLinkHeight2 = 13.f * kDefaultSize;
  parent->SetLinkSize(kLinkId1, {kNewLinkWidth2, kNewLinkHeight2});

  PRESENT(parent, true);

  // This should result in an expected matrix that applies the ratio of the scales.
  expected_scale_matrix = glm::scale(glm::mat3(), {kNewLinkWidth2 / kNewLinkLogicalWidth,
                                                   kNewLinkHeight2 / kNewLinkLogicalHeight});

  uber_struct = GetUberStruct(parent.get());
  EXPECT_MATRIX(uber_struct, link_handle, expected_scale_matrix);
}

TEST_F(FlatlandTest, CreateImageValidCase) {
  std::shared_ptr<Allocator> allocator = CreateAllocator();
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  // Setup a valid image.
  const ContentId kImageId = {1};
  BufferCollectionImportExportTokens ref_pair = BufferCollectionImportExportTokens::New();
  const uint32_t kWidth = 100;
  const uint32_t kHeight = 200;
  ImageProperties properties;
  properties.set_width(kWidth);
  properties.set_height(kHeight);

  CreateImage(flatland.get(), allocator.get(), kImageId, std::move(ref_pair),
              std::move(properties));
}

TEST_F(FlatlandTest, SetOpacityTestCases) {
  std::shared_ptr<Allocator> allocator = CreateAllocator();
  std::shared_ptr<Flatland> flatland = CreateFlatland();
  const TransformId kId = {1};

  // Zero is not a valid transform ID.
  {
    flatland->SetOpacity({0}, 0.5);
    PRESENT(flatland, false);
  }

  // The transform id hasn't been imported yet.
  {
    flatland->SetOpacity(kId, 0.5);
    PRESENT(flatland, false);
  }

  // Setup a valid transform.
  flatland->CreateTransform(kId);
  flatland->SetRootTransform(kId);

  // The alpha values are out of range.
  {
    flatland->SetOpacity(kId, -0.5);
    PRESENT(flatland, false);

    flatland->SetOpacity(kId, 1.5);
    PRESENT(flatland, false);
  }

  // Testing now with good values should finally work.
  {
    flatland->SetOpacity(kId, 0.5);
    PRESENT(flatland, true);
  }

  const TransformId kIdChild = {2};
  flatland->CreateTransform(kIdChild);

  // Adding a child should fail because the alpha value is not 1.0
  {
    flatland->AddChild(kId, kIdChild);
    PRESENT(flatland, false);
  }

  // We should still be able to add an *image* to the transform though since that is
  // content and is treated differently from a normal child->
  {
    const ContentId kImageId = {5};
    BufferCollectionImportExportTokens ref_pair = BufferCollectionImportExportTokens::New();
    ImageProperties properties;
    properties.set_width(150);
    properties.set_height(175);
    std::shared_ptr<Allocator> allocator = CreateAllocator();
    CreateImage(flatland.get(), allocator.get(), kImageId, std::move(ref_pair),
                std::move(properties));
    flatland->SetContent(kId, kImageId);
    PRESENT(flatland, true);
  }

  // We shold still be able to change the opacity to another value < 1 even with an image
  // on the transform.
  {
    flatland->SetOpacity(kId, 0.3);
    PRESENT(flatland, true);
  }

  // If we set the alpha to 1.0 again and then add the child, now it
  // should work.
  {
    flatland->SetOpacity(kId, 1.0);
    flatland->AddChild(kId, kIdChild);
    PRESENT(flatland, true);
  }

  // Now that a child is added, if we try to change the alpha again, it
  // should fail.
  {
    flatland->SetOpacity(kId, 0.5);
    PRESENT(flatland, false);
  }
}

TEST_F(FlatlandTest, CreateImageErrorCases) {
  std::shared_ptr<Allocator> allocator = CreateAllocator();
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  // Default image properties.
  const uint32_t kDefaultVmoIndex = 1;
  const uint32_t kDefaultWidth = 100;
  const uint32_t kDefaultHeight = 1000;

  // Setup a valid buffer collection.
  BufferCollectionImportExportTokens ref_pair = BufferCollectionImportExportTokens::New();
  REGISTER_BUFFER_COLLECTION(allocator, ref_pair.export_token, CreateToken(), true);

  // Zero is not a valid image ID.
  {
    flatland->CreateImage({0}, ref_pair.DuplicateImportToken(), kDefaultVmoIndex,
                          ImageProperties());
    PRESENT(flatland, false);
  }

  // The import token must also be valid.
  {
    flatland->CreateImage({1}, BufferCollectionImportToken(), kDefaultVmoIndex, ImageProperties());
    PRESENT(flatland, false);
  }

  // The buffer collection can fail to create an image.
  {
    flatland->CreateImage({1}, ref_pair.DuplicateImportToken(), kDefaultVmoIndex,
                          ImageProperties());
    PRESENT(flatland, false);
  }

  // Check to make sure that if the BufferCollectionImporter returns false, then the call
  // to Flatland::CreateImage() also returns false.
  {
    const ContentId kId = {100};
    ImageProperties properties;
    properties.set_width(kDefaultWidth);
    properties.set_height(kDefaultHeight);
    EXPECT_CALL(*mock_buffer_collection_importer_, ImportBufferImage(_)).WillOnce(Return(false));
    flatland->CreateImage(kId, ref_pair.DuplicateImportToken(), kDefaultVmoIndex,
                          std::move(properties));
    PRESENT(flatland, false);
  }

  // Two images cannot have the same ID.
  const ContentId kId = {1};
  {
    ImageProperties properties;
    properties.set_width(kDefaultWidth);
    properties.set_height(kDefaultHeight);

    // This is the first call in these series of test components that makes it down to
    // the BufferCollectionImporter. We have to make sure it returns true here so that
    // the test doesn't erroneously fail.
    EXPECT_CALL(*mock_buffer_collection_importer_, ImportBufferImage(_)).WillOnce(Return(true));

    flatland->CreateImage(kId, ref_pair.DuplicateImportToken(), kDefaultVmoIndex,
                          std::move(properties));
    PRESENT(flatland, true);
  }

  {
    ImageProperties properties;
    properties.set_width(kDefaultWidth);
    properties.set_height(kDefaultHeight);

    // We shouldn't even make it to the BufferCollectionImporter here due to the duplicate
    // ID causing CreateImage() to return early.
    EXPECT_CALL(*mock_buffer_collection_importer_, ImportBufferImage(_)).Times(0);
    flatland->CreateImage(kId, ref_pair.DuplicateImportToken(), kDefaultVmoIndex,
                          std::move(properties));
    PRESENT(flatland, false);
  }

  // A Link id cannot be used for an image.
  const ContentId kLinkId = {2};
  {
    ContentLinkToken parent_token;
    GraphLinkToken child_token;
    ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token.value, &child_token.value));

    fidl::InterfacePtr<ContentLink> content_link;
    LinkProperties link_properties;
    link_properties.set_logical_size({kDefaultSize, kDefaultSize});
    flatland->CreateLink(kLinkId, std::move(parent_token), std::move(link_properties),
                         content_link.NewRequest());
    PRESENT(flatland, true);

    ImageProperties image_properties;
    image_properties.set_width(kDefaultWidth);
    image_properties.set_height(kDefaultHeight);

    flatland->CreateImage(kLinkId, ref_pair.DuplicateImportToken(), kDefaultVmoIndex,
                          std::move(image_properties));
    PRESENT(flatland, false);
  }
}

TEST_F(FlatlandTest, CreateImageWithDuplicatedImportTokens) {
  std::shared_ptr<Allocator> allocator = CreateAllocator();
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  BufferCollectionImportExportTokens ref_pair = BufferCollectionImportExportTokens::New();
  REGISTER_BUFFER_COLLECTION(allocator, ref_pair.export_token, CreateToken(), true);

  const uint64_t kNumImages = 3;
  EXPECT_CALL(*mock_buffer_collection_importer_, ImportBufferImage(_))
      .Times(kNumImages)
      .WillRepeatedly(Return(true));

  for (uint64_t i = 0; i < kNumImages; ++i) {
    ImageProperties properties;
    properties.set_width(150);
    properties.set_height(175);
    flatland->CreateImage(/*image_id*/ {i + 1}, ref_pair.DuplicateImportToken(), /*vmo_idx*/ i,
                          std::move(properties));
    PRESENT(flatland, true);
  }
}

TEST_F(FlatlandTest, CreateImageInMultipleFlatlands) {
  std::shared_ptr<Allocator> allocator = CreateAllocator();
  std::shared_ptr<Flatland> flatland1 = CreateFlatland();
  std::shared_ptr<Flatland> flatland2 = CreateFlatland();

  BufferCollectionImportExportTokens ref_pair = BufferCollectionImportExportTokens::New();
  REGISTER_BUFFER_COLLECTION(allocator, ref_pair.export_token, CreateToken(), true);

  // We can import the same image in both flatland instances.
  {
    EXPECT_CALL(*mock_buffer_collection_importer_, ImportBufferImage(_)).WillOnce(Return(true));
    ImageProperties properties;
    properties.set_width(150);
    properties.set_height(175);
    flatland1->CreateImage({1}, ref_pair.DuplicateImportToken(), 0, std::move(properties));
    PRESENT(flatland1, true);
  }
  {
    EXPECT_CALL(*mock_buffer_collection_importer_, ImportBufferImage(_)).WillOnce(Return(true));
    ImageProperties properties;
    properties.set_width(150);
    properties.set_height(175);
    flatland2->CreateImage({1}, ref_pair.DuplicateImportToken(), 0, std::move(properties));
    PRESENT(flatland2, true);
  }

  // There are seperate ReleaseBufferImage calls to release them from importers.
  EXPECT_CALL(*mock_buffer_collection_importer_, ReleaseBufferImage(_)).Times(2);
  flatland1->ClearGraph();
  PRESENT(flatland1, true);
  flatland2->ClearGraph();
  PRESENT(flatland2, true);
}

TEST_F(FlatlandTest, SetContentErrorCases) {
  std::shared_ptr<Allocator> allocator = CreateAllocator();
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  // Setup a valid image.
  const ContentId kImageId = {1};
  BufferCollectionImportExportTokens ref_pair = BufferCollectionImportExportTokens::New();
  const uint32_t kWidth = 100;
  const uint32_t kHeight = 200;

  ImageProperties properties;
  properties.set_width(kWidth);
  properties.set_height(kHeight);

  CreateImage(flatland.get(), allocator.get(), kImageId, std::move(ref_pair),
              std::move(properties));

  // Create a transform.
  const TransformId kTransformId = {1};

  flatland->CreateTransform(kTransformId);
  PRESENT(flatland, true);

  // Zero is not a valid transform.
  flatland->SetContent({0}, kImageId);
  PRESENT(flatland, false);

  // The transform must exist.
  flatland->SetContent({2}, kImageId);
  PRESENT(flatland, false);

  // The image must exist.
  flatland->SetContent(kTransformId, {2});
  PRESENT(flatland, false);
}

TEST_F(FlatlandTest, ClearContentOnTransform) {
  std::shared_ptr<Allocator> allocator = CreateAllocator();
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  // Setup a valid image.
  const ContentId kImageId = {1};
  BufferCollectionImportExportTokens ref_pair = BufferCollectionImportExportTokens::New();

  ImageProperties properties;
  properties.set_width(100);
  properties.set_height(200);

  auto import_token_dup = ref_pair.DuplicateImportToken();
  auto global_collection_id = CreateImage(flatland.get(), allocator.get(), kImageId,
                                          std::move(ref_pair), std::move(properties))
                                  .collection_id;

  const auto maybe_image_handle = flatland->GetContentHandle(kImageId);
  ASSERT_TRUE(maybe_image_handle.has_value());
  const auto image_handle = maybe_image_handle.value();

  // Create a transform, make it the root transform, and attach the image.
  const TransformId kTransformId = {1};

  flatland->CreateTransform(kTransformId);
  flatland->SetRootTransform(kTransformId);
  flatland->SetContent(kTransformId, kImageId);
  PRESENT(flatland, true);

  // The image handle should be the last handle in the local_topology, and the image should be in
  // the image map.
  auto uber_struct = GetUberStruct(flatland.get());
  EXPECT_EQ(uber_struct->local_topology.back().handle, image_handle);

  auto image_kv = uber_struct->images.find(image_handle);
  EXPECT_NE(image_kv, uber_struct->images.end());
  EXPECT_EQ(image_kv->second.collection_id, global_collection_id);

  // An ContentId of 0 indicates to remove any content on the specified transform.
  flatland->SetContent(kTransformId, {0});
  PRESENT(flatland, true);

  uber_struct = GetUberStruct(flatland.get());
  for (const auto& entry : uber_struct->local_topology) {
    EXPECT_NE(entry.handle, image_handle);
  }
}

TEST_F(FlatlandTest, TopologyVisitsContentBeforeChildren) {
  std::shared_ptr<Allocator> allocator = CreateAllocator();
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  // Setup two valid images.
  const ContentId kImageId1 = {1};
  BufferCollectionImportExportTokens ref_pair_1 = BufferCollectionImportExportTokens::New();

  ImageProperties properties1;
  properties1.set_width(100);
  properties1.set_height(200);

  CreateImage(flatland.get(), allocator.get(), kImageId1, std::move(ref_pair_1),
              std::move(properties1));

  const auto maybe_image_handle1 = flatland->GetContentHandle(kImageId1);
  ASSERT_TRUE(maybe_image_handle1.has_value());
  const auto image_handle1 = maybe_image_handle1.value();

  const ContentId kImageId2 = {2};
  BufferCollectionImportExportTokens ref_pair_2 = BufferCollectionImportExportTokens::New();

  ImageProperties properties2;
  properties2.set_width(300);
  properties2.set_height(400);

  CreateImage(flatland.get(), allocator.get(), kImageId2, std::move(ref_pair_2),
              std::move(properties2));

  const auto maybe_image_handle2 = flatland->GetContentHandle(kImageId2);
  ASSERT_TRUE(maybe_image_handle2.has_value());
  const auto image_handle2 = maybe_image_handle2.value();

  // Create a root transform with two children.
  const TransformId kTransformId1 = {3};
  const TransformId kTransformId2 = {4};
  const TransformId kTransformId3 = {5};

  flatland->CreateTransform(kTransformId1);
  flatland->CreateTransform(kTransformId2);
  flatland->CreateTransform(kTransformId3);

  flatland->AddChild(kTransformId1, kTransformId2);
  flatland->AddChild(kTransformId1, kTransformId3);

  flatland->SetRootTransform(kTransformId1);
  PRESENT(flatland, true);

  // Attach image 1 to the root and the second child-> Attach image 2 to the first child->
  flatland->SetContent(kTransformId1, kImageId1);
  flatland->SetContent(kTransformId2, kImageId2);
  flatland->SetContent(kTransformId3, kImageId1);
  PRESENT(flatland, true);

  // The images should appear pre-order toplogically sorted: 1, 2, 1 again. The same image is
  // allowed to appear multiple times.
  std::queue<TransformHandle> expected_handle_order;
  expected_handle_order.push(image_handle1);
  expected_handle_order.push(image_handle2);
  expected_handle_order.push(image_handle1);
  auto uber_struct = GetUberStruct(flatland.get());
  for (const auto& entry : uber_struct->local_topology) {
    if (entry.handle == expected_handle_order.front()) {
      expected_handle_order.pop();
    }
  }
  EXPECT_TRUE(expected_handle_order.empty());

  // Clearing the image from the parent removes the first entry of the list since images are
  // visited before children.
  flatland->SetContent(kTransformId1, {0});
  PRESENT(flatland, true);

  // Meaning the new list of images should be: 2, 1.
  expected_handle_order.push(image_handle2);
  expected_handle_order.push(image_handle1);
  uber_struct = GetUberStruct(flatland.get());
  for (const auto& entry : uber_struct->local_topology) {
    if (entry.handle == expected_handle_order.front()) {
      expected_handle_order.pop();
    }
  }
  EXPECT_TRUE(expected_handle_order.empty());
}

// Tests that a buffer collection is released after CreateImage() if there are no more import
// tokens.
TEST_F(FlatlandTest, ReleaseBufferCollectionHappensAfterCreateImage) {
  std::shared_ptr<Allocator> allocator = CreateAllocator();
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  // Register a valid buffer collection.
  BufferCollectionImportExportTokens ref_pair = BufferCollectionImportExportTokens::New();
  REGISTER_BUFFER_COLLECTION(allocator, ref_pair.export_token, CreateToken(), true);

  const ContentId kImageId = {1};
  ImageProperties properties;
  properties.set_width(100);
  properties.set_height(200);

  // Send our only import token to CreateImage(). Buffer collection should be released only after
  // Image creation.
  {
    EXPECT_CALL(*mock_buffer_collection_importer_, ImportBufferImage(_)).WillOnce(Return(true));
    EXPECT_CALL(*mock_buffer_collection_importer_, ReleaseBufferCollection(_)).Times(1);
    flatland->CreateImage(kImageId, std::move(ref_pair.import_token), 0, std::move(properties));
    RunLoopUntilIdle();
  }
}

TEST_F(FlatlandTest, ReleaseBufferCollectionCompletesAfterFlatlandDestruction) {
  allocation::GlobalBufferCollectionId global_collection_id;
  ContentId global_image_id;
  {
    std::shared_ptr<Allocator> allocator = CreateAllocator();
    std::shared_ptr<Flatland> flatland = CreateFlatland();

    const ContentId kImageId = {3};
    BufferCollectionImportExportTokens ref_pair = BufferCollectionImportExportTokens::New();
    ImageProperties properties;
    properties.set_width(200);
    properties.set_height(200);
    auto import_token_dup = ref_pair.DuplicateImportToken();
    auto global_id_pair = CreateImage(flatland.get(), allocator.get(), kImageId,
                                      std::move(ref_pair), std::move(properties));
    global_collection_id = global_id_pair.collection_id;
    global_image_id = {global_id_pair.image_id};

    // Release the image.
    flatland->ReleaseImage(kImageId);

    // Release the buffer collection.

    EXPECT_CALL(*mock_buffer_collection_importer_, ReleaseBufferCollection(global_collection_id))
        .Times(1);
    import_token_dup.value.reset();
    RunLoopUntilIdle();

    // Skip session updates to test that release fences are what trigger the importer calls.
    EXPECT_CALL(*mock_buffer_collection_importer_, ReleaseBufferImage(global_image_id.value))
        .Times(0);
    PresentArgs args;
    args.skip_session_update_and_release_fences = true;
    { PRESENT_WITH_ARGS(flatland, std::move(args), true); }

    // |flatland| falls out of scope.
  }

  // Reset the last known reference to the BufferImporter to demonstrate that the Wait keeps it
  // alive.
  buffer_collection_importer_.reset();

  // Signal the release fences, which triggers the release call, even though the Flatland
  // instance and BufferCollectionImporter associated with the call have been cleaned up.
  EXPECT_CALL(*mock_buffer_collection_importer_, ReleaseBufferImage(global_image_id.value))
      .Times(1);
  ApplySessionUpdatesAndSignalFences();
  RunLoopUntilIdle();
}

// Tests that an Image is not released from the importer until it is not referenced and the release
// fence is signaled.
TEST_F(FlatlandTest, ReleaseImageWaitsForReleaseFence) {
  std::shared_ptr<Allocator> allocator = CreateAllocator();
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  // Setup a valid buffer collection and Image.
  const ContentId kImageId = {1};
  BufferCollectionImportExportTokens ref_pair = BufferCollectionImportExportTokens::New();

  ImageProperties properties;
  properties.set_width(100);
  properties.set_height(200);

  auto import_token_dup = ref_pair.DuplicateImportToken();
  const auto global_id_pair = CreateImage(flatland.get(), allocator.get(), kImageId,
                                          std::move(ref_pair), std::move(properties));
  auto& global_collection_id = global_id_pair.collection_id;

  // Attach the Image to a transform.
  const TransformId kTransformId = {3};
  flatland->CreateTransform(kTransformId);
  flatland->SetRootTransform(kTransformId);
  flatland->SetContent(kTransformId, kImageId);
  PRESENT(flatland, true);

  // Release the buffer collection, but ensure that the ReleaseBufferImage call on the importer has
  // not happened.
  EXPECT_CALL(*mock_buffer_collection_importer_, ReleaseBufferCollection(global_collection_id))
      .Times(1);
  EXPECT_CALL(*mock_buffer_collection_importer_, ReleaseBufferImage(_)).Times(0);
  import_token_dup.value.reset();
  RunLoopUntilIdle();

  // Release the Image that referenced the buffer collection. Because the Image is still attached
  // to a Transform, the deregestration call should still not happen.
  EXPECT_CALL(*mock_buffer_collection_importer_, ReleaseBufferImage(_)).Times(0);
  flatland->ReleaseImage(kImageId);
  PRESENT(flatland, true);

  // Remove the Image from the transform. This triggers the creation of the release fence, but
  // still does not result in a deregestration call. Skip session updates to test that release
  // fences are what trigger the importer calls.
  EXPECT_CALL(*mock_buffer_collection_importer_, ReleaseBufferImage(_)).Times(0);
  flatland->SetContent(kTransformId, {0});

  PresentArgs args;
  args.skip_session_update_and_release_fences = true;
  PRESENT_WITH_ARGS(flatland, std::move(args), true);

  // Signal the release fences, which triggers the release call.
  EXPECT_CALL(*mock_buffer_collection_importer_, ReleaseBufferImage(_)).Times(1);
  ApplySessionUpdatesAndSignalFences();
  RunLoopUntilIdle();
}

TEST_F(FlatlandTest, ReleaseImageErrorCases) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  // Zero is not a valid image ID.
  flatland->ReleaseImage({0});
  PRESENT(flatland, false);

  // The image must exist.
  flatland->ReleaseImage({1});
  PRESENT(flatland, false);

  // ContentId is not an Image.
  ContentLinkToken parent_token;
  GraphLinkToken child_token;
  ASSERT_EQ(ZX_OK, zx::eventpair::create(0, &parent_token.value, &child_token.value));

  const ContentId kLinkId = {2};

  fidl::InterfacePtr<ContentLink> content_link;
  LinkProperties properties;
  properties.set_logical_size({kDefaultSize, kDefaultSize});
  flatland->CreateLink(kLinkId, std::move(parent_token), std::move(properties),
                       content_link.NewRequest());

  flatland->ReleaseImage(kLinkId);
  PRESENT(flatland, false);
}

// If we have multiple BufferCollectionImporters, some of them may properly import
// an image while others do not. We have to therefore make sure that if importer A
// properly imports an image and then importer B fails, that Flatland automatically
// releases the image from importer A.
TEST_F(FlatlandTest, ImageImportPassesAndFailsOnDifferentImportersTest) {
  // Create a second buffer collection importer.
  auto local_mock_buffer_collection_importer = new MockBufferCollectionImporter();
  auto local_buffer_collection_importer =
      std::shared_ptr<allocation::BufferCollectionImporter>(local_mock_buffer_collection_importer);

  // Create flatland and allocator instances that has two BufferCollectionImporters.
  std::vector<std::shared_ptr<allocation::BufferCollectionImporter>> importers(
      {buffer_collection_importer_, local_buffer_collection_importer});
  std::vector<std::shared_ptr<allocation::BufferCollectionImporter>> screenshot_importers;
  std::shared_ptr<Allocator> allocator =
      std::make_shared<Allocator>(context_provider_.context(), importers, screenshot_importers,
                                  utils::CreateSysmemAllocatorSyncPtr());
  auto session_id = scheduling::GetNextSessionId();
  fuchsia::ui::scenic::internal::FlatlandPtr flatland_ptr;
  auto flatland = Flatland::New(
      std::make_shared<utils::UnownedDispatcherHolder>(dispatcher()), flatland_ptr.NewRequest(),
      session_id,
      /*destroy_instance_functon=*/[]() {}, flatland_presenter_, link_system_,
      uber_struct_system_->AllocateQueueForSession(session_id), importers);
  EXPECT_CALL(*local_mock_buffer_collection_importer, ImportBufferCollection(_, _, _))
      .WillOnce(Return(true));

  BufferCollectionImportExportTokens ref_pair = BufferCollectionImportExportTokens::New();
  REGISTER_BUFFER_COLLECTION(allocator, ref_pair.export_token, CreateToken(), true);

  ImageProperties properties;
  properties.set_width(100);
  properties.set_height(200);

  // We have the first importer return true, signifying a successful import, and the second one
  // returning false. This should trigger the first importer to call ReleaseBufferImage().
  EXPECT_CALL(*mock_buffer_collection_importer_, ImportBufferImage(_)).WillOnce(Return(true));
  EXPECT_CALL(*local_mock_buffer_collection_importer, ImportBufferImage(_)).WillOnce(Return(false));
  EXPECT_CALL(*mock_buffer_collection_importer_, ReleaseBufferImage(_)).WillOnce(Return());
  flatland->CreateImage(/*image_id*/ {1}, std::move(ref_pair.import_token), /*vmo_idx*/ 0,
                        std::move(properties));
}

// Test to make sure that if a buffer collection importer returns |false|
// on |ImportBufferImage()| that this is caught when we try to present.
TEST_F(FlatlandTest, BufferImporterImportImageReturnsFalseTest) {
  std::shared_ptr<Allocator> allocator = CreateAllocator();
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  BufferCollectionImportExportTokens ref_pair = BufferCollectionImportExportTokens::New();
  REGISTER_BUFFER_COLLECTION(allocator, ref_pair.export_token, CreateToken(), true);

  // Create a proper properties struct.
  ImageProperties properties;
  properties.set_width(150);
  properties.set_height(175);

  EXPECT_CALL(*mock_buffer_collection_importer_, ImportBufferImage(_)).WillOnce(Return(true));

  // We've imported a proper image and we have the importer returning true, so
  // PRESENT should return true.
  flatland->CreateImage(/*image_id*/ {1}, ref_pair.DuplicateImportToken(), /*vmo_idx*/ 0,
                        std::move(properties));
  PRESENT(flatland, true);

  // We're using the same buffer collection so we don't need to validate, only import.
  EXPECT_CALL(*mock_buffer_collection_importer_, ImportBufferImage(_)).WillOnce(Return(false));

  // Import again, but this time have the importer return false. Flatland should catch
  // this and PRESENT should return false.
  properties.set_width(150);
  properties.set_height(175);
  flatland->CreateImage(/*image_id*/ {2}, ref_pair.DuplicateImportToken(), /*vmo_idx*/ 0,
                        std::move(properties));
  PRESENT(flatland, false);
}

// Test to make sure that the release fences signal to the buffer importer
// to release the image.
TEST_F(FlatlandTest, BufferImporterImageReleaseTest) {
  std::shared_ptr<Allocator> allocator = CreateAllocator();
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  // Setup a valid image.
  const ContentId kImageId = {1};
  BufferCollectionImportExportTokens ref_pair = BufferCollectionImportExportTokens::New();

  ImageProperties properties1;
  properties1.set_width(100);
  properties1.set_height(200);

  const allocation::GlobalBufferCollectionId global_collection_id1 =
      CreateImage(flatland.get(), allocator.get(), kImageId, std::move(ref_pair),
                  std::move(properties1))
          .collection_id;

  // Create a transform, make it the root transform, and attach the image.
  const TransformId kTransformId = {2};

  flatland->CreateTransform(kTransformId);
  flatland->SetRootTransform(kTransformId);
  flatland->SetContent(kTransformId, kImageId);
  PRESENT(flatland, true);

  // Now release the image.
  flatland->ReleaseImage(kImageId);
  PRESENT(flatland, true);

  // Now remove the image from the transform, which should result in it being
  // garbage collected.
  flatland->SetContent(kTransformId, {0});
  PresentArgs args;
  args.skip_session_update_and_release_fences = true;
  PRESENT_WITH_ARGS(flatland, std::move(args), true);

  EXPECT_CALL(*mock_buffer_collection_importer_, ReleaseBufferImage(_)).Times(1);
  ApplySessionUpdatesAndSignalFences();
  RunLoopUntilIdle();
}

TEST_F(FlatlandTest, ReleasedImageRemainsUntilCleared) {
  std::shared_ptr<Allocator> allocator = CreateAllocator();
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  // Setup a valid image.
  const ContentId kImageId = {1};
  BufferCollectionImportExportTokens ref_pair = BufferCollectionImportExportTokens::New();

  ImageProperties properties1;
  properties1.set_width(100);
  properties1.set_height(200);

  const allocation::GlobalBufferCollectionId global_collection_id =
      CreateImage(flatland.get(), allocator.get(), kImageId, std::move(ref_pair),
                  std::move(properties1))
          .collection_id;

  const auto maybe_image_handle = flatland->GetContentHandle(kImageId);
  ASSERT_TRUE(maybe_image_handle.has_value());
  const auto image_handle = maybe_image_handle.value();

  // Create a transform, make it the root transform, and attach the image.
  const TransformId kTransformId = {2};

  flatland->CreateTransform(kTransformId);
  flatland->SetRootTransform(kTransformId);
  flatland->SetContent(kTransformId, kImageId);
  PRESENT(flatland, true);

  // The image handle should be the last handle in the local_topology, and the image should be in
  // the image map.
  auto uber_struct = GetUberStruct(flatland.get());
  EXPECT_EQ(uber_struct->local_topology.back().handle, image_handle);

  auto image_kv = uber_struct->images.find(image_handle);
  EXPECT_NE(image_kv, uber_struct->images.end());
  EXPECT_EQ(image_kv->second.collection_id, global_collection_id);

  // Releasing the image succeeds, but all data remains in the UberStruct.
  flatland->ReleaseImage(kImageId);
  PRESENT(flatland, true);

  uber_struct = GetUberStruct(flatland.get());
  EXPECT_EQ(uber_struct->local_topology.back().handle, image_handle);

  image_kv = uber_struct->images.find(image_handle);
  EXPECT_NE(image_kv, uber_struct->images.end());
  EXPECT_EQ(image_kv->second.collection_id, global_collection_id);

  // Clearing the Transform of its Image removes all references from the UberStruct.
  EXPECT_CALL(*mock_buffer_collection_importer_, ReleaseBufferImage(_)).Times(1);
  flatland->SetContent(kTransformId, {0});
  PRESENT(flatland, true);

  uber_struct = GetUberStruct(flatland.get());
  for (const auto& entry : uber_struct->local_topology) {
    EXPECT_NE(entry.handle, image_handle);
  }

  EXPECT_FALSE(uber_struct->images.count(image_handle));
}

TEST_F(FlatlandTest, ReleasedImageIdCanBeReused) {
  std::shared_ptr<Allocator> allocator = CreateAllocator();
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  // Setup a valid image.
  const ContentId kImageId = {1};
  BufferCollectionImportExportTokens ref_pair_1 = BufferCollectionImportExportTokens::New();

  ImageProperties properties1;
  properties1.set_width(100);
  properties1.set_height(200);

  const allocation::GlobalBufferCollectionId global_collection_id1 =
      CreateImage(flatland.get(), allocator.get(), kImageId, std::move(ref_pair_1),
                  std::move(properties1))
          .collection_id;

  const auto maybe_image_handle1 = flatland->GetContentHandle(kImageId);
  ASSERT_TRUE(maybe_image_handle1.has_value());
  const auto image_handle1 = maybe_image_handle1.value();

  // Create a transform, make it the root transform, attach the image, then release it.
  const TransformId kTransformId1 = {2};

  flatland->CreateTransform(kTransformId1);
  flatland->SetRootTransform(kTransformId1);
  flatland->SetContent(kTransformId1, kImageId);
  flatland->ReleaseImage(kImageId);
  PRESENT(flatland, true);

  // The ContentId can be re-used even though the old image is still present. Add a second
  // transform so that both images show up in the global image vector.
  BufferCollectionImportExportTokens ref_pair_2 = BufferCollectionImportExportTokens::New();
  ImageProperties properties2;
  properties2.set_width(300);
  properties2.set_height(400);

  const allocation::GlobalBufferCollectionId global_collection_id2 =
      CreateImage(flatland.get(), allocator.get(), kImageId, std::move(ref_pair_2),
                  std::move(properties2))
          .collection_id;

  const TransformId kTransformId2 = {3};

  flatland->CreateTransform(kTransformId2);
  flatland->AddChild(kTransformId1, kTransformId2);
  flatland->SetContent(kTransformId2, kImageId);
  PRESENT(flatland, true);

  const auto maybe_image_handle2 = flatland->GetContentHandle(kImageId);
  ASSERT_TRUE(maybe_image_handle2.has_value());
  const auto image_handle2 = maybe_image_handle2.value();

  // Both images should appear in the image map.
  auto uber_struct = GetUberStruct(flatland.get());

  auto image_kv1 = uber_struct->images.find(image_handle1);
  EXPECT_NE(image_kv1, uber_struct->images.end());
  EXPECT_EQ(image_kv1->second.collection_id, global_collection_id1);

  auto image_kv2 = uber_struct->images.find(image_handle2);
  EXPECT_NE(image_kv2, uber_struct->images.end());
  EXPECT_EQ(image_kv2->second.collection_id, global_collection_id2);
}

// Test that released Images, when attached to a Transform, are not garbage collected even if
// the Transform is not part of the most recently presented global topology.
TEST_F(FlatlandTest, ReleasedImagePersistsOutsideGlobalTopology) {
  std::shared_ptr<Allocator> allocator = CreateAllocator();
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  // Setup a valid image.
  const ContentId kImageId = {1};
  BufferCollectionImportExportTokens ref_pair = BufferCollectionImportExportTokens::New();

  ImageProperties properties1;
  properties1.set_width(100);
  properties1.set_height(200);

  const allocation::GlobalBufferCollectionId global_collection_id1 =
      CreateImage(flatland.get(), allocator.get(), kImageId, std::move(ref_pair),
                  std::move(properties1))
          .collection_id;

  const auto maybe_image_handle = flatland->GetContentHandle(kImageId);
  ASSERT_TRUE(maybe_image_handle.has_value());
  const auto image_handle = maybe_image_handle.value();

  // Create a transform, make it the root transform, attach the image, then release it.
  const TransformId kTransformId = {2};

  flatland->CreateTransform(kTransformId);
  flatland->SetRootTransform(kTransformId);
  flatland->SetContent(kTransformId, kImageId);
  flatland->ReleaseImage(kImageId);
  PRESENT(flatland, true);

  // Remove the entire hierarchy, then verify that the image is still present.
  flatland->SetRootTransform({0});
  PRESENT(flatland, true);

  auto uber_struct = GetUberStruct(flatland.get());
  auto image_kv = uber_struct->images.find(image_handle);
  EXPECT_NE(image_kv, uber_struct->images.end());
  EXPECT_EQ(image_kv->second.collection_id, global_collection_id1);

  // Reintroduce the hierarchy and confirm the Image is still present, even though it was
  // temporarily not reachable from the root transform.
  flatland->SetRootTransform(kTransformId);
  PRESENT(flatland, true);

  uber_struct = GetUberStruct(flatland.get());
  EXPECT_EQ(uber_struct->local_topology.back().handle, image_handle);

  image_kv = uber_struct->images.find(image_handle);
  EXPECT_NE(image_kv, uber_struct->images.end());
  EXPECT_EQ(image_kv->second.collection_id, global_collection_id1);
}

TEST_F(FlatlandTest, ClearGraphReleasesImagesAndBufferCollections) {
  std::shared_ptr<Allocator> allocator = CreateAllocator();
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  // Setup a valid image.
  const ContentId kImageId = {1};
  BufferCollectionImportExportTokens ref_pair_1 = BufferCollectionImportExportTokens::New();

  ImageProperties properties1;
  properties1.set_width(100);
  properties1.set_height(200);

  auto import_token_dup = ref_pair_1.DuplicateImportToken();
  const allocation::GlobalBufferCollectionId global_collection_id1 =
      CreateImage(flatland.get(), allocator.get(), kImageId, std::move(ref_pair_1),
                  std::move(properties1))
          .collection_id;

  // Create a transform, make it the root transform, and attach the Image.
  const TransformId kTransformId = {2};

  flatland->CreateTransform(kTransformId);
  flatland->SetRootTransform(kTransformId);
  flatland->SetContent(kTransformId, kImageId);
  PRESENT(flatland, true);

  // Clear the graph, then signal the release fence and ensure the buffer collection is released.
  flatland->ClearGraph();
  import_token_dup.value.reset();

  EXPECT_CALL(*mock_buffer_collection_importer_, ReleaseBufferCollection(global_collection_id1))
      .Times(1);
  EXPECT_CALL(*mock_buffer_collection_importer_, ReleaseBufferImage(_)).Times(1);
  PRESENT(flatland, true);

  // The Image ID should be available for re-use.
  BufferCollectionImportExportTokens ref_pair_2 = BufferCollectionImportExportTokens::New();
  ImageProperties properties2;
  properties2.set_width(400);
  properties2.set_height(800);

  const allocation::GlobalBufferCollectionId global_collection_id2 =
      CreateImage(flatland.get(), allocator.get(), kImageId, std::move(ref_pair_2),
                  std::move(properties2))
          .collection_id;

  EXPECT_NE(global_collection_id1, global_collection_id2);

  // Verify that the Image is valid and can be attached to a transform.
  flatland->CreateTransform(kTransformId);
  flatland->SetRootTransform(kTransformId);
  flatland->SetContent(kTransformId, kImageId);
  PRESENT(flatland, true);
}

TEST_F(FlatlandTest, UnsquashableUpdates_ShouldBeReflectedInScheduleUpdates) {
  std::shared_ptr<Flatland> flatland = CreateFlatland();

  // We call Present() twice, each time passing a different value as the squashable argument.
  // We EXPECT that the ensuing ScheduleUpdateForSession() call to the frame scheduler will reflect
  // the passed in squashable value.

  // Present with the squashable field set to true.
  {
    PresentArgs args;
    args.squashable = true;
    PRESENT_WITH_ARGS(flatland, std::move(args), true);
  }

  // Present with the squashable field set to false.
  {
    PresentArgs args;
    args.squashable = false;
    PRESENT_WITH_ARGS(flatland, std::move(args), true);
  }
}

#undef EXPECT_MATRIX
#undef PRESENT
#undef PRESENT_WITH_ARGS
#undef REGISTER_BUFFER_COLLECTION

}  // namespace test
}  // namespace flatland
