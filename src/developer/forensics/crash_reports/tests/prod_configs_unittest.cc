// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <optional>
#include <string>

#include <gtest/gtest.h>

#include "src/developer/forensics/crash_reports/config.h"
#include "src/lib/files/path.h"

namespace forensics::crash_reports {
namespace {

constexpr auto kDisabled = Config::UploadPolicy::kDisabled;
constexpr auto kEnabled = Config::UploadPolicy::kEnabled;
constexpr auto kReadFromPrivacySettings = Config::UploadPolicy::kReadFromPrivacySettings;

class ProdConfigTest : public testing::Test {
 public:
  static std::optional<Config> GetConfig(const std::string& config_filename) {
    return ParseConfig(files::JoinPath("/pkg/data/configs", config_filename));
  }
};

TEST_F(ProdConfigTest, Default) {
  const std::optional<Config> config = GetConfig("default.json");
  ASSERT_TRUE(config.has_value());

  EXPECT_EQ(config->crash_report_upload_policy, kDisabled);
}

TEST_F(ProdConfigTest, UploadToProdServer) {
  const std::optional<Config> config = GetConfig("upload_to_prod_server.json");
  ASSERT_TRUE(config.has_value());

  EXPECT_EQ(config->crash_report_upload_policy, kEnabled);
}

TEST_F(ProdConfigTest, User) {
  const std::optional<Config> config = GetConfig("user.json");
  ASSERT_TRUE(config.has_value());

  EXPECT_EQ(config->crash_report_upload_policy, kReadFromPrivacySettings);
}

TEST_F(ProdConfigTest, Userdebug) {
  const std::optional<Config> config = GetConfig("userdebug.json");
  ASSERT_TRUE(config.has_value());

  EXPECT_EQ(config->crash_report_upload_policy, kReadFromPrivacySettings);
}

}  // namespace
}  // namespace forensics::crash_reports
