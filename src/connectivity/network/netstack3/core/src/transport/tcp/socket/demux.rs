// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Defines the entry point of TCP packets, by directing them into the correct
//! state machine.

use alloc::vec::Vec;
use assert_matches::assert_matches;
use core::{convert::TryFrom, num::NonZeroU16};
use log::trace;

use net_types::{ip::IpAddress, SpecifiedAddr};
use packet::{Buf, BufferMut, Nested, Serializer as _};
use packet_formats::{
    ip::IpProto,
    tcp::{TcpParseArgs, TcpSegment, TcpSegmentBuilder},
};
use thiserror::Error;

use crate::{
    ip::{
        socket::DefaultSendOptions, BufferIpTransportContext, BufferTransportIpContext, IpExt,
        TransportReceiveError,
    },
    socket::{
        address::{AddrVecIter, ConnAddr, ConnIpAddr, IpPortSpec, ListenerAddr},
        AddrVec, SocketTypeState as _, SocketTypeStateMut as _,
    },
    transport::tcp::{
        buffer::SendPayload,
        segment::Segment,
        seqnum::WindowSize,
        socket::{
            Acceptor, Connection, ConnectionId, ListenerId, MaybeListener, SocketAddr,
            TcpIpTransportContext, TcpNonSyncContext, TcpSockets, TcpSyncContext, TimerId,
        },
        state::{BufferProvider, Closed, Initial, ListenOnSegmentDisposition, State},
        Control, UserError,
    },
    Instant as _,
};

impl<C: TcpNonSyncContext> BufferProvider<C::ReceiveBuffer, C::SendBuffer> for C {
    type ActiveOpen = C::NetstackEndBuffers;

    type PassiveOpen = C::ClientEndBuffers;

    fn new_passive_open_buffers() -> (C::ReceiveBuffer, C::SendBuffer, Self::PassiveOpen) {
        <C as TcpNonSyncContext>::new_passive_open_buffers()
    }
}

impl<I, B, C, SC> BufferIpTransportContext<I, C, SC, B> for TcpIpTransportContext
where
    I: IpExt,
    B: BufferMut,
    C: TcpNonSyncContext
        + BufferProvider<
            C::ReceiveBuffer,
            C::SendBuffer,
            ActiveOpen = <C as TcpNonSyncContext>::NetstackEndBuffers,
            PassiveOpen = <C as TcpNonSyncContext>::ClientEndBuffers,
        >,
    SC: TcpSyncContext<I, C> + BufferTransportIpContext<I, C, Buf<Vec<u8>>>,
{
    fn receive_ip_packet(
        sync_ctx: &mut SC,
        ctx: &mut C,
        device: SC::DeviceId,
        remote_ip: I::RecvSrcAddr,
        local_ip: SpecifiedAddr<I::Addr>,
        mut buffer: B,
    ) -> Result<(), (B, TransportReceiveError)> {
        let remote_ip = match SpecifiedAddr::new(remote_ip.into()) {
            None => {
                // TODO(https://fxbug.dev/101993): Increment the counter.
                trace!("tcp: source address unspecified, dropping the packet");
                return Ok(());
            }
            Some(src_ip) => src_ip,
        };
        let packet =
            match buffer.parse_with::<_, TcpSegment<_>>(TcpParseArgs::new(*remote_ip, *local_ip)) {
                Ok(packet) => packet,
                Err(err) => {
                    // TODO(https://fxbug.dev/101993): Increment the counter.
                    trace!("tcp: failed parsing incoming packet {:?}", err);
                    return Ok(());
                }
            };
        let local_port = packet.dst_port();
        let remote_port = packet.src_port();
        let incoming = match Segment::try_from(packet) {
            Ok(segment) => segment,
            Err(err) => {
                // TODO(https://fxbug.dev/101993): Increment the counter.
                trace!("tcp: malformed segment {:?}", err);
                return Ok(());
            }
        };
        let now = ctx.now();

        let conn_addr =
            ConnIpAddr { local: (local_ip, local_port), remote: (remote_ip, remote_port) };

        let mut addrs_to_search =
            AddrVecIter::<IpPortSpec<I, SC::DeviceId>>::with_device(conn_addr.into(), device);

        let find_result = addrs_to_search.find_map(|addr| {
            match addr {
                // Connections are always searched before listeners because they
                // are more specific.
                AddrVec::Conn(conn_addr) => sync_ctx.with_tcp_sockets_mut(|sockets| {
                    let conn_id = sockets.socketmap.conns().get_by_addr(&conn_addr).cloned();
                    conn_id.map(|conn_id| {
                        let (Connection { acceptor: _, state, ip_sock }, _, _): (
                            _,
                            &(),
                            &ConnAddr<_, _, _, _>,
                        ) = sockets
                            .socketmap
                            .conns_mut()
                            .get_by_id_mut(&conn_id)
                            .expect("inconsistent state: invalid connection id");

                        // Note: We should avoid the clone if we can teach rustc that
                        // `ip_sock` (which is inside `state`) is disjoint from the
                        // memory that `send_ip_packet` consults inside `sync_ctx`. If
                        // that is possible we can use the reference directly.
                        let (seg, passive_open) = state.on_segment::<_, C>(incoming, now);
                        (seg.map(|seg| (seg, ip_sock.clone())), passive_open.map(|p| (conn_id, p)))
                    })
                }),
                AddrVec::Listen(listener_addr) => {
                    // If we have a listener and the incoming segment is a SYN, we
                    // allocate a new connection entry in the demuxer.
                    // TODO(https://fxbug.dev/101992): Support SYN cookies.
                    let listener_id = if let Some(id) = sync_ctx.with_tcp_sockets_mut(|sockets| {
                        sockets.socketmap.listeners().get_by_addr(&listener_addr).cloned()
                    }) {
                        id
                    } else {
                        return None;
                    };

                    let is_listener_and_can_accept = sync_ctx.with_tcp_sockets_mut(|sockets| {
                        let socketmap = &mut sockets.socketmap;
                        let (maybe_listener, (), _): &(_, _, ListenerAddr<_, _, _>) = socketmap
                            .listeners()
                            .get_by_id(&listener_id)
                            .expect("invalid listener_id");

                        let listener = match maybe_listener {
                            MaybeListener::Bound(_) => {
                                // If the socket is only bound, but not listening.
                                return false;
                            }
                            MaybeListener::Listener(listener) => listener,
                        };

                        if listener.pending.len() == listener.backlog.get() {
                            // TODO(https://fxbug.dev/101993): Increment the counter.
                            trace!(
                                "incoming SYN dropped because of the full backlog of the listener"
                            );
                            return false;
                        }

                        true
                    });

                    if !is_listener_and_can_accept {
                        return None;
                    }

                    let ip_sock = match sync_ctx.new_ip_socket(
                        ctx,
                        None,
                        Some(local_ip),
                        remote_ip,
                        IpProto::Tcp.into(),
                        DefaultSendOptions,
                    ) {
                        Ok(ip_sock) => ip_sock,
                        Err(err) => {
                            // TODO(https://fxbug.dev/101993): Increment the counter.
                            trace!(
                            "cannot construct an ip socket to the SYN originator: {:?}, ignoring",
                            err
                        );
                            return None;
                        }
                    };

                    let reply = sync_ctx.with_isn_generator_and_tcp_sockets_mut(
                        |isn, TcpSockets { port_alloc: _, inactive: _, socketmap }| {
                            let now = ctx.now();
                            let isn = isn.generate(
                                now,
                                SocketAddr { ip: ip_sock.local_ip().clone(), port: local_port },
                                SocketAddr { ip: ip_sock.remote_ip().clone(), port: remote_port },
                            );

                            match Closed::<Initial>::listen(isn).on_segment(incoming, now) {
                                ListenOnSegmentDisposition::SendSynAckAndEnterSynRcvd(
                                    syn_ack,
                                    syn_rcvd,
                                ) => {
                                    // TODO(https://fxbug.dev/102135): Inherit the socket
                                    // options from the listener.
                                    let state = State::from(syn_rcvd);
                                    let poll_send_at =
                                        state.poll_send_at().expect("no retrans timer");
                                    let conn_id = socketmap
                                        .conns_mut()
                                        .try_insert(
                                            ConnAddr {
                                                ip: ConnIpAddr {
                                                    local: (local_ip, local_port),
                                                    remote: (remote_ip, remote_port),
                                                },
                                                device: None,
                                            },
                                            Connection {
                                                acceptor: Some(Acceptor::Pending(ListenerId(
                                                    listener_id.into(),
                                                ))),
                                                state,
                                                ip_sock: ip_sock.clone(),
                                            },
                                            // TODO(https://fxbug.dev/101596): Support sharing for TCP sockets.
                                            (),
                                        )
                                        .expect("failed to create a new connection");
                                    assert_eq!(
                                        ctx.schedule_timer_instant(
                                            poll_send_at,
                                            TimerId(conn_id, I::VERSION),
                                        ),
                                        None
                                    );
                                    let (maybe_listener, _, _): (_, &(), &ListenerAddr<_, _, _>) =
                                        socketmap
                                            .listeners_mut()
                                            .get_by_id_mut(&listener_id)
                                            .expect("the listener must still be active");

                                    match maybe_listener {
                                        MaybeListener::Bound(_) => {
                                            unreachable!(
                                                "the listener must be active because we got here"
                                            );
                                        }
                                        MaybeListener::Listener(listener) => {
                                            listener.pending.push(conn_id);
                                        }
                                    }
                                    Some(syn_ack)
                                }
                                ListenOnSegmentDisposition::SendRst(rst) => Some(rst),
                                ListenOnSegmentDisposition::Ignore => None,
                            }
                        },
                    );
                    Some((reply.map(|reply| (reply, ip_sock)), None))
                }
            }
        });

        match find_result {
            None => {
                // There is no existing TCP state, pretend it is closed
                // and generate a RST if needed.
                // Per RFC 793 (https://tools.ietf.org/html/rfc793#page-21):
                // CLOSED is fictional because it represents the state when
                // there is no TCB, and therefore, no connection.
                if let Some(seg) =
                    (Closed { reason: UserError::ConnectionClosed }.on_segment(incoming))
                {
                    match sync_ctx.new_ip_socket(
                        ctx,
                        None,
                        Some(local_ip),
                        remote_ip,
                        IpProto::Tcp.into(),
                        DefaultSendOptions,
                    ) {
                        Ok(ip_sock) => {
                            let body = tcp_serialize_segment(seg, conn_addr);
                            match sync_ctx.send_ip_packet(ctx, &ip_sock, body, None) {
                                Ok(()) => {}
                                Err((body, err)) => {
                                    // TODO(https://fxbug.dev/101993): Increment the counter.
                                    trace!("tcp: failed to send ip packet {:?}: {:?}", body, err)
                                }
                            }
                        }
                        Err(err) => {
                            // TODO(https://fxbug.dev/101993): Increment the counter.
                            trace!(
                                "cannot construct an ip socket to respond RST: {:?}, ignoring",
                                err
                            );
                        }
                    }
                }
            }
            Some((reply, passive_open)) => {
                if let Some((seg, ip_sock)) = reply {
                    let body = tcp_serialize_segment(seg, conn_addr);
                    match sync_ctx.send_ip_packet(ctx, &ip_sock, body, None) {
                        Ok(()) => {}
                        Err((body, err)) => {
                            // TODO(https://fxbug.dev/101993): Increment the counter.
                            trace!("tcp: failed to send ip packet {:?}: {:?}", body, err)
                        }
                    }
                }

                if let Some((conn_id, passive_open)) = passive_open {
                    sync_ctx.with_tcp_sockets_mut(|sockets| {
                        let (conn, _, _): (_, &(), &ConnAddr<_, _, _, _>) = sockets
                            .socketmap
                            .conns_mut()
                            .get_by_id_mut(&conn_id)
                            .expect("inconsistent state: invalid connection id");
                        let acceptor_id = assert_matches!(conn, Connection {
                            acceptor: Some(Acceptor::Pending(listener_id)),
                            state: _,
                            ip_sock: _,
                        } => {
                            let listener_id = *listener_id;
                            conn.acceptor = Some(Acceptor::Ready(listener_id));
                            listener_id
                        });
                        let acceptor =
                            sockets.get_listener_by_id_mut(acceptor_id).expect("orphaned acceptee");
                        let pos = acceptor
                            .pending
                            .iter()
                            .position(|x| x == &conn_id)
                            .expect("acceptee is not found in acceptor's pending queue");
                        let conn = acceptor.pending.swap_remove(pos);
                        acceptor.ready.push_back((conn, passive_open));
                        ctx.on_new_connection(acceptor_id);
                    })
                }
            }
        }

        Ok(())
    }
}

#[derive(Error, Debug)]
#[error("Multiple mutually exclusive flags are set: syn: {syn}, fin: {fin}, rst: {rst}")]
pub(crate) struct MalformedFlags {
    syn: bool,
    fin: bool,
    rst: bool,
}

impl<'a> TryFrom<TcpSegment<&'a [u8]>> for Segment<&'a [u8]> {
    type Error = MalformedFlags;

    fn try_from(from: TcpSegment<&'a [u8]>) -> Result<Self, Self::Error> {
        if usize::from(from.syn()) + usize::from(from.fin()) + usize::from(from.rst()) > 1 {
            return Err(MalformedFlags { syn: from.syn(), fin: from.fin(), rst: from.rst() });
        }
        let syn = from.syn().then(|| Control::SYN);
        let fin = from.fin().then(|| Control::FIN);
        let rst = from.rst().then(|| Control::RST);
        let control = syn.or(fin).or(rst);

        let (to, discarded) = Segment::with_data(
            from.seq_num().into(),
            from.ack_num().map(Into::into),
            control,
            WindowSize::from_u16(from.window_size()),
            from.into_body(),
        );
        debug_assert_eq!(discarded, 0);
        Ok(to)
    }
}

pub(super) fn tcp_serialize_segment<'a, S, A>(
    segment: S,
    conn_addr: ConnIpAddr<A, NonZeroU16, NonZeroU16>,
) -> Nested<Buf<Vec<u8>>, TcpSegmentBuilder<A>>
where
    S: Into<Segment<SendPayload<'a>>>,
    A: IpAddress,
{
    let Segment { seq, ack, wnd, contents } = segment.into();
    let ConnIpAddr { local: (local_ip, local_port), remote: (remote_ip, remote_port) } = conn_addr;
    let mut builder = TcpSegmentBuilder::new(
        *local_ip,
        *remote_ip,
        local_port,
        remote_port,
        seq.into(),
        ack.map(Into::into),
        u16::try_from(u32::from(wnd)).unwrap_or(u16::MAX),
    );
    let payload = match contents.data() {
        SendPayload::Contiguous(p) => (*p).to_vec(),
        SendPayload::Straddle(p1, p2) => [*p1, *p2].concat(),
    };
    match contents.control() {
        None => {}
        Some(Control::SYN) => builder.syn(true),
        Some(Control::FIN) => builder.fin(true),
        Some(Control::RST) => builder.rst(true),
    }
    Buf::new(payload, ..).encapsulate(builder)
}
