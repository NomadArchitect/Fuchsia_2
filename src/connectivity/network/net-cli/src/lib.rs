// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{format_err, Context as _, Error};
use fidl_fuchsia_hardware_ethernet as zx_eth;
use fidl_fuchsia_inspect_deprecated as inspect;
use fidl_fuchsia_net as net;
use fidl_fuchsia_net_ext as net_ext;
use fidl_fuchsia_net_filter::FilterProxy;
use fidl_fuchsia_net_neighbor as neighbor;
use fidl_fuchsia_net_neighbor_ext as neighbor_ext;
use fidl_fuchsia_net_stack::{self as netstack, InterfaceInfo, LogProxy, StackProxy};
use fidl_fuchsia_net_stack_ext::{self as pretty, exec_fidl as stack_fidl, FidlReturn};
use fidl_fuchsia_netstack::NetstackProxy;
use fuchsia_zircon as zx;
use futures::{FutureExt as _, StreamExt as _, TryFutureExt as _, TryStreamExt as _};
use glob::glob;
use log::info;
use netfilter::FidlReturn as FilterFidlReturn;
use prettytable::{cell, format, row, Row, Table};
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::str::FromStr;

mod opts;
pub use opts::Command;

use crate::opts::*;

macro_rules! filter_fidl {
    ($method:expr, $context:expr) => {
        $method.await.transform_result().context($context)
    };
}

fn add_row(t: &mut Table, row: Row) {
    let _: &mut Row = t.add_row(row);
}

pub async fn do_root(
    Command { cmd }: Command,
    stack: StackProxy,
    netstack: NetstackProxy,
    filter: FilterProxy,
    log: LogProxy,
    controller: neighbor::ControllerProxy,
    view: neighbor::ViewProxy,
) -> Result<(), Error> {
    match cmd {
        CommandEnum::If(If { if_cmd: cmd }) => {
            do_if(cmd, &stack, &netstack).await.context("failed during if command")
        }
        CommandEnum::Fwd(Fwd { fwd_cmd: cmd }) => {
            do_fwd(cmd, stack).await.context("failed during fwd command")
        }
        CommandEnum::Route(Route { route_cmd: cmd }) => {
            do_route(cmd, netstack).await.context("failed during route command")
        }
        CommandEnum::Filter(Filter { filter_cmd: cmd }) => {
            do_filter(cmd, filter).await.context("failed during filter command")
        }
        CommandEnum::IpFwd(IpFwd { ip_fwd_cmd: cmd }) => {
            do_ip_fwd(cmd, stack).await.context("failed during ip-fwd command")
        }
        CommandEnum::Log(crate::opts::Log { log_cmd: cmd }) => {
            do_log(cmd, log).await.context("failed during log command")
        }
        CommandEnum::Stat(Stat { stat_cmd: cmd }) => {
            do_stat(cmd).await.context("failed during stat command")
        }
        CommandEnum::Metric(Metric { metric_cmd: cmd }) => {
            do_metric(cmd, netstack).await.context("failed during metric command")
        }
        CommandEnum::Dhcp(Dhcp { dhcp_cmd: cmd }) => {
            do_dhcp(cmd, netstack).await.context("failed during dhcp command")
        }
        CommandEnum::Neigh(Neigh { neigh_cmd: cmd }) => {
            do_neigh(cmd, controller, view).await.context("failed during neigh command")
        }
    }
}

fn shortlist_interfaces(name_pattern: &str, interfaces: &mut Vec<InterfaceInfo>) {
    interfaces.retain(|i| i.properties.name.contains(name_pattern))
}

async fn tabulate_interfaces_info(interfaces: Vec<InterfaceInfo>) -> Result<String, Error> {
    let mut t = Table::new();
    t.set_format(format::FormatBuilder::new().padding(2, 2).build());

    for (i, info) in interfaces.into_iter().enumerate() {
        if i > 0 {
            let () = add_row(&mut t, row![]);
        }

        let pretty::InterfaceInfo {
            id,
            properties:
                pretty::InterfaceProperties {
                    name,
                    topopath,
                    filepath,
                    mac,
                    mtu,
                    features,
                    administrative_status,
                    physical_status,
                    addresses,
                },
        } = info.into();

        let () = add_row(&mut t, row!["nicid", id]);
        let () = add_row(&mut t, row!["name", name]);
        let () = add_row(&mut t, row!["topopath", topopath]);
        let () = add_row(&mut t, row!["filepath", filepath]);

        let () = if let Some(mac) = mac {
            add_row(&mut t, row!["mac", mac])
        } else {
            add_row(&mut t, row!["mac", "-"])
        };

        let () = add_row(&mut t, row!["mtu", mtu]);
        let () = add_row(&mut t, row!["features", format!("{:?}", features)]);
        let () = add_row(
            &mut t,
            row!["status", format!("{} | {}", administrative_status, physical_status)],
        );
        for addr in addresses {
            let () = add_row(&mut t, row!["addr", addr]);
        }
    }
    Ok(t.to_string())
}

async fn do_if(
    cmd: opts::IfEnum,
    stack: &StackProxy,
    netstack: &NetstackProxy,
) -> Result<(), Error> {
    match cmd {
        IfEnum::List(IfList { name_pattern }) => {
            let mut response = stack.list_interfaces().await.context("error getting response")?;
            if let Some(name_pattern) = name_pattern {
                let () = shortlist_interfaces(&name_pattern, &mut response);
            }
            let result = tabulate_interfaces_info(response)
                .await
                .context("error tabulating interface info")?;
            println!("{}", result);
        }
        IfEnum::Add(IfAdd { path }) => {
            let dev = File::open(&path).context("failed to open device")?;
            let topological_path =
                fdio::device_get_topo_path(&dev).context("failed to get topological path")?;
            let fd = dev.as_raw_fd();
            let mut client = 0;
            zx::Status::ok(unsafe { fdio::fdio_sys::fdio_get_service_handle(fd, &mut client) })
                .context("failed to get fdio service handle")?;
            let dev = fidl::endpoints::ClientEnd::<zx_eth::DeviceMarker>::new(
                // Safe because we checked the return status above.
                zx::Channel::from(unsafe { zx::Handle::from_raw(client) }),
            );
            let id = stack_fidl!(
                stack.add_ethernet_interface(&topological_path, dev),
                "error adding interface"
            )?;
            info!("Added interface {}", id);
        }
        IfEnum::Del(IfDel { id }) => {
            let () = stack_fidl!(stack.del_ethernet_interface(id), "error removing interface")?;
            info!("Deleted interface {}", id);
        }
        IfEnum::Get(IfGet { id }) => {
            let info = stack_fidl!(stack.get_interface_info(id), "error getting interface")?;
            println!("{}", pretty::InterfaceInfo::from(info));
        }
        IfEnum::Enable(IfEnable { id }) => {
            let () = stack_fidl!(stack.enable_interface(id), "error enabling interface")?;
            info!("Interface {} enabled", id);
        }
        IfEnum::Disable(IfDisable { id }) => {
            let () = stack_fidl!(stack.disable_interface(id), "error disabling interface")?;
            info!("Interface {} disabled", id);
        }
        IfEnum::Addr(IfAddr { addr_cmd }) => match addr_cmd {
            IfAddrEnum::Add(IfAddrAdd { id, addr, prefix }) => {
                let parsed_addr = net_ext::IpAddress::from_str(&addr)?.into();
                let mut fidl_addr = net::Subnet { addr: parsed_addr, prefix_len: prefix };
                let () = stack_fidl!(
                    stack.add_interface_address(id, &mut fidl_addr),
                    "error adding interface address"
                )?;
                info!("Address {} added to interface {}", net_ext::Subnet::from(fidl_addr), id);
            }
            IfAddrEnum::Del(IfAddrDel { id, addr, prefix }) => {
                let parsed_addr = net_ext::IpAddress::from_str(&addr)?.into();
                let prefix_len = prefix.unwrap_or_else(|| match parsed_addr {
                    net::IpAddress::Ipv4(_) => 32,
                    net::IpAddress::Ipv6(_) => 128,
                });
                let mut fidl_addr = net::Subnet { addr: parsed_addr, prefix_len };
                let () = stack_fidl!(
                    stack.del_interface_address(id, &mut fidl_addr),
                    "error deleting interface address"
                )?;
                info!("Address {} deleted from interface {}", net_ext::Subnet::from(fidl_addr), id);
            }
        },
        IfEnum::Bridge(IfBridge { ids }) => {
            let (result, bridge_id) = netstack.bridge_interfaces(&ids).await?;
            if result.status != fidl_fuchsia_netstack::Status::Ok {
                return Err(anyhow::anyhow!("{:?}: {}", result.status, result.message));
            } else {
                info!("network bridge created with id {}", bridge_id);
            }
        }
    }
    Ok(())
}

async fn do_fwd(cmd: opts::FwdEnum, stack: StackProxy) -> Result<(), Error> {
    match cmd {
        FwdEnum::List(_) => {
            let response =
                stack.get_forwarding_table().await.context("error retrieving forwarding table")?;
            for entry in response {
                println!("{}", pretty::ForwardingEntry::from(entry));
            }
        }
        FwdEnum::AddDevice(FwdAddDevice { id, addr, prefix }) => {
            let mut entry = netstack::ForwardingEntry {
                subnet: net::Subnet {
                    addr: net_ext::IpAddress::from_str(&addr)?.into(),
                    prefix_len: prefix,
                },
                destination: netstack::ForwardingDestination::DeviceId(id),
            };
            let () = stack_fidl!(
                stack.add_forwarding_entry(&mut entry),
                "error adding device forwarding entry"
            )?;
            info!("Added forwarding entry for {}/{} to device {}", addr, prefix, id);
        }
        FwdEnum::AddHop(FwdAddHop { next_hop, addr, prefix }) => {
            let mut entry = netstack::ForwardingEntry {
                subnet: net::Subnet {
                    addr: net_ext::IpAddress::from_str(&addr)?.into(),
                    prefix_len: prefix,
                },
                destination: netstack::ForwardingDestination::NextHop(
                    net_ext::IpAddress::from_str(&next_hop)?.into(),
                ),
            };
            let () = stack_fidl!(
                stack.add_forwarding_entry(&mut entry),
                "error adding next-hop forwarding entry"
            )?;
            info!("Added forwarding entry for {}/{} to {}", addr, prefix, next_hop);
        }
        FwdEnum::Del(FwdDel { addr, prefix }) => {
            let mut entry = net::Subnet {
                addr: net_ext::IpAddress::from_str(&addr)?.into(),
                prefix_len: prefix,
            };
            let () = stack_fidl!(
                stack.del_forwarding_entry(&mut entry),
                "error removing forwarding entry"
            )?;
            info!("Removed forwarding entry for {}/{}", addr, prefix);
        }
    }
    Ok(())
}

async fn do_route(cmd: opts::RouteEnum, netstack: NetstackProxy) -> Result<(), Error> {
    match cmd {
        RouteEnum::List(RouteList {}) => {
            let response =
                netstack.get_route_table2().await.context("error retrieving routing table")?;

            let mut t = Table::new();
            t.set_format(format::FormatBuilder::new().padding(2, 2).build());

            t.set_titles(row!["Destination", "Netmask", "Gateway", "NICID", "Metric"]);
            for entry in response {
                let route = fidl_fuchsia_netstack_ext::RouteTableEntry2::from(entry);
                let gateway_str = match route.gateway {
                    None => "-".to_string(),
                    Some(g) => format!("{}", g),
                };
                let () = add_row(
                    &mut t,
                    row![route.destination, route.netmask, gateway_str, route.nicid, route.metric],
                );
            }

            let _lines_printed: usize = t.printstd();
            println!();
        }
        RouteEnum::Add(route) => {
            let () = with_route_table_transaction_and_entry(&netstack, |transaction| {
                transaction.add_route(&mut route.into())
            })
            .await?;
        }
        RouteEnum::Del(route) => {
            let () = with_route_table_transaction_and_entry(&netstack, |transaction| {
                transaction.del_route(&mut route.into())
            })
            .await?;
        }
    }
    Ok(())
}

async fn with_route_table_transaction_and_entry<T, F>(
    netstack: &fidl_fuchsia_netstack::NetstackProxy,
    func: T,
) -> Result<(), Error>
where
    F: core::future::Future<Output = Result<i32, fidl::Error>>,
    T: FnOnce(&fidl_fuchsia_netstack::RouteTableTransactionProxy) -> F,
{
    let (route_table, server_end) =
        fidl::endpoints::create_proxy::<fidl_fuchsia_netstack::RouteTableTransactionMarker>()?;
    let () = fuchsia_zircon::Status::ok(netstack.start_route_table_transaction(server_end).await?)?;
    let status = func(&route_table).await?;
    let () = fuchsia_zircon::Status::ok(status)?;
    Ok(())
}

async fn do_filter(cmd: opts::FilterEnum, filter: FilterProxy) -> Result<(), Error> {
    match cmd {
        FilterEnum::Enable(_) => {
            let () = filter_fidl!(filter.enable(true), "error enabling filter")?;
            info!("successfully enabled filter");
        }
        FilterEnum::Disable(_) => {
            let () = filter_fidl!(filter.enable(false), "error disabling filter")?;
            info!("successfully disabled filter");
        }
        FilterEnum::IsEnabled(_) => {
            let is_enabled = filter.is_enabled().await.context("FIDL error")?;
            println!("{:?}", is_enabled);
        }
        FilterEnum::GetRules(_) => {
            let (rules, generation) =
                filter_fidl!(filter.get_rules(), "error getting filter rules")?;
            println!("{:?} (generation {})", rules, generation);
        }
        FilterEnum::SetRules(FilterSetRules { rules }) => {
            let (_cur_rules, generation) =
                filter_fidl!(filter.get_rules(), "error getting filter rules")?;
            let mut rules = netfilter::parser::parse_str_to_rules(&rules)?;
            let () = filter_fidl!(
                filter.update_rules(&mut rules.iter_mut(), generation),
                "error setting filter rules"
            )?;
            info!("successfully set filter rules");
        }
        FilterEnum::GetNatRules(_) => {
            let (rules, generation) =
                filter_fidl!(filter.get_nat_rules(), "error getting NAT rules")?;
            println!("{:?} (generation {})", rules, generation);
        }
        FilterEnum::SetNatRules(FilterSetNatRules { rules }) => {
            let (_cur_rules, generation) =
                filter_fidl!(filter.get_nat_rules(), "error getting NAT rules")?;
            let mut rules = netfilter::parser::parse_str_to_nat_rules(&rules)?;
            let () = filter_fidl!(
                filter.update_nat_rules(&mut rules.iter_mut(), generation),
                "error setting NAT rules"
            )?;
            info!("successfully set NAT rules");
        }
        FilterEnum::GetRdrRules(_) => {
            let (rules, generation) =
                filter_fidl!(filter.get_rdr_rules(), "error getting RDR rules")?;
            println!("{:?} (generation {})", rules, generation);
        }
        FilterEnum::SetRdrRules(FilterSetRdrRules { rules }) => {
            let (_cur_rules, generation) =
                filter_fidl!(filter.get_rdr_rules(), "error getting RDR rules")?;
            let mut rules = netfilter::parser::parse_str_to_rdr_rules(&rules)?;
            let () = filter_fidl!(
                filter.update_rdr_rules(&mut rules.iter_mut(), generation),
                "error setting RDR rules"
            )?;
            info!("successfully set RDR rules");
        }
    }
    Ok(())
}

async fn do_ip_fwd(cmd: opts::IpFwdEnum, stack: StackProxy) -> Result<(), Error> {
    match cmd {
        IpFwdEnum::Enable(IpFwdEnable {}) => {
            let () = stack
                .enable_ip_forwarding()
                .await
                .context("fuchsia.net.stack/Stack.EnableIpForwarding FIDL error")?;
            info!("Enabled IP forwarding");
        }
        IpFwdEnum::Disable(IpFwdDisable {}) => {
            let () = stack
                .disable_ip_forwarding()
                .await
                .context("fuchsia.net.stack/Stack.DisableIpForwarding FIDL error")?;
            info!("Disabled IP forwarding");
        }
    }
    Ok(())
}

async fn do_log(cmd: opts::LogEnum, log: LogProxy) -> Result<(), Error> {
    match cmd {
        LogEnum::SetLevel(LogSetLevel { log_level }) => {
            let () = stack_fidl!(log.set_log_level(log_level.into()), "error setting log level")?;
            info!("log level set to {:?}", log_level);
        }
        LogEnum::SetPackets(LogSetPackets { enabled }) => {
            let () = log.set_log_packets(enabled).await.context("error setting log packets")?;
            info!("log packets set to {:?}", enabled);
        }
    }
    Ok(())
}

async fn do_stat(cmd: opts::StatEnum) -> Result<(), Error> {
    match cmd {
        StatEnum::Show(_) => {
            let mut entries = Vec::new();
            let globs = [
                "/hub",         // accessed in "sys" realm
                "/hub/r/sys/*", // accessed in "app" realm
            ]
            .iter()
            .map(|prefix| {
                format!(
                    "{}/c/netstack.cmx/*/out/diagnostics/counters/{}",
                    prefix,
                    <inspect::InspectMarker as fidl::endpoints::ServiceMarker>::NAME
                )
            })
            .collect::<Vec<_>>();
            globs.iter().try_for_each(|pattern| {
                Ok::<_, glob::PatternError>(entries.extend(glob(pattern)?))
            })?;
            if entries.is_empty() {
                let () = Err(format_err!("failed to find netstack counters in {:?}", globs))?;
            }
            let multiple = entries.len() > 1;
            for (i, entry) in entries.into_iter().enumerate() {
                let path = entry?;
                if multiple {
                    if i > 0 {
                        println!();
                    }
                    // Show the stats path for distinction.
                    println!("Stats from {}", path.display());
                }

                let path = path
                    .to_str()
                    .ok_or_else(|| format_err!("failed to convert {} to str", path.display()))?;
                let object = inspect_fidl_load::load_hierarchy_from_path(path).await?;

                let mut t = Table::new();
                t.set_format(format::FormatBuilder::new().padding(2, 2).build());
                t.set_titles(row!["Packet Count", "Classification"]);
                let () = visit_inspect_object(&mut t, "", &object);
                let _lines_printed: usize = t.printstd();
            }
        }
    }
    Ok(())
}

async fn do_metric(cmd: opts::MetricEnum, netstack: NetstackProxy) -> Result<(), Error> {
    match cmd {
        MetricEnum::Set(MetricSet { id, metric }) => {
            let result = netstack.set_interface_metric(id, metric).await?;
            if result.status != fidl_fuchsia_netstack::Status::Ok {
                Err(anyhow::anyhow!("{:?}: {}", result.status, result.message))
            } else {
                info!("interface {} metric set to {}", id, metric);
                Ok(())
            }
        }
    }
}

async fn do_dhcp(cmd: opts::DhcpEnum, netstack: NetstackProxy) -> Result<(), Error> {
    let (dhcp, server_end) =
        fidl::endpoints::create_proxy::<fidl_fuchsia_net_dhcp::ClientMarker>()?;
    match cmd {
        DhcpEnum::Start(DhcpStart { id }) => {
            let () = netstack
                .get_dhcp_client(id, server_end)
                .await?
                .map_err(fuchsia_zircon::Status::from_raw)?;
            let () = dhcp.start().await?.map_err(fuchsia_zircon::Status::from_raw)?;
            info!("dhcp client started on interface {}", id);
        }
        DhcpEnum::Stop(DhcpStop { id }) => {
            let () = netstack
                .get_dhcp_client(id, server_end)
                .await?
                .map_err(fuchsia_zircon::Status::from_raw)?;
            let () = dhcp.stop().await?.map_err(fuchsia_zircon::Status::from_raw)?;
            info!("dhcp client stopped on interface {}", id);
        }
    }
    Ok(())
}

fn visit_inspect_object(
    t: &mut Table,
    prefix: &str,
    fuchsia_inspect::reader::DiagnosticsHierarchy { name: _, properties, children, missing: _ }: &fuchsia_inspect::reader::DiagnosticsHierarchy,
) {
    use fuchsia_inspect::reader::Property::*;

    for property in properties {
        let (key, value) = match property {
            Int(key, value) => (key, value.to_string()),
            Uint(key, value) => (key, value.to_string()),
            Double(key, value) => (key, value.to_string()),
            String(_, _)
            | Bytes(_, _)
            | Bool(_, _)
            | DoubleArray(_, _)
            | IntArray(_, _)
            | UintArray(_, _)
            | StringList(_, _) => continue,
        };
        let () = add_row(t, row![r->value, format!("{}{}", prefix, key)]);
    }
    for child in children {
        let prefix = format!("{}{}/", prefix, child.name);
        visit_inspect_object(t, &prefix, child);
    }
}

async fn do_neigh(
    cmd: opts::NeighEnum,
    controller: neighbor::ControllerProxy,
    view: neighbor::ViewProxy,
) -> Result<(), Error> {
    match cmd {
        NeighEnum::Add(NeighAdd { interface, ip, mac }) => {
            let () = do_neigh_add(interface, ip.into(), mac.into(), controller)
                .await
                .context("failed during neigh add command")?;
            info!("Added entry ({}, {}) for interface {}", ip, mac, interface);
        }
        NeighEnum::Clear(NeighClear { interface, ip_version }) => {
            let () = do_neigh_clear(interface, ip_version, controller)
                .await
                .context("failed during neigh clear command")?;
            info!("Cleared entries for interface {}", interface);
        }
        NeighEnum::Del(NeighDel { interface, ip }) => {
            let () = do_neigh_del(interface, ip.into(), controller)
                .await
                .context("failed during neigh del command")?;
            info!("Deleted entry {} for interface {}", ip, interface);
        }
        NeighEnum::List(NeighList {}) => {
            let () = print_neigh_entries(false /* watch_for_changes */, view)
                .await
                .context("error listing neighbor entries")?;
        }
        NeighEnum::Watch(NeighWatch {}) => {
            let () = print_neigh_entries(true /* watch_for_changes */, view)
                .await
                .context("error watching for changes to the neighbor table")?;
        }
        NeighEnum::Config(NeighConfig { neigh_config_cmd }) => match neigh_config_cmd {
            NeighConfigEnum::Get(NeighGetConfig { interface, ip_version }) => {
                let () = print_neigh_config(interface, ip_version, view)
                    .await
                    .context("failed during neigh config get command")?;
            }
            NeighConfigEnum::Update(NeighUpdateConfig {
                interface,
                ip_version,
                base_reachable_time,
                learn_base_reachable_time,
                min_random_factor,
                max_random_factor,
                retransmit_timer,
                learn_retransmit_timer,
                delay_first_probe_time,
                max_multicast_probes,
                max_unicast_probes,
                max_anycast_delay_time,
                max_reachability_confirmations,
            }) => {
                let updates = neighbor::UnreachabilityConfig {
                    base_reachable_time,
                    learn_base_reachable_time,
                    min_random_factor,
                    max_random_factor,
                    retransmit_timer,
                    learn_retransmit_timer,
                    delay_first_probe_time,
                    max_multicast_probes,
                    max_unicast_probes,
                    max_anycast_delay_time,
                    max_reachability_confirmations,
                    ..neighbor::UnreachabilityConfig::EMPTY
                };
                let () = update_neigh_config(interface, ip_version, updates, controller)
                    .await
                    .context("failed during neigh config update command")?;
                info!("Updated config for interface {}", interface);
            }
        },
    }
    Ok(())
}

async fn do_neigh_add(
    interface: u64,
    neighbor: net::IpAddress,
    mac: net::MacAddress,
    controller: neighbor::ControllerProxy,
) -> Result<(), Error> {
    controller
        .add_entry(interface, &mut neighbor.into(), &mut mac.into())
        .await
        .context("FIDL error adding neighbor entry")?
        .map_err(zx::Status::from_raw)
        .context("error adding neighbor entry")
}

async fn do_neigh_clear(
    interface: u64,
    ip_version: net::IpVersion,
    controller: neighbor::ControllerProxy,
) -> Result<(), Error> {
    controller
        .clear_entries(interface, ip_version)
        .await
        .context("FIDL error clearing neighbor table")?
        .map_err(zx::Status::from_raw)
        .context("error clearing neighbor table")
}

async fn do_neigh_del(
    interface: u64,
    neighbor: net::IpAddress,
    controller: neighbor::ControllerProxy,
) -> Result<(), Error> {
    controller
        .remove_entry(interface, &mut neighbor.into())
        .await
        .context("FIDL error removing neighbor entry")?
        .map_err(zx::Status::from_raw)
        .context("error removing neighbor entry")
}

async fn print_neigh_entries(
    watch_for_changes: bool,
    view: neighbor::ViewProxy,
) -> Result<(), Error> {
    let (it_client, it_server) =
        fidl::endpoints::create_endpoints::<neighbor::EntryIteratorMarker>()
            .context("error creating channel for entry iterator")?;
    let it = it_client.into_proxy().context("error creating proxy to entry iterator")?;

    let () = view
        .open_entry_iterator(it_server, neighbor::EntryIteratorOptions::EMPTY)
        .context("error opening a connection to the entry iterator")?;

    neigh_entry_stream(it, watch_for_changes)
        .map_ok(|item| {
            write_neigh_entry(&mut std::io::stdout(), item, watch_for_changes)
                .context("error writing entry")
        })
        .try_fold((), |(), r| futures::future::ready(r))
        .await
}

async fn print_neigh_config(
    interface: u64,
    version: net::IpVersion,
    view: neighbor::ViewProxy,
) -> Result<(), Error> {
    let config = view
        .get_unreachability_config(interface, version)
        .await
        .context("get_unreachability_config FIDL error")?
        .map_err(fuchsia_zircon::Status::from_raw)
        .context("get_unreachability_config failed")?;

    println!("{:#?}", config);
    Ok(())
}

async fn update_neigh_config(
    interface: u64,
    version: net::IpVersion,
    updates: neighbor::UnreachabilityConfig,
    controller: neighbor::ControllerProxy,
) -> Result<(), Error> {
    controller
        .update_unreachability_config(interface, version, updates)
        .await
        .context("update_unreachability_config FIDL error")?
        .map_err(fuchsia_zircon::Status::from_raw)
        .context("update_unreachability_config failed")
}

fn neigh_entry_stream(
    iterator: neighbor::EntryIteratorProxy,
    watch_for_changes: bool,
) -> impl futures::Stream<Item = Result<neighbor::EntryIteratorItem, Error>> {
    futures::stream::try_unfold(iterator, |iterator| {
        iterator
            .get_next()
            .map_ok(|items| Some((items, iterator)))
            .map(|r| r.context("error getting items from iterator"))
    })
    .map_ok(|items| futures::stream::iter(items.into_iter().map(Ok)))
    .try_flatten()
    .take_while(move |item| {
        futures::future::ready(item.as_ref().map_or(false, |item| {
            if let neighbor::EntryIteratorItem::Idle(neighbor::IdleEvent {}) = item {
                watch_for_changes
            } else {
                true
            }
        }))
    })
}

fn write_neigh_entry<W: std::io::Write>(
    f: &mut W,
    item: neighbor::EntryIteratorItem,
    watch_for_changes: bool,
) -> Result<(), std::io::Error> {
    match item {
        neighbor::EntryIteratorItem::Existing(entry) => {
            if watch_for_changes {
                writeln!(f, "EXISTING | {}", neighbor_ext::Entry::from(entry))
            } else {
                writeln!(f, "{}", neighbor_ext::Entry::from(entry))
            }
        }
        neighbor::EntryIteratorItem::Idle(neighbor::IdleEvent {}) => writeln!(f, "IDLE"),
        neighbor::EntryIteratorItem::Added(entry) => {
            writeln!(f, "ADDED    | {}", neighbor_ext::Entry::from(entry))
        }
        neighbor::EntryIteratorItem::Changed(entry) => {
            writeln!(f, "CHANGED  | {}", neighbor_ext::Entry::from(entry))
        }
        neighbor::EntryIteratorItem::Removed(entry) => {
            writeln!(f, "REMOVED  | {}", neighbor_ext::Entry::from(entry))
        }
    }
}

#[cfg(test)]
mod tests {
    use fidl_fuchsia_net as net;
    use fuchsia_async::{self as fasync, TimeoutExt as _};
    use futures::prelude::*;
    use net_declare::{fidl_mac, fidl_subnet};
    use {super::*, fidl_fuchsia_net_stack::*, fidl_fuchsia_netstack::*};

    const SUBNET_V4: net::Subnet = fidl_subnet!("192.168.0.1/32");
    const SUBNET_V6: net::Subnet = fidl_subnet!("fd00::1/128");

    const MAC_1: net::MacAddress = fidl_mac!("01:02:03:04:05:06");
    const MAC_2: net::MacAddress = fidl_mac!("02:03:04:05:06:07");

    fn get_fake_interface(id: u64, name: &str) -> InterfaceInfo {
        InterfaceInfo {
            id,
            properties: InterfaceProperties {
                name: name.to_string(),
                topopath: "loopback".to_string(),
                filepath: "[none]".to_string(),
                mac: None,
                mtu: 65536,
                features: zx_eth::Features::Loopback,
                administrative_status: AdministrativeStatus::Enabled,
                physical_status: PhysicalStatus::Up,
                addresses: vec![],
            },
        }
    }

    fn get_fake_interfaces() -> Vec<InterfaceInfo> {
        vec![
            get_fake_interface(1, "lo"),
            get_fake_interface(10, "eth001"),
            get_fake_interface(20, "eth002"),
            get_fake_interface(30, "eth003"),
            get_fake_interface(100, "wlan001"),
            get_fake_interface(200, "wlan002"),
            get_fake_interface(300, "wlan003"),
        ]
    }

    fn shortlist_interfaces_by_nicid(name_pattern: &str) -> Vec<u64> {
        let mut interfaces = get_fake_interfaces();
        let () = shortlist_interfaces(name_pattern, &mut interfaces);
        interfaces.into_iter().map(|i| i.id).collect()
    }

    #[test]
    fn test_shortlist_interfaces() {
        assert_eq!(vec![1, 10, 20, 30, 100, 200, 300], shortlist_interfaces_by_nicid(""));
        assert_eq!(vec![0_u64; 0], shortlist_interfaces_by_nicid("no such thing"));

        assert_eq!(vec![1], shortlist_interfaces_by_nicid("lo"));
        assert_eq!(vec![10, 20, 30], shortlist_interfaces_by_nicid("eth"));
        assert_eq!(vec![10, 20, 30], shortlist_interfaces_by_nicid("th"));
        assert_eq!(vec![100, 200, 300], shortlist_interfaces_by_nicid("wlan"));
        assert_eq!(vec![10, 100], shortlist_interfaces_by_nicid("001"));
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_if_del_addr() {
        async fn next_request(
            requests: &mut StackRequestStream,
        ) -> (u64, net::Subnet, StackDelInterfaceAddressResponder) {
            requests
                .try_next()
                .await
                .expect("del interface address FIDL error")
                .expect("request stream should not have ended")
                .into_del_interface_address()
                .expect("request should be of type DelInterfaceAddress")
        }

        let (stack, mut requests) =
            fidl::endpoints::create_proxy_and_stream::<StackMarker>().unwrap();
        let (netstack, _) = fidl::endpoints::create_proxy::<NetstackMarker>().unwrap();

        // Make the first request.
        let succeeds = do_if(
            IfEnum::Addr(IfAddr {
                addr_cmd: IfAddrEnum::Del(IfAddrDel {
                    id: 1,
                    addr: net_ext::IpAddress::from(SUBNET_V4.addr).to_string(),
                    prefix: None, // The prefix should be set to the default of 32 for IPv4.
                }),
            }),
            &stack,
            &netstack,
        );
        let success_response = async {
            // Verify that the first request is as expected and return OK.
            let (id, addr, responder) = next_request(&mut requests).await;
            assert_eq!(id, 1);
            assert_eq!(addr, SUBNET_V4);
            responder.send(&mut Ok(())).map_err(anyhow::Error::new)
        };
        let ((), ()) = futures::future::try_join(success_response, succeeds)
            .await
            .expect("do_if should succeed");

        // Make the second request.
        let fails = do_if(
            IfEnum::Addr(IfAddr {
                addr_cmd: IfAddrEnum::Del(IfAddrDel {
                    id: 2,
                    addr: net_ext::IpAddress::from(SUBNET_V6.addr).to_string(),
                    prefix: None, // The prefix should be set to the default of 128 for IPv6.
                }),
            }),
            &stack,
            &netstack,
        );
        let fail_response = async {
            // Verify that the second request is as expected and return a NotFound error.
            let (id, addr, responder) = next_request(&mut requests).await;
            assert_eq!(id, 2);
            assert_eq!(addr, SUBNET_V6);
            responder
                .send(&mut Err(fidl_fuchsia_net_stack::Error::NotFound))
                .map_err(anyhow::Error::new)
        };
        let (fails_response, fails) = futures::future::join(fail_response, fails).await;
        let () = fails_response.expect("responder.send should succeed");
        let fidl_err = fails.expect_err("do_if should fail");
        let fidl_fuchsia_net_stack_ext::NetstackError(underlying_error) = fidl_err
            .root_cause()
            .downcast_ref::<fidl_fuchsia_net_stack_ext::NetstackError>()
            .expect("fidl_err should downcast to NetstackError");
        assert_eq!(*underlying_error, fidl_fuchsia_net_stack::Error::NotFound);
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_metric_set() {
        async fn next_request(
            requests: &mut NetstackRequestStream,
        ) -> (u32, u32, NetstackSetInterfaceMetricResponder) {
            requests
                .try_next()
                .await
                .expect("set interface metric FIDL error")
                .expect("request stream should not have ended")
                .into_set_interface_metric()
                .expect("request should be of type SetInterfaceMetric")
        }

        let (netstack, mut requests) =
            fidl::endpoints::create_proxy_and_stream::<NetstackMarker>().unwrap();

        // both test values have been arbitrarily selected
        let expected_id = 1;
        let expected_metric = 64;
        let succeeds = do_metric(
            MetricEnum::Set(MetricSet { id: expected_id, metric: expected_metric }),
            netstack,
        );
        let response = async move {
            // Verify that the request is as expected and return OK.
            let (id, metric, responder) = next_request(&mut requests).await;
            assert_eq!(id, expected_id);
            assert_eq!(metric, expected_metric);
            responder
                .send(&mut NetErr { status: Status::Ok, message: String::from("") })
                .map_err(anyhow::Error::new)
        };
        let ((), ()) =
            futures::future::try_join(succeeds, response).await.expect("metric set should succeed");
    }

    async fn test_do_dhcp(cmd: DhcpEnum) {
        let (netstack, mut requests) =
            fidl::endpoints::create_proxy_and_stream::<NetstackMarker>().unwrap();
        let op = do_dhcp(cmd.clone(), netstack.clone());
        let op_succeeds = async move {
            let (received_id, dhcp_requests, netstack_responder) = requests
                .try_next()
                .await
                .expect("get dhcp client FIDL error")
                .expect("request stream should not have ended")
                .into_get_dhcp_client()
                .expect("request should be of type GetDhcpClient");
            let mut dhcp_requests =
                dhcp_requests.into_stream().expect("should convert to request stream");
            let () = netstack_responder
                .send(&mut Ok(()))
                .expect("netstack_responder.send should succeed");
            match cmd {
                DhcpEnum::Start(DhcpStart { id: expected_id }) => {
                    assert_eq!(received_id, expected_id);
                    dhcp_requests
                        .try_next()
                        .await
                        .expect("start FIDL error")
                        .expect("request stream should not have ended")
                        .into_start()
                        .expect("request should be of type Start")
                        .send(&mut Ok(()))
                        .map_err(anyhow::Error::new)
                }
                DhcpEnum::Stop(DhcpStop { id: expected_id }) => {
                    assert_eq!(received_id, expected_id);
                    dhcp_requests
                        .try_next()
                        .await
                        .expect("stop FIDL error")
                        .expect("request stream should not have ended")
                        .into_stop()
                        .expect("request should be of type Stop")
                        .send(&mut Ok(()))
                        .map_err(anyhow::Error::new)
                }
            }
        };
        let ((), ()) =
            futures::future::try_join(op, op_succeeds).await.expect("dhcp command should succeed");
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_dhcp_start() {
        let () = test_do_dhcp(DhcpEnum::Start(DhcpStart { id: 1 })).await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_dhcp_stop() {
        let () = test_do_dhcp(DhcpEnum::Stop(DhcpStop { id: 1 })).await;
    }

    async fn test_modify_route(cmd: RouteEnum) {
        let (netstack, mut requests) =
            fidl::endpoints::create_proxy_and_stream::<NetstackMarker>().unwrap();
        let op = do_route(cmd.clone(), netstack.clone());
        let op_succeeds = async move {
            let (route_table_requests, netstack_responder) = requests
                .try_next()
                .await
                .expect("start route table transaction FIDL error")
                .expect("request stream should not have ended")
                .into_start_route_table_transaction()
                .expect("request should be of type StartRouteTableTransaction");
            let mut route_table_requests =
                route_table_requests.into_stream().expect("should convert to request stream");
            let () = netstack_responder
                .send(fuchsia_zircon::Status::OK.into_raw())
                .expect("netstack_responder.send should succeed");
            let () = match cmd {
                RouteEnum::List(RouteList {}) => {
                    panic!("test_modify_route should not take a List command")
                }
                RouteEnum::Add(route) => {
                    let expected_entry = route.into();
                    let (entry, responder) = route_table_requests
                        .try_next()
                        .await
                        .expect("add route FIDL error")
                        .expect("request stream should not have ended")
                        .into_add_route()
                        .expect("request should be of type AddRoute");
                    assert_eq!(entry, expected_entry);
                    responder.send(fuchsia_zircon::Status::OK.into_raw())
                }
                RouteEnum::Del(route) => {
                    let expected_entry = route.into();
                    let (entry, responder) = route_table_requests
                        .try_next()
                        .await
                        .expect("del route FIDL error")
                        .expect("request stream should not have ended")
                        .into_del_route()
                        .expect("request should be of type DelRoute");
                    assert_eq!(entry, expected_entry);
                    responder.send(fuchsia_zircon::Status::OK.into_raw())
                }
            }?;
            Ok(())
        };
        let ((), ()) =
            futures::future::try_join(op, op_succeeds).await.expect("dhcp command should succeed");
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_route_add() {
        // Test arguments have been arbitrarily selected.
        let () = test_modify_route(RouteEnum::Add(RouteAdd {
            destination: std::net::IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 0)),
            netmask: std::net::IpAddr::V4(std::net::Ipv4Addr::new(255, 255, 255, 0)),
            gateway: None,
            nicid: 2,
            metric: 100,
        }))
        .await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_route_del() {
        // Test arguments have been arbitrarily selected.
        let () = test_modify_route(RouteEnum::Del(RouteDel {
            destination: std::net::IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 0)),
            netmask: std::net::IpAddr::V4(std::net::Ipv4Addr::new(255, 255, 255, 0)),
            gateway: None,
            nicid: 2,
            metric: 100,
        }))
        .await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_bridge() {
        let (stack, _) = fidl::endpoints::create_proxy::<StackMarker>().unwrap();
        let (netstack, mut requests) =
            fidl::endpoints::create_proxy_and_stream::<NetstackMarker>().unwrap();
        // interface id test values have been selected arbitrarily
        let bridge_ifs = vec![1, 2, 3];
        let bridge_id = 4;
        let bridge = do_if(IfEnum::Bridge(IfBridge { ids: bridge_ifs.clone() }), &stack, &netstack);
        let bridge_succeeds = async move {
            let (requested_ifs, netstack_responder) = requests
                .try_next()
                .await
                .expect("bridge_interfaces FIDL error")
                .expect("request stream should not have ended")
                .into_bridge_interfaces()
                .expect("request should be of type BridgeInterfaces");
            assert_eq!(requested_ifs, bridge_ifs);
            let () = netstack_responder
                .send(
                    &mut fidl_fuchsia_netstack::NetErr {
                        status: fidl_fuchsia_netstack::Status::Ok,
                        message: String::from(""),
                    },
                    bridge_id,
                )
                .expect("responder.send should succeed");
            Ok(())
        };
        let ((), ()) = futures::future::try_join(bridge, bridge_succeeds)
            .await
            .expect("if bridge should succeed");
    }

    async fn test_get_neigh_entries(
        watch_for_changes: bool,
        batches: Vec<Vec<neighbor::EntryIteratorItem>>,
        want: String,
    ) {
        let (it, mut requests) =
            fidl::endpoints::create_proxy_and_stream::<neighbor::EntryIteratorMarker>().unwrap();

        let server = async {
            for mut items in batches {
                let responder = requests
                    .try_next()
                    .await
                    .expect("neigh FIDL error")
                    .expect("request stream should not have ended")
                    .into_get_next()
                    .expect("request should be of type GetNext");
                let () =
                    responder.send(&mut items.iter_mut()).expect("responder.send should succeed");
            }
        }
        .on_timeout(std::time::Duration::from_secs(60), || panic!("server responder timed out"));

        let client = async {
            let mut stream = neigh_entry_stream(it, watch_for_changes);

            let item_to_string = |item| {
                let mut buf = Vec::new();
                let () = write_neigh_entry(&mut buf, item, watch_for_changes)
                    .expect("write_neigh_entry should succeed");
                String::from_utf8(buf).expect("string should be UTF-8")
            };

            // Check each string sent by get_neigh_entries
            for want_line in want.lines() {
                let got = stream
                    .next()
                    .await
                    .map(|item| item_to_string(item.expect("neigh_entry_stream should succeed")));
                assert_eq!(got, Some(format!("{}\n", want_line)));
            }

            // When listing entries, the sender should close after sending all existing entries.
            if !watch_for_changes {
                match stream.next().await {
                    Some(Ok(item)) => {
                        panic!("unexpected item from stream: {}", item_to_string(item))
                    }
                    Some(Err(err)) => panic!("unexpected error from stream: {}", err),
                    None => {}
                }
            }
        };

        let ((), ()) = futures::future::join(client, server).await;
    }

    async fn test_neigh_none(watch_for_changes: bool, want: String) {
        test_get_neigh_entries(
            watch_for_changes,
            vec![vec![neighbor::EntryIteratorItem::Idle(neighbor::IdleEvent {})]],
            want,
        )
        .await
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_neigh_list_none() {
        test_neigh_none(false /* watch_for_changes */, "".to_string()).await
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_neigh_watch_none() {
        test_neigh_none(true /* watch_for_changes */, "IDLE".to_string()).await
    }

    async fn test_neigh_one(watch_for_changes: bool, want: fn(neighbor_ext::Entry) -> String) {
        fn new_entry(updated_at: i64) -> neighbor::Entry {
            neighbor::Entry {
                interface: Some(1),
                neighbor: Some(SUBNET_V4.addr),
                state: Some(neighbor::EntryState::Reachable),
                mac: Some(MAC_1),
                updated_at: Some(updated_at),
                ..neighbor::Entry::EMPTY
            }
        }

        let now = fuchsia_runtime::utc_time();
        let updated_at = {
            let past = now - zx::Duration::from_minutes(1);
            past.into_nanos()
        };

        test_get_neigh_entries(
            watch_for_changes,
            vec![vec![
                neighbor::EntryIteratorItem::Existing(new_entry(updated_at)),
                neighbor::EntryIteratorItem::Idle(neighbor::IdleEvent {}),
            ]],
            want(neighbor_ext::Entry::from(new_entry(updated_at))),
        )
        .await
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_neigh_list_one() {
        test_neigh_one(false /* watch_for_changes */, |entry| format!("{}\n", entry)).await
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_neigh_watch_one() {
        test_neigh_one(true /* watch_for_changes */, |entry| {
            format!(
                "EXISTING | {}\n\
                 IDLE\n",
                entry
            )
        })
        .await
    }

    async fn test_neigh_many(
        watch_for_changes: bool,
        want: fn(neighbor_ext::Entry, neighbor_ext::Entry) -> String,
    ) {
        fn new_entry(ip: net::IpAddress, mac: net::MacAddress, updated_at: i64) -> neighbor::Entry {
            neighbor::Entry {
                interface: Some(1),
                neighbor: Some(ip),
                state: Some(neighbor::EntryState::Reachable),
                mac: Some(mac),
                updated_at: Some(updated_at),
                ..neighbor::Entry::EMPTY
            }
        }

        let now = fuchsia_runtime::utc_time();
        let updated_at = {
            let past = now - zx::Duration::from_minutes(1);
            past.into_nanos()
        };
        let offset = zx::Duration::from_minutes(1).into_nanos();

        test_get_neigh_entries(
            watch_for_changes,
            vec![vec![
                neighbor::EntryIteratorItem::Existing(new_entry(SUBNET_V4.addr, MAC_1, updated_at)),
                neighbor::EntryIteratorItem::Existing(new_entry(
                    SUBNET_V6.addr,
                    MAC_2,
                    updated_at - offset,
                )),
                neighbor::EntryIteratorItem::Idle(neighbor::IdleEvent {}),
            ]],
            want(
                neighbor_ext::Entry::from(new_entry(SUBNET_V4.addr, MAC_1, updated_at)),
                neighbor_ext::Entry::from(new_entry(SUBNET_V6.addr, MAC_2, updated_at - offset)),
            ),
        )
        .await
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_neigh_list_many() {
        test_neigh_many(false /* watch_for_changes */, |a, b| format!("{}\n{}\n", a, b)).await
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_neigh_watch_many() {
        test_neigh_many(true /* watch_for_changes */, |a, b| {
            format!(
                "EXISTING | {}\n\
                 EXISTING | {}\n\
                 IDLE\n",
                a, b
            )
        })
        .await
    }

    const INTERFACE_ID: u64 = 1;
    const IP_VERSION: net::IpVersion = net::IpVersion::V4;

    #[fasync::run_singlethreaded(test)]
    async fn test_neigh_add() {
        let (controller, mut requests) =
            fidl::endpoints::create_proxy_and_stream::<neighbor::ControllerMarker>().unwrap();
        let neigh = do_neigh_add(INTERFACE_ID, SUBNET_V4.addr, MAC_1, controller);
        let neigh_succeeds = async {
            let (got_interface_id, got_ip_address, got_mac, responder) = requests
                .try_next()
                .await
                .expect("neigh FIDL error")
                .expect("request stream should not have ended")
                .into_add_entry()
                .expect("request should be of type AddEntry");
            assert_eq!(got_interface_id, INTERFACE_ID);
            assert_eq!(got_ip_address, SUBNET_V4.addr);
            assert_eq!(got_mac, MAC_1);
            let () = responder.send(&mut Ok(())).expect("responder.send should succeed");
            Ok(())
        };
        let ((), ()) = futures::future::try_join(neigh, neigh_succeeds)
            .await
            .expect("neigh add should succeed");
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_neigh_clear() {
        let (controller, mut requests) =
            fidl::endpoints::create_proxy_and_stream::<neighbor::ControllerMarker>().unwrap();
        let neigh = do_neigh_clear(INTERFACE_ID, IP_VERSION, controller);
        let neigh_succeeds = async {
            let (got_interface_id, got_ip_version, responder) = requests
                .try_next()
                .await
                .expect("neigh FIDL error")
                .expect("request stream should not have ended")
                .into_clear_entries()
                .expect("request should be of type ClearEntries");
            assert_eq!(got_interface_id, INTERFACE_ID);
            assert_eq!(got_ip_version, IP_VERSION);
            let () = responder.send(&mut Ok(())).expect("responder.send should succeed");
            Ok(())
        };
        let ((), ()) = futures::future::try_join(neigh, neigh_succeeds)
            .await
            .expect("neigh clear should succeed");
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_neigh_del() {
        let (controller, mut requests) =
            fidl::endpoints::create_proxy_and_stream::<neighbor::ControllerMarker>().unwrap();
        let neigh = do_neigh_del(INTERFACE_ID, SUBNET_V4.addr, controller);
        let neigh_succeeds = async {
            let (got_interface_id, got_ip_address, responder) = requests
                .try_next()
                .await
                .expect("neigh FIDL error")
                .expect("request stream should not have ended")
                .into_remove_entry()
                .expect("request should be of type RemoveEntry");
            assert_eq!(got_interface_id, INTERFACE_ID);
            assert_eq!(got_ip_address, SUBNET_V4.addr);
            let () = responder.send(&mut Ok(())).expect("responder.send should succeed");
            Ok(())
        };
        let ((), ()) = futures::future::try_join(neigh, neigh_succeeds)
            .await
            .expect("neigh remove should succeed");
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_neigh_config_get() {
        let (view, mut requests) =
            fidl::endpoints::create_proxy_and_stream::<neighbor::ViewMarker>()
                .expect("creating a request stream and proxy for testing should succeed");
        let neigh = print_neigh_config(INTERFACE_ID, IP_VERSION, view);
        let neigh_succeeds = async {
            let (got_interface_id, got_ip_version, responder) = requests
                .try_next()
                .await
                .expect("neigh FIDL error")
                .expect("request stream should not have ended")
                .into_get_unreachability_config()
                .expect("request should be of type GetUnreachabilityConfig");
            assert_eq!(got_interface_id, INTERFACE_ID);
            assert_eq!(got_ip_version, IP_VERSION);
            let () = responder
                .send(&mut Ok(neighbor::UnreachabilityConfig::EMPTY))
                .expect("responder.send should succeed");
            Ok(())
        };
        let ((), ()) = futures::future::try_join(neigh, neigh_succeeds)
            .await
            .expect("neigh config get should succeed");
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_neigh_config_update() {
        const CONFIG: neighbor::UnreachabilityConfig = neighbor::UnreachabilityConfig::EMPTY;

        let (controller, mut requests) =
            fidl::endpoints::create_proxy_and_stream::<neighbor::ControllerMarker>()
                .expect("creating a request stream and proxy for testing should succeed");
        let neigh = update_neigh_config(
            INTERFACE_ID,
            IP_VERSION,
            neighbor::UnreachabilityConfig::EMPTY,
            controller,
        );
        let neigh_succeeds = async {
            let (got_interface_id, got_ip_version, got_config, responder) = requests
                .try_next()
                .await
                .expect("neigh FIDL error")
                .expect("request stream should not have ended")
                .into_update_unreachability_config()
                .expect("request should be of type UpdateUnreachabilityConfig");
            assert_eq!(got_interface_id, INTERFACE_ID);
            assert_eq!(got_ip_version, IP_VERSION);
            assert_eq!(got_config, CONFIG);
            let () = responder.send(&mut Ok(())).expect("responder.send should succeed");
            Ok(())
        };
        let ((), ()) = futures::future::try_join(neigh, neigh_succeeds)
            .await
            .expect("neigh config update should succeed");
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_ip_fwd_enable() {
        let (stack, mut requests) =
            fidl::endpoints::create_proxy_and_stream::<netstack::StackMarker>()
                .expect("creating a request stream and proxy for testing should succeed");
        let op = do_ip_fwd(IpFwdEnum::Enable(IpFwdEnable {}), stack);
        let op_succeeds = async {
            let responder = requests
                .try_next()
                .await
                .expect("ip-fwd FIDL error")
                .expect("request stream should not have ended")
                .into_enable_ip_forwarding()
                .expect("request should be of type EnableIpForwarding");
            let () = responder.send().expect("responder.send should succeed");
            Ok(())
        };
        let ((), ()) =
            futures::future::try_join(op, op_succeeds).await.expect("fwd enable should succeed");
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_ip_fwd_disable() {
        let (stack, mut requests) =
            fidl::endpoints::create_proxy_and_stream::<netstack::StackMarker>()
                .expect("creating a request stream and proxy for testing should succeed");
        let op = do_ip_fwd(IpFwdEnum::Disable(IpFwdDisable {}), stack);
        let op_succeeds = async {
            let responder = requests
                .try_next()
                .await
                .expect("fwd FIDL error")
                .expect("request stream should not have ended")
                .into_disable_ip_forwarding()
                .expect("request should be of type DisableIpForwarding");
            let () = responder.send().expect("responder.send should succeed");
            Ok(())
        };
        let ((), ()) =
            futures::future::try_join(op, op_succeeds).await.expect("fwd disable should succeed");
    }
}
