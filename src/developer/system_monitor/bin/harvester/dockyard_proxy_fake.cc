// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "dockyard_proxy_fake.h"

#include <gtest/gtest.h>
#include <rapidjson/document.h>
#include <rapidjson/pointer.h>

namespace harvester {

DockyardProxyStatus DockyardProxyFake::Init() {
  sent_json_.clear();
  sent_values_.clear();
  sent_strings_.clear();
  return DockyardProxyStatus::OK;
}

DockyardProxyStatus DockyardProxyFake::SendInspectJson(
    const std::string& stream_name, const std::string& json) {
  sent_json_.insert_or_assign(stream_name, json);
  return DockyardProxyStatus::OK;
}

DockyardProxyStatus DockyardProxyFake::SendLogs(
    const std::vector<const std::string>& batch) {
  for (const auto& log : batch) {
    sent_logs_.emplace_back(log);
  }
  return DockyardProxyStatus::OK;
}

DockyardProxyStatus DockyardProxyFake::SendSample(
    const std::string& stream_name, uint64_t value) {
  sent_values_.insert_or_assign(stream_name, value);
  return DockyardProxyStatus::OK;
}

DockyardProxyStatus DockyardProxyFake::SendSampleList(const SampleList& list) {
  EXPECT_TRUE(!list.empty());
  for (const auto& sample : list) {
#ifdef VERBOSE_OUTPUT
    std::cout << "Sending " << sample.first << " " << sample.second
              << std::endl;
#endif  // VERBOSE_OUTPUT
    sent_values_.insert_or_assign(sample.first, sample.second);
  }
  return DockyardProxyStatus::OK;
}

DockyardProxyStatus DockyardProxyFake::SendStringSampleList(
    const StringSampleList& list) {
  EXPECT_TRUE(!list.empty());
  for (const auto& sample : list) {
#ifdef VERBOSE_OUTPUT
    std::cout << "Sending " << sample.first << " " << sample.second
              << std::endl;
#endif  // VERBOSE_OUTPUT
    sent_strings_.insert_or_assign(sample.first, sample.second);
  }
  return DockyardProxyStatus::OK;
}

DockyardProxyStatus DockyardProxyFake::SendSamples(
    const SampleList& int_samples, const StringSampleList& string_samples) {
  // Either list may be empty, but not both (there's no use in calling this with
  // empty lists, no work will be done).
  EXPECT_FALSE(int_samples.empty() && string_samples.empty());

  for (const auto& sample : int_samples) {
#ifdef VERBOSE_OUTPUT
    std::cout << "Sending " << sample.first << " " << sample.second
              << std::endl;
#endif  // VERBOSE_OUTPUT
    sent_values_.insert_or_assign(sample.first, sample.second);
  }
  for (const auto& sample : string_samples) {
#ifdef VERBOSE_OUTPUT
    std::cout << "Sending " << sample.first << " " << sample.second
              << std::endl;
#endif  // VERBOSE_OUTPUT
    sent_strings_.insert_or_assign(sample.first, sample.second);
  }
  return DockyardProxyStatus::OK;
}

bool DockyardProxyFake::CheckJsonSent(const std::string& dockyard_path,
                                      std::string* json) const {
  const auto& iter = sent_json_.find(dockyard_path);
  if (iter == sent_json_.end()) {
    return false;
  }
  *json = iter->second;
  return true;
}

bool DockyardProxyFake::CheckValueSent(const std::string& dockyard_path,
                                       dockyard::SampleValue* value) const {
  const auto& iter = sent_values_.find(dockyard_path);
  if (iter == sent_values_.end()) {
    return false;
  }
  *value = iter->second;
  return true;
}

bool DockyardProxyFake::CheckValueSubstringSent(
    const std::string& dockyard_path_substring) const {
  for (const auto& iter : sent_values_) {
    if (iter.first.find(dockyard_path_substring) != std::string::npos) {
      return true;
    }
  }
  return false;
}

bool DockyardProxyFake::CheckValueSubstringSent(
    const std::string& dockyard_path_substring, std::string* key,
    uint64_t* value) const {
  for (const auto& iter : sent_values_) {
    if (iter.first.find(dockyard_path_substring) != std::string::npos) {
      *key = iter.first;
      *value = iter.second;
      return true;
    }
  }
  return false;
}

bool DockyardProxyFake::CheckLogSubstringSent(
    const std::string& log_message) const {
  auto expectedMessage =
      rapidjson::Value(log_message.c_str(), log_message.size());

  for (const auto& json_array : sent_logs_) {
    rapidjson::Document document;
    document.Parse(json_array);
    assert(document.IsArray());
    for (auto& json_log : document.GetArray()) {
      std::string message(rapidjson::GetValueByPointerWithDefault(
                              json_log, "/payload/root/message/value", "",
                              document.GetAllocator())
                              .GetString());
      if (message.find(log_message) != std::string::npos) {
        return true;
      }
    }
  }
  return false;
}

bool DockyardProxyFake::CheckStringSent(const std::string& dockyard_path,
                                        std::string* string) const {
  const auto& iter = sent_strings_.find(dockyard_path);
  if (iter == sent_strings_.end()) {
    return false;
  }
  *string = iter->second;
  return true;
}

bool DockyardProxyFake::CheckStringPrefixSent(
    const std::string& dockyard_path_prefix, std::string* string) const {
  for (const auto& iter : sent_strings_) {
    if (iter.first.find(dockyard_path_prefix) == 0) {
      *string = iter.second;
      return true;
    }
  }
  return false;
}

std::ostream& operator<<(std::ostream& out, const DockyardProxyFake& dockyard) {
  out << "DockyardProxyFake:" << std::endl;
  out << "  Strings:" << std::endl;
  for (const auto& str : dockyard.sent_strings_) {
    out << "    " << str.first << ": " << str.second << std::endl;
  }
  out << "  Values:" << std::endl;
  for (const auto& value : dockyard.sent_values_) {
    out << "    " << value.first << ": " << value.second << std::endl;
  }
  return out;
}

}  // namespace harvester
