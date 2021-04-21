// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use anyhow::{Context as _, Result};
use itertools::Itertools;
use libc;
use nix::{
    ifaddrs::{getifaddrs, InterfaceAddress},
    net::if_::InterfaceFlags,
    sys::socket::SockAddr,
};
use std::net::SocketAddr;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub trait IsLocalAddr {
    /// is_local_addr returns true if the address is not globally routable.
    fn is_local_addr(&self) -> bool;

    /// is_link_local_addr returns true if the address is an IPv6 link local address.
    fn is_link_local_addr(&self) -> bool;
}

impl IsLocalAddr for IpAddr {
    fn is_local_addr(&self) -> bool {
        match self {
            IpAddr::V4(ref ip) => ip.is_local_addr(),
            IpAddr::V6(ref ip) => ip.is_local_addr(),
        }
    }

    fn is_link_local_addr(&self) -> bool {
        match self {
            IpAddr::V4(ref ip) => ip.is_link_local_addr(),
            IpAddr::V6(ref ip) => ip.is_link_local_addr(),
        }
    }
}

impl IsLocalAddr for Ipv4Addr {
    fn is_local_addr(&self) -> bool {
        // TODO(fxbug.dev/58517): add the various RFC reserved addresses and ranges too
        match self.octets() {
            [10, _, _, _] => true,
            [127, _, _, 1] => true,
            [172, 16..=31, _, _] => true,
            [192, 168, _, _] => true,
            [169, 254, 1..=254, _] => true,
            _ => false,
        }
    }

    fn is_link_local_addr(&self) -> bool {
        false
    }
}

impl IsLocalAddr for Ipv6Addr {
    fn is_local_addr(&self) -> bool {
        let segments = self.segments();

        // localhost
        if segments[..7].iter().all(|n| *n == 0) && segments[7] == 1 {
            return true;
        }

        // ULA
        if segments[0] & 0xfe00 == 0xfc00 {
            return true;
        }

        self.is_link_local_addr()
    }

    fn is_link_local_addr(&self) -> bool {
        let segments = self.segments();

        return segments[0] & 0xffff == 0xfe80
            && segments[1] & 0xffff == 0
            && segments[2] & 0xffff == 0
            && segments[3] & 0xffff == 0;
    }
}

/// An Mcast interface is:
/// -- Not a loopback.
/// -- Up (as opposed to down).
/// -- Has mcast enabled.
/// -- Has at least one non-globally routed address.
#[derive(Debug, Hash, Clone, Eq, PartialEq)]
pub struct McastInterface {
    pub name: String,
    pub addrs: Vec<SocketAddr>,
}

impl McastInterface {
    pub fn id(&self) -> Result<u32> {
        nix::net::if_::if_nametoindex(self.name.as_str())
            .context(format!("Interface id for {}", self.name))
    }
}

fn is_local_multicast_addr(addr: &InterfaceAddress) -> bool {
    let inet_addr = match addr.address {
        Some(SockAddr::Inet(inet)) => inet,
        _ => return false,
    };

    if !(addr.flags.contains(InterfaceFlags::IFF_UP)
        && addr.flags.contains(InterfaceFlags::IFF_MULTICAST)
        && !addr.flags.contains(InterfaceFlags::IFF_LOOPBACK))
    {
        return false;
    }

    inet_addr.ip().to_std().is_local_addr()
}

#[cfg(target_os = "macos")]
fn is_not_apple_touchbar(addr: &InterfaceAddress) -> bool {
    // TOUCHBAR is the link-local IPv6 address used by the Apple Touchbar
    // interface on some MacBooks. This interface is always "up", declares
    // MULTICAST routable, and always configured with the same
    // link-local address.
    // Despite this, the interface never has a valid multicast route, and so
    // it is desirable to exclude it.
    const TOUCHBAR: IpAddr =
        IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0xaede, 0x48ff, 0xfe00, 0x1122));

    let inet_addr = match addr.address {
        Some(SockAddr::Inet(inet)) => inet,
        _ => return true,
    };

    inet_addr.ip().to_std() != TOUCHBAR
}

#[cfg(not(target_os = "macos"))]
fn is_not_apple_touchbar(_addr: &InterfaceAddress) -> bool {
    true
}

// ifaddr_to_socketaddr returns Some(std::net::SocketAddr) if ifaddr contains an inet addr, none otherwise.
fn ifaddr_to_socketaddr(ifaddr: InterfaceAddress) -> Option<std::net::SocketAddr> {
    match ifaddr.address {
        Some(SockAddr::Inet(sockaddr)) => Some(sockaddr.to_std()),
        _ => None,
    }
}

/// scope_id_to_name attempts to convert a scope_id to an interface name, otherwise it returns the
/// scopeid formatted as a string.
pub fn scope_id_to_name(scope_id: u32) -> String {
    let mut buf = vec![0; libc::IF_NAMESIZE];
    let res = unsafe { libc::if_indextoname(scope_id, buf.as_mut_ptr() as *mut libc::c_char) };
    if res.is_null() {
        format!("{}", scope_id)
    } else {
        String::from_utf8_lossy(&buf.split(|&c| c == 0u8).next().unwrap_or(&[0u8])).to_string()
    }
}

// select_mcast_interfaces iterates over a set of IterfaceAddresses,
// selecting only those that meet the McastInterface criteria (see
// McastInterface), and returns them in a McastInterface representation.
fn select_mcast_interfaces(
    iter: &mut dyn Iterator<Item = InterfaceAddress>,
) -> Vec<McastInterface> {
    iter.filter(is_local_multicast_addr)
        .filter(is_not_apple_touchbar)
        .sorted_by_key(|ifaddr| ifaddr.interface_name.to_string())
        .group_by(|ifaddr| ifaddr.interface_name.to_string())
        .into_iter()
        .map(|(name, ifaddrs)| McastInterface {
            name: name.to_string(),
            addrs: ifaddrs.filter_map(ifaddr_to_socketaddr).collect(),
        })
        .collect()
}

/// get_mcast_interfaces retrieves all local interfaces that are local
/// multicast enabled. See McastInterface for more detials.
// TODO(fxbug.dev/44855): This needs to be e2e tested.
pub fn get_mcast_interfaces() -> Result<Vec<McastInterface>> {
    Ok(select_mcast_interfaces(&mut getifaddrs().context("Failed to get all interface addresses")?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::socket::InetAddr;
    use std::str::FromStr;

    fn sockaddr(s: &str) -> SockAddr {
        SockAddr::new_inet(InetAddr::from_std(&SocketAddr::from_str(s).unwrap()))
    }

    #[test]
    fn test_scope_id_to_name_known_interface() {
        let mut ifaddrs = getifaddrs().unwrap();
        let addr = ifaddrs.next().unwrap();
        let index = nix::net::if_::if_nametoindex(addr.interface_name.as_str()).unwrap();
        assert_eq!(scope_id_to_name(index), addr.interface_name.to_string());
    }

    #[test]
    fn test_scope_id_to_name_unknown_interface() {
        let ifaddrs = getifaddrs().unwrap();
        let mut used_indices = ifaddrs
            .map(|addr| nix::net::if_::if_nametoindex(addr.interface_name.as_str()).unwrap_or(0))
            .collect::<Vec<u32>>();
        used_indices.sort();
        let unused_index = used_indices[used_indices.len() - 1] + 1;
        assert_eq!(scope_id_to_name(unused_index), format!("{}", unused_index));
    }

    #[test]
    fn test_local_interfaces_and_ids() {
        // This is an integration test. It may fail on a host system that has no
        // interfaces.
        let interfaces = get_mcast_interfaces().unwrap();
        assert!(interfaces.len() >= 1);
        for iface in &interfaces {
            // Note: could race if the host system is reconfigured in the
            // between the interface gathering above and this call, which is
            // unlikely.
            iface.id().unwrap();
        }

        // Assert that we find each interface and address from a raw getifaddrs call in the set of returned interfaces.
        for exiface in getifaddrs().unwrap() {
            if !is_local_multicast_addr(&exiface) {
                continue;
            }
            if !is_not_apple_touchbar(&exiface) {
                continue;
            }
            assert!(interfaces.iter().find(|iface| iface.name == exiface.interface_name).is_some());
            if let Some(SockAddr::Inet(exaddr)) = exiface.address {
                assert!(interfaces
                    .iter()
                    .find(|iface| {
                        iface.addrs.iter().find(|addr| **addr == exaddr.to_std()).is_some()
                    })
                    .is_some());
            }
        }
    }

    #[test]
    fn test_select_mcast_interfaces() {
        let multicast_interface = InterfaceAddress {
            interface_name: "test-interface".to_string(),
            flags: InterfaceFlags::IFF_UP | InterfaceFlags::IFF_MULTICAST,
            address: Some(sockaddr("192.168.0.1:1234")),
            netmask: Some(sockaddr("255.255.255.0:0")),
            broadcast: None,
            destination: None,
        };

        let mut down_interface = multicast_interface.clone();
        down_interface.interface_name = "down-interface".to_string();
        down_interface.flags.remove(InterfaceFlags::IFF_UP);

        let mut mult_disabled = multicast_interface.clone();
        mult_disabled.interface_name = "no_multi-interface".to_string();
        mult_disabled.flags.remove(InterfaceFlags::IFF_MULTICAST);

        let mut no_addr = multicast_interface.clone();
        no_addr.interface_name = "no_addr-interface".to_string();
        no_addr.address = None;

        let mut mult2 = multicast_interface.clone();
        mult2.interface_name = "test-interface2".to_string();

        let mut addr2 = multicast_interface.clone();
        addr2.address = Some(sockaddr("192.168.0.2:1234"));

        let interfaces =
            vec![multicast_interface, mult2, addr2, down_interface, mult_disabled, no_addr];

        let result = select_mcast_interfaces(&mut interfaces.into_iter());
        assert_eq!(2, result.len());

        let ti = result.iter().find(|mcast| mcast.name == "test-interface");
        assert!(ti.is_some());
        assert!(result.iter().find(|mcast| mcast.name == "test-interface2").is_some());

        let ti_addrs =
            ti.unwrap().addrs.iter().map(|addr| addr.to_string()).sorted().collect::<Vec<String>>();
        assert_eq!(ti_addrs, ["192.168.0.1:1234", "192.168.0.2:1234"]);
    }

    #[test]
    fn test_is_local_multicast_addr() {
        let multicast_interface = InterfaceAddress {
            interface_name: "test-interface".to_string(),
            flags: InterfaceFlags::IFF_UP | InterfaceFlags::IFF_MULTICAST,
            address: Some(sockaddr("192.168.0.1:1234")),
            netmask: Some(sockaddr("255.255.255.0:0")),
            broadcast: None,
            destination: None,
        };

        assert!(is_local_multicast_addr(&multicast_interface));

        let mut down_interface = multicast_interface.clone();
        down_interface.flags.remove(InterfaceFlags::IFF_UP);
        assert!(!is_local_multicast_addr(&down_interface));

        let mut mult_disabled = multicast_interface.clone();
        mult_disabled.flags.remove(InterfaceFlags::IFF_MULTICAST);
        assert!(!is_local_multicast_addr(&mult_disabled));

        let mut no_addr = multicast_interface.clone();
        no_addr.address = None;
        assert!(!is_local_multicast_addr(&no_addr));
    }

    #[test]
    fn test_is_local_addr() {
        let local_addresses = vec![
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 0, 1)),
            IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 1, 6, 7, 8)),
            IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1)),
        ];
        let not_local_addresses = vec![
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
            IpAddr::V6(Ipv6Addr::new(0x2607, 0xf8b0, 0x4005, 0x805, 0, 0, 0, 0x200e)),
        ];

        for addr in local_addresses {
            assert!(&addr.is_local_addr());
        }
        for addr in not_local_addresses {
            assert!(!&addr.is_local_addr());
        }
    }

    #[test]
    fn test_is_link_local_addr() {
        let link_local_addresses = vec![IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 1, 6, 7, 8))];
        let not_link_local_addresses = vec![
            IpAddr::V4(Ipv4Addr::new(192, 168, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
            IpAddr::V6(Ipv6Addr::new(0x2607, 0xf8b0, 0x4005, 0x805, 0, 0, 0, 0x200e)),
        ];

        for addr in link_local_addresses {
            assert!(&addr.is_link_local_addr());
        }
        for addr in not_link_local_addresses {
            assert!(!&addr.is_link_local_addr());
        }
    }

    #[test]
    fn test_is_local_v4() {
        let local_addresses = vec![
            Ipv4Addr::new(192, 168, 0, 1),
            Ipv4Addr::new(10, 0, 0, 1),
            Ipv4Addr::new(172, 16, 0, 1),
        ];

        let not_local_addresses = vec![
            Ipv4Addr::new(8, 8, 8, 8),
            Ipv4Addr::new(4, 4, 4, 4),
            Ipv4Addr::new(1, 1, 1, 1),
            Ipv4Addr::new(224, 1, 1, 1),
        ];

        for addr in local_addresses {
            assert!(&addr.is_local_addr());
        }
        for addr in not_local_addresses {
            assert!(!&addr.is_local_addr());
        }
    }

    #[test]
    fn test_is_local_v6() {
        let local_addresses = vec![
            Ipv6Addr::new(0xfe80, 0, 0, 0, 1, 6, 7, 8),
            Ipv6Addr::new(0xfc07, 0, 0, 0, 1, 6, 7, 8),
        ];

        let not_local_addresses = vec![
            Ipv6Addr::new(0xfe81, 0, 0, 0, 1, 6, 7, 8),
            Ipv6Addr::new(0xfe79, 0, 0, 0, 1, 6, 7, 8),
            Ipv6Addr::new(0x2607, 0xf8b0, 0x4005, 0x805, 0, 0, 0, 0x200e),
        ];

        for addr in local_addresses {
            assert!(&addr.is_local_addr());
        }
        for addr in not_local_addresses {
            assert!(!&addr.is_local_addr());
        }
    }

    #[test]
    fn test_is_not_apple_touchbar() {
        let not_touchbar = InterfaceAddress {
            interface_name: "not-touchbar".to_string(),
            flags: InterfaceFlags::IFF_UP | InterfaceFlags::IFF_MULTICAST,
            address: Some(sockaddr("[fe80::2]:1234")),
            netmask: Some(sockaddr("255.255.255.0:0")),
            broadcast: None,
            destination: None,
        };

        let touchbar = InterfaceAddress {
            interface_name: "touchbar".to_string(),
            flags: InterfaceFlags::IFF_UP | InterfaceFlags::IFF_MULTICAST,
            address: Some(sockaddr("[fe80::aede:48ff:fe00:1122]:1234")),
            netmask: Some(sockaddr("255.255.255.0:0")),
            broadcast: None,
            destination: None,
        };

        assert!(is_not_apple_touchbar(&not_touchbar));

        #[cfg(target_os = "macos")]
        assert!(!is_not_apple_touchbar(&touchbar));
        #[cfg(not(target_os = "macos"))]
        assert!(is_not_apple_touchbar(&touchbar));
    }
}
