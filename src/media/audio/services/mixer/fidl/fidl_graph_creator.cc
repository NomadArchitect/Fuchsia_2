// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/media/audio/services/mixer/fidl/fidl_graph_creator.h"

#include <lib/syslog/cpp/macros.h>
#include <lib/trace/event.h>

#include "src/media/audio/services/mixer/fidl/fidl_graph.h"
#include "src/media/audio/services/mixer/fidl/fidl_synthetic_clock.h"
#include "src/media/audio/services/mixer/fidl/real_clock_factory.h"

namespace media_audio {

// static
std::shared_ptr<FidlGraphCreator> FidlGraphCreator::Create(
    std::shared_ptr<const FidlThread> thread,
    fidl::ServerEnd<fuchsia_audio_mixer::GraphCreator> server_end) {
  return BaseFidlServer::Create(std::move(thread), std::move(server_end));
}

void FidlGraphCreator::Create(CreateRequestView request, CreateCompleter::Sync& completer) {
  TRACE_DURATION("audio", "GraphCreator::Create");
  ScopedThreadChecker checker(thread().checker());

  if (!request->has_graph()) {
    completer.ReplyError(fuchsia_audio_mixer::CreateGraphError::kInvalidGraphChannel);
    return;
  }

  FidlGraph::Args args;

  if (request->has_name()) {
    args.name = std::string(request->name().get());
  }

  if (request->has_realtime_fidl_thread_deadline_profile()) {
    args.realtime_fidl_thread_deadline_profile =
        std::move(request->realtime_fidl_thread_deadline_profile());
  }

  if (request->has_synthetic_clock_realm()) {
    auto realm =
        FidlSyntheticClockRealm::Create(thread_ptr(), std::move(request->synthetic_clock_realm()));
    args.clock_registry = realm->registry();
  } else {
    args.clock_registry = std::make_shared<ClockRegistry>(std::make_shared<RealClockFactory>());
  }

  // Create a server to control this graph.
  // The created object will live until `args.sever_end` is closed.
  AddChildServer(FidlGraph::Create(thread_ptr(), std::move(request->graph()), std::move(args)));

  fidl::Arena arena;
  completer.ReplySuccess(
      fuchsia_audio_mixer::wire::GraphCreatorCreateResponse::Builder(arena).Build());
}

}  // namespace media_audio
