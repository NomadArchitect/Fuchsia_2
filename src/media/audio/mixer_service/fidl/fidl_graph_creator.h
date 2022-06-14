// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_AUDIO_MIXER_SERVICE_FIDL_FIDL_GRAPH_CREATOR_H_
#define SRC_MEDIA_AUDIO_MIXER_SERVICE_FIDL_FIDL_GRAPH_CREATOR_H_

#include <fidl/fuchsia.audio.mixer/cpp/wire.h>
#include <zircon/errors.h>

#include <memory>
#include <optional>

#include "src/media/audio/mixer_service/fidl/base_fidl_server.h"

namespace media_audio {

class FidlGraphCreator
    : public BaseFidlServer<FidlGraphCreator, fuchsia_audio_mixer::GraphCreator> {
 public:
  static std::shared_ptr<FidlGraphCreator> Create(
      async_dispatcher_t* fidl_thread_dispatcher,
      fidl::ServerEnd<fuchsia_audio_mixer::GraphCreator> server_end);

  // Implementation of fidl::WireServer<fuchsia_audio_mixer::GraphCreator>.
  void Create(CreateRequestView request, CreateCompleter::Sync& completer) override;

 private:
  static inline constexpr std::string_view Name = "FidlGraphCreator";
  template <class ServerT, class ProtocolT>
  friend class BaseFidlServer;

  FidlGraphCreator() = default;
};

}  // namespace media_audio

#endif  // SRC_MEDIA_AUDIO_MIXER_SERVICE_FIDL_FIDL_GRAPH_CREATOR_H_
