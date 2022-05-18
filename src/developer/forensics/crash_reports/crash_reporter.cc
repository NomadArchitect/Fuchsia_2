// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/crash_reports/crash_reporter.h"

#include <fuchsia/mem/cpp/fidl.h>
#include <lib/async/cpp/task.h>
#include <lib/fit/defer.h>
#include <lib/fpromise/promise.h>
#include <lib/fpromise/result.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/clock.h>
#include <lib/zx/time.h>
#include <zircon/errors.h>
#include <zircon/types.h>
#include <zircon/utc.h>

#include <map>
#include <memory>
#include <optional>
#include <string>
#include <tuple>
#include <utility>

#include "src/developer/forensics/crash_reports/config.h"
#include "src/developer/forensics/crash_reports/constants.h"
#include "src/developer/forensics/crash_reports/crash_server.h"
#include "src/developer/forensics/crash_reports/default_annotations.h"
#include "src/developer/forensics/crash_reports/product.h"
#include "src/developer/forensics/crash_reports/report.h"
#include "src/developer/forensics/crash_reports/report_util.h"
#include "src/developer/forensics/feedback/annotations/annotation_manager.h"
#include "src/developer/forensics/feedback/annotations/constants.h"
#include "src/developer/forensics/utils/cobalt/metrics.h"
#include "src/developer/forensics/utils/errors.h"
#include "src/lib/timekeeper/system_clock.h"

namespace forensics {
namespace crash_reports {
namespace {

using FidlSnapshot = fuchsia::feedback::Snapshot;
using fuchsia::feedback::CrashReport;

constexpr zx::duration kSnapshotTimeout = zx::min(2);

// Returns what the initial ReportId should be, based on the contents of the store in the
// filesystem.
//
// Note: This function traverses store in the filesystem to and should be used sparingly.
ReportId SeedReportId() {
  // The next ReportId will be one more than the largest in the store.
  auto all_report_ids = StoreMetadata(kStoreTmpPath, kStoreMaxTmpSize).Reports();
  const auto all_cache_report_ids = StoreMetadata(kStoreCachePath, kStoreMaxCacheSize).Reports();
  all_report_ids.insert(all_report_ids.end(), all_cache_report_ids.begin(),
                        all_cache_report_ids.end());

  std::sort(all_report_ids.begin(), all_report_ids.end());
  return (all_report_ids.empty()) ? 0u : all_report_ids.back() + 1;
}

// Make the appropriate ReportingPolicyWatcher for the upload policy in |config|.
std::unique_ptr<ReportingPolicyWatcher> MakeReportingPolicyWatcher(
    async_dispatcher_t* dispatcher, std::shared_ptr<sys::ServiceDirectory> services,
    const Config& config) {
  switch (config.crash_server.upload_policy) {
    case CrashServerConfig::UploadPolicy::ENABLED:
      // Uploads being enabled in |config| is explcit consent to upload all reports.
      return std::make_unique<StaticReportingPolicyWatcher<ReportingPolicy::kUpload>>();
    case CrashServerConfig::UploadPolicy::DISABLED:
      // Uploads being disabled in |config| means that reports should be archived.
      return std::make_unique<StaticReportingPolicyWatcher<ReportingPolicy::kArchive>>();
    case CrashServerConfig::UploadPolicy::READ_FROM_PRIVACY_SETTINGS:
      return std::make_unique<UserReportingPolicyWatcher>(dispatcher, std::move(services));
  }
}

}  // namespace

CrashReporter::CrashReporter(async_dispatcher_t* dispatcher,
                             const std::shared_ptr<sys::ServiceDirectory>& services,
                             timekeeper::Clock* clock,
                             const std::shared_ptr<InfoContext>& info_context, Config config,
                             feedback::AnnotationManager* annotation_manager,
                             CrashRegister* crash_register, LogTags* tags,
                             SnapshotManager* snapshot_manager, CrashServer* crash_server)
    : dispatcher_(dispatcher),
      executor_(dispatcher),
      services_(services),
      tags_(tags),
      annotation_manager_(annotation_manager),
      crash_register_(crash_register),
      utc_provider_(dispatcher_, zx::unowned_clock(zx_utc_reference_get()), clock),
      snapshot_manager_(snapshot_manager),
      crash_server_(crash_server),
      queue_(dispatcher_, services_, info_context, tags_, crash_server_, snapshot_manager_),
      product_quotas_(dispatcher_, config.daily_per_product_quota),
      info_(info_context),
      network_watcher_(dispatcher_, *services_),
      reporting_policy_watcher_(MakeReportingPolicyWatcher(dispatcher_, services, config)) {
  FX_CHECK(dispatcher_);
  FX_CHECK(services_);
  FX_CHECK(crash_register_);
  FX_CHECK(crash_server_);

  next_report_id_ = SeedReportId();

  queue_.WatchReportingPolicy(reporting_policy_watcher_.get());
  queue_.WatchNetwork(&network_watcher_);

  info_.ExposeReportingPolicy(reporting_policy_watcher_.get());

  if (config.hourly_snapshot) {
    // We schedule the first hourly snapshot in 5 minutes and then it will auto-schedule itself
    // every hour after that.
    ScheduleHourlySnapshot(zx::min(5));
  }
}

void CrashReporter::PersistAllCrashReports() {
  queue_.StopUploading();
  snapshot_manager_->Shutdown();
}

void CrashReporter::File(fuchsia::feedback::CrashReport report, FileCallback callback) {
  if (!report.has_program_name()) {
    FX_LOGS(ERROR) << "Input report missing required program name. Won't file.";
    callback(::fpromise::error(ZX_ERR_INVALID_ARGS));
    info_.LogCrashState(cobalt::CrashState::kDropped);
    return;
  }

  // Execute the callback informing the client the report has been filed. The rest of the async flow
  // can take quite some time and blocking clients would defeat the purpose of sharing the snapshot.
  callback(::fpromise::ok());

  File(std::move(report), /*is_hourly_snapshot=*/false);
}

void CrashReporter::File(fuchsia::feedback::CrashReport report, const bool is_hourly_snapshot) {
  if (reporting_policy_watcher_->CurrentPolicy() == ReportingPolicy::kDoNotFileAndDelete) {
    info_.LogCrashState(cobalt::CrashState::kDeleted);
    return;
  }

  const auto program_name = report.program_name();
  const auto report_id = next_report_id_++;

  // Fetch the product as close to the crash as possible. The product may be re-registered / changed
  // after the crash and getting it now is an attempt to mitigate that race.
  const auto product = crash_register_->HasProduct(program_name)
                           ? crash_register_->GetProduct(program_name)
                           : Product::DefaultPlatformProduct();

  tags_->Register(report_id, {Logname(program_name)});

  // Logs a cobalt event and error message on why filing |report| didn't succeed.
  auto record_failure = [this, report_id](const auto cobalt_error, const auto log) {
    FX_LOGST(ERROR, tags_->Get(report_id)) << log;
    info_.LogCrashState(cobalt_error);
    tags_->Unregister(report_id);
  };

  if (!product_quotas_.HasQuotaRemaining(product)) {
    return record_failure(cobalt::CrashState::kOnDeviceQuotaReached,
                          "Daily report quota reached. Won't retry");
  }

  product_quotas_.DecrementRemainingQuota(product);

  if (is_hourly_snapshot) {
    FX_LOGST(INFO, tags_->Get(report_id)) << "Generating hourly snapshot";
  } else {
    FX_LOGST(INFO, tags_->Get(report_id)) << "Generating report";
  }

  auto p =
      snapshot_manager_->GetSnapshotUuid(kSnapshotTimeout)
          .and_then([this, fidl_report = std::move(report), product = std::move(product), report_id,
                     is_hourly_snapshot, record_failure](const std::string& snapshot_uuid) mutable {
            const auto snapshot = snapshot_manager_->GetSnapshot(snapshot_uuid);
            const auto current_time = utc_provider_.CurrentTime();
            const auto annotations = annotation_manager_->ImmediatelyAvailable();

            // Update the default product with the immediately available annotations (which should
            // contain the version and channel).
            if (product.IsDefaultPlatformProduct()) {
              CrashRegister::AddVersionAndChannel(product, annotations);
            }

            const auto device_id = (annotations.count(feedback::kDeviceFeedbackIdKey) != 0)
                                       ? annotations.at(feedback::kDeviceFeedbackIdKey)
                                       : ErrorOr<std::string>(Error::kMissingValue);

            auto report = MakeReport(std::move(fidl_report), report_id, snapshot_uuid, snapshot,
                                     current_time, device_id, BuildDefaultAnnotations(annotations),
                                     product, is_hourly_snapshot);

            if (is_hourly_snapshot) {
              FX_LOGST(INFO, tags_->Get(report_id)) << "Generated hourly snapshot";
            } else {
              FX_LOGST(INFO, tags_->Get(report_id)) << "Generated report";
            }

            if (!report.has_value()) {
              return record_failure(cobalt::CrashState::kDropped,
                                    "Failed to file report: MakeReport failed. Won't retry");
            }

            if (!queue_.Add(std::move(*report))) {
              return record_failure(cobalt::CrashState::kDropped,
                                    "Failed to file report: Queue::Add failed. Won't retry");
            }

            info_.LogCrashState(cobalt::CrashState::kFiled);
          });

  executor_.schedule_task(std::move(p));
}

void CrashReporter::ScheduleHourlySnapshot(const zx::duration delay) {
  async::PostDelayedTask(
      dispatcher_,
      [this]() {
        auto schedule_next = ::fit::defer([this] { ScheduleHourlySnapshot(zx::hour(1)); });

        if (queue_.HasHourlyReport()) {
          FX_LOGS(INFO) << "Skipping hourly snapshot as the last one has not been uploaded yet "
                           "– connectivity issues?";
          return;
        }

        fuchsia::feedback::CrashReport report;
        report.set_program_name(kHourlySnapshotProgramName)
            .set_program_uptime(zx_clock_get_monotonic())
            .set_is_fatal(false)
            .set_crash_signature(kHourlySnapshotSignature);

        File(std::move(report), /*is_hourly_snapshot=*/true);
      },
      delay);
}

}  // namespace crash_reports
}  // namespace forensics
