// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_logger as logger;
use fidl_fuchsia_net as net;
use fidl_fuchsia_net_ext as net_ext;

use argh::FromArgs;

fn parse_log_level_str(value: &str) -> Result<logger::LogLevelFilter, String> {
    match &value.to_lowercase()[..] {
        "trace" => Ok(logger::LogLevelFilter::Trace),
        "debug" => Ok(logger::LogLevelFilter::Debug),
        "info" => Ok(logger::LogLevelFilter::Info),
        "warn" => Ok(logger::LogLevelFilter::Warn),
        "error" => Ok(logger::LogLevelFilter::Error),
        "fatal" => Ok(logger::LogLevelFilter::Fatal),
        _ => Err("invalid log level".to_string()),
    }
}

fn parse_ip_version_str(value: &str) -> Result<net::IpVersion, String> {
    match &value.to_lowercase()[..] {
        "ipv4" => Ok(net::IpVersion::V4),
        "ipv6" => Ok(net::IpVersion::V6),
        _ => Err("invalid IP version".to_string()),
    }
}

#[derive(FromArgs)]
/// commands for net-cli
pub struct Command {
    #[argh(subcommand)]
    pub cmd: CommandEnum,
}

#[derive(FromArgs)]
#[argh(subcommand)]
pub enum CommandEnum {
    Filter(Filter),
    Fwd(Fwd),
    If(If),
    IpFwd(IpFwd),
    Log(Log),
    Neigh(Neigh),
    Route(Route),
    Stat(Stat),
    Metric(Metric),
    Dhcp(Dhcp),
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "filter")]
/// commands for packet filter
pub struct Filter {
    #[argh(subcommand)]
    pub filter_cmd: FilterEnum,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand)]
pub enum FilterEnum {
    Disable(FilterDisable),
    Enable(FilterEnable),
    GetNatRules(FilterGetNatRules),
    GetRdrRules(FilterGetRdrRules),
    GetRules(FilterGetRules),
    IsEnabled(FilterIsEnabled),
    SetNatRules(FilterSetNatRules),
    SetRdrRules(FilterSetRdrRules),
    SetRules(FilterSetRules),
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "disable")]
/// disables the packet filter
pub struct FilterDisable {}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "enable")]
/// enables the packet filter
pub struct FilterEnable {}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "get-nat-rules")]
/// gets nat rules
pub struct FilterGetNatRules {}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "get-rdr-rules")]
/// gets rdr rules
pub struct FilterGetRdrRules {}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "get-rules")]
/// gets filter rules
pub struct FilterGetRules {}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "is-enabled")]
/// is the packet filter enabled?
pub struct FilterIsEnabled {}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "set-nat-rules")]
/// sets nat rules (see the netfilter::parser library for the NAT rules format)
pub struct FilterSetNatRules {
    #[argh(positional)]
    pub rules: String,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "set-rdr-rules")]
/// sets rdr rules (see the netfilter::parser library for the RDR rules format)
pub struct FilterSetRdrRules {
    #[argh(positional)]
    pub rules: String,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "set-rules")]
/// sets filter rules (see the netfilter::parser library for the rules format)
pub struct FilterSetRules {
    #[argh(positional)]
    pub rules: String,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "fwd")]
/// commands for forwarding tables
pub struct Fwd {
    #[argh(subcommand)]
    pub fwd_cmd: FwdEnum,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand)]
pub enum FwdEnum {
    AddDevice(FwdAddDevice),
    AddHop(FwdAddHop),
    Del(FwdDel),
    List(FwdList),
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "add-device")]
/// adds a forwarding table entry to route to a device
pub struct FwdAddDevice {
    #[argh(positional)]
    pub id: u64,
    #[argh(positional)]
    pub addr: String,
    #[argh(positional)]
    pub prefix: u8,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "add-hop")]
/// adds a forwarding table entry to route to a IP address
pub struct FwdAddHop {
    #[argh(positional)]
    pub next_hop: String,
    #[argh(positional)]
    pub addr: String,
    #[argh(positional)]
    pub prefix: u8,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "del")]
/// deletes a forwarding table entry
pub struct FwdDel {
    #[argh(positional)]
    pub addr: String,
    #[argh(positional)]
    pub prefix: u8,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "list")]
/// lists forwarding table entries
pub struct FwdList {}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "if")]
/// commands for network interfaces
pub struct If {
    #[argh(subcommand)]
    pub if_cmd: IfEnum,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand)]
pub enum IfEnum {
    Add(IfAdd),
    Addr(IfAddr),
    Bridge(IfBridge),
    Del(IfDel),
    Disable(IfDisable),
    Enable(IfEnable),
    Get(IfGet),
    List(IfList),
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "add")]
/// adds a network interface by path
pub struct IfAdd {
    // The path must yield a handle to a fuchsia.hardware.ethernet.Device interface.
    // Currently this means paths under /dev/class/ethernet.
    #[argh(positional)]
    pub path: String,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "addr")]
/// commands for updates network interface addresses
pub struct IfAddr {
    #[argh(subcommand)]
    pub addr_cmd: IfAddrEnum,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand)]
pub enum IfAddrEnum {
    Add(IfAddrAdd),
    Del(IfAddrDel),
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "add")]
/// adds an address to the network interface
pub struct IfAddrAdd {
    #[argh(positional)]
    pub id: u64,
    #[argh(positional)]
    pub addr: String,
    #[argh(positional)]
    pub prefix: u8,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "del")]
/// deletes an address from the network interface
pub struct IfAddrDel {
    #[argh(positional)]
    pub id: u64,
    #[argh(positional)]
    pub addr: String,
    #[argh(positional)]
    pub prefix: Option<u8>,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "bridge")]
/// creates a bridge between network interfaces
pub struct IfBridge {
    #[argh(positional)]
    pub ids: Vec<u32>,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "del")]
/// removes a network interface
pub struct IfDel {
    #[argh(positional)]
    pub id: u64,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "disable")]
/// disables a network interface
pub struct IfDisable {
    #[argh(positional)]
    pub id: u64,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "enable")]
/// enables a network interface
pub struct IfEnable {
    #[argh(positional)]
    pub id: u64,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "get")]
/// queries a network interface
pub struct IfGet {
    #[argh(positional)]
    pub id: u64,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "list")]
/// lists network interfaces
pub struct IfList {
    #[argh(positional)]
    pub name_pattern: Option<String>,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "ip-fwd")]
/// commands for IP forwarding
pub struct IpFwd {
    #[argh(subcommand)]
    pub ip_fwd_cmd: IpFwdEnum,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand)]
pub enum IpFwdEnum {
    Disable(IpFwdDisable),
    Enable(IpFwdEnable),
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "disable")]
/// disables IP forwarding
pub struct IpFwdDisable {}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "enable")]
/// enables IP forwarding
pub struct IpFwdEnable {}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "log")]
/// commands for logging
pub struct Log {
    #[argh(subcommand)]
    pub log_cmd: LogEnum,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand)]
pub enum LogEnum {
    SetLevel(LogSetLevel),
    SetPackets(LogSetPackets),
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "set-level")]
/// syslog severity level / loglevel
pub struct LogSetLevel {
    #[argh(positional, from_str_fn(parse_log_level_str))]
    pub log_level: logger::LogLevelFilter,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "set-packets")]
/// log packets to stdout
pub struct LogSetPackets {
    #[argh(positional)]
    pub enabled: bool,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "neigh")]
/// commands for neighbor tables
pub struct Neigh {
    #[argh(subcommand)]
    pub neigh_cmd: NeighEnum,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand)]
pub enum NeighEnum {
    Add(NeighAdd),
    Clear(NeighClear),
    Del(NeighDel),
    List(NeighList),
    Watch(NeighWatch),
    Config(NeighConfig),
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "add")]
/// adds an entry to the neighbor table
pub struct NeighAdd {
    #[argh(positional)]
    pub interface: u64,
    #[argh(positional)]
    pub ip: net_ext::IpAddress,
    #[argh(positional)]
    pub mac: net_ext::MacAddress,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "clear")]
/// removes all entries associated with a network interface from the neighbor table
pub struct NeighClear {
    #[argh(positional)]
    pub interface: u64,

    #[argh(positional, from_str_fn(parse_ip_version_str))]
    pub ip_version: net::IpVersion,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "list")]
/// lists neighbor table entries
pub struct NeighList {}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "del")]
/// removes an entry from the neighbor table
pub struct NeighDel {
    #[argh(positional)]
    pub interface: u64,
    #[argh(positional)]
    pub ip: net_ext::IpAddress,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "watch")]
/// watches neighbor table entries for state changes
pub struct NeighWatch {}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "config")]
/// commands for the Neighbor Unreachability Detection configuration
pub struct NeighConfig {
    #[argh(subcommand)]
    pub neigh_config_cmd: NeighConfigEnum,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand)]
pub enum NeighConfigEnum {
    Get(NeighGetConfig),
    Update(NeighUpdateConfig),
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "get")]
/// returns the current NUD configuration options for the provided interface
pub struct NeighGetConfig {
    #[argh(positional)]
    pub interface: u64,

    #[argh(positional, from_str_fn(parse_ip_version_str))]
    pub ip_version: net::IpVersion,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "update")]
/// updates the current NUD configuration options for the provided interface
pub struct NeighUpdateConfig {
    #[argh(positional)]
    pub interface: u64,

    #[argh(positional, from_str_fn(parse_ip_version_str))]
    pub ip_version: net::IpVersion,

    /// a base duration, in nanoseconds, for computing the random reachable
    /// time
    #[argh(option)]
    pub base_reachable_time: Option<i64>,

    /// learn `base_reachable_time` during runtime from the neighbor discovery
    /// protocol, if supported
    #[argh(option)]
    pub learn_base_reachable_time: Option<bool>,

    /// the minimum value of the random factor used for computing reachable
    /// time
    #[argh(option)]
    pub min_random_factor: Option<f32>,

    /// the maximum value of the random factor used for computing reachable
    /// time
    #[argh(option)]
    pub max_random_factor: Option<f32>,

    /// duration, in nanoseconds, between retransmissions of reachability
    /// probes in the PROBE state
    #[argh(option)]
    pub retransmit_timer: Option<i64>,

    /// learn `retransmit_timer` during runtime from the neighbor discovery
    /// protocol, if supported
    #[argh(option)]
    pub learn_retransmit_timer: Option<bool>,

    /// duration, in nanoseconds, to wait for a non-Neighbor-Discovery related
    /// protocol to reconfirm reachability after entering the DELAY state
    #[argh(option)]
    pub delay_first_probe_time: Option<i64>,

    /// the number of reachability probes to send before concluding negative
    /// reachability and deleting the entry from the INCOMPLETE state
    #[argh(option)]
    pub max_multicast_probes: Option<u32>,

    /// the number of reachability probes to send before concluding
    /// retransmissions from within the PROBE state should cease and the entry
    /// SHOULD be deleted
    #[argh(option)]
    pub max_unicast_probes: Option<u32>,

    /// if the target address is an anycast address, the stack SHOULD delay
    /// sending a response for a random time between 0 and this duration, in
    /// nanoseconds
    #[argh(option)]
    pub max_anycast_delay_time: Option<i64>,

    /// a node MAY send up to this amount of unsolicited reachability
    /// confirmations messages to all-nodes multicast address when a node
    /// determines its link-layer address has changed
    #[argh(option)]
    pub max_reachability_confirmations: Option<u32>,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "route")]
/// commands for routing tables
pub struct Route {
    #[argh(subcommand)]
    pub route_cmd: RouteEnum,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand)]
pub enum RouteEnum {
    List(RouteList),
    Add(RouteAdd),
    Del(RouteDel),
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "list")]
/// lists devices
pub struct RouteList {}

macro_rules! route_struct {
    ($ty_name:ident, $name:literal, $comment:expr) => {
        #[derive(FromArgs, Clone, Debug)]
        #[argh(subcommand, name = $name)]
        #[doc = $comment]
        pub struct $ty_name {
            #[argh(option)]
            /// the network id of the destination network
            pub destination: std::net::IpAddr,
            #[argh(option)]
            /// the netmask corresponding to destination
            pub netmask: std::net::IpAddr,
            #[argh(option)]
            /// the ip address of the first hop router
            pub gateway: Option<std::net::IpAddr>,
            #[argh(option)]
            /// the outgoing network interface id of the route
            pub nicid: u32,
            #[argh(option)]
            /// the metric for the route
            pub metric: u32,
        }

        impl Into<fidl_fuchsia_netstack::RouteTableEntry> for $ty_name {
            fn into(self) -> fidl_fuchsia_netstack::RouteTableEntry {
                let Self { destination, netmask, gateway, nicid, metric } = self;
                fidl_fuchsia_netstack::RouteTableEntry {
                    destination: fidl_fuchsia_net_ext::IpAddress(destination).into(),
                    netmask: fidl_fuchsia_net_ext::IpAddress(netmask).into(),
                    gateway: gateway
                        .map(|gateway| Box::new(fidl_fuchsia_net_ext::IpAddress(gateway).into())),
                    nicid,
                    metric,
                }
            }
        }
    };
}

// TODO(https://github.com/google/argh/issues/48): do this more sanely.
route_struct!(RouteAdd, "add", "adds a route to the route table");
route_struct!(RouteDel, "del", "deletes a route from the route table");

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "stat")]
/// commands for aggregates statistics
pub struct Stat {
    #[argh(subcommand)]
    pub stat_cmd: StatEnum,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand)]
pub enum StatEnum {
    Show(StatShow),
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "show")]
/// shows classified netstack stats
pub struct StatShow {}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "metric")]
/// commands for interface route metrics
pub struct Metric {
    #[argh(subcommand)]
    pub metric_cmd: MetricEnum,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand)]
pub enum MetricEnum {
    Set(MetricSet),
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "set")]
/// assigns a route metric to the network interface
pub struct MetricSet {
    #[argh(positional)]
    // NOTE: id is a u32 because fuchsia.netstack interfaces take u32 interface ids.
    // TODO: change id to u64 once fuchsia.netstack is no longer in use.
    pub id: u32,
    #[argh(positional)]
    pub metric: u32,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "dhcp")]
/// commands for an interfaces dhcp client
pub struct Dhcp {
    #[argh(subcommand)]
    pub dhcp_cmd: DhcpEnum,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand)]
pub enum DhcpEnum {
    Start(DhcpStart),
    Stop(DhcpStop),
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "start")]
/// starts a dhcp client on the interface
pub struct DhcpStart {
    #[argh(positional)]
    pub id: u32,
}

#[derive(FromArgs, Clone, Debug)]
#[argh(subcommand, name = "stop")]
/// stops the dhcp client on the interface
pub struct DhcpStop {
    #[argh(positional)]
    pub id: u32,
}
