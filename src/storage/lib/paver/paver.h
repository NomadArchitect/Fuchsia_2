// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STORAGE_LIB_PAVER_PAVER_H_
#define SRC_STORAGE_LIB_PAVER_PAVER_H_

#include <fuchsia/paver/llcpp/fidl.h>
#include <lib/zx/channel.h>
#include <zircon/types.h>

#include <variant>

#include <fbl/mutex.h>
#include <fbl/string.h>
#include <fbl/unique_fd.h>

#include "abr-client.h"
#include "device-partitioner.h"
#include "lib/async/dispatcher.h"
#include "paver-context.h"

namespace paver {

class Paver : public fidl::WireRawChannelInterface<fuchsia_paver::Paver> {
  using fidl::WireRawChannelInterface<fuchsia_paver::Paver>::FindSysconfig;
  using fidl::WireRawChannelInterface<fuchsia_paver::Paver>::UseBlockDevice;

 public:
  void FindDataSink(zx::channel data_sink, FindDataSinkCompleter::Sync& completer) override;

  void UseBlockDevice(zx::channel block_device, zx::channel dynamic_data_sink,
                      UseBlockDeviceCompleter::Sync& completer) override;

  void UseBlockDevice(zx::channel block_device, zx::channel dynamic_data_sink);

  void FindBootManager(zx::channel boot_manager,
                       FindBootManagerCompleter::Sync& completer) override;

  void FindSysconfig(zx::channel sysconfig, FindSysconfigCompleter::Sync& completer) override;

  void FindSysconfig(zx::channel sysconfig);

  void set_dispatcher(async_dispatcher_t* dispatcher) { dispatcher_ = dispatcher; }
  void set_devfs_root(fbl::unique_fd devfs_root) { devfs_root_ = std::move(devfs_root); }
  void set_svc_root(fidl::ClientEnd<fuchsia_io::Directory> svc_root) {
    svc_root_ = std::move(svc_root);
  }

  Paver() : context_(std::make_shared<Context>()) {}

 private:
  // Used for test injection.
  fbl::unique_fd devfs_root_;
  fidl::ClientEnd<fuchsia_io::Directory> svc_root_;

  async_dispatcher_t* dispatcher_ = nullptr;

  // Declared as shared_ptr to avoid life time issues (i.e. Paver exiting before the created device
  // partitioners).
  std::shared_ptr<Context> context_;
};

// Common shared implementation for DataSink and DynamicDataSink. Necessary to work around lack of
// "is-a" relationship in llcpp bindings.
class DataSinkImpl {
 public:
  DataSinkImpl(fbl::unique_fd devfs_root, std::unique_ptr<DevicePartitioner> partitioner)
      : devfs_root_(std::move(devfs_root)), partitioner_(std::move(partitioner)) {}

  zx::status<fuchsia_mem::wire::Buffer> ReadAsset(fuchsia_paver::wire::Configuration configuration,
                                                  fuchsia_paver::wire::Asset asset);

  zx::status<> WriteAsset(fuchsia_paver::wire::Configuration configuration,
                          fuchsia_paver::wire::Asset asset, fuchsia_mem::wire::Buffer payload);

  // FIDL llcpp unions don't currently support memory ownership so we need to
  // return something that does own the underlying memory.
  //
  // Once unions do support owned memory we can just return
  // WriteBootloaderResult directly here.
  std::variant<zx_status_t, bool> WriteFirmware(fuchsia_paver::wire::Configuration configuration,
                                                fidl::StringView type,
                                                fuchsia_mem::wire::Buffer payload);

  zx::status<> WriteVolumes(zx::channel payload_stream);

  zx::status<> WriteBootloader(fuchsia_mem::wire::Buffer payload);

  zx::status<> WriteDataFile(fidl::StringView filename, fuchsia_mem::wire::Buffer payload);

  zx::status<zx::channel> WipeVolume();

  DevicePartitioner* partitioner() { return partitioner_.get(); }

 private:
  // Used for test injection.
  fbl::unique_fd devfs_root_;

  std::unique_ptr<DevicePartitioner> partitioner_;
};

class DataSink : public fuchsia_paver::DataSink::RawChannelInterface {
 public:
  DataSink(fbl::unique_fd devfs_root, std::unique_ptr<DevicePartitioner> partitioner)
      : sink_(std::move(devfs_root), std::move(partitioner)) {}

  // Automatically finds block device to use.
  static void Bind(async_dispatcher_t* dispatcher, fbl::unique_fd devfs_root,
                   fidl::ClientEnd<fuchsia_io::Directory> svc_root, zx::channel server,
                   std::shared_ptr<Context> context);

  void ReadAsset(fuchsia_paver::wire::Configuration configuration, fuchsia_paver::wire::Asset asset,
                 ReadAssetCompleter::Sync& completer) override;

  void WriteAsset(fuchsia_paver::wire::Configuration configuration,
                  fuchsia_paver::wire::Asset asset, fuchsia_mem::wire::Buffer payload,
                  WriteAssetCompleter::Sync& completer) override {
    completer.Reply(sink_.WriteAsset(configuration, asset, std::move(payload)).status_value());
  }

  void WriteFirmware(fuchsia_paver::wire::Configuration configuration, fidl::StringView type,
                     fuchsia_mem::wire::Buffer payload,
                     WriteFirmwareCompleter::Sync& completer) override;

  void WriteVolumes(zx::channel payload_stream, WriteVolumesCompleter::Sync& completer) override {
    completer.Reply(sink_.WriteVolumes(std::move(payload_stream)).status_value());
  }

  void WriteBootloader(fuchsia_mem::wire::Buffer payload,
                       WriteBootloaderCompleter::Sync& completer) override {
    completer.Reply(sink_.WriteBootloader(std::move(payload)).status_value());
  }

  void WriteDataFile(fidl::StringView filename, fuchsia_mem::wire::Buffer payload,
                     WriteDataFileCompleter::Sync& completer) override {
    completer.Reply(sink_.WriteDataFile(std::move(filename), std::move(payload)).status_value());
  }

  void WipeVolume(WipeVolumeCompleter::Sync& completer) override;

  void Flush(FlushCompleter::Sync& completer) override {
    completer.Reply(sink_.partitioner()->Flush().status_value());
  }

 private:
  DataSinkImpl sink_;
};

class DynamicDataSink : public fuchsia_paver::DynamicDataSink::RawChannelInterface {
 public:
  DynamicDataSink(fbl::unique_fd devfs_root, std::unique_ptr<DevicePartitioner> partitioner)
      : sink_(std::move(devfs_root), std::move(partitioner)) {}

  static void Bind(async_dispatcher_t* dispatcher, fbl::unique_fd devfs_root,
                   fidl::ClientEnd<fuchsia_io::Directory> svc_root, zx::channel block_device,
                   zx::channel server, std::shared_ptr<Context> context);

  void InitializePartitionTables(InitializePartitionTablesCompleter::Sync& completer) override;

  void WipePartitionTables(WipePartitionTablesCompleter::Sync& completer) override;

  void ReadAsset(fuchsia_paver::wire::Configuration configuration, fuchsia_paver::wire::Asset asset,
                 ReadAssetCompleter::Sync& completer) override;

  void WriteAsset(fuchsia_paver::wire::Configuration configuration,
                  fuchsia_paver::wire::Asset asset, fuchsia_mem::wire::Buffer payload,
                  WriteAssetCompleter::Sync& completer) override {
    completer.Reply(sink_.WriteAsset(configuration, asset, std::move(payload)).status_value());
  }

  void WriteFirmware(fuchsia_paver::wire::Configuration configuration, fidl::StringView type,
                     fuchsia_mem::wire::Buffer payload,
                     WriteFirmwareCompleter::Sync& completer) override;

  void WriteVolumes(zx::channel payload_stream, WriteVolumesCompleter::Sync& completer) override {
    completer.Reply(sink_.WriteVolumes(std::move(payload_stream)).status_value());
  }

  void WriteBootloader(fuchsia_mem::wire::Buffer payload,
                       WriteBootloaderCompleter::Sync& completer) override {
    completer.Reply(sink_.WriteBootloader(std::move(payload)).status_value());
  }

  void WriteDataFile(fidl::StringView filename, fuchsia_mem::wire::Buffer payload,
                     WriteDataFileCompleter::Sync& completer) override {
    completer.Reply(sink_.WriteDataFile(std::move(filename), std::move(payload)).status_value());
  }

  void WipeVolume(WipeVolumeCompleter::Sync& completer) override;

  void Flush(FlushCompleter::Sync& completer) override {
    completer.Reply(sink_.partitioner()->Flush().status_value());
  }

 private:
  DataSinkImpl sink_;
};

class BootManager : public fuchsia_paver::BootManager::Interface {
 public:
  BootManager(std::unique_ptr<abr::Client> abr_client,
              fidl::ClientEnd<fuchsia_io::Directory> svc_root)
      : abr_client_(std::move(abr_client)), svc_root_(std::move(svc_root)) {}

  static void Bind(async_dispatcher_t* dispatcher, fbl::unique_fd devfs_root,
                   fidl::ClientEnd<fuchsia_io::Directory> svc_root,
                   std::shared_ptr<Context> context, zx::channel server);

  void QueryCurrentConfiguration(QueryCurrentConfigurationCompleter::Sync& completer) override;

  void QueryActiveConfiguration(QueryActiveConfigurationCompleter::Sync& completer) override;

  void QueryConfigurationStatus(fuchsia_paver::wire::Configuration configuration,
                                QueryConfigurationStatusCompleter::Sync& completer) override;

  void SetConfigurationActive(fuchsia_paver::wire::Configuration configuration,
                              SetConfigurationActiveCompleter::Sync& completer) override;

  void SetConfigurationUnbootable(fuchsia_paver::wire::Configuration configuration,
                                  SetConfigurationUnbootableCompleter::Sync& completer) override;

  void SetConfigurationHealthy(fuchsia_paver::wire::Configuration configuration,
                               SetConfigurationHealthyCompleter::Sync& completer) override;

  void Flush(FlushCompleter::Sync& completer) override {
    completer.Reply(abr_client_->Flush().status_value());
  }

 private:
  std::unique_ptr<abr::Client> abr_client_;
  fidl::ClientEnd<fuchsia_io::Directory> svc_root_;
};

}  // namespace paver

#endif  // SRC_STORAGE_LIB_PAVER_PAVER_H_
