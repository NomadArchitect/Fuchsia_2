// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BIN_DRIVER_MANAGER_V2_NODE_H_
#define SRC_DEVICES_BIN_DRIVER_MANAGER_V2_NODE_H_

#include <fidl/fuchsia.driver.development/cpp/wire.h>
#include <fidl/fuchsia.driver.framework/cpp/wire.h>
#include <lib/zircon-internal/thread_annotations.h>

#include <list>

#include "src/devices/bin/driver_manager/v2/driver_component.h"
#include "src/devices/bin/driver_manager/v2/driver_host.h"

namespace dfv2 {

// This function creates a composite offer based on a 'directory service' offer.
std::optional<fuchsia_component_decl::wire::Offer> CreateCompositeDirOffer(
    fidl::AnyArena& arena, fuchsia_component_decl::wire::Offer& offer,
    std::string_view parents_name);

// This function creates a composite offer based on a service offer.
std::optional<fuchsia_component_decl::wire::Offer> CreateCompositeServiceOffer(
    fidl::AnyArena& arena, fuchsia_component_decl::wire::Offer& offer,
    std::string_view parents_name, bool primary_parent);

// TODO(fxbug.dev/66150): Once FIDL wire types support a Clone() method,
// stop encoding and decoding messages as a workaround.
template <typename T>
class OwnedMessage {
 public:
  static std::unique_ptr<OwnedMessage<T>> From(T& message) {
    // TODO(fxbug.dev/45252): Use FIDL at rest.
    fidl::unstable::OwnedEncodedMessage<T> encoded(fidl::internal::WireFormatVersion::kV2,
                                                   &message);
    ZX_ASSERT_MSG(encoded.ok(), "Failed to encode: %s", encoded.FormatDescription().data());
    return std::make_unique<OwnedMessage>(encoded);
  }

  T& get() { return *decoded_.PrimaryObject(); }

 private:
  friend std::unique_ptr<OwnedMessage<T>> std::make_unique<OwnedMessage<T>>(
      fidl::unstable::OwnedEncodedMessage<T>&);

  // TODO(fxbug.dev/45252): Use FIDL at rest.
  explicit OwnedMessage(fidl::unstable::OwnedEncodedMessage<T>& encoded)
      : converted_(encoded.GetOutgoingMessage()),
        decoded_(fidl::internal::WireFormatVersion::kV2, std::move(converted_.incoming_message())) {
    ZX_ASSERT_MSG(decoded_.ok(), "Failed to decode: %s", decoded_.FormatDescription().c_str());
  }

  fidl::OutgoingToIncomingMessage converted_;
  fidl::unstable::DecodedMessage<T> decoded_;
};

class Node;

using NodeBindingInfoResultCallback =
    fit::callback<void(fidl::VectorView<fuchsia_driver_development::wire::NodeBindingInfo>)>;

class BindResultTracker {
 public:
  explicit BindResultTracker(size_t expected_result_count,
                             NodeBindingInfoResultCallback result_callback);

  void ReportSuccessfulBind(const std::string_view& node_name, const std::string_view& driver);
  void ReportNoBind();

 private:
  void Complete(size_t current);
  fidl::Arena<> arena_;
  size_t expected_result_count_;
  size_t currently_reported_ TA_GUARDED(lock_);
  std::mutex lock_;
  NodeBindingInfoResultCallback result_callback_;
  std::vector<fuchsia_driver_development::wire::NodeBindingInfo> results_;
};

class DriverBinder {
 public:
  virtual ~DriverBinder() = default;

  // Attempt to bind `node`.
  // A nullptr for result_tracker is acceptable if the caller doesn't intend to
  // track the results.
  virtual void Bind(Node& node, std::shared_ptr<BindResultTracker> result_tracker) = 0;
};

enum class Collection {
  kNone,
  // Collection for driver hosts.
  kHost,
  // Collection for boot drivers.
  kBoot,
  // Collection for package drivers.
  kPackage,
  // Collection for universe package drivers.
  kUniversePackage,
};

class Node : public fidl::WireServer<fuchsia_driver_framework::NodeController>,
             public fidl::WireServer<fuchsia_driver_framework::Node>,
             public std::enable_shared_from_this<Node> {
 public:
  using OwnedOffer = std::unique_ptr<OwnedMessage<fuchsia_component_decl::wire::Offer>>;

  Node(std::string_view name, std::vector<Node*> parents, DriverBinder* driver_binder,
       async_dispatcher_t* dispatcher);
  ~Node() override;

  static zx::status<std::shared_ptr<Node>> CreateCompositeNode(
      std::string_view node_name, std::vector<Node*> parents,
      std::vector<std::string> parents_names,
      std::vector<fuchsia_driver_framework::wire::NodeProperty> properties,
      DriverBinder* driver_binder, async_dispatcher_t* dispatcher);

  fidl::VectorView<fuchsia_component_decl::wire::Offer> CreateOffers(fidl::AnyArena& arena) const;

  fuchsia_driver_framework::wire::NodeAddArgs CreateAddArgs(fidl::AnyArena& arena);

  void OnBind() const;

  // Begin the removal process for a Node. This function ensures that a Node is
  // only removed after all of its children are removed. It also ensures that
  // a Node is only removed after the driver that is bound to it has been stopped.
  // This is safe to call multiple times.
  // There are lots of reasons a Node's removal will be started:
  //   - The Node's driver component wants to exit.
  //   - The `node_ref` server has become unbound.
  //   - The Node's parent is being removed.
  void Remove();

  bool IsComposite() const;

  const std::string& name() const;
  const DriverComponent* driver_component() const;
  const std::vector<Node*>& parents() const;
  const std::list<std::shared_ptr<Node>>& children() const;
  std::vector<OwnedOffer>& offers() const;
  fidl::VectorView<fuchsia_driver_framework::wire::NodeSymbol> symbols() const;
  const std::vector<fuchsia_driver_framework::wire::NodeProperty>& properties() const;
  DriverHostComponent* driver_host() const;

  void set_collection(Collection collection);
  void set_driver_host(DriverHostComponent* driver_host);
  void set_node_ref(fidl::ServerBindingRef<fuchsia_driver_framework::Node> node_ref);
  void set_bound_driver_url(std::optional<std::string_view> bound_driver_url);
  void set_controller_ref(
      fidl::ServerBindingRef<fuchsia_driver_framework::NodeController> controller_ref);
  void set_driver_component(std::unique_ptr<DriverComponent> driver_component);

  std::string TopoName() const;

 private:
  // fidl::WireServer<fuchsia_driver_framework::NodeController>
  void Remove(RemoveRequestView request, RemoveCompleter::Sync& completer) override;
  // fidl::WireServer<fuchsia_driver_framework::Node>
  void AddChild(AddChildRequestView request, AddChildCompleter::Sync& completer) override;

  // Add this Node to its parents. This should be called when the node is created.
  void AddToParents();

  std::string name_;
  // If this is a composite device, this stores the list of each parent's names.
  std::vector<std::string> parents_names_;
  std::vector<Node*> parents_;
  std::list<std::shared_ptr<Node>> children_;
  fit::nullable<DriverBinder*> driver_binder_;
  async_dispatcher_t* const dispatcher_;

  fidl::Arena<128> arena_;
  std::vector<OwnedOffer> offers_;
  std::vector<fuchsia_driver_framework::wire::NodeSymbol> symbols_;
  std::vector<fuchsia_driver_framework::wire::NodeProperty> properties_;

  Collection collection_ = Collection::kNone;
  fit::nullable<DriverHostComponent*> driver_host_;

  bool removal_in_progress_ = false;

  // If this exists, then this `driver_component_` is bound to this node.
  std::unique_ptr<DriverComponent> driver_component_;
  std::optional<std::string> bound_driver_url_;
  std::optional<fidl::ServerBindingRef<fuchsia_driver_framework::Node>> node_ref_;
  std::optional<fidl::ServerBindingRef<fuchsia_driver_framework::NodeController>> controller_ref_;
};

}  // namespace dfv2

#endif  // SRC_DEVICES_BIN_DRIVER_MANAGER_V2_NODE_H_
