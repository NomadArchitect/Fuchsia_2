// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <zxtest/zxtest.h>

#include "tools/fidl/fidlc/include/fidl/diagnostic_types.h"
#include "tools/fidl/fidlc/include/fidl/diagnostics.h"
#include "tools/fidl/fidlc/include/fidl/reporter.h"
#include "tools/fidl/fidlc/include/fidl/source_span.h"
#include "tools/fidl/fidlc/include/fidl/virtual_source_file.h"
#include "tools/fidl/fidlc/tests/test_library.h"

namespace {

using fidl::Diagnostic;
using fidl::ErrorDef;
using fidl::Reporter;
using fidl::SourceSpan;
using fidl::VirtualSourceFile;
using fidl::WarningDef;

const fidl::ErrorId kTestErrorId = 9998;
const std::string kTestErrorIdStr = "fi-9998";
const fidl::ErrorId kTestWarningId = 9999;
const std::string kTestWarningIdStr = "fi-9999";

constexpr ErrorDef<kTestErrorId, std::string_view, std::string_view> ErrTest(
    "This test error has one string param '{}' and another '{}'.");
constexpr WarningDef<kTestWarningId, std::string_view, std::string_view> WarnTest(
    "This test warning has one string param '{}' and another '{}'.");

TEST(ReporterTests, ReportErrorFormatParams) {
  Reporter reporter;
  VirtualSourceFile file("fake");
  SourceSpan span("span text", file);
  reporter.Fail(ErrTest, span, "param1", "param2");

  const auto& errors = reporter.errors();
  ASSERT_EQ(errors.size(), 1);
  ASSERT_EQ(errors[0]->span, span);
  EXPECT_EQ(errors[0]->PrintId(), kTestErrorIdStr);
  EXPECT_SUBSTR(errors[0]->Print().c_str(), kTestErrorIdStr);
  EXPECT_NOT_SUBSTR(errors[0]->msg.c_str(), kTestErrorIdStr);
  EXPECT_SUBSTR(errors[0]->msg.c_str(),
                "This test error has one string param 'param1' and another 'param2'.");
}

TEST(ReporterTests, MakeErrorThenReportIt) {
  Reporter reporter;
  VirtualSourceFile file("fake");
  SourceSpan span("span text", file);
  std::unique_ptr<Diagnostic> diag = Diagnostic::MakeError(ErrTest, span, "param1", "param2");
  reporter.Report(std::move(diag));

  const auto& errors = reporter.errors();
  ASSERT_EQ(errors.size(), 1);
  ASSERT_EQ(errors[0]->span, span);
  EXPECT_EQ(errors[0]->PrintId(), kTestErrorIdStr);
  EXPECT_SUBSTR(errors[0]->Print().c_str(), kTestErrorIdStr);
  EXPECT_NOT_SUBSTR(errors[0]->msg.c_str(), kTestErrorIdStr);
  ASSERT_SUBSTR(errors[0]->msg.c_str(),
                "This test error has one string param 'param1' and another 'param2'.");
}

TEST(ReporterTests, ReportWarningFormatParams) {
  Reporter reporter;
  VirtualSourceFile file("fake");
  SourceSpan span("span text", file);
  reporter.Warn(WarnTest, span, "param1", "param2");

  const auto& warnings = reporter.warnings();
  ASSERT_EQ(warnings.size(), 1);
  ASSERT_EQ(warnings[0]->span, span);
  EXPECT_EQ(warnings[0]->PrintId(), kTestWarningIdStr);
  EXPECT_SUBSTR(warnings[0]->Print().c_str(), kTestWarningIdStr);
  EXPECT_NOT_SUBSTR(warnings[0]->msg.c_str(), kTestWarningIdStr);
  EXPECT_SUBSTR(warnings[0]->msg.c_str(),
                "This test warning has one string param 'param1' and another 'param2'.");
}

TEST(ReporterTests, MakeWarningThenReportIt) {
  Reporter reporter;
  VirtualSourceFile file("fake");
  SourceSpan span("span text", file);
  std::unique_ptr<Diagnostic> diag = Diagnostic::MakeWarning(WarnTest, span, "param1", "param2");
  reporter.Report(std::move(diag));

  const auto& warnings = reporter.warnings();
  ASSERT_EQ(warnings.size(), 1);
  ASSERT_EQ(warnings[0]->span, span);
  EXPECT_EQ(warnings[0]->PrintId(), kTestWarningIdStr);
  EXPECT_SUBSTR(warnings[0]->Print().c_str(), kTestWarningIdStr);
  EXPECT_NOT_SUBSTR(warnings[0]->msg.c_str(), kTestWarningIdStr);
  EXPECT_SUBSTR(warnings[0]->msg.c_str(),
                "This test warning has one string param 'param1' and another 'param2'.");
}

TEST(ReporterTests, CheckpointNumNewErrors) {
  Reporter reporter;
  VirtualSourceFile file("fake");
  SourceSpan span("span text", file);
  reporter.Fail(ErrTest, span, "1", "");

  auto checkpoint = reporter.Checkpoint();
  EXPECT_EQ(checkpoint.NumNewErrors(), 0);
  EXPECT_TRUE(checkpoint.NoNewErrors());

  reporter.Fail(ErrTest, span, "2", "");
  EXPECT_EQ(checkpoint.NumNewErrors(), 1);
  EXPECT_FALSE(checkpoint.NoNewErrors());

  reporter.Fail(ErrTest, span, "3", "");
  EXPECT_EQ(checkpoint.NumNewErrors(), 2);
  EXPECT_FALSE(checkpoint.NoNewErrors());
}

}  // namespace
