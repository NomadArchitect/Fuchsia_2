// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_CRASH_REPORTS_CRASH_REPORTER_H_
#define SRC_DEVELOPER_FORENSICS_CRASH_REPORTS_CRASH_REPORTER_H_

#include <fuchsia/feedback/cpp/fidl.h>
#include <lib/async/cpp/executor.h>
#include <lib/async/dispatcher.h>
#include <lib/sys/cpp/service_directory.h>

#include <memory>
#include <string>

#include "src/developer/forensics/crash_reports/config.h"
#include "src/developer/forensics/crash_reports/crash_register.h"
#include "src/developer/forensics/crash_reports/crash_server.h"
#include "src/developer/forensics/crash_reports/info/crash_reporter_info.h"
#include "src/developer/forensics/crash_reports/info/info_context.h"
#include "src/developer/forensics/crash_reports/log_tags.h"
#include "src/developer/forensics/crash_reports/network_watcher.h"
#include "src/developer/forensics/crash_reports/product_quotas.h"
#include "src/developer/forensics/crash_reports/queue.h"
#include "src/developer/forensics/crash_reports/report_id.h"
#include "src/developer/forensics/crash_reports/reporting_policy_watcher.h"
#include "src/developer/forensics/crash_reports/snapshot_manager.h"
#include "src/developer/forensics/utils/errors.h"
#include "src/developer/forensics/utils/fidl/device_id_provider_ptr.h"
#include "src/developer/forensics/utils/utc_time_provider.h"
#include "src/lib/fxl/macros.h"
#include "src/lib/timekeeper/clock.h"

namespace forensics {
namespace crash_reports {

class CrashReporter : public fuchsia::feedback::CrashReporter {
 public:
  // Static factory method.
  //
  // Returns nullptr if the crash reporter cannot be instantiated.
  static std::unique_ptr<CrashReporter> TryCreate(
      async_dispatcher_t* dispatcher, const std::shared_ptr<sys::ServiceDirectory>& services,
      timekeeper::Clock* clock, const std::shared_ptr<InfoContext>& info_context, Config config,
      AnnotationMap default_annotations, CrashRegister* crash_register);

  // For testing purposes and injecting a stub CrashServer.
  CrashReporter(async_dispatcher_t* dispatcher,
                const std::shared_ptr<sys::ServiceDirectory>& services, timekeeper::Clock* clock,
                const std::shared_ptr<InfoContext>& info_context, Config config,
                AnnotationMap default_annotations, CrashRegister* crash_register,
                std::unique_ptr<LogTags> tags, std::unique_ptr<SnapshotManager> snapshot_manager,
                std::unique_ptr<CrashServer> crash_server);

  // The crash reporter should stop uploading crash reports and persist any future and pending crash
  // reports.
  void PersistAllCrashReports();

  // |fuchsia::feedback::CrashReporter|
  void File(fuchsia::feedback::CrashReport report, FileCallback callback) override;

 private:
  void File(fuchsia::feedback::CrashReport report, bool is_hourly_snapshot);
  void ScheduleHourlySnapshot(zx::duration delay);

  async_dispatcher_t* dispatcher_;
  async::Executor executor_;
  const std::shared_ptr<sys::ServiceDirectory> services_;
  std::unique_ptr<LogTags> tags_;
  AnnotationMap default_annotations_;
  CrashRegister* crash_register_;
  const UtcTimeProvider utc_provider_;
  std::unique_ptr<SnapshotManager> snapshot_manager_;
  std::unique_ptr<CrashServer> crash_server_;
  Queue queue_;

  ProductQuotas product_quotas_;
  CrashReporterInfo info_;
  NetworkWatcher network_watcher_;
  std::unique_ptr<ReportingPolicyWatcher> reporting_policy_watcher_;
  fidl::DeviceIdProviderPtr device_id_provider_ptr_;

  ReportId next_report_id_;
};

}  // namespace crash_reports
}  // namespace forensics

#endif  // SRC_DEVELOPER_FORENSICS_CRASH_REPORTS_CRASH_REPORTER_H_
