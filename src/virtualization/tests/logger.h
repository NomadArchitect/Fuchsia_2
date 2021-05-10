// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_VIRTUALIZATION_TESTS_LOGGER_H_
#define SRC_VIRTUALIZATION_TESTS_LOGGER_H_

#include <string>

// Logger is a singleton class that GuestConsole uses to write the guest's logs
// to. Then a test listener outputs the buffer if a test fails.
class Logger {
 public:
  static Logger& Get();
  void Reset() { buffer_.clear(); }
  void Write(const char* s, size_t count);
  void Write(const std::string& buffer);
  const std::string& Buffer() { return buffer_; }

 private:
  Logger() = default;

  Logger(const Logger&) = delete;
  Logger& operator=(const Logger&) = delete;

  // TODO(fxbug.dev/56119): Currently enabled to diagnose ongoing test flakes.
  static constexpr bool kGuestOutput = true;

  std::string buffer_;
};

#endif  // SRC_VIRTUALIZATION_TESTS_LOGGER_H_
