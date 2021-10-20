// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/storage/blobfs/query.h"

#include <fidl/fuchsia.fs/cpp/wire.h>
#include <lib/fidl-async/cpp/bind.h>

#include "src/storage/blobfs/blobfs.h"
#include "src/storage/blobfs/format.h"
#include "src/storage/blobfs/runner.h"

namespace blobfs {

constexpr char kFsName[] = "blobfs";

QueryService::QueryService(async_dispatcher_t* dispatcher, Blobfs* blobfs, Runner* runner)
    : fs::Service([dispatcher, this](fidl::ServerEnd<fuchsia_fs::Query> server_end) {
        return fidl::BindSingleInFlightOnly(dispatcher, std::move(server_end), this);
      }),
      blobfs_(blobfs),
      runner_(runner) {}

void QueryService::GetInfo(GetInfoRequestView request, GetInfoCompleter::Sync& completer) {
  static_assert(sizeof(kFsName) < fuchsia_fs::wire::kMaxFsNameLength, "Blobfs name too long");

  fidl::Arena allocator;
  fuchsia_fs::wire::FilesystemInfo filesystem_info(allocator);

  filesystem_info.set_total_bytes(allocator,
                                  blobfs_->Info().data_block_count * blobfs_->Info().block_size);
  filesystem_info.set_used_bytes(allocator,
                                 blobfs_->Info().alloc_block_count * blobfs_->Info().block_size);
  filesystem_info.set_total_nodes(allocator, blobfs_->Info().inode_count);
  filesystem_info.set_used_nodes(allocator, blobfs_->Info().alloc_inode_count);
  filesystem_info.set_fs_id(allocator, blobfs_->GetFsId());
  filesystem_info.set_block_size(allocator, kBlobfsBlockSize);
  filesystem_info.set_max_node_name_size(allocator, digest::kSha256HexLength);
  filesystem_info.set_fs_type(allocator, fuchsia_fs::wire::FsType::kBlobfs);

  fidl::StringView name(kFsName);
  filesystem_info.set_name(allocator, std::move(name));

  std::string device_path;
  if (auto device_path_or = blobfs_->Device()->GetDevicePath(); device_path_or.is_error()) {
    completer.ReplyError(device_path_or.error_value());
    return;
  } else {
    device_path = std::move(device_path_or).value();
  }
  filesystem_info.set_device_path(allocator, fidl::StringView::FromExternal(device_path));

  completer.ReplySuccess(std::move(filesystem_info));
}

void QueryService::IsNodeInFilesystem(IsNodeInFilesystemRequestView request,
                                      IsNodeInFilesystemCompleter::Sync& completer) {
  completer.Reply(runner_->IsTokenAssociatedWithVnode(std::move(request->token)));
}

}  // namespace blobfs
