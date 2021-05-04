// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// clang-format off
#include <Weave/DeviceLayer/internal/WeaveDeviceLayerInternal.h>
#include <Weave/DeviceLayer/ConnectivityManager.h>
#include <Weave/DeviceLayer/ThreadStackManager.h>
#include <Warm/Warm.h>
// clang-format on

#include <fuchsia/net/cpp/fidl.h>
#include <fuchsia/netstack/cpp/fidl.h>
#include <lib/syslog/cpp/macros.h>
#include <netinet/ip6.h>

#include <optional>

// ==================== WARM Platform Functions ====================

namespace nl {
namespace Weave {
namespace Warm {
namespace Platform {

namespace {
using DeviceLayer::ConnectivityMgrImpl;
using DeviceLayer::ThreadStackMgrImpl;

// Fixed name for tunnel interface.
constexpr char kTunInterfaceName[] = "weav-tun0";

// Route metric values for primary and backup tunnels. Higher priority tunnels
// have lower metric values so that they are prioritized in the routing table.
constexpr uint32_t kRouteMetric_HighPriority = 0;
constexpr uint32_t kRouteMetric_MediumPriority = 99;
constexpr uint32_t kRouteMetric_LowPriority = 999;

// Returns the interface name associated with the given interface type.
// Unsupported interface types will not populate the optional.
std::optional<std::string> GetInterfaceName(InterfaceType interface_type) {
  switch (interface_type) {
    case kInterfaceTypeThread:
      return ThreadStackMgrImpl().GetInterfaceName();
    case kInterfaceTypeTunnel:
      return kTunInterfaceName;
    case kInterfaceTypeWiFi:
      return ConnectivityMgrImpl().GetWiFiInterfaceName();
    default:
      FX_LOGS(ERROR) << "Unknown interface type: " << interface_type;
      return std::nullopt;
  }
}

// Returns the interface id associated with the given interface name. On
// failures to fetch the list, no value will be returned. When the interface
// does not exist, the interface ID '0' will be returned (it is guaranteed by
// the networking stack that all valid interface IDs are positive).
std::optional<uint64_t> GetInterfaceId(fuchsia::netstack::NetstackSyncPtr &stack_sync_ptr,
                                       std::string interface_name) {
  std::vector<fuchsia::netstack::NetInterface> ifs;

  zx_status_t status = stack_sync_ptr->GetInterfaces(&ifs);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to acquire interface list: " << zx_status_get_string(status);
    return std::nullopt;
  }

  std::vector<fuchsia::netstack::NetInterface>::iterator it = std::find_if(
      ifs.begin(), ifs.end(),
      [&](const fuchsia::netstack::NetInterface &info) { return info.name == interface_name; });
  if (it == ifs.end()) {
    FX_LOGS(ERROR) << "Failed to acquire interface id for " << interface_name;
    return 0;
  }
  return it->id;
}

}  // namespace

WEAVE_ERROR Init(WarmFabricStateDelegate *inFabricStateDelegate) { return WEAVE_NO_ERROR; }

NL_DLL_EXPORT
void CriticalSectionEnter(void) {}

NL_DLL_EXPORT
void CriticalSectionExit(void) {}

NL_DLL_EXPORT
void RequestInvokeActions(void) { ::nl::Weave::Warm::InvokeActions(); }

// Add or remove address on tunnel interface.
PlatformResult AddRemoveHostAddress(InterfaceType interface_type, const Inet::IPAddress &address,
                                    uint8_t prefix_length, bool add) {
  auto svc = nl::Weave::DeviceLayer::PlatformMgrImpl().GetComponentContextForProcess()->svc();

  // Determine interface name to add/remove from.
  std::optional<std::string> interface_name = GetInterfaceName(interface_type);
  if (!interface_name) {
    return kPlatformResultFailure;
  }

  fuchsia::netstack::NetstackSyncPtr stack_sync_ptr;
  zx_status_t status = svc->Connect(stack_sync_ptr.NewRequest());
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to connect to netstack: " << zx_status_get_string(status);
    return kPlatformResultFailure;
  }

  // Determine the interface ID to add/remove from.
  std::optional<uint64_t> interface_id = GetInterfaceId(stack_sync_ptr, interface_name.value());
  if (!add && interface_id && interface_id.value() == 0) {
    // When removing, don't report an error if the interface wasn't found. The
    // interface may already have been removed at this point.
    FX_LOGS(INFO) << "Interface " << interface_name.value() << " has already been removed.";
    return kPlatformResultSuccess;
  } else if (!interface_id) {
    return kPlatformResultFailure;
  }

  // Construct the IP address for the interface.
  fuchsia::net::IpAddress ip_addr;
  fuchsia::net::Ipv6Address ipv6_addr;

  std::memcpy(ipv6_addr.addr.data(), (uint8_t *)(address.Addr), ipv6_addr.addr.size());
  ip_addr.set_ipv6(ipv6_addr);

  // Add or remove the address from the interface.
  fuchsia::netstack::NetErr result;
  status = add ? stack_sync_ptr->SetInterfaceAddress(interface_id.value(), std::move(ip_addr),
                                                     prefix_length, &result)
               : stack_sync_ptr->RemoveInterfaceAddress(interface_id.value(), std::move(ip_addr),
                                                        prefix_length, &result);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to configure interface address to interface id "
                   << interface_id.value() << ": " << zx_status_get_string(status);
    return kPlatformResultFailure;
  } else if (result.status != fuchsia::netstack::Status::OK) {
    FX_LOGS(ERROR) << "Unable to configure interface address to interface id "
                   << interface_id.value() << ": " << result.message;
    return kPlatformResultFailure;
  }

  FX_LOGS(INFO) << (add ? "Added" : "Removed") << " host address from interface id "
                << interface_id.value();

  // If this is not a Thread interface, adding the host address is sufficient.
  // Otherwise, move onto register the on-mesh prefix.
  if (interface_type != InterfaceType::kInterfaceTypeThread) {
    return kPlatformResultSuccess;
  }

  fuchsia::lowpan::device::LookupSyncPtr device_lookup;
  fuchsia::lowpan::device::Lookup_LookupDevice_Result device_lookup_result;
  fuchsia::lowpan::device::Protocols device_protocols;
  fuchsia::lowpan::device::DeviceRouteSyncPtr route_sync_ptr;

  status = svc->Connect(device_lookup.NewRequest());
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to connect to lowpan service: " << zx_status_get_string(status);
    return kPlatformResultFailure;
  }

  device_protocols.set_device_route(route_sync_ptr.NewRequest());
  status = device_lookup->LookupDevice(interface_name.value(), std::move(device_protocols),
                                       &device_lookup_result);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to lookup device: " << zx_status_get_string(status);
    return kPlatformResultFailure;
  } else if (device_lookup_result.is_err()) {
    FX_LOGS(ERROR) << "Failed during lookup: " << static_cast<uint32_t>(device_lookup_result.err());
    return kPlatformResultFailure;
  }

  fuchsia::lowpan::Ipv6Subnet mesh_prefix_subnet;
  std::memcpy(mesh_prefix_subnet.addr.addr.data(), (uint8_t *)(address.Addr),
              mesh_prefix_subnet.addr.addr.size());
  mesh_prefix_subnet.prefix_len = prefix_length;

  fuchsia::lowpan::device::OnMeshPrefix mesh_prefix;
  mesh_prefix.set_subnet(mesh_prefix_subnet);
  mesh_prefix.set_default_route_preference(fuchsia::lowpan::device::RoutePreference::MEDIUM);
  mesh_prefix.set_stable(true);
  mesh_prefix.set_slaac_preferred(true);
  mesh_prefix.set_slaac_valid(true);

  status = add ? route_sync_ptr->RegisterOnMeshPrefix(std::move(mesh_prefix))
               : route_sync_ptr->UnregisterOnMeshPrefix(mesh_prefix.subnet());
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to " << (add ? "register" : "unregister") << " on-mesh prefix.";
    return kPlatformResultFailure;
  }

  FX_LOGS(INFO) << (add ? "Registered" : "Unregistered") << " on-mesh prefix for Thread.";
  return kPlatformResultSuccess;
}

// Add or remove route to/from forwarding table.
PlatformResult AddRemoveHostRoute(InterfaceType interface_type, const Inet::IPPrefix &prefix,
                                  RoutePriority priority, bool add) {
  auto svc = nl::Weave::DeviceLayer::PlatformMgrImpl().GetComponentContextForProcess()->svc();

  // Determine interface name to add to/remove from.
  std::optional<std::string> interface_name = GetInterfaceName(interface_type);
  if (!interface_name) {
    return kPlatformResultFailure;
  }

  fuchsia::netstack::NetstackSyncPtr stack_sync_ptr;
  zx_status_t status = svc->Connect(stack_sync_ptr.NewRequest());
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to connect to netstack: " << zx_status_get_string(status);
    return kPlatformResultFailure;
  }

  // Determine the interface ID to add/remove from.
  std::optional<uint64_t> interface_id = GetInterfaceId(stack_sync_ptr, interface_name.value());
  if (!add && interface_id && interface_id.value() == 0) {
    // When removing, don't report an error if the interface wasn't found. The
    // interface may already have been removed at this point.
    FX_LOGS(INFO) << "Interface " << interface_name.value() << " has already been removed.";
    return kPlatformResultSuccess;
  } else if (!interface_id) {
    return kPlatformResultFailure;
  }

  // Begin route table transaction to add or remove forwarding entries.
  fuchsia::netstack::RouteTableTransactionSyncPtr route_table_sync_ptr;
  zx_status_t transaction_status;
  status = stack_sync_ptr->StartRouteTableTransaction(route_table_sync_ptr.NewRequest(),
                                                      &transaction_status);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to start route table transaction: " << zx_status_get_string(status);
    return kPlatformResultFailure;
  } else if (transaction_status != ZX_OK) {
    FX_LOGS(ERROR) << "Unable to start route table transaction: "
                   << zx_status_get_string(transaction_status);
    return kPlatformResultFailure;
  }

  // Construct route table entry to add or remove.
  fuchsia::netstack::RouteTableEntry route_table_entry;
  fuchsia::net::IpAddress destination;
  fuchsia::net::IpAddress netmask;

  fuchsia::net::Ipv6Address ipv6_addr;
  std::memcpy(ipv6_addr.addr.data(), (uint8_t *)(prefix.IPAddr.Addr), ipv6_addr.addr.size());
  destination.set_ipv6(ipv6_addr);

  fuchsia::net::Ipv6Address subnet_addr;
  size_t subnet_addr_size_bytes = prefix.Length / 8;
  if (subnet_addr_size_bytes >= subnet_addr.addr.size()) {
    FX_LOGS(ERROR) << "Unexpected prefix /" << prefix.Length;
    return kPlatformResultFailure;
  }
  std::memset(subnet_addr.addr.data(), 0, subnet_addr.addr.size());
  std::memset(subnet_addr.addr.data(), 0xFF, subnet_addr_size_bytes);
  netmask.set_ipv6(subnet_addr);

  route_table_entry.destination = std::move(destination);
  route_table_entry.netmask = std::move(netmask);
  route_table_entry.nicid = interface_id.value();
  switch (priority) {
    case RoutePriority::kRoutePriorityHigh:
      route_table_entry.metric = kRouteMetric_HighPriority;
      break;
    case RoutePriority::kRoutePriorityMedium:
      route_table_entry.metric = kRouteMetric_MediumPriority;
      break;
    case RoutePriority::kRoutePriorityLow:
      route_table_entry.metric = kRouteMetric_LowPriority;
      break;
    default:
      FX_LOGS(WARNING) << "Unhandled route priority type, using lowest priority.";
      route_table_entry.metric = kRouteMetric_LowPriority;
  }

  // Start route table transaction.
  status = add ? route_table_sync_ptr->AddRoute(std::move(route_table_entry), &transaction_status)
               : route_table_sync_ptr->DelRoute(std::move(route_table_entry), &transaction_status);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to modify route: " << zx_status_get_string(status);
    return kPlatformResultFailure;
  } else if (transaction_status != ZX_OK) {
    FX_LOGS(ERROR) << "Unable to modify route: " << zx_status_get_string(transaction_status);
    return kPlatformResultFailure;
  }

  FX_LOGS(INFO) << (add ? "Added" : "Removed") << " host route to/from interface id "
                << interface_id.value();
  return kPlatformResultSuccess;
}

#if WARM_CONFIG_SUPPORT_THREAD
PlatformResult AddRemoveThreadAddress(InterfaceType inInterfaceType,
                                      const Inet::IPAddress &inAddress, bool inAdd) {
  // This will be handled during the subsequent AddRemoveHostAddress from WARM.
  return kPlatformResultSuccess;
}
#endif  // WARM_CONFIG_SUPPORT_THREAD

#if WARM_CONFIG_SUPPORT_THREAD_ROUTING
PlatformResult StartStopThreadAdvertisement(InterfaceType inInterfaceType,
                                            const Inet::IPPrefix &inPrefix, bool inStart) {
  // This is handled by the LoWPAN service, nothing to do here.
  return kPlatformResultSuccess;
}
#endif  // WARM_CONFIG_SUPPORT_THREAD_ROUTING

#if WARM_CONFIG_SUPPORT_BORDER_ROUTING
PlatformResult AddRemoveThreadRoute(InterfaceType inInterfaceType, const Inet::IPPrefix &inPrefix,
                                    RoutePriority inPriority, bool inAdd) {
  // This will be handled during the subsequent AddRemoveHostAddress from WARM.
  return kPlatformResultSuccess;
}

PlatformResult SetThreadRoutePriority(InterfaceType inInterfaceType, const Inet::IPPrefix &inPrefix,
                                      RoutePriority inPriority) {
  // This will be handled during the subsequent AddRemoveHostAddress from WARM.
  return kPlatformResultSuccess;
}
#endif  // WARM_CONFIG_SUPPORT_BORDER_ROUTING

}  // namespace Platform
}  // namespace Warm
}  // namespace Weave
}  // namespace nl
