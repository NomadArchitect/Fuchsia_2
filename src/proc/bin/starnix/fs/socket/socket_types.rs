// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::logging::not_implemented;
use bitflags::bitflags;
use zerocopy::AsBytes;

use crate::fs::*;
use crate::types::*;

bitflags! {
    pub struct SocketMessageFlags: u32 {
        const PEEK = MSG_PEEK;
        const DONTROUTE = MSG_DONTROUTE;
        const TRYHARD = MSG_TRYHARD;
        const CTRUNC = MSG_CTRUNC;
        const PROBE = MSG_PROBE;
        const TRUNC = MSG_TRUNC;
        const DONTWAIT = MSG_DONTWAIT;
        const EOR = MSG_EOR;
        const WAITALL = MSG_WAITALL;
        const FIN = MSG_FIN;
        const SYN = MSG_SYN;
        const CONFIRM = MSG_CONFIRM;
        const RST = MSG_RST;
        const ERRQUEUE = MSG_ERRQUEUE;
        const NOSIGNAL = MSG_NOSIGNAL;
        const MORE = MSG_MORE;
        const WAITFORONE = MSG_WAITFORONE;
        const BATCH = MSG_BATCH;
        const FASTOPEN = MSG_FASTOPEN;
        const CMSG_CLOEXEC = MSG_CMSG_CLOEXEC;
    }
}

bitflags! {
    /// The flags for shutting down sockets.
    pub struct SocketShutdownFlags: u32 {
        /// Further receptions will be disallowed.
        const READ = 1 << 0;

        /// Durther transmissions will be disallowed.
        const WRITE = 1 << 2;
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum SocketDomain {
    /// The `Unix` socket domain contains sockets that were created with the `AF_UNIX` domain. These
    /// sockets communicate locally, with other sockets on the same host machine.
    Unix,

    /// An AF_VSOCK socket for communication from a controlling operating system
    Vsock,

    /// An AF_INET socket (currently stubbed out)
    Inet,
}

impl SocketDomain {
    pub fn from_raw(raw: u16) -> Option<SocketDomain> {
        match raw {
            AF_UNIX => Some(SocketDomain::Unix),
            AF_VSOCK => Some(SocketDomain::Vsock),
            // Conflate AF_INET and AF_INET6 while they are both stubbed
            AF_INET => Some(SocketDomain::Inet),
            AF_INET6 => Some(SocketDomain::Inet),

            _ => None,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum SocketType {
    Stream,
    Datagram,
    Raw,
    SeqPacket,
}

impl SocketType {
    pub fn from_raw(raw: u32) -> Option<SocketType> {
        match raw {
            SOCK_STREAM => Some(SocketType::Stream),
            SOCK_DGRAM => Some(SocketType::Datagram),
            SOCK_RAW => Some(SocketType::Datagram),
            SOCK_SEQPACKET => Some(SocketType::SeqPacket),
            _ => None,
        }
    }

    pub fn as_raw(&self) -> u32 {
        match self {
            SocketType::Stream => SOCK_STREAM,
            SocketType::Datagram => SOCK_DGRAM,
            SocketType::Raw => SOCK_RAW,
            SocketType::SeqPacket => SOCK_SEQPACKET,
        }
    }

    pub fn is_stream(&self) -> bool {
        match self {
            SocketType::Stream | SocketType::Raw => true,
            SocketType::Datagram | SocketType::SeqPacket => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketAddress {
    /// An address in the AF_UNSPEC domain.
    #[allow(dead_code)]
    Unspecified,

    /// A `Unix` socket address contains the filesystem path that was used to bind the socket.
    Unix(FsString),

    /// An AF_VSOCK socket is just referred to by its listening port on the client
    Vsock(u32),

    /// No address for Inet sockets while stubbed
    Inet(u32),
}

pub const SA_FAMILY_SIZE: usize = std::mem::size_of::<uapi::__kernel_sa_family_t>();

impl SocketAddress {
    pub fn default_for_domain(domain: SocketDomain) -> SocketAddress {
        match domain {
            SocketDomain::Unix => SocketAddress::Unix(FsString::new()),
            SocketDomain::Vsock => SocketAddress::Vsock(0xffff),
            SocketDomain::Inet => SocketAddress::Inet(0),
        }
    }

    pub fn valid_for_domain(&self, domain: SocketDomain) -> bool {
        match self {
            SocketAddress::Unspecified => false,
            SocketAddress::Unix(_) => domain == SocketDomain::Unix,
            SocketAddress::Vsock(_) => domain == SocketDomain::Vsock,
            SocketAddress::Inet(_) => domain == SocketDomain::Inet,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            SocketAddress::Unspecified => AF_UNSPEC.to_ne_bytes().to_vec(),
            SocketAddress::Unix(name) => {
                if name.len() > 0 {
                    let template = sockaddr_un::default();
                    let path_length = std::cmp::min(template.sun_path.len() - 1, name.len());
                    let mut bytes = vec![0u8; SA_FAMILY_SIZE + path_length + 1];
                    bytes[..SA_FAMILY_SIZE].copy_from_slice(&AF_UNIX.to_ne_bytes());
                    bytes[SA_FAMILY_SIZE..(SA_FAMILY_SIZE + path_length)]
                        .copy_from_slice(&name[..path_length]);
                    bytes
                } else {
                    AF_UNIX.to_ne_bytes().to_vec()
                }
            }
            SocketAddress::Vsock(port) => {
                let mut bytes = vec![0u8; std::mem::size_of::<sockaddr_vm>()];
                let vm_addr = sockaddr_vm::new(*port);
                vm_addr.write_to(&mut bytes[..]);
                bytes
            }
            SocketAddress::Inet(_) => {
                not_implemented!("SocketAddress::to_bytes is stubbed for Inet");
                vec![]
            }
        }
    }

    pub fn is_abstract_unix(&self) -> bool {
        match self {
            SocketAddress::Unix(name) => name.first() == Some(&b'\0'),
            _ => false,
        }
    }
}
