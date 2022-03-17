// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_AUDIO_MIXER_SERVICE_FIDL_FIDL_GRAPH_H_
#define SRC_MEDIA_AUDIO_MIXER_SERVICE_FIDL_FIDL_GRAPH_H_

#include <fidl/fuchsia.audio.mixer/cpp/wire.h>
#include <zircon/errors.h>

#include <optional>

#include "src/media/audio/mixer_service/fidl/ptr_decls.h"

namespace media_audio_mixer_service {

class FidlGraph : public fidl::WireServer<fuchsia_audio_mixer::Graph> {
 public:
  static FidlGraphPtr Create(async_dispatcher_t* fidl_thread_dispatcher,
                             fidl::ServerEnd<fuchsia_audio_mixer::Graph> server_end);

  // Shutdown this server.
  // This closes the channel, which eventually deletes this server.
  void Shutdown();

  // Implementation of fidl::WireServer<fuchsia_audio_mixer::Graph>.
  void CreateProducer(CreateProducerRequestView request,
                      CreateProducerCompleter::Sync& completer) override;
  void CreateConsumer(CreateConsumerRequestView request,
                      CreateConsumerCompleter::Sync& completer) override;
  void CreateMixer(CreateMixerRequestView request, CreateMixerCompleter::Sync& completer) override;
  void CreateSplitter(CreateSplitterRequestView request,
                      CreateSplitterCompleter::Sync& completer) override;
  void CreateCustom(CreateCustomRequestView request,
                    CreateCustomCompleter::Sync& completer) override;
  void DeleteNode(DeleteNodeRequestView request, DeleteNodeCompleter::Sync& completer) override;
  void CreateEdge(CreateEdgeRequestView request, CreateEdgeCompleter::Sync& completer) override;
  void DeleteEdge(DeleteEdgeRequestView request, DeleteEdgeCompleter::Sync& completer) override;
  void CreateThread(CreateThreadRequestView request,
                    CreateThreadCompleter::Sync& completer) override;
  void DeleteThread(DeleteThreadRequestView request,
                    DeleteThreadCompleter::Sync& completer) override;
  void CreateGainStage(CreateGainStageRequestView request,
                       CreateGainStageCompleter::Sync& completer) override;
  void DeleteGainStage(DeleteGainStageRequestView request,
                       DeleteGainStageCompleter::Sync& completer) override;
  void CreateGraphControlledReferenceClock(
      CreateGraphControlledReferenceClockRequestView request,
      CreateGraphControlledReferenceClockCompleter::Sync& completer) override;
  void ForgetGraphControlledReferenceClock(
      ForgetGraphControlledReferenceClockRequestView request,
      ForgetGraphControlledReferenceClockCompleter::Sync& completer) override;

 private:
  FidlGraph() = default;
  std::optional<fidl::ServerBindingRef<fuchsia_audio_mixer::Graph>> binding_;
};

}  // namespace media_audio_mixer_service

#endif  // SRC_MEDIA_AUDIO_MIXER_SERVICE_FIDL_FIDL_GRAPH_H_
