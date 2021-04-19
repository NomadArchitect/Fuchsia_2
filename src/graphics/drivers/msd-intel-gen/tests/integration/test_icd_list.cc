// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fuchsia/device/llcpp/fidl.h>
#include <fuchsia/gpu/magma/llcpp/fidl.h>
#include <lib/fdio/directory.h>
#include <lib/zx/channel.h>

#include <gtest/gtest.h>

#include "helper/test_device_helper.h"
#include "lib/fidl/llcpp/client_end.h"
#include "magma.h"
#include "magma_util/macros.h"
#include "msd_intel_gen_query.h"

namespace {

TEST(Intel, IcdList) {
  magma::TestDeviceBase test_device(MAGMA_VENDOR_ID_INTEL);
  auto rsp = fidl::WireCall<fuchsia_gpu_magma::Device>(test_device.channel()).GetIcdList();
  EXPECT_TRUE(rsp.ok());
  EXPECT_EQ(rsp->icd_list.count(), 2u);
  auto& icd_item = rsp->icd_list[0];
  EXPECT_TRUE(icd_item.has_flags());
  EXPECT_TRUE(icd_item.flags() & fuchsia_gpu_magma::wire::IcdFlags::kSupportsVulkan);
  std::string res_string(icd_item.component_url().get());
  EXPECT_EQ(res_string.length(), icd_item.component_url().size());
  EXPECT_EQ(std::string("fuchsia-pkg://fuchsia.com/libvulkan_intel_gen_test#meta/vulkan.cm"),
            res_string);
}

}  // namespace
