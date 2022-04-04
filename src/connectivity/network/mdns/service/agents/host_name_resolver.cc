// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/connectivity/network/mdns/service/agents/host_name_resolver.h"

#include <lib/syslog/cpp/macros.h>
#include <lib/zx/time.h>

#include "src/connectivity/network/mdns/service/common/mdns_names.h"

namespace mdns {

HostNameResolver::HostNameResolver(MdnsAgent::Owner* owner, const std::string& host_name,
                                   zx::time timeout, Mdns::ResolveHostNameCallback callback)
    : MdnsAgent(owner),
      host_name_(host_name),
      host_full_name_(MdnsNames::HostFullName(host_name)),
      timeout_(timeout),
      callback_(std::move(callback)) {
  FX_DCHECK(callback_);
}

HostNameResolver::~HostNameResolver() {}

void HostNameResolver::Start(const std::string& local_host_full_name) {
  // Note that |host_full_name_| is the name we're trying to resolve, not the
  // name of the local host, which is the parameter to this method.

  MdnsAgent::Start(local_host_full_name);

  SendQuestion(std::make_shared<DnsQuestion>(host_full_name_, DnsType::kA),
               ReplyAddress::Multicast(Media::kBoth, IpVersions::kBoth));
  SendQuestion(std::make_shared<DnsQuestion>(host_full_name_, DnsType::kAaaa),
               ReplyAddress::Multicast(Media::kBoth, IpVersions::kBoth));

  PostTaskForTime(
      [this]() {
        if (callback_) {
          callback_(host_name_, v4_address_, v6_address_);
          callback_ = nullptr;
          RemoveSelf();
        }
      },
      timeout_);
}

void HostNameResolver::ReceiveResource(const DnsResource& resource, MdnsResourceSection section,
                                       ReplyAddress sender_address) {
  if (resource.name_.dotted_string_ != host_full_name_) {
    return;
  }

  if (resource.type_ == DnsType::kA) {
    v4_address_ = resource.a_.address_.address_;
  } else if (resource.type_ == DnsType::kAaaa) {
    v6_address_ = resource.aaaa_.address_.address_;
  }
}

void HostNameResolver::EndOfMessage() {
  if (!callback_) {
    // This can happen when a redundant response is received after the block below runs and before
    // the posted task runs, e.g. when two NICs are connected to the same LAN.
    return;
  }

  if (v4_address_ || v6_address_) {
    callback_(host_name_, v4_address_, v6_address_);
    callback_ = nullptr;
    PostTaskForTime([this]() { RemoveSelf(); }, now());
  }
}

void HostNameResolver::Quit() {
  if (callback_) {
    callback_(host_name_, v4_address_, v6_address_);
    callback_ = nullptr;
  }

  MdnsAgent::Quit();
}

}  // namespace mdns
