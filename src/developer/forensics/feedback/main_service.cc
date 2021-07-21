// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/main_service.h"

#include <fuchsia/feedback/cpp/fidl.h>
#include <lib/syslog/cpp/macros.h>

#include <vector>

namespace forensics::feedback {

MainService::MainService(async_dispatcher_t* dispatcher,
                         std::shared_ptr<sys::ServiceDirectory> services, timekeeper::Clock* clock,
                         inspect::Node* inspect_root, LastReboot::Options last_reboot_options,
                         CrashReports::Options crash_reports_options)
    : dispatcher_(dispatcher),
      services_(services),
      clock_(clock),
      inspect_root_(inspect_root),
      cobalt_(dispatcher, services, clock),
      crash_reports_(dispatcher_, services_, clock_, inspect_root_, crash_reports_options),
      last_reboot_(dispatcher_, services_, &cobalt_, crash_reports_.CrashReporter(),
                   last_reboot_options),
      inspect_node_manager_(inspect_root),
      last_reboot_info_provider_stats_(&inspect_node_manager_,
                                       "/fidl/fuchsia.feedback.LastRebootInfoProvider"),
      crash_reporter_stats_(&inspect_node_manager_, "/fidl/fuchsia.feedback.CrashReporter"),
      crash_reporting_product_register_stats_(
          &inspect_node_manager_, "/fidl/fuchsia.feedback.CrashReportingProductRegister") {}

void MainService::ReportMigrationError(const std::map<std::string, std::string>& annotations) {
  fuchsia::feedback::CrashReport crash_report;
  crash_report.set_program_name("feedback")
      .set_crash_signature("fuchsia-feedback-component-merge-failure");

  std::vector<fuchsia::feedback::Annotation> report_annotations;
  for (const auto& [k, v] : annotations) {
    report_annotations.push_back(fuchsia::feedback::Annotation{
        .key = k,
        .value = v,
    });
  }
  crash_report.set_annotations(std::move(report_annotations));

  crash_reports_.CrashReporter()->File(std::move(crash_report),
                                       [](fuchsia::feedback::CrashReporter_File_Result) {});
}

void MainService::ShutdownImminent() { crash_reports_.ShutdownImminent(); }

template <>
::fidl::InterfaceRequestHandler<fuchsia::feedback::LastRebootInfoProvider>
MainService::GetHandler() {
  return [this](::fidl::InterfaceRequest<fuchsia::feedback::LastRebootInfoProvider> request) {
    last_reboot_info_provider_stats_.NewConnection();
    last_reboot_.Handle(std::move(request), [this](zx_status_t) {
      last_reboot_info_provider_stats_.CloseConnection();
    });
  };
}

template <>
::fidl::InterfaceRequestHandler<fuchsia::feedback::CrashReporter> MainService::GetHandler() {
  return [this](::fidl::InterfaceRequest<fuchsia::feedback::CrashReporter> request) {
    crash_reporter_stats_.NewConnection();
    crash_reports_.Handle(std::move(request),
                          [this](zx_status_t) { crash_reporter_stats_.CloseConnection(); });
  };
}

template <>
::fidl::InterfaceRequestHandler<fuchsia::feedback::CrashReportingProductRegister>
MainService::GetHandler() {
  return
      [this](::fidl::InterfaceRequest<fuchsia::feedback::CrashReportingProductRegister> request) {
        crash_reporting_product_register_stats_.NewConnection();
        crash_reports_.Handle(std::move(request), [this](zx_status_t) {
          crash_reporting_product_register_stats_.CloseConnection();
        });
      };
}

}  // namespace forensics::feedback
