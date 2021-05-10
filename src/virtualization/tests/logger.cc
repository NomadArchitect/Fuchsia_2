// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "logger.h"

#include <iostream>

Logger& Logger::Get() {
  static Logger logger;
  return logger;
}

void Logger::Write(const char* s, size_t count) {
  buffer_.append(s, count);
  if (kLogAllGuestOutput) {
    std::cout.write(s, count);
    std::cout.flush();
  }
}

void Logger::Write(const std::string& buffer) { Write(buffer.data(), buffer.size()); }
