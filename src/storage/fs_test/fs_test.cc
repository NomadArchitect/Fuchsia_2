// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/storage/fs_test/fs_test.h"

#include <dlfcn.h>
#include <errno.h>
#include <fidl/fuchsia.device/cpp/wire.h>
#include <fidl/fuchsia.fs/cpp/wire.h>
#include <fidl/fuchsia.hardware.ramdisk/cpp/wire.h>
#include <fuchsia/fs/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/fdio/directory.h>
#include <lib/fdio/namespace.h>
#include <lib/fidl/llcpp/connect_service.h>
#include <lib/fzl/vmo-mapper.h>
#include <lib/service/llcpp/service.h>
#include <lib/sync/completion.h>
#include <lib/zx/channel.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>
#include <zircon/errors.h>

#include <iostream>
#include <unordered_map>
#include <utility>

#include <fbl/unique_fd.h>

#include "sdk/lib/syslog/cpp/macros.h"
#include "src/lib/json_parser/json_parser.h"
#include "src/lib/storage/fs_management/cpp/admin.h"
#include "src/lib/storage/fs_management/cpp/format.h"
#include "src/lib/storage/fs_management/cpp/fvm.h"
#include "src/lib/storage/fs_management/cpp/launch.h"
#include "src/lib/storage/fs_management/cpp/mount.h"
#include "src/lib/storage/vfs/cpp/fuchsia_vfs.h"
#include "src/storage/blobfs/blob_layout.h"
#include "src/storage/blobfs/compression_settings.h"
#include "src/storage/fs_test/blobfs_test.h"
#include "src/storage/fs_test/json_filesystem.h"
#include "src/storage/fs_test/test_filesystem.h"
#include "src/storage/testing/fvm.h"

namespace fs_test {
namespace {

/// Amount of time to wait for a given device to be available.
constexpr zx_duration_t kDeviceWaitTime = zx::sec(30).get();

// Creates a ram-disk with an optional FVM partition. Returns the ram-disk and the device path.
zx::status<std::pair<storage::RamDisk, std::string>> CreateRamDisk(
    const TestFilesystemOptions& options) {
  if (options.use_ram_nand) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  zx::vmo vmo;
  if (options.vmo->is_valid()) {
    uint64_t vmo_size;
    auto status = zx::make_status(options.vmo->get_size(&vmo_size));
    if (status.is_error()) {
      return status.take_error();
    }
    status = zx::make_status(options.vmo->create_child(ZX_VMO_CHILD_SLICE, 0, vmo_size, &vmo));
    if (status.is_error()) {
      return status.take_error();
    }
  } else {
    fzl::VmoMapper mapper;
    auto status =
        zx::make_status(mapper.CreateAndMap(options.device_block_size * options.device_block_count,
                                            ZX_VM_PERM_READ | ZX_VM_PERM_WRITE, nullptr, &vmo));
    if (status.is_error()) {
      std::cout << "Unable to create VMO for ramdisk: " << status.status_string() << std::endl;
      return status.take_error();
    }

    // Fill the ram-disk with a non-zero value so that we don't inadvertently depend on it being
    // zero filled.
    if (!options.zero_fill) {
      memset(mapper.start(), 0xaf, mapper.size());
    }
  }

  // Create a ram-disk.
  auto ram_disk_or = storage::RamDisk::CreateWithVmo(std::move(vmo), options.device_block_size);
  if (ram_disk_or.is_error()) {
    return ram_disk_or.take_error();
  }

  if (options.fail_after) {
    if (auto status = ram_disk_or->SleepAfter(options.fail_after); status.is_error()) {
      return status.take_error();
    }
  }

  if (options.ram_disk_discard_random_after_last_flush) {
    ramdisk_set_flags(ram_disk_or->client(),
                      fuchsia_hardware_ramdisk::wire::kRamdiskFlagDiscardRandom |
                          fuchsia_hardware_ramdisk::wire::kRamdiskFlagDiscardNotFlushedOnWake);
  }

  std::string device_path = ram_disk_or.value().path();
  return zx::ok(std::make_pair(std::move(ram_disk_or).value(), std::move(device_path)));
}

// Creates a ram-nand device.  It does not create an FVM partition; that is left to the caller.
zx::status<std::pair<ramdevice_client::RamNand, std::string>> CreateRamNand(
    const TestFilesystemOptions& options) {
  constexpr int kPageSize = 4096;
  constexpr int kPagesPerBlock = 64;
  constexpr int kOobSize = 8;

  uint32_t block_count;
  zx::vmo vmo;
  if (options.vmo->is_valid()) {
    uint64_t vmo_size;
    auto status = zx::make_status(options.vmo->get_size(&vmo_size));
    if (status.is_error()) {
      return status.take_error();
    }
    block_count = static_cast<uint32_t>(vmo_size / (kPageSize + kOobSize) / kPagesPerBlock);
    // For now, when using a ram-nand device, the only supported device block size is 8 KiB, so
    // raise an error if the user tries to ask for something different.
    if ((options.device_block_size != 0 && options.device_block_size != 8192) ||
        (options.device_block_count != 0 &&
         options.device_block_size * options.device_block_count !=
             block_count * kPageSize * kPagesPerBlock)) {
      std::cout << "Bad device parameters" << std::endl;
      return zx::error(ZX_ERR_INVALID_ARGS);
    }
    status = zx::make_status(options.vmo->create_child(ZX_VMO_CHILD_SLICE, 0, vmo_size, &vmo));
    if (status.is_error()) {
      return status.take_error();
    }
  } else if (options.device_block_size != 8192) {  // FTL exports a device with 8 KiB blocks.
    return zx::error(ZX_ERR_INVALID_ARGS);
  } else {
    block_count = static_cast<uint32_t>(options.device_block_size * options.device_block_count /
                                        kPageSize / kPagesPerBlock);
  }

  auto status =
      zx::make_status(wait_for_device("/dev/sys/platform/00:00:2e/nand-ctl", kDeviceWaitTime));
  if (status.is_error()) {
    std::cout << "Failed waiting for /dev/sys/platform/00:00:2e/nand-ctl to appear: "
              << status.status_string() << std::endl;
    return status.take_error();
  }

  std::optional<ramdevice_client::RamNand> ram_nand;
  fuchsia_hardware_nand_RamNandInfo config = {
      .vmo = vmo.release(),
      .nand_info =
          {
              .page_size = kPageSize,
              .pages_per_block = kPagesPerBlock,
              .num_blocks = block_count,
              .ecc_bits = 8,
              .oob_size = kOobSize,
              .nand_class = fuchsia_hardware_nand_Class_FTL,
          },
      .fail_after = options.fail_after,
  };
  status = zx::make_status(ramdevice_client::RamNand::Create(&config, &ram_nand));
  if (status.is_error()) {
    std::cout << "RamNand::Create failed: " << status.status_string() << std::endl;
    return status.take_error();
  }

  std::string ftl_path = std::string(ram_nand->path()) + "/ftl/block";
  status = zx::make_status(wait_for_device(ftl_path.c_str(), kDeviceWaitTime));
  if (status.is_error()) {
    std::cout << "Timed out waiting for RamNand" << std::endl;
    return status.take_error();
  }
  return zx::ok(std::make_pair(*std::move(ram_nand), std::move(ftl_path)));
}

}  // namespace

std::string StripTrailingSlash(const std::string& in) {
  if (!in.empty() && in.back() == '/') {
    return in.substr(0, in.length() - 1);
  } else {
    return in;
  }
}

zx::status<> FsUnbind(const std::string& mount_path) {
  fdio_ns_t* ns;
  if (auto status = zx::make_status(fdio_ns_get_installed(&ns)); status.is_error()) {
    return status;
  }
  if (auto status = zx::make_status(fdio_ns_unbind(ns, StripTrailingSlash(mount_path).c_str()));
      status.is_error()) {
    std::cout << "Unable to unbind: " << status.status_string() << std::endl;
    return status;
  }
  return zx::ok();
}

// Returns device and device path.
zx::status<std::pair<RamDevice, std::string>> CreateRamDevice(
    const TestFilesystemOptions& options) {
  RamDevice ram_device;
  std::string device_path;

  if (options.use_ram_nand) {
    auto ram_nand_or = CreateRamNand(options);
    if (ram_nand_or.is_error()) {
      return ram_nand_or.take_error();
    }
    auto [ram_nand, nand_device_path] = std::move(ram_nand_or).value();
    ram_device = RamDevice(std::move(ram_nand));
    device_path = std::move(nand_device_path);
  } else {
    auto ram_disk_or = CreateRamDisk(options);
    if (ram_disk_or.is_error()) {
      return ram_disk_or.take_error();
    }
    auto [device, ram_disk_path] = std::move(ram_disk_or).value();
    ram_device = RamDevice(std::move(device));
    device_path = std::move(ram_disk_path);
  }

  // Create an FVM partition if requested.
  if (options.use_fvm) {
    storage::FvmOptions fvm_options = {.initial_fvm_slice_count = options.initial_fvm_slice_count};
    auto fvm_partition_or = storage::CreateFvmPartition(
        device_path, static_cast<int>(options.fvm_slice_size), fvm_options);
    if (fvm_partition_or.is_error()) {
      return fvm_partition_or.take_error();
    }

    if (options.dummy_fvm_partition_size > 0) {
      auto fvm_fd = fbl::unique_fd(open((device_path + "/fvm").c_str(), O_RDWR));
      if (!fvm_fd) {
        std::cout << "Could not open FVM driver: " << strerror(errno) << std::endl;
        return zx::error(ZX_ERR_BAD_STATE);
      }

      alloc_req_t request = {
          .slice_count = options.dummy_fvm_partition_size / options.fvm_slice_size,
          .type = {0x01, 0x02, 0x03, 0x04, 0x01, 0x02, 0x03, 0x04, 0x01, 0x02, 0x03, 0x04, 0x01,
                   0x02, 0x03, 0x04},
          .guid = {0x01, 0x02, 0x03, 0x04, 0x01, 0x02, 0x03, 0x04, 0x01, 0x02, 0x03, 0x04, 0x01,
                   0x02, 0x03, 0x04},
          .name = "dummy",
      };
      if (fs_management::FvmAllocatePartition(fvm_fd.get(), &request).is_error()) {
        std::cout << "Could not allocate dummy FVM partition" << std::endl;
        return zx::error(ZX_ERR_BAD_STATE);
      }
    }

    return zx::ok(std::make_pair(std::move(ram_device), std::move(fvm_partition_or).value()));
  } else {
    return zx::ok(std::make_pair(std::move(ram_device), std::move(device_path)));
  }
}

zx::status<> FsFormat(const std::string& device_path, fs_management::DiskFormat format,
                      const fs_management::MkfsOptions& options) {
  auto status = zx::make_status(
      fs_management::Mkfs(device_path.c_str(), format, fs_management::LaunchStdioSync, options));
  if (status.is_error()) {
    std::cout << "Could not format " << fs_management::DiskFormatString(format)
              << " file system: " << status.status_string() << std::endl;
    return status;
  }
  return zx::ok();
}

zx::status<fidl::ClientEnd<fuchsia_io::Directory>> FsMount(
    const std::string& device_path, const std::string& mount_path, fs_management::DiskFormat format,
    const fs_management::MountOptions& mount_options) {
  auto fd = fbl::unique_fd(open(device_path.c_str(), O_RDWR));
  if (!fd) {
    std::cout << "Could not open device: " << device_path << ": errno=" << errno << std::endl;
    return zx::error(ZX_ERR_BAD_STATE);
  }

  fs_management::MountOptions options = mount_options;

  // Uncomment the following line to force an fsck at the end of every transaction (where
  // supported).
  // options.fsck_after_every_transaction = true;

  // |fd| is consumed by mount.
  auto result = fs_management::Mount(std::move(fd), StripTrailingSlash(mount_path).c_str(), format,
                                     options, fs_management::LaunchStdioAsync);
  if (result.is_error()) {
    std::cout << "Could not mount " << fs_management::DiskFormatString(format)
              << " file system: " << result.status_string() << std::endl;
    return result.take_error();
  }
  return zx::ok(std::move(*result).TakeExportRoot());
}

// Returns device and device path.
zx::status<std::pair<RamDevice, std::string>> OpenRamDevice(const TestFilesystemOptions& options) {
  if (!options.vmo->is_valid()) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  RamDevice ram_device;
  std::string device_path;

  if (options.use_ram_nand) {
    // First create the ram-nand device.
    auto ram_nand_or = CreateRamNand(options);
    if (ram_nand_or.is_error()) {
      return ram_nand_or.take_error();
    }
    auto [ram_nand, ftl_device_path] = std::move(ram_nand_or).value();
    ram_device = RamDevice(std::move(ram_nand));
    device_path = std::move(ftl_device_path);
  } else {
    auto ram_disk_or = CreateRamDisk(options);
    if (ram_disk_or.is_error()) {
      std::cout << "Unable to create ram-disk" << std::endl;
    }

    auto [device, ram_disk_path] = std::move(ram_disk_or).value();
    ram_device = RamDevice(std::move(device));
    device_path = std::move(ram_disk_path);
  }

  if (options.use_fvm) {
    // Now bind FVM to it.
    fbl::unique_fd ftl_device(open(device_path.c_str(), O_RDWR));
    if (!ftl_device)
      return zx::error(ZX_ERR_BAD_STATE);
    auto status = storage::BindFvm(ftl_device.get());
    if (status.is_error()) {
      std::cout << "Unable to bind FVM: " << status.status_string() << std::endl;
      return status.take_error();
    }

    device_path.append("/fvm/fs-test-partition-p-1/block");
  }

  auto status = zx::make_status(wait_for_device(device_path.c_str(), kDeviceWaitTime));
  if (status.is_error()) {
    std::cout << "Timed out waiting for partition to show up" << std::endl;
    return status.take_error();
  }

  return zx::ok(std::make_pair(std::move(ram_device), std::move(device_path)));
}

TestFilesystemOptions TestFilesystemOptions::DefaultBlobfs() {
  return TestFilesystemOptions{.description = "Blobfs",
                               .use_fvm = true,
                               .device_block_size = 512,
                               .device_block_count = 196'608,
                               .fvm_slice_size = 32'768,
                               .num_inodes = 512,  // blobfs can grow as needed.
                               .filesystem = &BlobfsFilesystem::SharedInstance()};
}

TestFilesystemOptions TestFilesystemOptions::BlobfsWithoutFvm() {
  TestFilesystemOptions blobfs_with_no_fvm = TestFilesystemOptions::DefaultBlobfs();
  blobfs_with_no_fvm.description = "BlobfsWithoutFvm";
  blobfs_with_no_fvm.use_fvm = false;
  blobfs_with_no_fvm.num_inodes = 2048;
  return blobfs_with_no_fvm;
}

std::ostream& operator<<(std::ostream& out, const TestFilesystemOptions& options) {
  return out << options.description;
}

std::vector<TestFilesystemOptions> AllTestFilesystems() {
  static const std::vector<TestFilesystemOptions>* options = [] {
    const char kConfigFile[] = "/pkg/config/config.json";
    json_parser::JSONParser parser;
    auto config = parser.ParseFromFile(std::string(kConfigFile));
    auto iter = config.FindMember("library");
    std::unique_ptr<Filesystem> filesystem;
    if (iter != config.MemberEnd()) {
      void* handle = dlopen(iter->value.GetString(), RTLD_NOW);
      FX_CHECK(handle) << dlerror();
      auto get_filesystem =
          reinterpret_cast<std::unique_ptr<Filesystem> (*)()>(dlsym(handle, "_Z13GetFilesystemv"));
      FX_CHECK(get_filesystem) << dlerror();
      filesystem = get_filesystem();
    } else {
      filesystem = JsonFilesystem::NewFilesystem(config).value();
    }
    std::string name = config["name"].GetString();
    auto options = new std::vector<TestFilesystemOptions>;
    iter = config.FindMember("options");
    if (iter == config.MemberEnd()) {
      name[0] = static_cast<char>(toupper(name[0]));
      options->push_back(TestFilesystemOptions{.description = name,
                                               .use_fvm = false,
                                               .device_block_size = 512,
                                               .device_block_count = 196'608,
                                               .filesystem = filesystem.get()});
    } else {
      for (rapidjson::SizeType i = 0; i < iter->value.Size(); ++i) {
        const auto& opt = iter->value[i];
        options->push_back(TestFilesystemOptions{
            .description = opt["description"].GetString(),
            .use_fvm = opt["use_fvm"].GetBool(),
            .has_min_volume_size = ConfigGetOrDefault<bool>(opt, "has_min_volume_size", false),
            .device_block_size = ConfigGetOrDefault<uint64_t>(opt, "device_block_size", 512),
            .device_block_count = ConfigGetOrDefault<uint64_t>(opt, "device_block_count", 196'608),
            .fvm_slice_size = 32'768,
            .filesystem = filesystem.get()});
      }
    }
    filesystem.release();  // Deliberate leak
    return options;
  }();

  return *options;
}

TestFilesystemOptions OptionsWithDescription(std::string_view description) {
  for (const auto& options : AllTestFilesystems()) {
    if (options.description == description) {
      return options;
    }
  }
  FX_LOGS(FATAL) << "No test options with description: " << description;
  abort();
}

std::vector<TestFilesystemOptions> MapAndFilterAllTestFilesystems(
    std::function<std::optional<TestFilesystemOptions>(const TestFilesystemOptions&)>
        map_and_filter) {
  std::vector<TestFilesystemOptions> results;
  for (const TestFilesystemOptions& options : AllTestFilesystems()) {
    auto r = map_and_filter(options);
    if (r) {
      results.push_back(*std::move(r));
    }
  }
  return results;
}

// -- FilesystemInstance --

// Default implementation
zx::status<> FilesystemInstance::Unmount(const std::string& mount_path) {
  // Detach from the namespace.
  if (auto status = FsUnbind(mount_path); status.is_error()) {
    return status;
  }

  auto status = fs_management::Shutdown(GetOutgoingDirectory());
  if (status.is_error()) {
    std::cout << "Shut down failed: " << status.status_string() << std::endl;
    return status;
  }
  return zx::ok();
}

// -- Blobfs --

class BlobfsInstance : public FilesystemInstance {
 public:
  BlobfsInstance(RamDevice device, std::string device_path)
      : device_(std::move(device)), device_path_(std::move(device_path)) {}

  zx::status<> Format(const TestFilesystemOptions& options) override {
    fs_management::MkfsOptions mkfs_options;
    mkfs_options.deprecated_padded_blobfs_format =
        options.blob_layout_format == blobfs::BlobLayoutFormat::kDeprecatedPaddedMerkleTreeAtStart;
    mkfs_options.num_inodes = options.num_inodes;
    return FsFormat(device_path_, fs_management::kDiskFormatBlobfs, mkfs_options);
  }

  zx::status<> Mount(const std::string& mount_path,
                     const fs_management::MountOptions& options) override {
    auto export_root_or =
        FsMount(device_path_, mount_path, fs_management::kDiskFormatBlobfs, options);
    if (export_root_or.is_error())
      return export_root_or.take_error();
    outgoing_directory_ = std::move(*export_root_or);
    return zx::ok();
  }

  zx::status<> Fsck() override {
    fs_management::FsckOptions options{
        .verbose = false,
        .never_modify = true,
        .always_modify = false,
        .force = true,
    };
    return zx::make_status(fs_management::Fsck(device_path_.c_str(),
                                               fs_management::kDiskFormatBlobfs, options,
                                               fs_management::LaunchStdioSync));
  }

  zx::status<std::string> DevicePath() const override { return zx::ok(std::string(device_path_)); }
  storage::RamDisk* GetRamDisk() override { return std::get_if<storage::RamDisk>(&device_); }
  ramdevice_client::RamNand* GetRamNand() override {
    return std::get_if<ramdevice_client::RamNand>(&device_);
  }
  fidl::UnownedClientEnd<fuchsia_io::Directory> GetOutgoingDirectory() const override {
    return outgoing_directory_.borrow();
  }
  void ResetOutgoingDirectory() override { outgoing_directory_.reset(); }

 private:
  RamDevice device_;
  std::string device_path_;
  fidl::ClientEnd<fuchsia_io::Directory> outgoing_directory_;
};

std::unique_ptr<FilesystemInstance> BlobfsFilesystem::Create(RamDevice device,
                                                             std::string device_path) const {
  return std::make_unique<BlobfsInstance>(std::move(device), std::move(device_path));
}

zx::status<std::unique_ptr<FilesystemInstance>> BlobfsFilesystem::Open(
    const TestFilesystemOptions& options) const {
  auto result = OpenRamDevice(options);
  if (result.is_error()) {
    return result.take_error();
  }
  auto [ram_nand, device_path] = std::move(result).value();
  return zx::ok(std::make_unique<BlobfsInstance>(std::move(ram_nand), std::move(device_path)));
}

}  // namespace fs_test
