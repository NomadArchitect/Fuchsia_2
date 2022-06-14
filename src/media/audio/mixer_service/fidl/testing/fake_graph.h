// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_AUDIO_MIXER_SERVICE_FIDL_TESTING_FAKE_NODE_H_
#define SRC_MEDIA_AUDIO_MIXER_SERVICE_FIDL_TESTING_FAKE_NODE_H_

#include <lib/syslog/cpp/macros.h>

#include <unordered_map>
#include <unordered_set>
#include <vector>

#include "src/media/audio/mixer_service/fidl/node.h"

namespace media_audio {

class FakeNode;
class FakeGraph;

using FakeNodePtr = std::shared_ptr<FakeNode>;

// A fake node for use in tests.
// See FakeGraph for creation methods.
//
// Not safe for concurrent use.
class FakeNode : public Node, public std::enable_shared_from_this<FakeNode> {
 public:
  // Register a handler for `CreateNewChildInput`.
  // If a handler is not registered, a default handler is used.
  void SetOnCreateNewChildInput(std::function<NodePtr()> handler) {
    on_create_new_child_input_ = std::move(handler);
  }

  // Register a handler for `CreateNewChildOutput`.
  // If a handler is not registered, a default handler is used.
  void SetOnCreateNewChildOutput(std::function<NodePtr()> handler) {
    on_create_new_child_output_ = std::move(handler);
  }

  // Register a handler for `CanAcceptInput`.
  // The default handler always returns true.
  void SetOnCreateNewChildInput(std::function<bool(NodePtr)> handler) {
    on_can_accept_input_ = std::move(handler);
  }

 protected:
  // Creates an ordinary child node to accept the next input edge.
  // Returns nullptr if no more child input nodes can be created.
  // REQUIRED: is_meta()
  NodePtr CreateNewChildInput() override;

  // Creates an ordinary child node to accept the next output edge.
  // Returns nullptr if no more child output nodes can be created.
  // REQUIRED: is_meta()
  NodePtr CreateNewChildOutput() override;

  // Reports whether this node can accept input from the given src node.
  // REQUIRED: !is_meta()
  bool CanAcceptInput(NodePtr src) const override;

 private:
  // All FakeNodes belong to a FakeGraph. The constructor is private to ensure that it's impossible
  // to create a FakeNode which outlives its parent FakeGraph.
  friend class FakeGraph;
  FakeNode(FakeGraph& graph, NodeId id, bool is_meta, FakeNodePtr parent);

  FakeGraph& graph_;
  std::optional<std::function<NodePtr()>> on_create_new_child_input_;
  std::optional<std::function<NodePtr()>> on_create_new_child_output_;
  std::optional<std::function<bool(NodePtr)>> on_can_accept_input_;
};

// This class makes it easy to create graphs of FakeNodes during tests. For example, the following
// code:
//
//   auto graph = FakeGraph::Create({
//       .meta_nodes = {
//           {1, {
//               .input_children = {2, 3},
//               .output_children = {4, 5},
//           }},
//       },
//       .edges = {
//           {0, 2},
//           {4, 6},
//           {5, 7},
//       },
//    });
//
// Creates a graph that looks like:
//
//     0
//     |
//   +-V-----+
//   | 2   3 |
//   |   1   |
//   | 4   5 |
//   +-|---|-+
//     V   V
//     6   7
//
// The destructor deletes all edges (to remove circular references) and drops all FakeNodes so the
// FakeNodes can be destructed once all external references are gone.
//
// Not safe for concurrent use.
class FakeGraph {
 public:
  struct MetaNodeArgs {
    std::unordered_set<NodeId> input_children;
    std::unordered_set<NodeId> output_children;
  };

  struct Edge {
    NodeId src;
    NodeId dest;
  };

  struct Args {
    // Meta nodes and their children.
    std::unordered_map<NodeId, MetaNodeArgs> meta_nodes;

    // Adjaceny list.
    // All nodes must be ordinary nodes (i.e. not a key of `meta_nodes`).
    std::vector<Edge> edges;
  };

  explicit FakeGraph(Args args);
  ~FakeGraph();

  // Creates a meta node or return the node if the `id` already exists.
  // It is illegal to call CreateMetaNode and CreateOrdinaryNode with the same `id`.
  //
  // If `id` is unspecified, an `id` is selected automatically.
  FakeNodePtr CreateMetaNode(std::optional<NodeId> id);

  // Creates an ordinary node or return the node if `id` already exists.
  // It is illegal to call CreateMetaNode and CreateOrdinaryNode with the same `id`.
  //
  // If `id` is unspecified, an `id` is selected automatically.
  // If `parent` is specified and `id` already exists, the given `parent` must match the old parent.
  FakeNodePtr CreateOrdinaryNode(std::optional<NodeId> id, FakeNodePtr parent);

  // Returns the node with the given ID.
  // Must exist.
  FakeNodePtr node(NodeId id) const {
    auto it = nodes_.find(id);
    FX_CHECK(it != nodes_.end()) << "FakeGraph does have node " << id;
    return it->second;
  }

 private:
  NodeId NextId();

  std::unordered_map<NodeId, FakeNodePtr> nodes_;
};

}  // namespace media_audio

#endif  // SRC_MEDIA_AUDIO_MIXER_SERVICE_FIDL_TESTING_FAKE_NODE_H_
