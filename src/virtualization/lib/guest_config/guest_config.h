// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_VIRTUALIZATION_LIB_GUEST_CONFIG_GUEST_CONFIG_H_
#define SRC_VIRTUALIZATION_LIB_GUEST_CONFIG_GUEST_CONFIG_H_

#include <fuchsia/io/cpp/fidl.h>
#include <fuchsia/virtualization/cpp/fidl.h>

namespace guest_config {

using OpenAt = fit::function<zx_status_t(const std::string& path,
                                         fidl::InterfaceRequest<fuchsia::io::File> file)>;
// Parses json configuration.
zx_status_t ParseConfig(const std::string& data, OpenAt open_at,
                        fuchsia::virtualization::GuestConfig* cfg);

fuchsia::virtualization::GuestConfig MergeConfigs(fuchsia::virtualization::GuestConfig base,
                                                  fuchsia::virtualization::GuestConfig overrides);

void SetDefaults(fuchsia::virtualization::GuestConfig* cfg);

}  // namespace guest_config

#endif  // SRC_VIRTUALIZATION_LIB_GUEST_CONFIG_GUEST_CONFIG_H_
