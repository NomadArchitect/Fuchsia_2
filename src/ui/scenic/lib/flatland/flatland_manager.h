// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_FLATLAND_FLATLAND_MANAGER_H_
#define SRC_UI_SCENIC_LIB_FLATLAND_FLATLAND_MANAGER_H_

#include <fuchsia/ui/scenic/internal/cpp/fidl.h>
#include <lib/async/default.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async/cpp/executor.h>
#include <lib/fidl/cpp/binding.h>

#include <map>
#include <mutex>
#include <thread>
#include <unordered_map>

#include "src/ui/scenic/lib/display/display.h"
#include "src/ui/scenic/lib/flatland/flatland.h"
#include "src/ui/scenic/lib/flatland/flatland_presenter.h"
#include "src/ui/scenic/lib/flatland/link_system.h"
#include "src/ui/scenic/lib/flatland/uber_struct_system.h"
#include "src/ui/scenic/lib/scheduling/frame_scheduler.h"
#include "src/ui/scenic/lib/scheduling/id.h"
#include "src/ui/scenic/lib/utils/dispatcher_holder.h"

namespace flatland {

class FlatlandManager : public scheduling::SessionUpdater {
 public:
  FlatlandManager(async_dispatcher_t* dispatcher,
                  const std::shared_ptr<FlatlandPresenter>& flatland_presenter,
                  const std::shared_ptr<UberStructSystem>& uber_struct_system,
                  const std::shared_ptr<LinkSystem>& link_system,
                  std::shared_ptr<scenic_impl::display::Display> display,
                  const std::vector<std::shared_ptr<allocation::BufferCollectionImporter>>&
                      buffer_collection_importers);
  ~FlatlandManager() override;

  void CreateFlatland(fidl::InterfaceRequest<fuchsia::ui::scenic::internal::Flatland> flatland);

  // |scheduling::SessionUpdater|
  scheduling::SessionUpdater::UpdateResults UpdateSessions(
      const std::unordered_map<scheduling::SessionId, scheduling::PresentId>& sessions_to_update,
      uint64_t trace_id) override;

  // |scheduling::SessionUpdater|
  void OnCpuWorkDone() override;

  // |scheduling::SessionUpdater|
  void OnFramePresented(
      const std::unordered_map<scheduling::SessionId,
                               std::map<scheduling::PresentId, /*latched_time*/ zx::time>>&
          latched_times,
      scheduling::PresentTimestamps present_times) override;

  // For validating test logic.
  size_t GetSessionCount() const;

 private:
  // Represents an individual Flatland session for a client.
  struct FlatlandInstance {
    // The looper for this Flatland instance, which will be run on a worker thread spawned by the
    // async::Loop itself. It must be the first member of this struct so that |impl| is
    // destroyed first in the default destruction order, else it will attempt to run on a shutdown
    // looper.
    std::shared_ptr<utils::LoopDispatcherHolder> loop;

    // The implementation of Flatland, which includes the bindings for the instance. This must come
    // before |peer_closed_waiter| so that the Wait is destroyed, and therefore cancelled, before
    // the impl is destroyed in the default destruction order.
    std::shared_ptr<Flatland> impl;
  };

  // Sends |num_present_tokens| to a particular Flatland |instance|.
  void SendPresentTokens(FlatlandInstance* instance, uint32_t num_present_tokens,
                         Flatland::FuturePresentationInfos presentation_infos);

  // Sends the OnFramePresented event to a particular Flatland |instance|.
  void SendFramePresented(
      FlatlandInstance* instance,
      const std::map<scheduling::PresentId, /*latched_time*/ zx::time>& latched_times,
      scheduling::PresentTimestamps present_times);

  // Removes the Flatland instance associated with |session_id|.
  void RemoveFlatlandInstance(scheduling::SessionId session_id);

  // The function passed into a Flatland constructor that allows the Flatland instance to trigger
  // its own destruction when the client makes an unrecoverable error. This function will be called
  // on Flatland instance worker threads.
  void DestroyInstanceFunction(scheduling::SessionId session_id);

  // Used to assert that code is running on the expected thread.
  void CheckIsOnMainThread() const {
    FX_DCHECK(executor_.dispatcher() == async_get_default_dispatcher());
  }

  std::shared_ptr<FlatlandPresenter> flatland_presenter_;
  std::shared_ptr<UberStructSystem> uber_struct_system_;
  std::shared_ptr<LinkSystem> link_system_;
  std::vector<std::shared_ptr<allocation::BufferCollectionImporter>> buffer_collection_importers_;
  std::unordered_map<scheduling::SessionId, /*num_present_tokens*/ uint64_t>
      flatland_instances_updated_;

  // FlatlandInstances must be dynamically allocated because fidl::Binding is not movable.
  std::unordered_map<scheduling::SessionId, std::unique_ptr<FlatlandInstance>> flatland_instances_;

  // Stores and executes async tasks on the dispatcher provided in this object's constructor. This
  // object is the final member of this class to ensure that async tasks are cancelled and
  // destroyed first during destruction, else they might access already-destroyed members.
  async::Executor executor_;

  // Eventually we will support multiple displays, but as we bootstrap Flatland we assume that
  // there is a single primary display.
  std::shared_ptr<scenic_impl::display::Display> primary_display_;
};

}  // namespace flatland

#endif  // SRC_UI_SCENIC_LIB_FLATLAND_FLATLAND_MANAGER_H_
