// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/crash_reports/snapshot_collector.h"

#include <lib/async/cpp/executor.h>
#include <lib/fit/function.h>
#include <lib/zx/time.h>

#include <fstream>
#include <map>
#include <memory>
#include <optional>
#include <string>
#include <variant>
#include <vector>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/developer/forensics/crash_reports/product.h"
#include "src/developer/forensics/crash_reports/tests/scoped_test_report_store.h"
#include "src/developer/forensics/feedback/annotations/annotation_manager.h"
#include "src/developer/forensics/testing/gmatchers.h"
#include "src/developer/forensics/testing/gpretty_printers.h"
#include "src/developer/forensics/testing/stubs/data_provider.h"
#include "src/developer/forensics/testing/unit_test_fixture.h"
#include "src/lib/files/path.h"
#include "src/lib/files/scoped_temp_dir.h"
#include "src/lib/timekeeper/clock.h"
#include "src/lib/timekeeper/test_clock.h"

namespace forensics {
namespace crash_reports {
namespace {

using testing::Contains;
using testing::IsEmpty;
using testing::IsSupersetOf;
using testing::Pair;
using testing::UnorderedElementsAreArray;

constexpr zx::duration kWindow{zx::min(1)};

const std::map<std::string, std::string> kDefaultAnnotations = {
    {"annotation.key.one", "annotation.value.one"},
    {"annotation.key.two", "annotation.value.two"},
};

const std::string kDefaultArchiveKey = "snapshot.key";
constexpr char kProgramName[] = "crashing_program";

MissingSnapshot AsMissing(Snapshot snapshot) {
  FX_CHECK(std::holds_alternative<MissingSnapshot>(snapshot));
  return std::get<MissingSnapshot>(snapshot);
}

feedback::Annotations BuildFeedbackAnnotations(
    const std::map<std::string, std::string>& annotations) {
  feedback::Annotations ret_annotations;
  for (const auto& [key, value] : annotations) {
    ret_annotations.insert({key, value});
  }
  return ret_annotations;
}

class SnapshotCollectorTest : public UnitTestFixture {
 public:
  SnapshotCollectorTest()
      : UnitTestFixture(),
        clock_(),
        executor_(dispatcher()),
        snapshot_collector_(nullptr),
        report_store_(&annotation_manager_, std::make_shared<InfoContext>(
                                                &InspectRoot(), &clock_, dispatcher(), services())),
        path_(files::JoinPath(tmp_dir_.path(), "garbage_collected_snapshots.txt")) {}

 protected:
  void SetUpDefaultSnapshotManager() {
    SetUpSnapshotManager(StorageSize::Megabytes(1u), StorageSize::Megabytes(1u));
  }

  void SetUpSnapshotManager(StorageSize max_annotations_size, StorageSize max_archives_size) {
    FX_CHECK(data_provider_server_);
    clock_.Set(zx::time(0u));
    snapshot_collector_ = std::make_unique<SnapshotCollector>(
        dispatcher(), &clock_, data_provider_server_.get(),
        report_store_.GetReportStore().GetSnapshotStore(), kWindow);
  }

  std::set<std::string> ReadGarbageCollectedSnapshots() {
    std::set<std::string> garbage_collected_snapshots;

    std::ifstream file(path_);
    for (std::string uuid; getline(file, uuid);) {
      garbage_collected_snapshots.insert(uuid);
    }

    return garbage_collected_snapshots;
  }

  void ClearGarbageCollectedSnapshots() { files::DeletePath(path_, /*recursive=*/true); }

  void SetUpDefaultDataProviderServer() {
    SetUpDataProviderServer(
        std::make_unique<stubs::DataProvider>(kDefaultAnnotations, kDefaultArchiveKey));
  }

  void SetUpDataProviderServer(std::unique_ptr<stubs::DataProviderBase> data_provider_server) {
    data_provider_server_ = std::move(data_provider_server);
  }

  void ScheduleGetReportAndThen(const zx::duration timeout, ReportId report_id,
                                ::fit::function<void(Report&)> and_then) {
    timekeeper::time_utc utc_time;
    FX_CHECK(clock_.UtcNow(&utc_time) == ZX_OK);

    Product product{
        .name = "some name",
        .version = "some version",
        .channel = "some channel",
    };

    fuchsia::feedback::CrashReport report;
    report.set_program_name(kProgramName);

    executor_.schedule_task(snapshot_collector_
                                ->GetReport(timeout, std::move(report), report_id, utc_time,
                                            product, false, ReportingPolicy::kUpload)
                                .and_then(std::move(and_then))
                                .or_else([]() { FX_CHECK(false); }));
  }

  void CloseConnection() { data_provider_server_->CloseConnection(); }

  bool is_server_bound() { return data_provider_server_->IsBound(); }

  Snapshot GetSnapshot(const std::string& uuid) {
    return report_store_.GetReportStore().GetSnapshotStore()->GetSnapshot(uuid);
  }

  timekeeper::TestClock clock_;
  async::Executor executor_;
  std::unique_ptr<SnapshotCollector> snapshot_collector_;
  feedback::AnnotationManager annotation_manager_{dispatcher(), {}};
  ScopedTestReportStore report_store_;

 private:
  std::unique_ptr<stubs::DataProviderBase> data_provider_server_;
  files::ScopedTempDir tmp_dir_;
  std::string path_;
};

TEST_F(SnapshotCollectorTest, Check_GetReport) {
  SetUpDefaultDataProviderServer();
  SetUpDefaultSnapshotManager();

  std::optional<Report> report{std::nullopt};
  ScheduleGetReportAndThen(zx::duration::infinite(), 0,
                           ([&report](Report& new_report) { report = std::move(new_report); }));

  // |report| should only have a value once |kWindow| has passed.
  RunLoopUntilIdle();
  ASSERT_FALSE(report.has_value());

  RunLoopFor(kWindow);
  ASSERT_TRUE(report.has_value());
}

TEST_F(SnapshotCollectorTest, Check_GetReportRequestsCombined) {
  SetUpDefaultDataProviderServer();
  SetUpDefaultSnapshotManager();

  const size_t kNumRequests{5u};

  size_t num_snapshot_uuid1{0};
  std::optional<std::string> snapshot_uuid1{std::nullopt};
  for (size_t i = 0; i < kNumRequests; ++i) {
    ScheduleGetReportAndThen(zx::duration::infinite(), i,
                             ([&snapshot_uuid1, &num_snapshot_uuid1](Report& new_report) {
                               if (!snapshot_uuid1.has_value()) {
                                 snapshot_uuid1 = new_report.SnapshotUuid();
                               } else {
                                 FX_CHECK(snapshot_uuid1.value() == new_report.SnapshotUuid());
                               }
                               ++num_snapshot_uuid1;
                             }));
  }
  RunLoopFor(kWindow);
  ASSERT_EQ(num_snapshot_uuid1, kNumRequests);

  size_t num_snapshot_uuid2{0};
  std::optional<std::string> snapshot_uuid2{std::nullopt};
  for (size_t i = 0; i < kNumRequests; ++i) {
    ScheduleGetReportAndThen(zx::duration::infinite(), kNumRequests + i,
                             ([&snapshot_uuid2, &num_snapshot_uuid2](Report& new_report) {
                               if (!snapshot_uuid2.has_value()) {
                                 snapshot_uuid2 = new_report.SnapshotUuid();
                               } else {
                                 FX_CHECK(snapshot_uuid2.value() == new_report.SnapshotUuid());
                               }
                               ++num_snapshot_uuid2;
                             }));
  }
  RunLoopFor(kWindow);
  ASSERT_EQ(num_snapshot_uuid2, kNumRequests);

  ASSERT_TRUE(snapshot_uuid1.has_value());
  ASSERT_TRUE(snapshot_uuid2.has_value());
  EXPECT_NE(snapshot_uuid1.value(), snapshot_uuid2.value());
}

TEST_F(SnapshotCollectorTest, Check_Timeout) {
  SetUpDefaultDataProviderServer();
  SetUpDefaultSnapshotManager();

  std::optional<Report> report{std::nullopt};
  ScheduleGetReportAndThen(zx::sec(0), 0,
                           ([&report](Report& new_report) { report = std::move(new_report); }));
  RunLoopFor(kWindow);

  ASSERT_TRUE(report.has_value());
  auto snapshot = AsMissing(GetSnapshot(report->SnapshotUuid()));
  EXPECT_THAT(snapshot.PresenceAnnotations(), UnorderedElementsAreArray({
                                                  Pair("debug.snapshot.error", "timeout"),
                                                  Pair("debug.snapshot.present", "false"),
                                              }));
}

TEST_F(SnapshotCollectorTest, Check_Shutdown) {
  SetUpDefaultDataProviderServer();
  SetUpDefaultSnapshotManager();

  std::optional<Report> report{std::nullopt};
  ScheduleGetReportAndThen(zx::duration::infinite(), 0,
                           ([&report](Report& new_report) { report = std::move(new_report); }));
  snapshot_collector_->Shutdown();
  RunLoopUntilIdle();

  ASSERT_TRUE(report.has_value());
  auto snapshot = AsMissing(GetSnapshot(report->SnapshotUuid()));
  EXPECT_THAT(snapshot.PresenceAnnotations(), IsSupersetOf({
                                                  Pair("debug.snapshot.error", "system shutdown"),
                                                  Pair("debug.snapshot.present", "false"),
                                              }));

  report = std::nullopt;
  ScheduleGetReportAndThen(zx::duration::infinite(), 1,
                           ([&report](Report& new_report) { report = std::move(new_report); }));
  RunLoopUntilIdle();

  ASSERT_TRUE(report.has_value());
  snapshot = AsMissing(GetSnapshot(report->SnapshotUuid()));
  EXPECT_THAT(snapshot.PresenceAnnotations(), IsSupersetOf({
                                                  Pair("debug.snapshot.error", "system shutdown"),
                                                  Pair("debug.snapshot.present", "false"),
                                              }));
}

TEST_F(SnapshotCollectorTest, Check_SetsPresenceAnnotations) {
  SetUpDefaultDataProviderServer();
  SetUpDefaultSnapshotManager();

  std::optional<Report> report{std::nullopt};
  ScheduleGetReportAndThen(zx::duration::infinite(), 0,
                           ([&report](Report& new_report) { report = std::move(new_report); }));

  RunLoopFor(kWindow);
  ASSERT_TRUE(report.has_value());

  EXPECT_THAT(BuildFeedbackAnnotations(report->Annotations().Raw()),
              IsSupersetOf({
                  Pair("debug.snapshot.shared-request.num-clients", std::to_string(1)),
                  Pair("debug.snapshot.shared-request.uuid", report->SnapshotUuid()),
              }));
}

}  // namespace
}  // namespace crash_reports
}  // namespace forensics
