// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "tools/fidlcat/lib/exception_decoder.h"

#include <lib/syslog/cpp/macros.h>

#include "src/developer/debug/zxdb/client/frame.h"
#include "src/developer/debug/zxdb/client/process.h"
#include "src/developer/debug/zxdb/client/thread.h"
#include "tools/fidlcat/lib/interception_workflow.h"

namespace fidlcat {

void ExceptionUse::ExceptionDecoded(ExceptionDecoder* decoder) { decoder->Destroy(); }

void ExceptionUse::DecodingError(const DecoderError& error, ExceptionDecoder* decoder) {
  FX_LOGS(ERROR) << error.message();
  decoder->Destroy();
}

void ExceptionDecoder::Decode() {
  zxdb::Thread* thread = get_thread();
  if (thread == nullptr) {
    Destroy();
    return;
  }
  if (thread->GetStack().has_all_frames()) {
    Display();
  } else {
    thread->GetStack().SyncFrames([this](const zxdb::Err& /*err*/) { Display(); });
  }
}

void ExceptionDecoder::Display() {
  zxdb::Thread* thread = get_thread();
  if (thread == nullptr) {
    Destroy();
    return;
  }
  const zxdb::Stack& stack = thread->GetStack();
  if (stack.size() > 0) {
    for (size_t i = stack.size() - 1;; --i) {
      const zxdb::Frame* caller = stack[i];
      caller_locations_.push_back(caller->GetLocation());
      if (i == 0) {
        break;
      }
    }
  }
  use_->ExceptionDecoded(this);
}

void ExceptionDecoder::Destroy() {
  InterceptionWorkflow* workflow = workflow_;
  uint64_t process_id = process_id_;
  uint64_t timestamp = timestamp_;
  dispatcher_->DeleteDecoder(this);
  workflow->ProcessDetached(process_id, timestamp);
}

void ExceptionDisplay::ExceptionDecoded(ExceptionDecoder* decoder) {
  Thread* thread = dispatcher_->SearchThread(decoder->thread_id());
  if (thread == nullptr) {
    Process* process = dispatcher_->SearchProcess(decoder->process_id());
    if (process == nullptr) {
      zxdb::Thread* zxdb_thread = decoder->get_thread();
      if (zxdb_thread == nullptr) {
        decoder->Destroy();
        return;
      }
      process = dispatcher_->CreateProcess(decoder->process_name(), decoder->process_id(),
                                           zxdb_thread->GetProcess()->GetWeakPtr());
    }
    thread = dispatcher_->CreateThread(decoder->thread_id(), process);
  }
  auto event = std::make_shared<ExceptionEvent>(decoder->timestamp(), thread);
  CopyStackFrame(decoder->caller_locations(), &event->stack_frame());
  dispatcher_->AddExceptionEvent(std::move(event));

  // Now our job is done, we can destroy the object.
  decoder->Destroy();
}

void ExceptionDisplay::DecodingError(const DecoderError& error, ExceptionDecoder* decoder) {
  std::string message = error.message();
  size_t pos = 0;
  for (;;) {
    size_t end = message.find('\n', pos);
    const fidl_codec::Colors& colors = dispatcher_->colors();
    os_ << decoder->process_name() << ' ' << colors.red << decoder->process_id() << colors.reset
        << ':' << colors.red << decoder->thread_id() << colors.reset << ": " << colors.red
        << error.message().substr(pos, end) << colors.reset << '\n';
    if (end == std::string::npos) {
      break;
    }
    pos = end + 1;
  }
  os_ << '\n';
  decoder->Destroy();
}

}  // namespace fidlcat
