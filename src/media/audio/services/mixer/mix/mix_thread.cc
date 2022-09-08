// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/media/audio/services/mixer/mix/mix_thread.h"

#include <lib/syslog/cpp/macros.h>
#include <lib/zx/thread.h>
#include <zircon/syscalls/clock.h>

#include <algorithm>
#include <thread>

#include <fbl/algorithm.h>

#include "src/media/audio/services/mixer/mix/consumer_stage.h"

namespace media_audio {

namespace {
// The fastest rate a zx::clock can run relative to the system monotonic clock rate;
const TimelineRate kMonoTicksPerFastestRefTicks(1'000'000,
                                                1'000'000 + ZX_CLOCK_UPDATE_MAX_RATE_ADJUST);
}  // namespace

MixThreadPtr MixThread::Create(Args args) {
  // std::make_shared requires a public ctor, but we hide our ctor to force callers to use Create.
  struct WithPublicCtor : public MixThread {
    WithPublicCtor(Args args) : MixThread(std::move(args)) {}
  };

  MixThreadPtr thread = std::make_shared<WithPublicCtor>(std::move(args));

  // Start the kernel thread. This can't happen in the constructor because we want MixThread::Run to
  // hold a MixThreadPtr, which we can't get until after the constructor.
  auto checker_ready = std::make_shared<libsync::Completion>();
  auto task_queue_ready = std::make_shared<libsync::Completion>();
  std::thread t(MixThread::Run, thread, checker_ready, task_queue_ready);

  // Now that we have a std::thread, we can create the checker.
  thread->checker_ = std::make_unique<ThreadChecker>(t.get_id());
  checker_ready->Signal();

  // Wait until the task queue is fully initialized. If we don't wait, external calls to
  // `global_task_queue_->Push(thread->id(), _)` might be dropped due to a race with task queue
  // initialization.
  FX_CHECK(task_queue_ready->Wait(zx::sec(5)) == ZX_OK);

  // Now that the thread is started, we can detach and discard the std::thread. Shutdown is async so
  // we have no need to join.
  t.detach();

  return thread;
}

MixThreadPtr MixThread::CreateWithoutLoop(Args args) {
  struct WithPublicCtor : public MixThread {
    WithPublicCtor(Args args) : MixThread(std::move(args)) {}
  };

  MixThreadPtr thread = std::make_shared<WithPublicCtor>(std::move(args));
  thread->checker_ = std::make_unique<ThreadChecker>(std::this_thread::get_id());
  return thread;
}

MixThread::MixThread(Args args)
    : id_(args.id),
      name_(args.name),
      deadline_profile_(std::move(args.deadline_profile)),
      mix_period_(args.mix_period),
      cpu_per_period_(args.cpu_per_period),
      global_task_queue_(std::move(args.global_task_queue)),
      timer_(std::move(args.timer)),
      mono_clock_(std::move(args.mono_clock)) {
  FX_CHECK(mix_period_ > zx::nsec(0));
  FX_CHECK(zx::nsec(0) <= cpu_per_period_ && cpu_per_period_ <= mix_period_);
}

void MixThread::AddConsumer(ConsumerStagePtr consumer) {
  FX_CHECK(FindConsumer(consumer) == consumers_.end())
      << "cannot add Consumer twice: " << consumer->name();

  // Since we don't know if this consumer is actually started, pretend it's about to start, which
  // wakes the loop and tries to run a mix job on the consumer, which will tell us the consumer's
  // actual status.
  consumers_.push_back({.consumer = consumer});
  ReSortConsumers();
  NotifyConsumerStarting(consumer);
}

void MixThread::RemoveConsumer(ConsumerStagePtr consumer) {
  auto it = FindConsumer(consumer);
  FX_CHECK(it != consumers_.end()) << "cannot find Consumer to remove: " << consumer->name();
  consumers_.erase(it);
}

void MixThread::Shutdown() {
  // Run will exit the next time it wakes up.
  // Technically this is thread safe, but Shutdown is annotated with TA_REQ(checker())
  // anyway because it's simpler to say that all non-const methods are not thread safe.
  timer_->SetShutdownBit();
}

void MixThread::NotifyConsumerStarting(ConsumerStagePtr consumer) {
  auto it = FindConsumer(consumer);
  FX_CHECK(it != consumers_.end()) << "cannot find Consumer to start: " << consumer->name();

  it->maybe_started = true;
  if (state_ == State::Idle) {
    state_ = State::WakeFromIdle;
    timer_->SetEventBit();  // wake the loop
  }
}

void MixThread::ReSortConsumers() {
  std::sort(consumers_.begin(), consumers_.end(), [](const ConsumerInfo& a, const ConsumerInfo& b) {
    // We want a topological sort with sources ordered before sinks, so `a` goes first if it's
    // "higher" in the graph.
    return a.consumer->downstream_consumers() > b.consumer->downstream_consumers();
  });
}

// static
void MixThread::Run(MixThreadPtr thread, std::shared_ptr<libsync::Completion> checker_ready,
                    std::shared_ptr<libsync::Completion> task_queue_ready) {
  if (thread->deadline_profile_) {
    if (auto status = zx::thread::self()->set_profile(thread->deadline_profile_, 0);
        status != ZX_OK) {
      FX_PLOGS(WARNING, status) << "Failed to set deadline profile for thread '" << thread->name()
                                << "'";
    }
  }

  // Wait until private fields are fully initialized.
  FX_CHECK(checker_ready->Wait(zx::sec(5)) == ZX_OK);

  FX_LOGS(INFO) << "MixThread starting: id=" << thread->id() << " name='" << thread->name()
                << "' ptr=" << thread.get();
  thread->global_task_queue_->RegisterTimer(thread->id_, thread->timer_);
  task_queue_ready->Signal();

  // Main thread loop.
  ScopedThreadChecker check(thread->checker());
  thread->RunLoop();

  FX_LOGS(INFO) << "MixThread stopping: id=" << thread->id() << " name='" << thread->name()
                << "' ptr=" << thread.get();
  thread->global_task_queue_->UnregisterTimer(thread->id_);
  thread->timer_->Stop();
}

void MixThread::RunLoop() {
  std::optional<zx::time> prior_job_time;

  zx::time current_job_time = zx::time::infinite();
  FX_CHECK(state_ == State::Idle);

  for (;;) {
    const auto wake_reason = timer_->SleepUntil(current_job_time);
    if (wake_reason.shutdown_set) {
      return;
    }

    const auto wake_time = mono_clock_->now();
    bool run_mix_jobs = wake_reason.deadline_expired;

    // An "event" means tasks are available in the global task queue.
    if (wake_reason.event_set) {
      // TODO(fxbug.dev/87651): Measure the amount of time spent running these tasks per mix period
      // (this can be recorded as a "MixJobSubtask" in RunMixJobs) and protect against "task spam".
      global_task_queue_->RunForThread(id());

      // Check if we are being asked to start running mix jobs after an idle period.
      if (state_ == State::WakeFromIdle) {
        state_ = State::Running;
        if (prior_job_time && wake_time < *prior_job_time + mix_period_) {
          // Mix jobs must be separated by at least one period. If we were asked to wake immediately
          // after completing a mix job and going idle, wait until one period after the last job.
          current_job_time = *prior_job_time + mix_period_;
          continue;
        } else {
          // This is the first mix job after an idle period.
          current_job_time = wake_time;
          run_mix_jobs = true;
        }
      }
    }

    if (!run_mix_jobs) {
      continue;
    }

    FX_CHECK(state_ == State::Running);
    FX_CHECK(current_job_time != zx::time::infinite());

    auto next_job_time = RunMixJobs(current_job_time, wake_time);

    // The next mix job should happen at least one period in the future.
    FX_CHECK(next_job_time >= current_job_time + mix_period_)
        << "next_job_time=" << next_job_time.get()
        << ", current_job_time=" << current_job_time.get() << ", period=" << mix_period_.get();

    prior_job_time = current_job_time;
    current_job_time = next_job_time;
    if (current_job_time == zx::time::infinite()) {
      state_ = State::Idle;
    }
  }
}

zx::time MixThread::RunMixJobs(const zx::time mono_start_time, const zx::time mono_now) {
  MixJobContext ctx(clocks_);
  MixJobSubtask subtask("MixThread::RunMixJobs");

  clocks_.Update(mono_start_time);

  // If we woke up after this job's deadline, skip ahead to the next job.
  const auto mono_deadline = mono_start_time + mix_period_;
  if (mono_now >= mono_deadline) {
    // Round the underflow length up to the next period. Cast to uint64_t to use `fbl::round_up`.
    // These casts are safe because the values must be positive.
    const uint64_t now_minus_start = static_cast<uint64_t>((mono_now - mono_start_time).get());
    const uint64_t period = static_cast<uint64_t>(mix_period_.get());
    const auto underflow_duration =
        zx::duration(static_cast<int64_t>(fbl::round_up(now_minus_start + 1, period)));
    // TODO(fxbug.dev/87651): report underflow
    return mono_start_time + underflow_duration;
  }

  // If we woke up late enough that we're not guaranteed at least `cpu_per_period` CPU time
  // for this mix job, it's possible we might underflow. This is worth noting in metrics.
  if (const auto t = mono_deadline - cpu_per_period_; mono_now > t) {
    MixJobSubtask::Metrics late_metrics;
    late_metrics.name.Append("MixThread::LateWakeup");
    late_metrics.wall_time = mono_now - t;
    ctx.AddSubtaskMetrics(late_metrics);
  }

  // When the next RunMixJobs call should happen, or `infinite` if there are no future jobs.
  zx::time next_job_mono_start_time = zx::time::infinite();

  // Run each consumer that might be started.
  for (auto& c : consumers_) {
    auto clock = clocks_.SnapshotFor(c.consumer->reference_clock_koid());
    const auto& mono_to_ref = clock.to_clock_mono().Inverse();

    // Mix periods are defined relative to the system monotonic clock. Translate this mix period to
    // the consumer's reference clock.
    auto ref_start_time = zx::time(mono_to_ref.Apply(mono_start_time.get()));
    auto ref_deadline = zx::time(mono_to_ref.Apply((mono_start_time + mix_period_).get()));
    auto ref_period = ref_deadline - ref_start_time;

    if (c.maybe_started ||
        (c.next_mix_job_start_time && *c.next_mix_job_start_time < ref_deadline)) {
      auto status = c.consumer->RunMixJob(ctx, ref_start_time, ref_period);
      if (std::holds_alternative<ConsumerStage::StartedStatus>(status)) {
        // We have another job one period from now.
        next_job_mono_start_time = mono_start_time + mix_period_;
        c.next_mix_job_start_time = std::nullopt;
      } else {
        c.maybe_started = false;
        c.next_mix_job_start_time =
            std::get<ConsumerStage::StoppedStatus>(status).next_mix_job_start_time;
      }
    }

    if (c.next_mix_job_start_time) {
      // If stopped, but there's a scheduled start command in the future, wake up in time to execute
      // that command. This must be at least one period in the future, otherwise the start command
      // should have happened already.
      FX_CHECK(*c.next_mix_job_start_time >= ref_deadline)
          << "next_mix_job_start_time=" << c.next_mix_job_start_time->get()
          << ", ref_deadline=" << ref_deadline.get();

      // Translate the next start time back to the monotonic clock using a worst-case conservative
      // assumption that the reference clock is running at the fastest possible rate.
      auto fastest_ref_time_to_mono_time =
          TimelineFunction(mono_deadline.get(), ref_deadline.get(), kMonoTicksPerFastestRefTicks);

      next_job_mono_start_time =
          std::min(next_job_mono_start_time,
                   zx::time(fastest_ref_time_to_mono_time.Apply(c.next_mix_job_start_time->get())));
    }
  }

  subtask.Done();
  ctx.AddSubtaskMetrics(subtask.FinalMetrics());

  // If we ran for too long, we underflowed.
  const auto mono_actual_end_time = mono_clock_->now();
  if (mono_actual_end_time > mono_deadline) {
    // TODO(fxbug.dev/87651): report underflow
  }

  return next_job_mono_start_time;
}

std::vector<MixThread::ConsumerInfo>::iterator MixThread::FindConsumer(
    const ConsumerStagePtr& consumer) {
  return std::find_if(consumers_.begin(), consumers_.end(),
                      [&consumer](const auto& info) { return info.consumer == consumer; });
}

}  // namespace media_audio
