// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/sys/fuzzing/libfuzzer/runner.h"

#include <fcntl.h>
#include <lib/fdio/spawn.h>
#include <lib/fit/defer.h>
#include <lib/fpromise/single_threaded_executor.h>
#include <lib/syslog/cpp/macros.h>
#include <stddef.h>
#include <stdio.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>
#include <zircon/status.h>
#include <zircon/time.h>

#include <filesystem>
#include <iostream>
#include <sstream>

#include <openssl/sha.h>
#include <re2/re2.h>

#include "src/lib/files/eintr_wrapper.h"
#include "src/lib/files/file.h"
#include "src/lib/files/path.h"
#include "src/sys/fuzzing/common/status.h"

namespace fuzzing {
namespace {

using fuchsia::fuzzer::ProcessStats;

const char* kTestInputPath = "/tmp/test_input";
const char* kLiveCorpusPath = "/tmp/live_corpus";
const char* kSeedCorpusPath = "/tmp/seed_corpus";
const char* kTempCorpusPath = "/tmp/temp_corpus";
const char* kDictionaryPath = "/tmp/dictionary";
const char* kResultInputPath = "/tmp/result_input";

constexpr zx_duration_t kOneSecond = ZX_SEC(1);
constexpr size_t kOneKb = 1ULL << 10;
constexpr size_t kOneMb = 1ULL << 20;

// Returns |one| if |original| is non-zero and less than |one|, otherwise returns |original|.
template <typename T>
T Clamp(T original, const T& one, const char* type, const char* unit, const char* flag) {
  if (!original) {
    return 0;
  }
  if (original < one) {
    FX_LOGS(WARNING) << "libFuzzer does not support " << type << "s of less than 1 " << unit
                     << " for '" << flag << "'.";
    return one;
  }
  return original;
}

// Converts a flag into a libFuzzer command line argument.
template <typename T>
std::string MakeArg(const std::string& flag, T value) {
  std::ostringstream oss;
  oss << "-" << flag << "=" << value;
  return oss.str();
}

// Reads a byte sequence from a file.
Input ReadInputFromFile(const std::string& pathname) {
  std::vector<uint8_t> data;
  if (!files::ReadFileToVector(pathname, &data)) {
    FX_LOGS(FATAL) << "Failed to read input from '" << pathname << "': " << strerror(errno);
  }
  return Input(data);
}

// Writes a byte sequence to a file.
void WriteInputToFile(const Input& input, const std::string& pathname) {
  FX_DCHECK(input.size() < std::numeric_limits<ssize_t>::max());
  const auto* data = reinterpret_cast<const char*>(input.data());
  auto size = static_cast<ssize_t>(input.size());
  if (!files::WriteFile(pathname, data, size)) {
    FX_LOGS(FATAL) << "Failed to write input to '" << pathname << "': " << strerror(errno);
  }
}

}  // namespace

RunnerPtr LibFuzzerRunner::MakePtr(ExecutorPtr executor) {
  return RunnerPtr(new LibFuzzerRunner(std::move(executor)));
}

LibFuzzerRunner::LibFuzzerRunner(ExecutorPtr executor)
    : Runner(executor), process_(executor), workflow_(this) {
  FX_CHECK(std::filesystem::create_directory(kSeedCorpusPath));
  FX_CHECK(std::filesystem::create_directory(kLiveCorpusPath));
}

void LibFuzzerRunner::AddDefaults(Options* options) {
  if (!options->has_detect_exits()) {
    options->set_detect_exits(true);
  }
}

///////////////////////////////////////////////////////////////
// Corpus-related methods.

zx_status_t LibFuzzerRunner::AddToCorpus(CorpusType corpus_type, Input input) {
  uint8_t digest[SHA_DIGEST_LENGTH];
  SHA1(input.data(), input.size(), digest);
  std::ostringstream filename;
  filename << std::hex;
  for (size_t i = 0; i < SHA_DIGEST_LENGTH; ++i) {
    filename << std::setw(2) << std::setfill('0') << size_t(digest[i]);
  }
  std::ostringstream pathname;
  switch (corpus_type) {
    case CorpusType::SEED:
      pathname << kSeedCorpusPath << "/" << filename.str();
      seed_corpus_.push_back(filename.str());
      break;
    case CorpusType::LIVE:
      pathname << kLiveCorpusPath << "/" << filename.str();
      live_corpus_.push_back(filename.str());
      break;
    default:
      return ZX_ERR_INVALID_ARGS;
  }
  WriteInputToFile(std::move(input), pathname.str());
  return ZX_OK;
}

Input LibFuzzerRunner::ReadFromCorpus(CorpusType corpus_type, size_t offset) {
  switch (corpus_type) {
    case CorpusType::SEED:
      if (offset < seed_corpus_.size()) {
        return ReadInputFromFile(files::JoinPath(kSeedCorpusPath, seed_corpus_[offset]));
      }
      break;
    case CorpusType::LIVE:
      if (offset < live_corpus_.size()) {
        return ReadInputFromFile(files::JoinPath(kLiveCorpusPath, live_corpus_[offset]));
      }
      break;
    default:
      FX_NOTIMPLEMENTED();
  }
  return Input();
}

void LibFuzzerRunner::ReloadLiveCorpus() {
  live_corpus_.clear();
  std::vector<std::string> dups;
  for (const auto& dir_entry : std::filesystem::directory_iterator(kLiveCorpusPath)) {
    auto filename = dir_entry.path().filename().string();
    if (files::IsFile(files::JoinPath(kSeedCorpusPath, filename))) {
      dups.push_back(files::JoinPath(kLiveCorpusPath, filename));
    } else {
      live_corpus_.push_back(std::move(filename));
    }
  }
  for (const auto& dup_path : dups) {
    std::filesystem::remove(dup_path);
  }
}

///////////////////////////////////////////////////////////////
// Dictionary-related methods.

zx_status_t LibFuzzerRunner::ParseDictionary(const Input& input) {
  WriteInputToFile(input, kDictionaryPath);
  has_dictionary_ = true;
  return ZX_OK;
}

Input LibFuzzerRunner::GetDictionaryAsInput() const {
  return has_dictionary_ ? ReadInputFromFile(kDictionaryPath) : Input();
}

///////////////////////////////////////////////////////////////
// Fuzzing workflows.

ZxPromise<> LibFuzzerRunner::Configure(const OptionsPtr& options) {
  return fpromise::make_promise([this, options]() -> ZxResult<> {
           options_ = options;
           return fpromise::ok();
         })
      .wrap_with(workflow_);
}

ZxPromise<FuzzResult> LibFuzzerRunner::Execute(Input input) {
  auto args = MakeArgs();
  auto test_input = kTestInputPath;
  WriteInputToFile(input, test_input);
  args.push_back(test_input);
  return RunAsync(std::move(args))
      .and_then([](const Artifact& artifact) { return fpromise::ok(artifact.fuzz_result()); })
      .wrap_with(workflow_);
}

ZxPromise<Artifact> LibFuzzerRunner::Fuzz() {
  auto args = MakeArgs();
  args.push_back(kLiveCorpusPath);
  args.push_back(kSeedCorpusPath);
  return RunAsync(std::move(args))
      .and_then([this](Artifact& artifact) {
        ReloadLiveCorpus();
        return fpromise::ok(std::move(artifact));
      })
      .wrap_with(workflow_);
}

ZxPromise<Input> LibFuzzerRunner::Minimize(Input input) {
  auto args = MakeArgs();
  WriteInputToFile(input, kTestInputPath);
  args.push_back("-minimize_crash=1");
  args.push_back(kTestInputPath);
  minimized_ = false;
  return RunAsync(std::move(args))
      .and_then([](Artifact& artifact) { return fpromise::ok(artifact.take_input()); })
      .wrap_with(workflow_);
}

ZxPromise<Input> LibFuzzerRunner::Cleanse(Input input) {
  auto args = MakeArgs();
  WriteInputToFile(input, kTestInputPath);
  args.push_back("-cleanse_crash=1");
  args.push_back(kTestInputPath);
  return RunAsync(std::move(args))
      .and_then([input = std::move(input)](Artifact& artifact) mutable {
        auto result = artifact.take_input();
        // A quirk of libFuzzer's cleanse workflow is that it doesn't return *anything* if the input
        // doesn't crash or is already "clean". |RunAsync| translates this to an empty |Artifact|.
        return fpromise::ok(result.size() == input.size() ? std::move(result) : std::move(input));
      })
      .wrap_with(workflow_);
}

ZxPromise<> LibFuzzerRunner::Merge() {
  std::filesystem::create_directory(kTempCorpusPath);
  auto args = MakeArgs();
  args.push_back("-merge=1");
  args.push_back(kTempCorpusPath);
  args.push_back(kSeedCorpusPath);
  args.push_back(kLiveCorpusPath);
  return RunAsync(std::move(args))
      .and_then([this](const Artifact& artifact) {
        std::filesystem::remove_all(kLiveCorpusPath);
        std::filesystem::rename(kTempCorpusPath, kLiveCorpusPath);
        ReloadLiveCorpus();
        return fpromise::ok();
      })
      .or_else([](const zx_status_t& status) {
        std::filesystem::remove_all(kTempCorpusPath);
        return fpromise::error(status);
      })
      .wrap_with(workflow_);
}

ZxPromise<> LibFuzzerRunner::Stop() {
  // TODO(fxbug.dev/87155): If libFuzzer-for-Fuchsia watches for something sent to stdin in order to
  // call its |Fuzzer::StaticInterruptCallback|, we could ask libFuzzer to shut itself down. This
  // would guarantee we get all of its output.
  return process_.Kill().and_then(workflow_.Stop());
}

Status LibFuzzerRunner::CollectStatus() {
  // For libFuzzer, we return the most recently parsed status rather than point-in-time status.
  return CopyStatus(status_);
}

///////////////////////////////////////////////////////////////
// Process-related methods.

std::vector<std::string> LibFuzzerRunner::MakeArgs() {
  std::vector<std::string> args;
  auto cmdline_iter = cmdline_.begin();
  while (cmdline_iter != cmdline_.end() && *cmdline_iter != "--") {
    args.push_back(*cmdline_iter++);
  }
  if (options_->has_runs()) {
    args.push_back(MakeArg("runs", options_->runs()));
  }
  if (options_->has_max_total_time()) {
    auto max_total_time = options_->max_total_time();
    max_total_time = Clamp(max_total_time, kOneSecond, "duration", "second", "max_total_time");
    options_->set_max_total_time(max_total_time);
    args.push_back(MakeArg("max_total_time", max_total_time / kOneSecond));
  }
  if (options_->has_seed()) {
    args.push_back(MakeArg("seed", options_->seed()));
  }
  if (options_->has_max_input_size()) {
    args.push_back(MakeArg("max_len", options_->max_input_size()));
  }
  if (options_->has_mutation_depth()) {
    args.push_back(MakeArg("mutate_depth", options_->mutation_depth()));
  }
  if (options_->has_dictionary_level()) {
    FX_LOGS(WARNING) << "libFuzzer does not support setting the dictionary level.";
  }
  if (options_->has_detect_exits() && !options_->detect_exits()) {
    FX_LOGS(WARNING) << "libFuzzer does not support ignoring process exits.";
  }
  if (options_->has_detect_leaks()) {
    args.push_back(MakeArg("detect_leaks", options_->detect_leaks() ? "1" : "0"));
  }
  if (options_->has_run_limit()) {
    auto run_limit = options_->run_limit();
    run_limit = Clamp(run_limit, kOneSecond, "duration", "second", "run_limit");
    options_->set_run_limit(run_limit);
    args.push_back(MakeArg("timeout", run_limit / kOneSecond));
  }
  if (options_->has_malloc_limit()) {
    auto malloc_limit = options_->malloc_limit();
    malloc_limit = Clamp(malloc_limit, kOneMb, "memory amount", "MB", "malloc_limit");
    options_->set_malloc_limit(malloc_limit);
    args.push_back(MakeArg("malloc_limit_mb", malloc_limit / kOneMb));
  }
  if (options_->has_oom_limit()) {
    auto oom_limit = options_->oom_limit();
    oom_limit = Clamp(oom_limit, kOneMb, "memory amount", "MB", "oom_limit");
    options_->set_oom_limit(oom_limit);
    args.push_back(MakeArg("rss_limit_mb", oom_limit / kOneMb));
  }
  if (options_->has_purge_interval()) {
    auto purge_interval = options_->purge_interval();
    purge_interval = Clamp(purge_interval, kOneSecond, "duration", "second", "purge_interval");
    options_->set_purge_interval(purge_interval);
    args.push_back(MakeArg("purge_allocator_interval", purge_interval / kOneSecond));
  }
  if (options_->has_malloc_exitcode()) {
    FX_LOGS(WARNING) << "libFuzzer does not support setting the 'malloc_exitcode'.";
  }
  if (options_->has_death_exitcode()) {
    FX_LOGS(WARNING) << "libFuzzer does not support setting the 'death_exitcode'.";
  }
  if (options_->has_leak_exitcode()) {
    FX_LOGS(WARNING) << "libFuzzer does not support setting the 'leak_exitcode'.";
  }
  if (options_->has_oom_exitcode()) {
    FX_LOGS(WARNING) << "libFuzzer does not support setting the 'oom_exitcode'.";
  }
  if (options_->has_pulse_interval()) {
    FX_LOGS(WARNING) << "libFuzzer does not support setting the 'pulse_interval'.";
  }
  if (has_dictionary_) {
    args.push_back(MakeArg("dict", kDictionaryPath));
  }
  std::filesystem::remove(kResultInputPath);
  args.push_back(MakeArg("exact_artifact_path", kResultInputPath));
  while (cmdline_iter != cmdline_.end()) {
    args.push_back(*cmdline_iter++);
  }
  return args;
}

std::vector<fdio_spawn_action_t> LibFuzzerRunner::MakeSpawnActions(int* stdin_fd, int* stderr_fd) {
  int fds[2];
  std::vector<fdio_spawn_action_t> spawn_actions;

  // Standard input.
  if (pipe(fds) != 0) {
    FX_LOGS(FATAL) << "Failed to pipe stdin to libfuzzer " << strerror(errno);
  }
  FX_DCHECK(stdin_fd);
  *stdin_fd = fds[1];
  spawn_actions.push_back({
      .action = FDIO_SPAWN_ACTION_TRANSFER_FD,
      .fd =
          {
              .local_fd = fds[0],
              .target_fd = STDIN_FILENO,
          },
  });

  // Standard output.
  spawn_actions.push_back({
      .action = FDIO_SPAWN_ACTION_CLONE_FD,
      .fd =
          {
              .local_fd = STDOUT_FILENO,
              .target_fd = STDOUT_FILENO,
          },
  });

  // Standard error.
  if (pipe(fds) != 0) {
    FX_LOGS(FATAL) << "Failed to pipe stderr from libfuzzer " << strerror(errno);
  }
  FX_DCHECK(stderr_fd);
  *stderr_fd = fds[0];
  spawn_actions.push_back({
      .action = FDIO_SPAWN_ACTION_TRANSFER_FD,
      .fd =
          {
              .local_fd = fds[1],
              .target_fd = STDERR_FILENO,
          },
  });

  return spawn_actions;
}

ZxPromise<Artifact> LibFuzzerRunner::RunAsync(std::vector<std::string> args) {
  return fpromise::make_promise([this, args = std::move(args)](Context& context) {
           process_.set_verbose(verbose_);
           process_.SetStdoutSpawnAction(SpawnAction::kClone);
           return process_.Spawn(args);
         })
      .and_then([this, parse = ZxFuture<FuzzResult>(),
                 kill = ZxFuture<>()](Context& context) mutable -> ZxResult<FuzzResult> {
        if (!parse) {
          parse = ParseOutput();
        }
        if (!parse(context)) {
          return fpromise::pending();
        }
        if (!kill) {
          kill = process_.Kill();
        }
        if (!kill(context)) {
          return fpromise::pending();
        }
        process_.Reset();
        return parse.take_result();
      })
      .and_then([](FuzzResult& fuzz_result) {
        auto input =
            files::IsFile(kResultInputPath) ? ReadInputFromFile(kResultInputPath) : Input();
        return fpromise::ok(Artifact(fuzz_result, std::move(input)));
      });
}

///////////////////////////////////////////////////////////////
// Output parsing methods.

ZxPromise<FuzzResult> LibFuzzerRunner::ParseOutput() {
  return fpromise::make_promise(
      [this, read_line = ZxFuture<std::string>(),
       result = ZxResult<FuzzResult>(fpromise::ok(FuzzResult::NO_ERRORS))](
          Context& context) mutable -> ZxResult<FuzzResult> {
        while (true) {
          if (!read_line) {
            read_line = process_.ReadFromStderr();
          }
          if (!read_line(context)) {
            return fpromise::pending();
          }
          if (read_line.is_error()) {
            auto status = read_line.error();
            if (status != ZX_ERR_STOP) {
              return fpromise::error(status);
            }
            return std::move(result);
          }
          // Save the first interesting result.
          auto parse_result = ParseLine(read_line.take_value());
          if (result.is_ok() && result.value() == FuzzResult::NO_ERRORS) {
            result = std::move(parse_result);
          }
        }
      });
}

ZxResult<FuzzResult> LibFuzzerRunner::ParseLine(const std::string& line) {
  re2::StringPiece input(line);

  // Start with normal status, the most common output.
  uint32_t runs;
  if (re2::RE2::Consume(&input, "#(\\d+)", &runs)) {
    status_.set_runs(runs);
    ParseStatus(&input);
    return fpromise::ok(FuzzResult::NO_ERRORS);
  }

  // Check for easily identifiable errors.
  if (re2::RE2::Consume(&input, "==\\d+== ERROR: libFuzzer: ")) {
    return ParseError(&input);
  }
  // See libFuzzer's |Fuzzer::TryDetectingAMemoryLeak|.
  // This match is ugly, but it's the only message in current libFuzzer that this code can
  // rely on for a leak.
  if (line == "INFO: to ignore leaks on libFuzzer side use -detect_leaks=0.") {
    return fpromise::ok(FuzzResult::LEAK);
  }

  // See libFuzzer's |Fuzzer::MinimizeCrashInput|.
  // Annoyingly, libFuzzer prints the same "error" message for an invalid input and a
  // minimize loop that no longer triggers an error (see below). Use this diagnostic to
  // distinguish.
  if (re2::RE2::PartialMatch(
          input,
          "CRASH_MIN: '\\S+' \\(\\d+ bytes\\) caused a crash. Will try to minimize it further")) {
    minimized_ = true;
    return fpromise::ok(FuzzResult::NO_ERRORS);
  }

  // See libFuzzer's |Fuzzer::MinimizeCrashInput|.
  if (re2::RE2::PartialMatch(input, "ERROR: the input \\S+ did not crash") && !minimized_) {
    FX_LOGS(WARNING) << "Test input did not trigger an error.";
    return fpromise::error(ZX_ERR_INVALID_ARGS);
  }

  return fpromise::ok(FuzzResult::NO_ERRORS);
}

void LibFuzzerRunner::ParseStatus(re2::StringPiece* input) {
  // The patterns in this function should match libFuzzer's |Fuzzer::PrintStats|.

  // Parse reason.
  std::string reason_str;
  if (!re2::RE2::Consume(input, "\\t(\\S+)", &reason_str)) {
    return;
  }
  auto reason = UpdateReason::PULSE;  // By default, assume it's just a status update.
  if (reason_str == "INITED") {
    status_.set_running(true);
    start_ = zx::clock::get_monotonic();
    reason = UpdateReason::INIT;
  } else if (reason_str == "NEW") {
    reason = UpdateReason::NEW;
  } else if (reason_str == "REDUCE") {
    reason = UpdateReason::REDUCE;
  } else if (reason_str == "DONE") {
    status_.set_running(false);
    reason = UpdateReason::DONE;
  }

  // Parse covered PCs.
  size_t covered_pcs;
  if (re2::RE2::FindAndConsume(input, "cov: (\\d+)", &covered_pcs)) {
    status_.set_covered_pcs(covered_pcs);
  }

  // Parse covered features.
  size_t covered_features;
  if (re2::RE2::FindAndConsume(input, "ft: (\\d+)", &covered_features)) {
    status_.set_covered_features(covered_features);
  }

  // Parse corpus stats.
  size_t corpus_num_inputs;
  if (re2::RE2::FindAndConsume(input, "corp: (\\d+)", &corpus_num_inputs)) {
    size_t corpus_total_size;
    status_.set_corpus_num_inputs(corpus_num_inputs);
    if (re2::RE2::Consume(input, "/(\\d+)b", &corpus_total_size)) {
      status_.set_corpus_total_size(corpus_total_size);
    } else if (re2::RE2::Consume(input, "/(\\d+)Kb", &corpus_total_size)) {
      status_.set_corpus_total_size(corpus_total_size * kOneKb);
    } else if (re2::RE2::Consume(input, "/(\\d+)Mb", &corpus_total_size)) {
      status_.set_corpus_total_size(corpus_total_size * kOneMb);
    }
  }

  // Add other stats.
  auto elapsed = zx::clock::get_monotonic() - start_;
  status_.set_elapsed(elapsed.to_nsecs());

  std::vector<ProcessStats> process_stats;
  status_.set_process_stats(std::move(process_stats));

  UpdateMonitors(reason);
}

ZxResult<FuzzResult> LibFuzzerRunner::ParseError(re2::StringPiece* input) {
  if (re2::RE2::PartialMatch(*input, "fuzz target exited")) {
    // See libFuzzer's |Fuzzer::ExitCallback|.
    return fpromise::ok(FuzzResult::EXIT);
  }
  if (re2::RE2::PartialMatch(*input, "deadly signal")) {
    // See libFuzzer's |Fuzzer::CrashCallback|.
    return fpromise::ok(FuzzResult::CRASH);
  }
  if (re2::RE2::PartialMatch(*input, "timeout after \\d+ seconds")) {
    // See libFuzzer's |Fuzzer::AlarmCallback|.
    return fpromise::ok(FuzzResult::TIMEOUT);
  }
  if (re2::RE2::PartialMatch(*input, "out-of-memory \\(malloc\\(-?\\d+\\)\\)")) {
    // See libFuzzer's |Fuzzer::HandleMalloc|.
    return fpromise::ok(FuzzResult::BAD_MALLOC);
  }
  if (re2::RE2::PartialMatch(*input, "out-of-memory \\(used: \\d+Mb; limit: \\d+Mb\\)")) {
    // See libFuzzer's |Fuzzer::RssLimitCallback|.
    return fpromise::ok(FuzzResult::OOM);
  }

  // See libFuzzer's |Fuzzer::DeathCallback|.
  return fpromise::ok(FuzzResult::DEATH);
}

}  // namespace fuzzing
