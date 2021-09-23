// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_AUDIO_AUDIO_CORE_UTILS_H_
#define SRC_MEDIA_AUDIO_AUDIO_CORE_UTILS_H_

#include <fuchsia/hardware/audio/cpp/fidl.h>
#include <fuchsia/media/cpp/fidl.h>
#include <lib/fit/function.h>
#include <lib/fzl/vmo-mapper.h>
#include <lib/sys/cpp/component_context.h>
#include <lib/zx/profile.h>
#include <lib/zx/thread.h>
#include <stdint.h>
#include <zircon/device/audio.h>
#include <zircon/types.h>

#include <atomic>
#include <vector>

#include <fbl/ref_counted.h>

#include "src/media/audio/audio_core/mixer/constants.h"

namespace media::audio {

class GenerationId {
 public:
  uint32_t get() const { return id_; }
  uint32_t Next() {
    uint32_t ret;
    do {
      ret = ++id_;
    } while (ret == kInvalidGenerationId);
    return ret;
  }

 private:
  uint32_t id_ = kInvalidGenerationId + 1;
};

class AtomicGenerationId {
 public:
  AtomicGenerationId() : id_(kInvalidGenerationId + 1) {}

  uint32_t get() const { return id_.load(); }
  uint32_t Next() {
    uint32_t ret;
    do {
      ret = id_.fetch_add(1);
    } while (ret == kInvalidGenerationId);
    return ret;
  }

 private:
  std::atomic<uint32_t> id_;
};

// Given a preferred format and a list of driver supported formats, select
// the "best" form and update the in/out parameters, then return ZX_OK.  If no
// formats exist, or all format ranges get completely rejected, return an error
// and leave the in/out params as they were.
zx_status_t SelectBestFormat(const std::vector<fuchsia::hardware::audio::PcmSupportedFormats>& fmts,
                             uint32_t* frames_per_second_inout, uint32_t* channels_inout,
                             fuchsia::media::AudioSampleFormat* sample_format_inout);
zx_status_t SelectBestFormat(const std::vector<audio_stream_format_range_t>& fmts,
                             uint32_t* frames_per_second_inout, uint32_t* channels_inout,
                             fuchsia::media::AudioSampleFormat* sample_format_inout);

// Given a format and a list of driver supported formats, if the format is found in
// the driver supported list then return true, otherwise return false.
bool IsFormatInSupported(
    const fuchsia::media::AudioStreamType& stream_type,
    const std::vector<fuchsia::hardware::audio::PcmSupportedFormats>& supported_formats);

// A simple extension to the libfzl VmoMapper which mixes in ref counting state
// to allow for shared VmoMapper semantics.
class RefCountedVmoMapper : public fzl::VmoMapper, public fbl::RefCounted<fzl::VmoMapper> {};

zx_status_t AcquireHighPriorityProfile(zx::profile* profile);

void AcquireAudioCoreImplProfile(sys::ComponentContext* context,
                                 fit::function<void(zx_status_t, zx::profile)> callback);

void AcquireRelativePriorityProfile(uint32_t priority, sys::ComponentContext* context,
                                    fit::function<void(zx_status_t, zx::profile)> callback);

// A timer which computes the amount of time the current thread spends scheduled
// (running) on a CPU, or queued.
class ThreadCpuTimer {
 public:
  // Start running the timer on the current thread.
  void Start() {
    thread_ = zx::thread::self();
    start_status_ =
        thread_->get_info(ZX_INFO_TASK_RUNTIME, &start_, sizeof(start_), nullptr, nullptr);
    end_status_ = ZX_ERR_BAD_STATE;
  }

  // Stop running the timer.
  void Stop() {
    end_status_ = thread_->get_info(ZX_INFO_TASK_RUNTIME, &end_, sizeof(end_), nullptr, nullptr);
  }

  // Reports how long the current thread spent running on a CPU. See ZX_INFO_TASK_RUNTIME.
  // Cannot be called while the timer is running; the timer must be stopped.
  zx::duration cpu() const {
    if (start_status_ != ZX_OK || end_status_ != ZX_OK) {
      return zx::duration::infinite_past();
    }
    return zx::duration(end_.cpu_time) - zx::duration(start_.cpu_time);
  }

  // Reports how long the current thread spent waiting to run. See ZX_INFO_TASK_RUNTIME.
  // Does not include time spent blocked; only includes time the thread is "ready" but waiting.
  // Cannot be called while the timer is running; the timer must be stopped.
  zx::duration queue() const {
    if (start_status_ != ZX_OK || end_status_ != ZX_OK) {
      return zx::duration::infinite_past();
    }
    return zx::duration(end_.queue_time) - zx::duration(start_.queue_time);
  }

  // Reports how long the current thread spent handling page faults. See ZX_INFO_TASK_RUNTIME.
  // Cannot be called while the timer is running; the timer must be stopped.
  zx::duration page_faults() const {
    if (start_status_ != ZX_OK || end_status_ != ZX_OK) {
      return zx::duration::infinite_past();
    }
    return zx::duration(end_.page_fault_time) - zx::duration(start_.page_fault_time);
  }

  // Reports how long the current thread spent blocked on kernel locks. See ZX_INFO_TASK_RUNTIME.
  // Cannot be called while the timer is running; the timer must be stopped.
  zx::duration lock_contention() const {
    if (start_status_ != ZX_OK || end_status_ != ZX_OK) {
      return zx::duration::infinite_past();
    }
    return zx::duration(end_.lock_contention_time) - zx::duration(start_.lock_contention_time);
  }

 private:
  zx::unowned_thread thread_;
  zx_info_task_runtime_t start_;
  zx_info_task_runtime_t end_;
  zx_status_t start_status_;
  zx_status_t end_status_;
};

}  // namespace media::audio

#endif  // SRC_MEDIA_AUDIO_AUDIO_CORE_UTILS_H_
