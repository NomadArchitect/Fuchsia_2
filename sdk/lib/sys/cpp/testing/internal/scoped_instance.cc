
// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fuchsia/component/cpp/fidl.h>
#include <fuchsia/io/cpp/fidl.h>
#include <fuchsia/realm/builder/cpp/fidl.h>
#include <fuchsia/sys2/cpp/fidl.h>
#include <lib/fdio/directory.h>
#include <lib/fdio/io.h>
#include <lib/sys/cpp/component_context.h>
#include <lib/sys/cpp/service_directory.h>
#include <lib/sys/cpp/testing/internal/errors.h>
#include <lib/sys/cpp/testing/internal/realm.h>
#include <lib/sys/cpp/testing/internal/scoped_instance.h>
#include <lib/syslog/cpp/log_level.h>
#include <lib/syslog/cpp/macros.h>
#include <zircon/assert.h>
#include <zircon/status.h>

#include <random>

namespace sys::testing::internal {

namespace {

std::size_t random_unsigned() {
  std::random_device random_device;
  std::mt19937 generator(random_device());
  std::uniform_int_distribution<std::size_t> distribution;
  return distribution(generator);
}
}  // namespace

ScopedInstance::ScopedInstance(fuchsia::sys2::RealmSyncPtr realm_proxy,
                               fuchsia::sys2::ChildRef child_ref, ServiceDirectory exposed_dir)
    : realm_proxy_(std::move(realm_proxy)),
      child_ref_(std::move(child_ref)),
      exposed_dir_(std::move(exposed_dir)),
      has_moved_(false) {}

ScopedInstance::~ScopedInstance() {
  if (has_moved_) {
    return;
  }
  DestroyChild(realm_proxy_.get(), child_ref_);
}

ScopedInstance::ScopedInstance(ScopedInstance&& other) noexcept
    : child_ref_(std::move(other.child_ref_)),
      exposed_dir_(std::move(other.exposed_dir_)),
      has_moved_(false) {
  realm_proxy_.Bind(other.realm_proxy_.Unbind());
  other.has_moved_ = true;
}

ScopedInstance& ScopedInstance::operator=(ScopedInstance&& other) noexcept {
  this->realm_proxy_ = std::move(other.realm_proxy_);
  this->child_ref_ = std::move(other.child_ref_);
  this->exposed_dir_ = std::move(other.exposed_dir_);
  this->has_moved_ = false;
  other.has_moved_ = true;
  return *this;
}

ScopedInstance ScopedInstance::New(fuchsia::sys2::RealmSyncPtr realm_proxy, std::string collection,
                                   std::string url) {
  std::string name = "auto-" + std::to_string(random_unsigned());
  return New(std::move(realm_proxy), std::move(collection), std::move(name), std::move(url));
}

ScopedInstance ScopedInstance::New(fuchsia::sys2::RealmSyncPtr realm_proxy, std::string collection,
                                   std::string name, std::string url) {
  CreateChild(realm_proxy.get(), collection, name, std::move(url));
  auto exposed_dir =
      BindChild(realm_proxy.get(), fuchsia::sys2::ChildRef{.name = name, .collection = collection});
  return ScopedInstance(std::move(realm_proxy),
                        fuchsia::sys2::ChildRef{.name = name, .collection = collection},
                        std::move(exposed_dir));
}

std::string ScopedInstance::GetChildName() const { return child_ref_.name; }

}  // namespace sys::testing::internal
