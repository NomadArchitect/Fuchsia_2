// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/console/commands/verb_ps.h"

#include <iomanip>
#include <optional>
#include <set>
#include <sstream>
#include <string_view>

#include "src/developer/debug/zxdb/client/job.h"
#include "src/developer/debug/zxdb/client/process.h"
#include "src/developer/debug/zxdb/client/session.h"
#include "src/developer/debug/zxdb/client/system.h"
#include "src/developer/debug/zxdb/client/target.h"
#include "src/developer/debug/zxdb/console/command.h"
#include "src/developer/debug/zxdb/console/console.h"
#include "src/developer/debug/zxdb/console/output_buffer.h"
#include "src/developer/debug/zxdb/console/string_util.h"
#include "src/developer/debug/zxdb/console/verbs.h"

namespace zxdb {

namespace {

// Computes the set of attached job and process koids so they can be marked in the output.
std::set<uint64_t> ComputeAttachedKoidMap() {
  std::set<uint64_t> attached;

  System& system = Console::get()->context().session()->system();

  for (Target* target : system.GetTargets()) {
    if (Process* process = target->GetProcess())
      attached.insert(process->GetKoid());
  }

  for (Job* job : system.GetJobs()) {
    if (job->state() == Job::State::kAttached)
      attached.insert(job->koid());
  }

  return attached;
}

void OutputProcessTreeRecord(const debug_ipc::ProcessTreeRecord& rec, int indent,
                             const std::set<uint64_t>& attached, OutputBuffer* output) {
  // Row marker for attached processes/jobs.
  std::string prefix;
  Syntax syntax = Syntax::kNormal;
  if (auto found = attached.find(rec.koid); found != attached.end()) {
    syntax = Syntax::kHeading;
    prefix = GetCurrentRowMarker();
  } else {
    prefix = " ";  // Account for no prefix so everything is aligned.
  }

  // Indentation.
  prefix.append(indent * 2, ' ');

  // Record type.
  switch (rec.type) {
    case debug_ipc::ProcessTreeRecord::Type::kJob:
      prefix.append("j: ");
      break;
    case debug_ipc::ProcessTreeRecord::Type::kProcess:
      prefix.append("p: ");
      break;
    default:
      prefix.append("?: ");
      break;
  }

  output->Append(syntax, prefix);
  output->Append(Syntax::kSpecial, std::to_string(rec.koid));
  if (!rec.name.empty())
    output->Append(syntax, " " + rec.name);
  if (!rec.component_moniker.empty())
    output->Append(" " + rec.component_moniker, TextForegroundColor::kCyan);
  if (!rec.component_url.empty())
    output->Append(" " + rec.component_url, TextForegroundColor::kGray);
  output->Append(syntax, "\n");

  for (const auto& child : rec.children)
    OutputProcessTreeRecord(child, indent + 1, attached, output);
}

// Recursively filters the given process tree. All jobs and processes that contain the given filter
// string in their name are matched. These are added to the result, along with any parent job nodes
// required to get to the matched records.
std::optional<debug_ipc::ProcessTreeRecord> FilterProcessTree(
    const debug_ipc::ProcessTreeRecord& rec, const std::string& filter) {
  debug_ipc::ProcessTreeRecord result;

  // A record matches if its name or component_moniker matches.
  bool matched = rec.name.find(filter) != std::string::npos;
  if (!matched && !rec.component_moniker.empty()) {
    // Use the last segment of the moniker as the "component name".
    std::string_view moniker = rec.component_moniker;
    std::string_view name = moniker.substr(moniker.find_last_of('/') + 1);
    matched = name.find(filter) != std::string_view::npos;
  }

  // If a record matches, show all its children.
  if (matched) {
    result.children = rec.children;
  } else {
    for (const auto& child : rec.children) {
      if (auto matched_child = FilterProcessTree(child, filter))
        result.children.push_back(*matched_child);
    }
  }

  // Return the node when it matches or any of its children do.
  if (matched || !result.children.empty()) {
    result.type = rec.type;
    result.koid = rec.koid;
    result.name = rec.name;
    result.component_url = rec.component_url;
    result.component_moniker = rec.component_moniker;
    return result;
  }

  return std::nullopt;
}

void OnListProcessesComplete(const std::string& filter, const Err& err,
                             const debug_ipc::ProcessTreeReply& reply) {
  std::set<uint64_t> attached = ComputeAttachedKoidMap();

  OutputBuffer out;
  if (err.has_error()) {
    out.Append(err);
  } else if (filter.empty()) {
    // Output everything.
    OutputProcessTreeRecord(reply.root, 0, attached, &out);
  } else {
    // Filter the results.
    if (auto filtered = FilterProcessTree(reply.root, filter)) {
      OutputProcessTreeRecord(*filtered, 0, attached, &out);
    } else {
      out.Append("No processes or jobs matching \"" + filter + "\".\n");
    }
  }
  Console::get()->Output(out);
}

const char kPsShortHelp[] = "ps: Prints the process tree of the debugged system.";
const char kPsHelp[] =
    R"(ps [ <filter-string> ]

  Prints the process tree of the debugged system.

  If a filter-string is provided only jobs and processes whose names contain the
  given case-sensitive substring. It does not support regular expressions.

  If a job is the root job of a component, the component information will also
  be printed.

  Jobs are annotated with "j: <job koid>"
  Processes are annotated with "p: <process koid>")";

Err RunVerbPs(ConsoleContext* context, const Command& cmd) {
  std::string filter_string;
  if (!cmd.args().empty())
    filter_string = cmd.args()[0];

  context->session()->system().GetProcessTree(
      [filter_string](const Err& err, debug_ipc::ProcessTreeReply reply) {
        OnListProcessesComplete(filter_string, err, reply);
      });
  return Err();
}

}  // namespace

VerbRecord GetPsVerbRecord() {
  VerbRecord record(&RunVerbPs, {"ps"}, kPsShortHelp, kPsHelp, CommandGroup::kGeneral);
  record.param_type = VerbRecord::kOneParam;  // Allow spaces in the filter string.
  return record;
}

}  // namespace zxdb
