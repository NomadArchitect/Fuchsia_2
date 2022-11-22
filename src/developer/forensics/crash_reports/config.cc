// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/crash_reports/config.h"

#include <lib/syslog/cpp/macros.h>
#include <zircon/errors.h>

#include <memory>
#include <optional>

#include "src/developer/forensics/crash_reports/constants.h"
#include "src/lib/files/file.h"
#include "third_party/rapidjson/include/rapidjson/document.h"
#include "third_party/rapidjson/include/rapidjson/error/en.h"
#include "third_party/rapidjson/include/rapidjson/schema.h"
#include "third_party/rapidjson/include/rapidjson/stringbuffer.h"

namespace forensics {
namespace crash_reports {
namespace {

constexpr char kCrashReportUploadPolicyKey[] = "crash_report_upload_policy";

const char kSchema[] = R"({
  "type": "object",
  "properties": {
    "crash_report_upload_policy": {
      "type": "string",
      "enum": [
        "disabled",
        "enabled",
        "read_from_privacy_settings"
      ]
    }
  },
  "required": [
    "crash_report_upload_policy"
  ],
  "additionalProperties": false
})";

bool CheckAgainstSchema(rapidjson::Document& doc) {
  // Check that the schema is actually valid.
  rapidjson::Document sd;
  rapidjson::ParseResult ok = sd.Parse(kSchema);
  if (!ok) {
    FX_LOGS(ERROR) << "invalid JSON schema for config at offset " << ok.Offset() << " "
                   << rapidjson::GetParseError_En(ok.Code());
    return false;
  }

  // Check the document against the schema.
  rapidjson::SchemaDocument schema(sd);
  rapidjson::SchemaValidator validator(schema);
  if (!doc.Accept(validator)) {
    rapidjson::StringBuffer sb;
    validator.GetInvalidSchemaPointer().StringifyUriFragment(sb);
    FX_LOGS(ERROR) << "config does not match schema, violating '"
                   << validator.GetInvalidSchemaKeyword() << "' rule";
    return false;
  }
  return true;
}

}  // namespace

std::optional<Config> ParseConfig(const std::string& filepath) {
  std::string json;
  if (!files::ReadFileToString(filepath, &json)) {
    FX_LOGS(ERROR) << "error reading config file at " << filepath;
    return std::nullopt;
  }

  rapidjson::Document doc;
  rapidjson::ParseResult ok = doc.Parse(json.c_str());
  if (!ok) {
    FX_LOGS(ERROR) << "error parsing config as JSON at offset " << ok.Offset() << " "
                   << rapidjson::GetParseError_En(ok.Code());
    return std::nullopt;
  }

  if (!CheckAgainstSchema(doc)) {
    return std::nullopt;
  }

  Config config;
  if (const std::string upload_policy = doc[kCrashReportUploadPolicyKey].GetString();
      upload_policy == "disabled") {
    config.crash_report_upload_policy = Config::UploadPolicy::kDisabled;
  } else if (upload_policy == "enabled") {
    config.crash_report_upload_policy = Config::UploadPolicy::kEnabled;
  } else if (upload_policy == "read_from_privacy_settings") {
    config.crash_report_upload_policy = Config::UploadPolicy::kReadFromPrivacySettings;
  } else {
    FX_LOGS(FATAL) << "Upload policy '" << upload_policy << "' not permitted by schema";
  }

  return config;
}

std::string ToString(const Config::UploadPolicy upload_policy) {
  switch (upload_policy) {
    case Config::UploadPolicy::kDisabled:
      return "DISABLED";
    case Config::UploadPolicy::kEnabled:
      return "ENABLED";
    case Config::UploadPolicy::kReadFromPrivacySettings:
      return "READ_FROM_PRIVACY_SETTINGS";
  }
}

}  // namespace crash_reports
}  // namespace forensics
