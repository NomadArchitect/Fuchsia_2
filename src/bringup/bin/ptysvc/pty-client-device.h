// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_BRINGUP_BIN_PTYSVC_PTY_CLIENT_DEVICE_H_
#define SRC_BRINGUP_BIN_PTYSVC_PTY_CLIENT_DEVICE_H_

#include <fuchsia/hardware/pty/llcpp/fidl.h>
#include <lib/zx/channel.h>

#include <fbl/ref_ptr.h>

#include "pty-client.h"
#include "src/lib/storage/vfs/cpp/vfs.h"
#include "src/lib/storage/vfs/cpp/vfs_types.h"
#include "src/lib/storage/vfs/cpp/vnode.h"

class PtyClientDevice : public fidl::WireInterface<fuchsia_hardware_pty::Device> {
 public:
  explicit PtyClientDevice(fbl::RefPtr<PtyClient> client) : client_(std::move(client)) {}

  ~PtyClientDevice() override = default;

  // fuchsia.hardware.pty.Device methods
  void OpenClient(uint32_t id, fidl::ServerEnd<fuchsia_hardware_pty::Device> client,
                  OpenClientCompleter::Sync& completer) final;
  void ClrSetFeature(uint32_t clr, uint32_t set, ClrSetFeatureCompleter::Sync& completer) final;
  void GetWindowSize(GetWindowSizeCompleter::Sync& completer) final;
  void MakeActive(uint32_t client_pty_id, MakeActiveCompleter::Sync& completer) final;
  void ReadEvents(ReadEventsCompleter::Sync& completer) final;
  void SetWindowSize(fuchsia_hardware_pty::wire::WindowSize size,
                     SetWindowSizeCompleter::Sync& completer) final;

  // fuchsia.io.File methods
  void Read(uint64_t count, ReadCompleter::Sync& completer) final;
  void ReadAt(uint64_t count, uint64_t offset, ReadAtCompleter::Sync& completer) final;

  void Write(fidl::VectorView<uint8_t> data, WriteCompleter::Sync& completer) final;
  void WriteAt(fidl::VectorView<uint8_t> data, uint64_t offset,
               WriteAtCompleter::Sync& completer) final;

  void Seek(int64_t offset, fuchsia_io::wire::SeekOrigin start,
            SeekCompleter::Sync& completer) final;
  void Truncate(uint64_t length, TruncateCompleter::Sync& completer) final;
  void GetFlags(GetFlagsCompleter::Sync& completer) final;
  void SetFlags(uint32_t flags, SetFlagsCompleter::Sync& completer) final;
  void GetBuffer(uint32_t flags, GetBufferCompleter::Sync& completer) final;

  void Clone(uint32_t flags, fidl::ServerEnd<fuchsia_io::Node> node,
             CloneCompleter::Sync& completer) final;
  void Close(CloseCompleter::Sync& completer) final;
  void Describe(DescribeCompleter::Sync& completer) final;
  void Sync(SyncCompleter::Sync& completer) final;
  void GetAttr(GetAttrCompleter::Sync& completer) final;
  void SetAttr(uint32_t flags, fuchsia_io::wire::NodeAttributes attributes,
               SetAttrCompleter::Sync& completer) final;

 private:
  fbl::RefPtr<PtyClient> client_;
};

#endif  // SRC_BRINGUP_BIN_PTYSVC_PTY_CLIENT_DEVICE_H_
