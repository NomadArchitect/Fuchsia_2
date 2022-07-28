// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/bin/driver_manager/v2/driver_runner.h"

#include <fidl/fuchsia.driver.development/cpp/wire.h>
#include <fidl/fuchsia.driver.host/cpp/wire.h>
#include <fidl/fuchsia.driver.index/cpp/wire.h>
#include <fidl/fuchsia.process/cpp/wire.h>
#include <lib/async/cpp/task.h>
#include <lib/driver2/start_args.h>
#include <lib/fdio/directory.h>
#include <lib/fidl/llcpp/server.h>
#include <lib/fidl/llcpp/wire_messaging.h>
#include <lib/fit/defer.h>
#include <lib/service/llcpp/service.h>
#include <zircon/errors.h>
#include <zircon/rights.h>
#include <zircon/status.h>

#include <forward_list>
#include <queue>
#include <stack>
#include <unordered_set>

#include "src/devices/lib/log/log.h"
#include "src/lib/fxl/strings/join_strings.h"
#include "src/lib/storage/vfs/cpp/service.h"

namespace fdf = fuchsia_driver_framework;
namespace fdh = fuchsia_driver_host;
namespace fdi = fuchsia_driver_index;
namespace fio = fuchsia_io;
namespace fprocess = fuchsia_process;
namespace frunner = fuchsia_component_runner;
namespace fcomponent = fuchsia_component;
namespace fdecl = fuchsia_component_decl;

using InspectStack = std::stack<std::pair<inspect::Node*, const dfv2::Node*>>;

namespace dfv2 {

namespace {

constexpr uint32_t kTokenId = PA_HND(PA_USER0, 0);
constexpr auto kBootScheme = "fuchsia-boot://";

template <typename R, typename F>
std::optional<R> VisitOffer(fdecl::wire::Offer& offer, F apply) {
  // Note, we access each field of the union as mutable, so that `apply` can
  // modify the field if necessary.
  switch (offer.Which()) {
    case fdecl::wire::Offer::Tag::kService:
      return apply(offer.service());
    case fdecl::wire::Offer::Tag::kProtocol:
      return apply(offer.protocol());
    case fdecl::wire::Offer::Tag::kDirectory:
      return apply(offer.directory());
    case fdecl::wire::Offer::Tag::kStorage:
      return apply(offer.storage());
    case fdecl::wire::Offer::Tag::kRunner:
      return apply(offer.runner());
    case fdecl::wire::Offer::Tag::kResolver:
      return apply(offer.resolver());
    case fdecl::wire::Offer::Tag::kEvent:
      return apply(offer.event());
    case fdecl::wire::Offer::Tag::kEventStream:
      return apply(offer.event_stream());
    case fdecl::wire::Offer::Tag::kUnknown:
      return {};
  }
}

void InspectNode(inspect::Inspector& inspector, InspectStack& stack) {
  const auto inspect_decl = [](auto& decl) -> std::string_view {
    if (decl.has_target_name()) {
      return decl.target_name().get();
    }
    if (decl.has_source_name()) {
      return decl.source_name().get();
    }
    return "<missing>";
  };

  std::forward_list<inspect::Node> roots;
  std::unordered_set<const Node*> unique_nodes;
  while (!stack.empty()) {
    // Pop the current root and node to operate on.
    auto [root, node] = stack.top();
    stack.pop();

    auto [_, inserted] = unique_nodes.insert(node);
    if (!inserted) {
      // Only insert unique nodes from the DAG.
      continue;
    }

    // Populate root with data from node.
    if (auto offers = node->offers(); !offers.empty()) {
      std::vector<std::string_view> strings;
      for (auto& offer : offers) {
        auto string = VisitOffer<std::string_view>(offer, inspect_decl);
        strings.push_back(string.value_or("unknown"));
      }
      root->CreateString("offers", fxl::JoinStrings(strings, ", "), &inspector);
    }
    if (auto symbols = node->symbols(); !symbols.empty()) {
      std::vector<std::string_view> strings;
      for (auto& symbol : symbols) {
        strings.push_back(symbol.name().get());
      }
      root->CreateString("symbols", fxl::JoinStrings(strings, ", "), &inspector);
    }
    std::string driver_string = "unbound";
    if (node->driver_component()) {
      driver_string = std::string(node->driver_component()->url());
    }
    root->CreateString("driver", driver_string, &inspector);

    // Push children of this node onto the stack. We do this in reverse order to
    // ensure the children are handled in order, from first to last.
    auto& children = node->children();
    for (auto child = children.rbegin(), end = children.rend(); child != end; ++child) {
      auto& name = (*child)->name();
      auto& root_for_child = roots.emplace_front(root->CreateChild(name));
      stack.emplace(&root_for_child, child->get());
    }
  }

  // Store all of the roots in the inspector.
  for (auto& root : roots) {
    inspector.emplace(std::move(root));
  }
}

fidl::StringView CollectionName(Collection collection) {
  switch (collection) {
    case Collection::kNone:
      return {};
    case Collection::kHost:
      return "driver-hosts";
    case Collection::kBoot:
      return "boot-drivers";
    case Collection::kPackage:
      return "pkg-drivers";
    case Collection::kUniversePackage:
      return "universe-pkg-drivers";
  }
}

}  // namespace

DriverRunner::DriverRunner(fidl::ClientEnd<fcomponent::Realm> realm,
                           fidl::ClientEnd<fdi::DriverIndex> driver_index,
                           inspect::Inspector& inspector, async_dispatcher_t* dispatcher)
    : realm_(std::move(realm), dispatcher),
      driver_index_(std::move(driver_index), dispatcher),
      dispatcher_(dispatcher),
      root_node_(std::make_shared<Node>("root", std::vector<Node*>{}, this, dispatcher)),
      composite_device_manager_(this, dispatcher,
                                [this]() { this->TryBindAllOrphansUntracked(); }) {
  inspector.GetRoot().CreateLazyNode(
      "driver_runner", [this] { return Inspect(); }, &inspector);
}

fpromise::promise<inspect::Inspector> DriverRunner::Inspect() const {
  inspect::Inspector inspector;

  // Make the device tree inspect nodes.
  auto device_tree = inspector.GetRoot().CreateChild("node_topology");
  auto root = device_tree.CreateChild(root_node_->name());
  InspectStack stack{{std::make_pair(&root, root_node_.get())}};
  InspectNode(inspector, stack);
  inspector.emplace(std::move(root));
  inspector.emplace(std::move(device_tree));

  // Make the unbound composite devices inspect nodes.
  auto composite = inspector.GetRoot().CreateChild("unbound_composites");
  for (auto& args : composite_args_) {
    auto child = composite.CreateChild(args.first);
    for (size_t i = 0; i < args.second.size(); i++) {
      auto& node = args.second[i];
      if (auto real = node.lock()) {
        child.CreateString(std::string("parent-").append(std::to_string(i)), real->TopoName(),
                           &inspector);
      } else {
        child.CreateString(std::string("parent-").append(std::to_string(i)), "<empty>", &inspector);
      }
    }
    inspector.emplace(std::move(child));
  }
  inspector.emplace(std::move(composite));

  // Make the orphaned devices inspect nodes.
  auto orphans = inspector.GetRoot().CreateChild("orphan_nodes");
  for (size_t i = 0; i < orphaned_nodes_.size(); i++) {
    if (auto node = orphaned_nodes_[i].lock()) {
      orphans.CreateString(std::to_string(i), node->TopoName(), &inspector);
    }
  }
  inspector.emplace(std::move(orphans));

  auto dfv1_composites = inspector.GetRoot().CreateChild("dfv1_composites");
  composite_device_manager_.Inspect(inspector, dfv1_composites);
  inspector.emplace(std::move(dfv1_composites));

  return fpromise::make_ok_promise(inspector);
}

size_t DriverRunner::NumOrphanedNodes() const { return orphaned_nodes_.size(); }

zx::status<> DriverRunner::PublishComponentRunner(const fbl::RefPtr<fs::PseudoDir>& svc_dir) {
  const auto service = [this](fidl::ServerEnd<frunner::ComponentRunner> request) {
    fidl::BindServer<fidl::WireServer<frunner::ComponentRunner>>(dispatcher_, std::move(request),
                                                                 this);
    return ZX_OK;
  };
  zx_status_t status = svc_dir->AddEntry(fidl::DiscoverableProtocolName<frunner::ComponentRunner>,
                                         fbl::MakeRefCounted<fs::Service>(service));
  if (status != ZX_OK) {
    LOGF(ERROR, "Failed to add directory entry '%s': %s",
         fidl::DiscoverableProtocolName<frunner::ComponentRunner>, zx_status_get_string(status));
  }

  auto composite_publish = composite_device_manager_.Publish(svc_dir);
  if (composite_publish.is_error()) {
    return composite_publish.take_error();
  }

  return zx::ok();
}

zx::status<> DriverRunner::StartRootDriver(std::string_view url) {
  return StartDriver(*root_node_, url, fdi::DriverPackageType::kBase);
}

std::shared_ptr<const Node> DriverRunner::root_node() const { return root_node_; }

void DriverRunner::ScheduleBaseDriversBinding() {
  driver_index_->WaitForBaseDrivers().Then(
      [this](fidl::WireUnownedResult<fdi::DriverIndex::WaitForBaseDrivers>& result) mutable {
        if (!result.ok()) {
          // It's possible in tests that the test can finish before WaitForBaseDrivers
          // finishes.
          if (result.status() == ZX_ERR_PEER_CLOSED) {
            LOGF(WARNING, "Connection to DriverIndex closed during WaitForBaseDrivers.");
          } else {
            LOGF(ERROR, "DriverIndex::WaitForBaseDrivers failed with: %s",
                 result.error().FormatDescription().c_str());
          }
          return;
        }

        TryBindAllOrphansUntracked();
      });
}

void DriverRunner::TryBindAllOrphans(NodeBindingInfoResultCallback result_callback) {
  // Clear our stored vector of orphaned nodes, we will repopulate it with the
  // new orphans.
  std::vector<std::weak_ptr<Node>> orphaned_nodes = std::move(orphaned_nodes_);
  orphaned_nodes_ = {};

  std::shared_ptr<BindResultTracker> tracker =
      std::make_shared<BindResultTracker>(orphaned_nodes.size(), std::move(result_callback));

  for (auto& weak_node : orphaned_nodes) {
    auto node = weak_node.lock();
    if (!node) {
      tracker->ReportNoBind();
      continue;
    }

    Bind(*node, tracker);
  }
}

void DriverRunner::TryBindAllOrphansUntracked() {
  NodeBindingInfoResultCallback empty_callback =
      [](fidl::VectorView<fuchsia_driver_development::wire::NodeBindingInfo>) {};
  TryBindAllOrphans(std::move(empty_callback));
}

zx::status<> DriverRunner::StartDriver(Node& node, std::string_view url,
                                       fdi::DriverPackageType package_type) {
  zx::event token;
  zx_status_t status = zx::event::create(0, &token);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  zx_info_handle_basic_t info{};
  status = token.get_info(ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr, nullptr);
  if (status != ZX_OK) {
    return zx::error(status);
  }

  // TODO(fxb/98474) Stop doing the url prefix check and just rely on the package_type.
  auto collection = cpp20::starts_with(url, kBootScheme) ? Collection::kBoot : Collection::kPackage;
  if (package_type == fdi::DriverPackageType::kUniverse) {
    collection = Collection::kUniversePackage;
  }
  node.set_collection(collection);
  auto create = CreateComponent(node.TopoName(), collection, std::string(url),
                                {.node = &node, .token = std::move(token)});
  if (create.is_error()) {
    return create.take_error();
  }
  driver_args_.emplace(info.koid, node);
  return zx::ok();
}

void DriverRunner::Start(StartRequestView request, StartCompleter::Sync& completer) {
  auto url = request->start_info.resolved_url().get();

  // When we start a driver, we associate an unforgeable token (the KOID of a
  // zx::event) with the start request, through the use of the numbered_handles
  // field. We do this so:
  //  1. We can securely validate the origin of the request
  //  2. We avoid collisions that can occur when relying on the package URL
  //  3. We avoid relying on the resolved URL matching the package URL
  if (!request->start_info.has_numbered_handles()) {
    LOGF(ERROR, "Failed to start driver '%.*s', invalid request for driver",
         static_cast<int>(url.size()), url.data());
    completer.Close(ZX_ERR_INVALID_ARGS);
    return;
  }
  auto& handles = request->start_info.numbered_handles();
  if (handles.count() != 1 || !handles[0].handle || handles[0].id != kTokenId) {
    LOGF(ERROR, "Failed to start driver '%.*s', invalid request for driver",
         static_cast<int>(url.size()), url.data());
    completer.Close(ZX_ERR_INVALID_ARGS);
    return;
  }
  zx_info_handle_basic_t info{};
  zx_status_t status =
      handles[0].handle.get_info(ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr, nullptr);
  if (status != ZX_OK) {
    completer.Close(ZX_ERR_INVALID_ARGS);
    return;
  }
  auto it = driver_args_.find(info.koid);
  if (it == driver_args_.end()) {
    LOGF(ERROR, "Failed to start driver '%.*s', unknown request for driver",
         static_cast<int>(url.size()), url.data());
    completer.Close(ZX_ERR_UNAVAILABLE);
    return;
  }
  auto& [_, node] = *it;
  driver_args_.erase(it);

  // Launch a driver host, or use an existing driver host.
  if (driver::ProgramValue(request->start_info.program(), "colocate").value_or("") == "true") {
    if (&node == root_node_.get()) {
      LOGF(ERROR, "Failed to start driver '%.*s', root driver cannot colocate",
           static_cast<int>(url.size()), url.data());
      completer.Close(ZX_ERR_INVALID_ARGS);
      return;
    }
  } else {
    auto result = StartDriverHost();
    if (result.is_error()) {
      completer.Close(result.error_value());
      return;
    }
    node.set_driver_host(result.value().get());
    driver_hosts_.push_back(std::move(*result));
  }

  // Bind the Node associated with the driver.
  auto endpoints = fidl::CreateEndpoints<fdf::Node>();
  if (endpoints.is_error()) {
    completer.Close(endpoints.error_value());
    return;
  }
  auto bind_node = fidl::BindServer<fidl::WireServer<fdf::Node>>(
      dispatcher_, std::move(endpoints->server), node.shared_from_this(),
      [](fidl::WireServer<fdf::Node>* node, auto, auto) { static_cast<Node*>(node)->Remove(); });
  node.set_node_ref(bind_node);

  LOGF(INFO, "Binding %.*s to  %s", static_cast<int>(url.size()), url.data(), node.name().c_str());
  // Start the driver within the driver host.
  auto start = node.driver_host()->Start(std::move(endpoints->client), node.symbols(),
                                         std::move(request->start_info));
  if (start.is_error()) {
    completer.Close(start.error_value());
    return;
  }

  // Create a DriverComponent to manage the driver.
  auto driver = std::make_unique<DriverComponent>(
      std::move(*start), std::move(request->controller), dispatcher_, url,
      [node = &node](auto status) { node->Remove(); },
      [node = &node](auto status) { node->Remove(); });
  node.set_driver_component(std::move(driver));
}

void DriverRunner::Bind(Node& node, std::shared_ptr<BindResultTracker> result_tracker) {
  // Check the DFv1 composites first, and don't bind to others if they match.
  if (composite_device_manager_.BindNode(node.shared_from_this())) {
    return;
  }

  auto match_callback = [this, weak_node = node.weak_from_this(), result_tracker](
                            fidl::WireUnownedResult<fdi::DriverIndex::MatchDriver>& result) {
    auto shared_node = weak_node.lock();

    auto report_no_bind = fit::defer([&result_tracker]() {
      if (result_tracker) {
        result_tracker->ReportNoBind();
      }
    });

    if (!shared_node) {
      LOGF(WARNING, "Node was freed before it could be bound");
      return;
    }

    Node& node = *shared_node;
    auto driver_node = &node;
    auto orphaned = [this, &driver_node] {
      orphaned_nodes_.push_back(driver_node->weak_from_this());
    };

    if (!result.ok()) {
      orphaned();
      LOGF(ERROR, "Failed to call match Node '%s': %s", node.name().data(),
           result.error().FormatDescription().data());
      return;
    }

    if (result->is_error()) {
      orphaned();
      // Log the failed MatchDriver only if we are not tracking the results with a tracker
      // or if the error is not a ZX_ERR_NOT_FOUND error (meaning it could not find a driver).
      // When we have a tracker, the bind is happening for all the orphan nodes and the
      // not found errors get very noisy.
      zx_status_t match_error = result->error_value();
      if (!result_tracker || match_error != ZX_ERR_NOT_FOUND) {
        LOGF(WARNING, "Failed to match Node '%s': %s", driver_node->name().data(),
             zx_status_get_string(match_error));
      }

      return;
    }

    auto& matched_driver = result->value()->driver;
    if (!matched_driver.is_driver() && !matched_driver.is_composite_driver()) {
      orphaned();
      LOGF(WARNING,
           "Failed to match Node '%s', the MatchedDriver is not a normal or composite"
           "driver.",
           driver_node->name().data());
      return;
    }

    if (matched_driver.is_composite_driver() &&
        !matched_driver.composite_driver().has_driver_info()) {
      orphaned();
      LOGF(WARNING,
           "Failed to match Node '%s', the MatchedDriver is missing driver info for a composite "
           "driver.",
           driver_node->name().data());
      return;
    }

    auto driver_info = matched_driver.is_driver() ? matched_driver.driver()
                                                  : matched_driver.composite_driver().driver_info();

    if (!driver_info.has_url()) {
      orphaned();
      LOGF(ERROR, "Failed to match Node '%s', the driver URL is missing",
           driver_node->name().data());
      return;
    }

    // This is a composite driver, create a composite node for it.
    if (matched_driver.is_composite_driver()) {
      auto composite = CreateCompositeNode(node, matched_driver.composite_driver());

      // Orphaned nodes are handled by CreateCompositeNode().
      if (composite.is_error()) {
        return;
      }
      driver_node = *composite;
    }

    auto pkg_type =
        driver_info.has_package_type() ? driver_info.package_type() : fdi::DriverPackageType::kBase;
    auto start_result = StartDriver(*driver_node, driver_info.url().get(), pkg_type);
    if (start_result.is_error()) {
      orphaned();
      LOGF(ERROR, "Failed to start driver '%s': %s", driver_node->name().data(),
           zx_status_get_string(start_result.error_value()));
      return;
    }

    node.OnBind();
    report_no_bind.cancel();
    if (result_tracker) {
      result_tracker->ReportSuccessfulBind(node.TopoName(), driver_info.url().get());
    }
  };
  fidl::Arena<> arena;
  driver_index_->MatchDriver(node.CreateAddArgs(arena)).Then(std::move(match_callback));
}

zx::status<Node*> DriverRunner::CreateCompositeNode(
    Node& node, const fdi::wire::MatchedCompositeInfo& matched_driver) {
  auto it = AddToCompositeArgs(node.name(), matched_driver);
  if (it.is_error()) {
    orphaned_nodes_.push_back(node.weak_from_this());
    return it.take_error();
  }
  auto& [_, nodes] = **it;

  std::vector<Node*> parents;
  // Store the node arguments inside the composite arguments.
  nodes[matched_driver.node_index()] = node.weak_from_this();
  // Check if we have all the nodes for the composite driver.
  for (auto& node : nodes) {
    if (auto parent = node.lock()) {
      parents.push_back(parent.get());
    } else {
      // We are missing a node or it has been removed, continue to wait.
      return zx::error(ZX_ERR_NEXT);
    }
  }
  composite_args_.erase(*it);

  // We have all the nodes, create a composite node for the composite driver.
  std::vector<std::string> parents_names;
  for (auto name : matched_driver.node_names()) {
    parents_names.emplace_back(name.data(), name.size());
  }
  auto composite = Node::CreateCompositeNode("composite", std::move(parents),
                                             std::move(parents_names), {}, this, dispatcher_);
  if (composite.is_error()) {
    return composite.take_error();
  }

  // We can return a pointer, as the composite node is owned by its parents.
  return zx::ok(composite.value().get());
}

zx::status<DriverRunner::CompositeArgsIterator> DriverRunner::AddToCompositeArgs(
    const std::string& name, const fdi::wire::MatchedCompositeInfo& composite_info) {
  if (!composite_info.has_node_index() || !composite_info.has_num_nodes()) {
    LOGF(ERROR, "Failed to match Node '%s', missing fields for composite driver", name.data());
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  if (composite_info.node_index() >= composite_info.num_nodes()) {
    LOGF(ERROR, "Failed to match Node '%s', the node index is out of range", name.data());
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  if (!composite_info.has_driver_info() || !composite_info.driver_info().has_url()) {
    LOGF(ERROR, "Failed to match Node '%s', missing driver info fields for composite driver",
         name.data());
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  auto url = std::string(composite_info.driver_info().url().get());

  // Check if there are existing composite arguments for the composite driver.
  // We do this by checking if the node index within an existing set of
  // composite arguments has not been set, or has become available.
  auto [it, end] = composite_args_.equal_range(url);
  for (; it != end; ++it) {
    auto& [_, nodes] = *it;
    if (nodes.size() != composite_info.num_nodes()) {
      LOGF(ERROR, "Failed to match Node '%s', the number of nodes does not match", name.data());
      return zx::error(ZX_ERR_INVALID_ARGS);
    }
    if (nodes[composite_info.node_index()].expired()) {
      break;
    }
  }
  // No composite arguments exist for the composite driver, create a new set.
  if (it == end) {
    it = composite_args_.emplace(std::move(url), CompositeArgs{composite_info.num_nodes()});
  }
  return zx::ok(it);
}

zx::status<std::unique_ptr<DriverHostComponent>> DriverRunner::StartDriverHost() {
  zx::status endpoints = fidl::CreateEndpoints<fio::Directory>();
  if (endpoints.is_error()) {
    return endpoints.take_error();
  }
  auto name = "driver-host-" + std::to_string(next_driver_host_id_++);
  auto create = CreateComponent(name, Collection::kHost, "#meta/driver_host2.cm",
                                {.exposed_dir = std::move(endpoints->server)});
  if (create.is_error()) {
    return create.take_error();
  }

  auto client_end = service::ConnectAt<fdh::DriverHost>(endpoints->client);
  if (client_end.is_error()) {
    LOGF(ERROR, "Failed to connect to service '%s': %s",
         fidl::DiscoverableProtocolName<fdh::DriverHost>, client_end.status_string());
    return client_end.take_error();
  }

  auto driver_host =
      std::make_unique<DriverHostComponent>(std::move(*client_end), dispatcher_, &driver_hosts_);
  return zx::ok(std::move(driver_host));
}

zx::status<> DriverRunner::CreateComponent(std::string name, Collection collection, std::string url,
                                           CreateComponentOpts opts) {
  fidl::Arena arena;
  fdecl::wire::Child child_decl(arena);
  child_decl.set_name(arena, fidl::StringView::FromExternal(name))
      .set_url(arena, fidl::StringView::FromExternal(url))
      .set_startup(fdecl::wire::StartupMode::kLazy);
  fcomponent::wire::CreateChildArgs child_args(arena);
  if (opts.node != nullptr) {
    child_args.set_dynamic_offers(arena, opts.node->offers());
  }
  fprocess::wire::HandleInfo handle_info;
  if (opts.token) {
    handle_info = {
        .handle = std::move(opts.token),
        .id = kTokenId,
    };
    child_args.set_numbered_handles(
        arena, fidl::VectorView<fprocess::wire::HandleInfo>::FromExternal(&handle_info, 1));
  }
  auto open_callback = [name,
                        url](fidl::WireUnownedResult<fcomponent::Realm::OpenExposedDir>& result) {
    if (!result.ok()) {
      LOGF(ERROR, "Failed to open exposed directory for component '%s' (%s): %s", name.data(),
           url.data(), result.FormatDescription().data());
      return;
    }
    if (result->is_error()) {
      LOGF(ERROR, "Failed to open exposed directory for component '%s' (%s): %u", name.data(),
           url.data(), result->error_value());
    }
  };
  auto create_callback =
      [this, name, url, collection, exposed_dir = std::move(opts.exposed_dir),
       open_callback = std::move(open_callback)](
          fidl::WireUnownedResult<fcomponent::Realm::CreateChild>& result) mutable {
        if (!result.ok()) {
          LOGF(ERROR, "Failed to create component '%s' (%s): %s", name.data(), url.data(),
               result.error().FormatDescription().data());
          return;
        }
        if (result->is_error()) {
          LOGF(ERROR, "Failed to create component '%s' (%s): %u", name.data(), url.data(),
               result->error_value());
          return;
        }
        if (exposed_dir) {
          fdecl::wire::ChildRef child_ref{
              .name = fidl::StringView::FromExternal(name),
              .collection = CollectionName(collection),
          };
          realm_->OpenExposedDir(child_ref, std::move(exposed_dir))
              .ThenExactlyOnce(std::move(open_callback));
        }
      };
  realm_
      ->CreateChild(fdecl::wire::CollectionRef{.name = CollectionName(collection)}, child_decl,
                    child_args)
      .Then(std::move(create_callback));
  return zx::ok();
}

}  // namespace dfv2
