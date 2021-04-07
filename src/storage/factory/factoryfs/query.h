// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STORAGE_FACTORY_FACTORYFS_QUERY_H_
#define SRC_STORAGE_FACTORY_FACTORYFS_QUERY_H_

#include <fuchsia/fs/llcpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>

#include "src/lib/storage/vfs/cpp/service.h"
#include "src/storage/factory/factoryfs/factoryfs.h"
#include "src/storage/factory/factoryfs/runner.h"

namespace factoryfs {

class QueryService final : public fidl::WireInterface<fuchsia_fs::Query>, public fs::Service {
 public:
  QueryService(async_dispatcher_t* dispatcher, Factoryfs* factoryfs, Runner* runner);

  void GetInfo(fuchsia_fs::wire::FilesystemInfoQuery query,
               GetInfoCompleter::Sync& completer) final;

  void IsNodeInFilesystem(zx::event token, IsNodeInFilesystemCompleter::Sync& completer) final;

 private:
  const Factoryfs* const factoryfs_;
  Runner* const runner_;
};

}  // namespace factoryfs

#endif  // SRC_STORAGE_FACTORY_FACTORYFS_QUERY_H_
