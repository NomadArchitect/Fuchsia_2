// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_FEEDBACK_DATA_SYSTEM_LOG_RECORDER_SYSTEM_LOG_RECORDER_H_
#define SRC_DEVELOPER_FORENSICS_FEEDBACK_DATA_SYSTEM_LOG_RECORDER_SYSTEM_LOG_RECORDER_H_

#include <lib/async/cpp/task.h>
#include <lib/sys/cpp/service_directory.h>

#include "lib/zx/time.h"
#include "src/developer/forensics/feedback_data/archive_accessor_ptr.h"
#include "src/developer/forensics/feedback_data/system_log_recorder/encoding/encoder.h"
#include "src/developer/forensics/feedback_data/system_log_recorder/log_message_store.h"
#include "src/developer/forensics/feedback_data/system_log_recorder/writer.h"

namespace forensics {
namespace feedback_data {
namespace system_log_recorder {

class SystemLogRecorder {
 public:
  struct WriteParameters {
    zx::duration period;
    size_t max_write_size_bytes;
    std::string logs_dir;
    size_t max_num_files;
    size_t total_log_size_bytes;
  };

  SystemLogRecorder(async_dispatcher_t* archive_dispatcher, async_dispatcher_t* write_dispatcher,
                    std::shared_ptr<sys::ServiceDirectory> services,
                    WriteParameters write_parameters, std::unique_ptr<Encoder> encoder);
  void Start();

  void Flush(std::optional<std::string> message);
  void StopAndDeleteLogs();

 private:
  void PeriodicWriteTask();

  async_dispatcher_t* archive_dispatcher_;
  async_dispatcher_t* write_dispatcher_;
  const zx::duration write_period_;
  const std::string logs_dir_;

  LogMessageStore store_;
  ArchiveAccessor archive_accessor_;
  SystemLogWriter writer_;

  async::TaskClosureMethod<SystemLogRecorder, &SystemLogRecorder::PeriodicWriteTask>
      periodic_write_task_{this};
};

}  // namespace system_log_recorder
}  // namespace feedback_data
}  // namespace forensics

#endif  // SRC_DEVELOPER_FORENSICS_FEEDBACK_DATA_SYSTEM_LOG_RECORDER_SYSTEM_LOG_RECORDER_H_
