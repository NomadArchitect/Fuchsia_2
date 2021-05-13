// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/flatland/flatland.h"

#include <lib/async/default.h>
#include <lib/async/time.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/eventpair.h>

#include <memory>
#include <utility>

#include "src/lib/fsl/handles/object_info.h"

#include <glm/gtc/constants.hpp>
#include <glm/gtc/matrix_access.hpp>
#include <glm/gtc/type_ptr.hpp>

using fuchsia::ui::scenic::internal::ContentLink;
using fuchsia::ui::scenic::internal::ContentLinkStatus;
using fuchsia::ui::scenic::internal::ContentLinkToken;
using fuchsia::ui::scenic::internal::Error;
using fuchsia::ui::scenic::internal::GraphLink;
using fuchsia::ui::scenic::internal::GraphLinkToken;
using fuchsia::ui::scenic::internal::ImageProperties;
using fuchsia::ui::scenic::internal::LinkProperties;
using fuchsia::ui::scenic::internal::Orientation;
using fuchsia::ui::scenic::internal::Vec2;

namespace flatland {

Flatland::Flatland(std::shared_ptr<utils::DispatcherHolder> dispatcher_holder,
                   fidl::InterfaceRequest<fuchsia::ui::scenic::internal::Flatland> request,
                   scheduling::SessionId session_id,
                   std::function<void()> destroy_instance_function,
                   std::shared_ptr<FlatlandPresenter> flatland_presenter,
                   std::shared_ptr<LinkSystem> link_system,
                   std::shared_ptr<UberStructSystem::UberStructQueue> uber_struct_queue,
                   const std::vector<std::shared_ptr<allocation::BufferCollectionImporter>>&
                       buffer_collection_importers)
    : dispatcher_holder_(std::move(dispatcher_holder)),
      binding_(this, std::move(request), dispatcher_holder_->dispatcher()),
      session_id_(session_id),
      destroy_instance_function_(std::move(destroy_instance_function)),
      peer_closed_waiter_(binding_.channel().get(), ZX_CHANNEL_PEER_CLOSED),
      present2_helper_([this](fuchsia::scenic::scheduling::FramePresentedInfo info) {
        if (binding_.is_bound()) {
          binding_.events().OnFramePresented(std::move(info));
        }
      }),
      flatland_presenter_(std::move(flatland_presenter)),
      link_system_(std::move(link_system)),
      uber_struct_queue_(std::move(uber_struct_queue)),
      buffer_collection_importers_(buffer_collection_importers),
      transform_graph_(session_id_),
      local_root_(transform_graph_.CreateTransform()) {
  zx_status_t status = peer_closed_waiter_.Begin(
      dispatcher(),
      [this](async_dispatcher_t* dispatcher, async::WaitOnce* wait, zx_status_t status,
             const zx_packet_signal_t* signal) { destroy_instance_function_(); });
  FX_DCHECK(status == ZX_OK);
}

Flatland::~Flatland() {
  // TODO(fxbug.dev/55374): consider if Link tokens should be returned or not.
}

void Flatland::Present(fuchsia::ui::scenic::internal::PresentArgs args, PresentCallback callback) {
  // Close any clients that call Present() without any present tokens.
  if (present_tokens_remaining_ == 0) {
    callback(fit::error(Error::NO_PRESENTS_REMAINING));
    CloseConnection();
    return;
  }
  present_tokens_remaining_--;

  // If any fields are missing, replace them with the default values.
  if (!args.has_requested_presentation_time()) {
    args.set_requested_presentation_time(0);
  }
  if (!args.has_release_fences()) {
    args.set_release_fences({});
  }
  if (!args.has_acquire_fences()) {
    args.set_acquire_fences({});
  }
  if (!args.has_squashable()) {
    args.set_squashable(true);
  }

  auto root_handle = GetRoot();

  // TODO(fxbug.dev/40818): Decide on a proper limit on compute time for topological sorting.
  auto data = transform_graph_.ComputeAndCleanup(root_handle, std::numeric_limits<uint64_t>::max());
  FX_DCHECK(data.iterations != std::numeric_limits<uint64_t>::max());

  // TODO(fxbug.dev/36166): Once the 2D scene graph is externalized, don't commit changes if a cycle
  // is detected. Instead, kill the channel and remove the sub-graph from the global graph.
  failure_since_previous_present_ |= !data.cyclical_edges.empty();

  if (!failure_since_previous_present_) {
    FX_DCHECK(data.sorted_transforms[0].handle == root_handle);

    // Cleanup released resources. Here we also collect the list of unused images so they can be
    // released by the buffer collection importers.
    std::vector<allocation::ImageMetadata> images_to_release;
    for (const auto& dead_handle : data.dead_transforms) {
      matrices_.erase(dead_handle);

      auto image_kv = image_metadatas_.find(dead_handle);
      if (image_kv != image_metadatas_.end()) {
        images_to_release.push_back(image_kv->second);
        image_metadatas_.erase(image_kv);
      }
    }

    // If there are images ready for release, create a release fence for the current Present() and
    // delay release until that fence is reached to ensure that the images are no longer referenced
    // in any render data.
    if (!images_to_release.empty()) {
      // Create a release fence specifically for the images.
      zx::event image_release_fence;
      zx_status_t status = zx::event::create(0, &image_release_fence);
      FX_DCHECK(status == ZX_OK);

      // Use a self-referencing async::WaitOnce to perform ImageImporter deregistration.
      // This is primarily so the handler does not have to live in the Flatland instance, which may
      // be destroyed before the release fence is signaled. WaitOnce moves the handler to the stack
      // prior to invoking it, so it is safe for the handler to delete the WaitOnce on exit.
      // Specifically, we move the wait object into the lambda function via |copy_ref = wait| to
      // ensure that the wait object lives. The callback will not trigger without this.
      auto wait = std::make_shared<async::WaitOnce>(image_release_fence.get(), ZX_EVENT_SIGNALED);
      status =
          wait->Begin(dispatcher(),
                      [copy_ref = wait, importer_ref = buffer_collection_importers_,
                       images_to_release](async_dispatcher_t*, async::WaitOnce*, zx_status_t status,
                                          const zx_packet_signal_t* /*signal*/) mutable {
                        FX_DCHECK(status == ZX_OK);

                        for (auto& image_id : images_to_release) {
                          for (auto& importer : importer_ref) {
                            importer->ReleaseBufferImage(image_id.identifier);
                          }
                        }
                      });
      FX_DCHECK(status == ZX_OK);

      // Push the new release fence into the user-provided list.
      args.mutable_release_fences()->push_back(std::move(image_release_fence));
    }

    auto uber_struct = std::make_unique<UberStruct>();
    uber_struct->local_topology = std::move(data.sorted_transforms);

    for (const auto& [link_id, child_link] : child_links_) {
      LinkProperties initial_properties;
      fidl::Clone(child_link.properties, &initial_properties);
      uber_struct->link_properties[child_link.link.graph_handle] = std::move(initial_properties);
    }

    for (const auto& [handle, matrix_data] : matrices_) {
      uber_struct->local_matrices[handle] = matrix_data.GetMatrix();
    }

    for (const auto& [handle, opacity_value] : opacity_values_) {
      uber_struct->local_opacity_values[handle] = opacity_value;
    }

    uber_struct->images = image_metadatas_;

    // Register a Present to get the PresentId needed to queue the UberStruct. This happens before
    // waiting on the acquire fences to indicate that a Present is pending.
    auto present_id = flatland_presenter_->RegisterPresent(
        session_id_, std::move(*args.mutable_release_fences()));
    present2_helper_.RegisterPresent(present_id,
                                     /*present_received_time=*/zx::time(async_now(dispatcher())));

    // Safe to capture |this| because the Flatland is guaranteed to outlive |fence_queue_|,
    // Flatland is non-movable and FenceQueue does not fire closures after destruction.
    fence_queue_->QueueTask(
        [this, present_id, requested_presentation_time = args.requested_presentation_time(),
         squashable = args.squashable(), uber_struct = std::move(uber_struct),
         link_operations = std::move(pending_link_operations_),
         release_fences = std::move(*args.mutable_release_fences())]() mutable {
          // Push the UberStruct, then schedule the associated Present that will eventually publish
          // it to the InstanceMap used for rendering.
          uber_struct_queue_->Push(present_id, std::move(uber_struct));
          flatland_presenter_->ScheduleUpdateForSession(zx::time(requested_presentation_time),
                                                        {session_id_, present_id}, squashable);

          // Finalize Link destruction operations after publishing the new UberStruct. This
          // ensures that any local Transforms referenced by the to-be-deleted Links are already
          // removed from the now-published UberStruct.
          for (auto& operation : link_operations) {
            operation();
          }
        },
        std::move(*args.mutable_acquire_fences()));

    callback(fit::ok());
  } else {
    // TODO(fxbug.dev/56869): determine if pending link operations should still be run here.
    callback(fit::error(Error::BAD_OPERATION));
  }

  failure_since_previous_present_ = false;
}

void Flatland::LinkToParent(GraphLinkToken token, fidl::InterfaceRequest<GraphLink> graph_link) {
  // Attempting to link with an invalid token will never succeed, so its better to fail early and
  // immediately close the link connection.
  if (!token.value.is_valid()) {
    FX_LOGS(ERROR) << "LinkToParent failed, GraphLinkToken was invalid";
    ReportError();
    return;
  }

  FX_DCHECK(link_system_);

  // This portion of the method is not feed forward. This makes it possible for clients to receive
  // layout information before this operation has been presented. By initializing the link
  // immediately, parents can inform children of layout changes, and child clients can perform
  // layout decisions before their first call to Present().
  auto link_origin = transform_graph_.CreateTransform();
  LinkSystem::ParentLink link = link_system_->CreateParentLink(dispatcher_holder_, std::move(token),
                                                               std::move(graph_link), link_origin);

  // This portion of the method is feed-forward. The parent-child relationship between
  // |link_origin| and |local_root_| establishes the Transform hierarchy between the two instances,
  // but the operation will not be visible until the next Present() call includes that topology.
  if (parent_link_.has_value()) {
    bool child_removed = transform_graph_.RemoveChild(parent_link_->link_origin, local_root_);
    FX_DCHECK(child_removed);

    bool transform_released = transform_graph_.ReleaseTransform(parent_link_->link_origin);
    FX_DCHECK(transform_released);

    // Delay the destruction of the previous parent link until the next Present().
    pending_link_operations_.push_back(
        [local_link = std::move(parent_link_)]() mutable { local_link.reset(); });
  }

  bool child_added = transform_graph_.AddChild(link.link_origin, local_root_);
  FX_DCHECK(child_added);
  parent_link_ = std::move(link);
}

void Flatland::UnlinkFromParent(
    fuchsia::ui::scenic::internal::Flatland::UnlinkFromParentCallback callback) {
  if (!parent_link_) {
    FX_LOGS(ERROR) << "UnlinkFromParent failed, no existing parent Link";
    ReportError();
    return;
  }

  // Deleting the old ParentLink's Transform effectively changes this intance's root back to
  // |local_root_|.
  bool child_removed = transform_graph_.RemoveChild(parent_link_->link_origin, local_root_);
  FX_DCHECK(child_removed);

  bool transform_released = transform_graph_.ReleaseTransform(parent_link_->link_origin);
  FX_DCHECK(transform_released);

  // Move the old parent link into the delayed operation so that it isn't taken into account when
  // computing the local topology, but doesn't get deleted until after the new UberStruct is
  // published.
  auto local_link = std::move(parent_link_.value());
  parent_link_.reset();

  // Delay the actual destruction of the Link until the next Present().
  pending_link_operations_.push_back(
      [local_link = std::move(local_link), callback = std::move(callback)]() mutable {
        GraphLinkToken return_token;

        // If the link is still valid, return the original token. If not, create an orphaned
        // zx::eventpair and return it since the ObjectLinker does not retain the orphaned token.
        auto link_token = local_link.exporter.ReleaseToken();
        if (link_token.has_value()) {
          return_token.value = zx::eventpair(std::move(link_token.value()));
        } else {
          // |peer_token| immediately falls out of scope, orphaning |return_token|.
          zx::eventpair peer_token;
          zx::eventpair::create(0, &return_token.value, &peer_token);
        }

        callback(std::move(return_token));
      });
}

void Flatland::ClearGraph() {
  // Clear user-defined mappings and local matrices.
  transforms_.clear();
  content_handles_.clear();
  matrices_.clear();

  // We always preserve the link origin when clearing the graph. This call will place all other
  // TransformHandles in the dead_transforms set in the next Present(), which will trigger cleanup
  // of Images and BufferCollections.
  transform_graph_.ResetGraph(local_root_);

  // If a parent Link exists, delay its destruction until Present().
  if (parent_link_.has_value()) {
    auto local_link = std::move(parent_link_);
    parent_link_.reset();

    pending_link_operations_.push_back(
        [local_link = std::move(local_link)]() mutable { local_link.reset(); });
  }

  // Delay destruction of all child Links until Present().
  auto local_links = std::move(child_links_);
  child_links_.clear();

  pending_link_operations_.push_back(
      [local_links = std::move(local_links)]() mutable { local_links.clear(); });
}

void Flatland::CreateTransform(TransformId transform_id) {
  if (transform_id.value == kInvalidId) {
    FX_LOGS(ERROR) << "CreateTransform called with transform_id 0";
    ReportError();
    return;
  }

  if (transforms_.count(transform_id.value)) {
    FX_LOGS(ERROR) << "CreateTransform called with pre-existing transform_id "
                   << transform_id.value;
    ReportError();
    return;
  }

  TransformHandle handle = transform_graph_.CreateTransform();
  transforms_.insert({transform_id.value, handle});
}

void Flatland::SetTranslation(TransformId transform_id, Vec2 translation) {
  if (transform_id.value == kInvalidId) {
    FX_LOGS(ERROR) << "SetTranslation called with transform_id 0";
    ReportError();
    return;
  }

  auto transform_kv = transforms_.find(transform_id.value);

  if (transform_kv == transforms_.end()) {
    FX_LOGS(ERROR) << "SetTranslation failed, transform_id " << transform_id.value << " not found";
    ReportError();
    return;
  }

  matrices_[transform_kv->second].SetTranslation(translation);
}

void Flatland::SetOrientation(TransformId transform_id, Orientation orientation) {
  if (transform_id.value == kInvalidId) {
    FX_LOGS(ERROR) << "SetOrientation called with transform_id 0";
    ReportError();
    return;
  }

  auto transform_kv = transforms_.find(transform_id.value);

  if (transform_kv == transforms_.end()) {
    FX_LOGS(ERROR) << "SetOrientation failed, transform_id " << transform_id.value << " not found";
    ReportError();
    return;
  }

  matrices_[transform_kv->second].SetOrientation(orientation);
}

void Flatland::SetScale(TransformId transform_id, Vec2 scale) {
  if (transform_id.value == kInvalidId) {
    FX_LOGS(ERROR) << "SetScale called with transform_id 0";
    ReportError();
    return;
  }

  auto transform_kv = transforms_.find(transform_id.value);

  if (transform_kv == transforms_.end()) {
    FX_LOGS(ERROR) << "SetScale failed, transform_id " << transform_id.value << " not found";
    ReportError();
    return;
  }

  matrices_[transform_kv->second].SetScale(scale);
}

void Flatland::AddChild(TransformId parent_transform_id, TransformId child_transform_id) {
  if (parent_transform_id.value == kInvalidId || child_transform_id.value == kInvalidId) {
    FX_LOGS(ERROR) << "AddChild called with transform_id zero";
    ReportError();
    return;
  }

  auto parent_global_kv = transforms_.find(parent_transform_id.value);
  auto child_global_kv = transforms_.find(child_transform_id.value);

  if (parent_global_kv == transforms_.end()) {
    FX_LOGS(ERROR) << "AddChild failed, parent_transform_id " << parent_transform_id.value
                   << " not found";
    ReportError();
    return;
  }

  if (child_global_kv == transforms_.end()) {
    FX_LOGS(ERROR) << "AddChild failed, child_transform_id " << child_transform_id.value
                   << " not found";
    ReportError();
    return;
  }

  if (opacity_values_.find(parent_global_kv->second) != opacity_values_.end() &&
      opacity_values_[parent_global_kv->second] != 1.f) {
    FX_LOGS(ERROR) << "Cannot add a child to a node with an opacity value < 1.0.";
    ReportError();
    return;
  }

  bool added = transform_graph_.AddChild(parent_global_kv->second, child_global_kv->second);

  if (!added) {
    FX_LOGS(ERROR) << "AddChild failed, connection already exists between parent "
                   << parent_transform_id.value << " and child " << child_transform_id.value;
    ReportError();
  }
}

void Flatland::RemoveChild(TransformId parent_transform_id, TransformId child_transform_id) {
  if (parent_transform_id.value == kInvalidId || child_transform_id.value == kInvalidId) {
    FX_LOGS(ERROR) << "RemoveChild failed, transform_id " << parent_transform_id.value
                   << " not found";
    ReportError();
    return;
  }

  auto parent_global_kv = transforms_.find(parent_transform_id.value);
  auto child_global_kv = transforms_.find(child_transform_id.value);

  if (parent_global_kv == transforms_.end()) {
    FX_LOGS(ERROR) << "RemoveChild failed, parent_transform_id " << parent_transform_id.value
                   << " not found";
    ReportError();
    return;
  }

  if (child_global_kv == transforms_.end()) {
    FX_LOGS(ERROR) << "RemoveChild failed, child_transform_id " << child_transform_id.value
                   << " not found";
    ReportError();
    return;
  }

  bool removed = transform_graph_.RemoveChild(parent_global_kv->second, child_global_kv->second);

  if (!removed) {
    FX_LOGS(ERROR) << "RemoveChild failed, connection between parent " << parent_transform_id.value
                   << " and child " << child_transform_id.value << " not found";
    ReportError();
  }
}

void Flatland::SetRootTransform(TransformId transform_id) {
  // SetRootTransform(0) is special -- it only clears the existing root transform.
  if (transform_id.value == kInvalidId) {
    transform_graph_.ClearChildren(local_root_);
    return;
  }

  auto global_kv = transforms_.find(transform_id.value);
  if (global_kv == transforms_.end()) {
    FX_LOGS(ERROR) << "SetRootTransform failed, transform_id " << transform_id.value
                   << " not found";
    ReportError();
    return;
  }

  transform_graph_.ClearChildren(local_root_);

  bool added = transform_graph_.AddChild(local_root_, global_kv->second);
  FX_DCHECK(added);
}

void Flatland::CreateLink(ContentId link_id, ContentLinkToken token, LinkProperties properties,
                          fidl::InterfaceRequest<ContentLink> content_link) {
  // Attempting to link with an invalid token will never succeed, so its better to fail early and
  // immediately close the link connection.
  if (!token.value.is_valid()) {
    FX_LOGS(ERROR) << "CreateLink failed, ContentLinkToken was invalid";
    ReportError();
    return;
  }

  if (!properties.has_logical_size()) {
    FX_LOGS(ERROR) << "CreateLink must be provided a LinkProperties with a logical size";
    ReportError();
    return;
  }

  auto logical_size = properties.logical_size();
  if (logical_size.x <= 0.f || logical_size.y <= 0.f) {
    FX_LOGS(ERROR) << "CreateLink must be provided a logical size with positive X and Y values";
    ReportError();
    return;
  }

  FX_DCHECK(link_system_);

  // The LinkProperties and ContentLinkImpl live on a handle from this Flatland instance.
  auto graph_handle = transform_graph_.CreateTransform();

  // We can initialize the Link importer immediately, since no state changes actually occur before
  // the feed-forward portion of this method. We also forward the initial LinkProperties through
  // the LinkSystem immediately, so the child can receive them as soon as possible.
  LinkProperties initial_properties;
  fidl::Clone(properties, &initial_properties);
  LinkSystem::ChildLink link = link_system_->CreateChildLink(dispatcher_holder_, std::move(token),
                                                             std::move(initial_properties),
                                                             std::move(content_link), graph_handle);

  if (link_id.value == kInvalidId) {
    FX_LOGS(ERROR) << "CreateLink called with ContentId zero";
    ReportError();
    return;
  }

  if (content_handles_.count(link_id.value)) {
    FX_LOGS(ERROR) << "CreateLink called with existing ContentId " << link_id.value;
    ReportError();
    return;
  }

  // This is the feed-forward portion of the method. Here, we add the link to the map, and
  // initialize its layout with the desired properties. The Link will not actually result in
  // additions to the Transform hierarchy until it is added to a Transform.
  bool child_added = transform_graph_.AddChild(link.graph_handle, link.link_handle);
  FX_DCHECK(child_added);

  // Default the link size to the logical size, which is just an identity scale matrix, so
  // that future logical size changes will result in the correct scale matrix.
  Vec2 size = properties.logical_size();

  content_handles_[link_id.value] = link.graph_handle;
  child_links_[link.graph_handle] = {
      .link = std::move(link), .properties = std::move(properties), .size = std::move(size)};
}

void Flatland::CreateImage(ContentId image_id,
                           fuchsia::scenic::allocation::BufferCollectionImportToken import_token,
                           uint32_t vmo_index, ImageProperties properties) {
  if (image_id.value == kInvalidId) {
    FX_LOGS(ERROR) << "CreateImage called with image_id 0";
    ReportError();
    return;
  }

  if (content_handles_.count(image_id.value)) {
    FX_LOGS(ERROR) << "CreateImage called with pre-existing image_id " << image_id.value;
    ReportError();
    return;
  }

  const BufferCollectionId global_collection_id = fsl::GetRelatedKoid(import_token.value.get());

  // Check if there is a valid peer.
  if (global_collection_id == ZX_KOID_INVALID) {
    FX_LOGS(ERROR) << "CreateImage called with no valid export token";
    ReportError();
    return;
  }

  if (!properties.has_width()) {
    FX_LOGS(ERROR) << "CreateImage failed, ImageProperties did not specify a width";
    ReportError();
    return;
  }

  if (!properties.has_height()) {
    FX_LOGS(ERROR) << "CreateImage failed, ImageProperties did not specify a height";
    ReportError();
    return;
  }

  allocation::ImageMetadata metadata;
  metadata.identifier = allocation::GenerateUniqueImageId();
  metadata.collection_id = global_collection_id;
  metadata.vmo_index = vmo_index;
  metadata.width = properties.width();
  metadata.height = properties.height();
  metadata.is_opaque = false;

  for (uint32_t i = 0; i < buffer_collection_importers_.size(); i++) {
    auto& importer = buffer_collection_importers_[i];

    // TODO(62240): Give more detailed errors.
    auto result = importer->ImportBufferImage(metadata);
    if (!result) {
      // If this importer fails, we need to release the image from
      // all of the importers that it passed on. Luckily we can do
      // this right here instead of waiting for a fence since we know
      // this image isn't being used by anything yet.
      for (uint32_t j = 0; j < i; j++) {
        buffer_collection_importers_[j]->ReleaseBufferImage(metadata.identifier);
      }

      FX_LOGS(ERROR) << "Importer could not import image.";
      ReportError();
      return;
    }
  }

  // Now that we've successfully been able to import the image into the importers,
  // we can now create a handle for it in the transform graph, and add the metadata
  // to our map.
  auto handle = transform_graph_.CreateTransform();
  content_handles_[image_id.value] = handle;
  image_metadatas_[handle] = metadata;
}

void Flatland::SetOpacity(TransformId transform_id, float val) {
  if (transform_id.value == kInvalidId) {
    FX_LOGS(ERROR) << "SetOpacity called with transform_id 0";
    ReportError();
    return;
  }

  if (val < 0.f || val > 1.f) {
    FX_LOGS(ERROR) << "Opacity value is not within valid range [0,1].";
    ReportError();
    return;
  }

  auto transform_kv = transforms_.find(transform_id.value);

  if (transform_kv == transforms_.end()) {
    FX_LOGS(ERROR) << "SetOpacity failed, transform_id " << transform_id.value << " not found";
    ReportError();
    return;
  }

  if (transform_graph_.HasChildren(transform_kv->second)) {
    FX_LOGS(ERROR) << "Cannot set the opacity value of a non-leaf node below 1.0";
    ReportError();
    return;
  }

  // Erase the value from the map since we store 1.f implicity.
  if (val == 1.f) {
    opacity_values_.erase(transform_kv->second);
  } else {
    opacity_values_[transform_kv->second] = val;
  }
}

void Flatland::SetContentOnTransform(TransformId transform_id, ContentId content_id) {
  if (transform_id.value == kInvalidId) {
    FX_LOGS(ERROR) << "SetContentOnTransform called with transform_id zero";
    ReportError();
    return;
  }

  auto transform_kv = transforms_.find(transform_id.value);

  if (transform_kv == transforms_.end()) {
    FX_LOGS(ERROR) << "SetContentOnTransform failed, transform_id " << transform_id.value
                   << " not found";
    ReportError();
    return;
  }

  if (content_id.value == kInvalidId) {
    transform_graph_.ClearPriorityChild(transform_kv->second);
    return;
  }

  auto handle_kv = content_handles_.find(content_id.value);

  if (handle_kv == content_handles_.end()) {
    FX_LOGS(ERROR) << "SetContentOnTransform failed, content_id " << content_id.value
                   << " not found";
    ReportError();
    return;
  }

  transform_graph_.SetPriorityChild(transform_kv->second, handle_kv->second);
}

void Flatland::SetLinkProperties(ContentId link_id, LinkProperties properties) {
  if (link_id.value == kInvalidId) {
    FX_LOGS(ERROR) << "SetLinkProperties called with link_id zero.";
    ReportError();
    return;
  }

  auto content_kv = content_handles_.find(link_id.value);

  if (content_kv == content_handles_.end()) {
    FX_LOGS(ERROR) << "SetLinkProperties failed, link_id " << link_id.value << " not found";
    ReportError();
    return;
  }

  auto link_kv = child_links_.find(content_kv->second);

  if (link_kv == child_links_.end()) {
    FX_LOGS(ERROR) << "SetLinkProperties failed, content_id " << link_id.value << " is not a Link";
    ReportError();
    return;
  }

  // Callers do not have to provide a new logical size on every call to SetLinkProperties, but if
  // they do, it must have positive X and Y values.
  if (properties.has_logical_size()) {
    auto logical_size = properties.logical_size();
    if (logical_size.x <= 0.f || logical_size.y <= 0.f) {
      FX_LOGS(ERROR) << "SetLinkProperties failed, logical_size components must be positive, "
                     << "given (" << logical_size.x << ", " << logical_size.y << ")";
      ReportError();
      return;
    }
  } else {
    // Preserve the old logical size if no logical size was passed as an argument. The
    // HangingGetHelper no-ops if no data changes, so if logical size is empty and no other
    // properties changed, the hanging get won't fire.
    properties.set_logical_size(link_kv->second.properties.logical_size());
  }

  FX_DCHECK(link_kv->second.link.importer.valid());

  link_kv->second.properties = std::move(properties);
  UpdateLinkScale(link_kv->second);
}

void Flatland::SetLinkSize(ContentId link_id, Vec2 size) {
  if (link_id.value == kInvalidId) {
    FX_LOGS(ERROR) << "SetLinkSize called with link_id zero";
    ReportError();
    return;
  }

  if (size.x <= 0.f || size.y <= 0.f) {
    FX_LOGS(ERROR) << "SetLinkSize failed, size components must be positive, given (" << size.x
                   << ", " << size.y << ")";
    ReportError();
    return;
  }

  auto content_kv = content_handles_.find(link_id.value);

  if (content_kv == content_handles_.end()) {
    FX_LOGS(ERROR) << "SetLinkSize failed, link_id " << link_id.value << " not found";
    ReportError();
    return;
  }

  auto link_kv = child_links_.find(content_kv->second);

  if (link_kv == child_links_.end()) {
    FX_LOGS(ERROR) << "SetLinkSize failed, content_id " << link_id.value << " is not a Link";
    ReportError();
    return;
  }

  FX_DCHECK(link_kv->second.link.importer.valid());

  link_kv->second.size = std::move(size);
  UpdateLinkScale(link_kv->second);
}

void Flatland::ReleaseTransform(TransformId transform_id) {
  if (transform_id.value == kInvalidId) {
    FX_LOGS(ERROR) << "ReleaseTransform called with transform_id zero";
    ReportError();
    return;
  }

  auto transform_kv = transforms_.find(transform_id.value);

  if (transform_kv == transforms_.end()) {
    FX_LOGS(ERROR) << "ReleaseTransform failed, transform_id " << transform_id.value
                   << " not found";
    ReportError();
    return;
  }

  bool erased_from_graph = transform_graph_.ReleaseTransform(transform_kv->second);
  FX_DCHECK(erased_from_graph);
  transforms_.erase(transform_kv);
}

void Flatland::ReleaseLink(ContentId link_id,
                           fuchsia::ui::scenic::internal::Flatland::ReleaseLinkCallback callback) {
  if (link_id.value == kInvalidId) {
    FX_LOGS(ERROR) << "ReleaseLink called with link_id zero";
    ReportError();
    return;
  }

  auto content_kv = content_handles_.find(link_id.value);

  if (content_kv == content_handles_.end()) {
    FX_LOGS(ERROR) << "ReleaseLink failed, link_id " << link_id.value << " not found";
    ReportError();
    return;
  }

  auto link_kv = child_links_.find(content_kv->second);

  if (link_kv == child_links_.end()) {
    FX_LOGS(ERROR) << "ReleaseLink failed, content_id " << link_id.value << " is not a Link";
    ReportError();
    return;
  }

  // Deleting the ChildLink's |graph_handle| effectively deletes the link from the local topology,
  // even if the link object itself is not deleted.
  bool child_removed = transform_graph_.RemoveChild(link_kv->second.link.graph_handle,
                                                    link_kv->second.link.link_handle);
  FX_DCHECK(child_removed);

  bool content_released = transform_graph_.ReleaseTransform(link_kv->second.link.graph_handle);
  FX_DCHECK(content_released);

  // Move the old child link into the delayed operation so that the ContentId is immeditely free
  // for re-use, but it doesn't get deleted until after the new UberStruct is published.
  auto child_link = std::move(link_kv->second);
  child_links_.erase(content_kv->second);
  content_handles_.erase(content_kv);

  // Delay the actual destruction of the link until the next Present().
  pending_link_operations_.push_back(
      [child_link = std::move(child_link), callback = std::move(callback)]() mutable {
        ContentLinkToken return_token;

        // If the link is still valid, return the original token. If not, create an orphaned
        // zx::eventpair and return it since the ObjectLinker does not retain the orphaned token.
        auto link_token = child_link.link.importer.ReleaseToken();
        if (link_token.has_value()) {
          return_token.value = zx::eventpair(std::move(link_token.value()));
        } else {
          // |peer_token| immediately falls out of scope, orphaning |return_token|.
          zx::eventpair peer_token;
          zx::eventpair::create(0, &return_token.value, &peer_token);
        }

        callback(std::move(return_token));
      });
}

void Flatland::ReleaseImage(ContentId image_id) {
  if (image_id.value == kInvalidId) {
    FX_LOGS(ERROR) << "ReleaseImage called with image_id zero";
    ReportError();
    return;
  }

  auto content_kv = content_handles_.find(image_id.value);

  if (content_kv == content_handles_.end()) {
    FX_LOGS(ERROR) << "ReleaseImage failed, image_id " << image_id.value << " not found";
    ReportError();
    return;
  }

  auto image_kv = image_metadatas_.find(content_kv->second);

  if (image_kv == image_metadatas_.end()) {
    FX_LOGS(ERROR) << "ReleaseImage failed, content_id " << image_id.value << " is not an Image";
    ReportError();
    return;
  }

  bool erased_from_graph = transform_graph_.ReleaseTransform(content_kv->second);
  FX_DCHECK(erased_from_graph);

  // Even though the handle is released, it may still be referenced by client Transforms. The
  // image_metadatas_ map preserves the entry until it shows up in the dead_transforms list.
  content_handles_.erase(image_id.value);
}

void Flatland::OnPresentProcessed(uint32_t num_present_tokens,
                                  FuturePresentationInfos presentation_infos) {
  present_tokens_remaining_ += num_present_tokens;
  if (binding_.is_bound()) {
    binding_.events().OnPresentProcessed(num_present_tokens, std::move(presentation_infos));
  }
}

void Flatland::OnFramePresented(const std::map<scheduling::PresentId, zx::time>& latched_times,
                                scheduling::PresentTimestamps present_times) {
  present2_helper_.OnPresented(latched_times, present_times, /*num_presents_allowed=*/0);
}

TransformHandle Flatland::GetRoot() const {
  return parent_link_ ? parent_link_->link_origin : local_root_;
}

std::optional<TransformHandle> Flatland::GetContentHandle(ContentId content_id) const {
  auto handle_kv = content_handles_.find(content_id.value);
  if (handle_kv == content_handles_.end()) {
    return std::nullopt;
  }
  return handle_kv->second;
}

void Flatland::ReportError() { failure_since_previous_present_ = true; }

void Flatland::CloseConnection() {
  // Cancel the async::Wait before closing the connection, or it will assert on destruction.
  zx_status_t status = peer_closed_waiter_.Cancel();

  // Immediately close the FIDL interface to prevent future requests.
  binding_.Close(ZX_ERR_BAD_STATE);

  // Finally, trigger the destruction of this instance.
  destroy_instance_function_();
}

void Flatland::UpdateLinkScale(const ChildLinkData& link_data) {
  FX_DCHECK(link_data.properties.has_logical_size());

  auto logical_size = link_data.properties.logical_size();
  matrices_[link_data.link.graph_handle].SetScale(
      {link_data.size.x / logical_size.x, link_data.size.y / logical_size.y});
}

// MatrixData function implementations

// static
float Flatland::MatrixData::GetOrientationAngle(
    fuchsia::ui::scenic::internal::Orientation orientation) {
  switch (orientation) {
    case Orientation::CCW_0_DEGREES:
      return 0.f;
    case Orientation::CCW_90_DEGREES:
      return glm::half_pi<float>();
    case Orientation::CCW_180_DEGREES:
      return glm::pi<float>();
    case Orientation::CCW_270_DEGREES:
      return glm::three_over_two_pi<float>();
  }
}

void Flatland::MatrixData::SetTranslation(fuchsia::ui::scenic::internal::Vec2 translation) {
  translation_.x = translation.x;
  translation_.y = translation.y;

  RecomputeMatrix();
}

void Flatland::MatrixData::SetOrientation(fuchsia::ui::scenic::internal::Orientation orientation) {
  angle_ = GetOrientationAngle(orientation);

  RecomputeMatrix();
}

void Flatland::MatrixData::SetScale(fuchsia::ui::scenic::internal::Vec2 scale) {
  scale_.x = scale.x;
  scale_.y = scale.y;

  RecomputeMatrix();
}

void Flatland::MatrixData::RecomputeMatrix() {
  // Manually compose the matrix rather than use glm transformations since the order of operations
  // is always the same. glm matrices are column-major.
  float* vals = static_cast<float*>(glm::value_ptr(matrix_));

  // Translation in the third column.
  vals[6] = translation_.x;
  vals[7] = translation_.y;

  // Rotation and scale combined into the first two columns.
  const float s = sin(angle_);
  const float c = cos(angle_);

  vals[0] = c * scale_.x;
  vals[1] = s * scale_.x;
  vals[3] = -1.f * s * scale_.y;
  vals[4] = c * scale_.y;
}

glm::mat3 Flatland::MatrixData::GetMatrix() const { return matrix_; }

}  // namespace flatland
