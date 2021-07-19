// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STORAGE_FSHOST_COPIER_H_
#define SRC_STORAGE_FSHOST_COPIER_H_

#include <lib/zx/status.h>

#include <memory>
#include <utility>
#include <variant>
#include <vector>

#include <fbl/unique_fd.h>

namespace fshost {

class Copier {
 public:
  // Reads all the data at root fd.
  static zx::status<Copier> Read(fbl::unique_fd root_fd);

  Copier() = default;
  Copier(Copier&&) = default;
  Copier& operator=(Copier&&) = default;

  // Writes all data to the given root fd.
  zx_status_t Write(fbl::unique_fd root_fd) const;

 private:
  struct Tree {
    std::vector<std::pair<std::string, std::variant<std::vector<uint8_t>, std::unique_ptr<Tree>>>>
        tree;
  };

  Tree tree_;
};

}  // namespace fshost

#endif  // SRC_STORAGE_FSHOST_COPIER_H_
