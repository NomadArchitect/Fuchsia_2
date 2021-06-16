// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//go:build !build_with_native_toolchain
// +build !build_with_native_toolchain

package netstack

import (
	"bytes"
	"encoding/binary"
	"fmt"
	"math"
	"net"
	"reflect"
	"time"
	"unsafe"

	syslog "go.fuchsia.dev/fuchsia/src/lib/syslog/go"

	fidlnet "fidl/fuchsia/net"

	"gvisor.dev/gvisor/pkg/tcpip"
	"gvisor.dev/gvisor/pkg/tcpip/header"
	"gvisor.dev/gvisor/pkg/tcpip/network/ipv4"
	"gvisor.dev/gvisor/pkg/tcpip/network/ipv6"
	"gvisor.dev/gvisor/pkg/tcpip/transport/tcp"
	"gvisor.dev/gvisor/pkg/tcpip/transport/udp"
)

// TODO(fxbug.dev/44347) We shouldn't need any of this includes after we remove
// C structs from the wire.

// #cgo CFLAGS: -D_GNU_SOURCE
// #include <netinet/in.h>
// #include <netinet/tcp.h>
// #include <netinet/udp.h>
import "C"

// TODO(https://fxbug.dev/44347) Remove this file after ABI transition.

// Functions below are adapted from
// https://github.com/google/gvisor/blob/HEAD/pkg/sentry/socket/netstack/netstack.go
//
// At the time of writing, this command produces a reasonable diff:
//
/*
   curl -sfSL https://raw.githubusercontent.com/google/gvisor/HEAD/pkg/sentry/socket/netstack/netstack.go |
   sed s/linux/C/g | \
   sed 's/, outLen)/)/g' | \
   sed 's/(t, /(/g' | \
   sed 's/(s, /(/g' | \
   sed 's/, family,/,/g' | \
   sed 's/, skType,/, transProto,/g' | \
   diff --color --ignore-all-space --unified - src/connectivity/network/netstack/socket_conv.go
*/

const (
	// DefaultTTL is linux's default TTL. All network protocols in all stacks used
	// with this package must have this value set as their default TTL.
	DefaultTTL = 64

	sizeOfInt32 int = 4

	// Max values for sockopt TCP_KEEPIDLE and TCP_KEEPINTVL in Linux.
	//
	// https://github.com/torvalds/linux/blob/f2850dd5ee015bd7b77043f731632888887689c7/include/net/tcp.h#L156-L158
	maxTCPKeepIdle  = 32767
	maxTCPKeepIntvl = 32767
	maxTCPKeepCnt   = 127
)

func boolToInt32(v bool) int32 {
	if v {
		return 1
	}
	return 0
}

func GetSockOpt(ep tcpip.Endpoint, ns *Netstack, terminal *terminalError, netProto tcpip.NetworkProtocolNumber, transProto tcpip.TransportProtocolNumber, level, name int16) (interface{}, tcpip.Error) {
	switch level {
	case C.SOL_SOCKET:
		return getSockOptSocket(ep, ns, terminal, netProto, transProto, name)

	case C.SOL_TCP:
		return getSockOptTCP(ep, name)

	case C.SOL_IPV6:
		return getSockOptIPv6(ep, name)

	case C.SOL_IP:
		return getSockOptIP(ep, name)

	case
		C.SOL_UDP,
		C.SOL_ICMPV6,
		C.SOL_RAW,
		C.SOL_PACKET:

	default:
		_ = syslog.Infof("unimplemented getsockopt: level=%d name=%d", level, name)
	}

	return nil, &tcpip.ErrUnknownProtocol{}
}

func getSockOptSocket(ep tcpip.Endpoint, ns *Netstack, terminal *terminalError, netProto tcpip.NetworkProtocolNumber, transProto tcpip.TransportProtocolNumber, name int16) (interface{}, tcpip.Error) {
	switch name {
	case C.SO_TYPE:
		switch transProto {
		case tcp.ProtocolNumber:
			return int32(C.SOCK_STREAM), nil
		case udp.ProtocolNumber:
			return int32(C.SOCK_DGRAM), nil
		default:
			return 0, &tcpip.ErrNotSupported{}
		}

	case C.SO_DOMAIN:
		switch netProto {
		case ipv4.ProtocolNumber:
			return int32(C.AF_INET), nil
		case ipv6.ProtocolNumber:
			return int32(C.AF_INET6), nil
		default:
			return 0, &tcpip.ErrNotSupported{}
		}

	case C.SO_PROTOCOL:
		switch transProto {
		case tcp.ProtocolNumber:
			return int32(C.IPPROTO_TCP), nil
		case udp.ProtocolNumber:
			return int32(C.IPPROTO_UDP), nil
		case header.ICMPv4ProtocolNumber:
			return int32(C.IPPROTO_ICMP), nil
		case header.ICMPv6ProtocolNumber:
			return int32(C.IPPROTO_ICMPV6), nil
		default:
			return 0, &tcpip.ErrNotSupported{}
		}

	case C.SO_ERROR:
		err := func() tcpip.Error {
			terminal.mu.Lock()
			defer terminal.mu.Unlock()
			if ch := terminal.mu.ch; ch != nil {
				err := <-ch
				_ = syslog.DebugTf("SO_ERROR", "%p: err=%#v", ep, err)
				return err
			}
			err := ep.LastError()
			terminal.setConsumedLocked(err)
			_ = syslog.DebugTf("SO_ERROR", "%p: err=%#v", ep, err)
			return err
		}()
		if err == nil {
			return int32(0), nil
		}
		return int32(tcpipErrorToCode(err)), nil

	case C.SO_PEERCRED:
		return nil, &tcpip.ErrNotSupported{}

	case C.SO_PASSCRED:
		v := ep.SocketOptions().GetPassCred()
		return boolToInt32(v), nil

	case C.SO_SNDBUF:
		size := ep.SocketOptions().GetSendBufferSize()
		if size > math.MaxInt32 {
			size = math.MaxInt32
		}

		return int32(size), nil

	case C.SO_RCVBUF:
		size := ep.SocketOptions().GetReceiveBufferSize()
		if size > math.MaxInt32 {
			size = math.MaxInt32
		}

		return int32(size), nil

	case C.SO_REUSEADDR:
		v := ep.SocketOptions().GetReuseAddress()
		return boolToInt32(v), nil

	case C.SO_REUSEPORT:
		v := ep.SocketOptions().GetReusePort()
		return boolToInt32(v), nil

	case C.SO_BINDTODEVICE:
		v := ep.SocketOptions().GetBindToDevice()
		if v == 0 {
			return []byte(nil), nil
		}
		nicInfos := ns.stack.NICInfo()
		for id, info := range nicInfos {
			if id == tcpip.NICID(v) {
				return append([]byte(info.Name), 0), nil
			}
		}
		return nil, &tcpip.ErrUnknownDevice{}

	case C.SO_BROADCAST:
		v := ep.SocketOptions().GetBroadcast()
		return boolToInt32(v), nil

	case C.SO_KEEPALIVE:
		v := ep.SocketOptions().GetKeepAlive()
		return boolToInt32(v), nil

	case C.SO_LINGER:
		v := ep.SocketOptions().GetLinger()
		linger := C.struct_linger{
			l_linger: C.int(v.Timeout.Seconds()),
		}
		if v.Enabled {
			linger.l_onoff = 1
		}

		return linger, nil

	case C.SO_SNDTIMEO:
		return nil, &tcpip.ErrNotSupported{}

	case C.SO_RCVTIMEO:
		return nil, &tcpip.ErrNotSupported{}

	case C.SO_OOBINLINE:
		v := ep.SocketOptions().GetOutOfBandInline()
		return boolToInt32(v), nil

	case C.SO_NO_CHECK:
		v := ep.SocketOptions().GetNoChecksum()
		return boolToInt32(v), nil

	case C.SO_ACCEPTCONN:
		var v bool
		// From `man socket.7`, SO_ACCEPTCONN:
		//
		//   Returns a value indicating whether or not this socket has been marked
		//   to accept connections with listen(2).
		//
		// And among the options here, `listen` only makes sense on TCP sockets.
		if transProto == tcp.ProtocolNumber {
			v = tcp.EndpointState(ep.State()) == tcp.StateListen
		}
		return boolToInt32(v), nil

	default:
		_ = syslog.Infof("unimplemented getsockopt: SOL_SOCKET name=%d", name)
		return nil, &tcpip.ErrUnknownProtocolOption{}
	}
}

func getSockOptTCP(ep tcpip.Endpoint, name int16) (interface{}, tcpip.Error) {
	switch name {
	case C.TCP_NODELAY:
		return boolToInt32(!ep.SocketOptions().GetDelayOption()), nil

	case C.TCP_CORK:
		return boolToInt32(ep.SocketOptions().GetCorkOption()), nil

	case C.TCP_QUICKACK:
		return boolToInt32(ep.SocketOptions().GetQuickAck()), nil

	case C.TCP_MAXSEG:
		v, err := ep.GetSockOptInt(tcpip.MaxSegOption)
		if err != nil {
			return nil, err
		}

		return int32(v), nil

	case C.TCP_KEEPIDLE:
		var v tcpip.KeepaliveIdleOption
		if err := ep.GetSockOpt(&v); err != nil {
			return nil, err
		}

		return int32(time.Duration(v).Seconds()), nil

	case C.TCP_KEEPINTVL:
		var v tcpip.KeepaliveIntervalOption
		if err := ep.GetSockOpt(&v); err != nil {
			return nil, err
		}

		return int32(time.Duration(v).Seconds()), nil

	case C.TCP_KEEPCNT:
		v, err := ep.GetSockOptInt(tcpip.KeepaliveCountOption)
		if err != nil {
			return nil, err
		}

		return int32(v), nil

	case C.TCP_USER_TIMEOUT:
		var v tcpip.TCPUserTimeoutOption
		if err := ep.GetSockOpt(&v); err != nil {
			return nil, err
		}

		return int32(time.Duration(v).Milliseconds()), nil

	case C.TCP_CONGESTION:
		var v tcpip.CongestionControlOption
		if err := ep.GetSockOpt(&v); err != nil {
			return nil, err
		}
		// https://github.com/torvalds/linux/blob/f2850dd5ee015bd7b77043f731632888887689c7/include/net/tcp.h#L1012
		const tcpCANameMax = 16

		// Always send back the maximum length; truncation happens in the client.
		b := make([]byte, tcpCANameMax)
		_ = copy(b, v)
		return b, nil

	case C.TCP_DEFER_ACCEPT:
		var v tcpip.TCPDeferAcceptOption
		if err := ep.GetSockOpt(&v); err != nil {
			return nil, err
		}

		return int32(time.Duration(v).Seconds()), nil

	case C.TCP_INFO:
		var v tcpip.TCPInfoOption
		if err := ep.GetSockOpt(&v); err != nil {
			return nil, err
		}

		var info C.struct_tcp_info
		slice := *(*[]byte)(unsafe.Pointer(&reflect.SliceHeader{
			Data: uintptr(unsafe.Pointer(&info)),
			Len:  int(unsafe.Sizeof(info)),
			Cap:  int(unsafe.Sizeof(info)),
		}))
		for i := range slice {
			slice[i] = 0xff
		}
		info.tcpi_state = C.uint8_t(v.State)
		info.tcpi_rto = C.uint(v.RTO.Microseconds())
		info.tcpi_rtt = C.uint(v.RTT.Microseconds())
		info.tcpi_rttvar = C.uint(v.RTTVar.Microseconds())
		info.tcpi_snd_ssthresh = C.uint(v.SndSsthresh)
		info.tcpi_snd_cwnd = C.uint(v.SndCwnd)
		switch state := v.CcState; state {
		case tcpip.Open:
			info.tcpi_ca_state = C.TCP_CA_Open
		case tcpip.RTORecovery:
			info.tcpi_ca_state = C.TCP_CA_Loss
		case tcpip.FastRecovery, tcpip.SACKRecovery:
			info.tcpi_ca_state = C.TCP_CA_Recovery
		case tcpip.Disorder:
			info.tcpi_ca_state = C.TCP_CA_Disorder
		default:
			panic(fmt.Sprintf("unknown congestion control state: %d", state))
		}
		if v.ReorderSeen {
			info.tcpi_reord_seen = 1
		} else {
			info.tcpi_reord_seen = 0
		}
		return info, nil

	case C.TCP_SYNCNT:
		v, err := ep.GetSockOptInt(tcpip.TCPSynCountOption)
		if err != nil {
			return nil, err
		}
		return int32(v), nil

	case C.TCP_WINDOW_CLAMP:
		v, err := ep.GetSockOptInt(tcpip.TCPWindowClampOption)
		if err != nil {
			return nil, err
		}
		return int32(v), nil

	case C.TCP_LINGER2:
		var v tcpip.TCPLingerTimeoutOption
		if err := ep.GetSockOpt(&v); err != nil {
			return nil, err
		}
		// Match Linux by clamping to -1.
		//
		// https://github.com/torvalds/linux/blob/15bc20c/net/ipv4/tcp.c#L3216-L3218
		if v < 0 {
			return int32(-1), nil
		}
		// Linux uses this socket option to override `tcp_fin_timeout`, which is in
		// seconds.
		//
		// See the man page for details: https://man7.org/linux/man-pages/man7/tcp.7.html
		return int32(time.Duration(v) / time.Second), nil

	case
		C.TCP_CC_INFO,
		C.TCP_NOTSENT_LOWAT:

	default:
		_ = syslog.Infof("unimplemented getsockopt: SOL_TCP name=%d", name)
	}

	return nil, &tcpip.ErrUnknownProtocolOption{}
}

func getSockOptIPv6(ep tcpip.Endpoint, name int16) (interface{}, tcpip.Error) {
	switch name {
	case C.IPV6_V6ONLY:
		return boolToInt32(ep.SocketOptions().GetV6Only()), nil

	case C.IPV6_PATHMTU:

	case C.IPV6_TCLASS:
		v, err := ep.GetSockOptInt(tcpip.IPv6TrafficClassOption)
		if err != nil {
			return nil, err
		}
		return int32(v), nil

	case C.IPV6_MULTICAST_IF:
		var v tcpip.MulticastInterfaceOption
		if err := ep.GetSockOpt(&v); err != nil {
			return nil, err
		}

		return int32(v.NIC), nil

	case C.IPV6_MULTICAST_HOPS:
		v, err := ep.GetSockOptInt(tcpip.MulticastTTLOption)
		if err != nil {
			return nil, err
		}
		return int32(v), nil

	case C.IPV6_MULTICAST_LOOP:
		return boolToInt32(ep.SocketOptions().GetMulticastLoop()), nil

	case C.IPV6_RECVTCLASS:
		return boolToInt32(ep.SocketOptions().GetReceiveTClass()), nil

	default:
		_ = syslog.Infof("unimplemented getsockopt: SOL_IPV6 name=%d", name)
	}

	return nil, &tcpip.ErrUnknownProtocolOption{}
}

func getSockOptIP(ep tcpip.Endpoint, name int16) (interface{}, tcpip.Error) {
	switch name {
	case C.IP_TTL:
		v, err := ep.GetSockOptInt(tcpip.TTLOption)
		if err != nil {
			return nil, err
		}

		// Fill in default value, if needed.
		if v == 0 {
			v = DefaultTTL
		}

		return int32(v), nil

	case C.IP_MULTICAST_TTL:
		v, err := ep.GetSockOptInt(tcpip.MulticastTTLOption)
		if err != nil {
			return nil, err
		}

		return int32(v), nil

	case C.IP_MULTICAST_IF:
		var v tcpip.MulticastInterfaceOption
		if err := ep.GetSockOpt(&v); err != nil {
			return nil, err
		}

		if len(v.InterfaceAddr) == 0 {
			return []byte(net.IPv4zero.To4()), nil
		}

		return []byte((v.InterfaceAddr)), nil

	case C.IP_MULTICAST_LOOP:
		return boolToInt32(ep.SocketOptions().GetMulticastLoop()), nil

	case C.IP_TOS:
		v, err := ep.GetSockOptInt(tcpip.IPv4TOSOption)
		if err != nil {
			return nil, err
		}
		return int32(v), nil

	case C.IP_RECVTOS:
		return boolToInt32(ep.SocketOptions().GetReceiveTOS()), nil

	case C.IP_PKTINFO:
		return boolToInt32(ep.SocketOptions().GetReceivePacketInfo()), nil

	default:
		_ = syslog.Infof("unimplemented getsockopt: SOL_IP name=%d", name)
		return nil, &tcpip.ErrUnknownProtocolOption{}
	}
}

func SetSockOpt(ep tcpip.Endpoint, ns *Netstack, level, name int16, optVal []uint8) tcpip.Error {
	switch level {
	case C.SOL_SOCKET:
		return setSockOptSocket(ep, ns, name, optVal)

	case C.SOL_TCP:
		return setSockOptTCP(ep, name, optVal)

	case C.SOL_IPV6:
		return setSockOptIPv6(ep, name, optVal)

	case C.SOL_IP:
		return setSockOptIP(ep, name, optVal)

	case C.SOL_UDP,
		C.SOL_ICMPV6,
		C.SOL_RAW,
		C.SOL_PACKET:

	default:
		_ = syslog.Infof("unimplemented setsockopt: level=%d name=%d optVal=%x", level, name, optVal)
	}

	return &tcpip.ErrUnknownProtocolOption{}
}

func setSockOptSocket(ep tcpip.Endpoint, ns *Netstack, name int16, optVal []byte) tcpip.Error {
	switch name {
	case C.SO_SNDBUF:
		if len(optVal) < sizeOfInt32 {
			return &tcpip.ErrInvalidOptionValue{}
		}

		v := binary.LittleEndian.Uint32(optVal)
		ep.SocketOptions().SetSendBufferSize(int64(v), true)
		return nil

	case C.SO_RCVBUF:
		if len(optVal) < sizeOfInt32 {
			return &tcpip.ErrInvalidOptionValue{}
		}

		v := binary.LittleEndian.Uint32(optVal)
		ep.SocketOptions().SetReceiveBufferSize(int64(v), true)
		return nil

	case C.SO_REUSEADDR:
		if len(optVal) < sizeOfInt32 {
			return &tcpip.ErrInvalidOptionValue{}
		}

		v := binary.LittleEndian.Uint32(optVal)
		ep.SocketOptions().SetReuseAddress(v != 0)
		return nil

	case C.SO_REUSEPORT:
		if len(optVal) < sizeOfInt32 {
			return &tcpip.ErrInvalidOptionValue{}
		}

		v := binary.LittleEndian.Uint32(optVal)
		ep.SocketOptions().SetReusePort(v != 0)
		return nil

	case C.SO_BINDTODEVICE:
		n := bytes.IndexByte(optVal, 0)
		if n == -1 {
			n = len(optVal)
		}
		if n == 0 {
			return ep.SocketOptions().SetBindToDevice(0)
		}
		name := string(optVal[:n])
		nicInfos := ns.stack.NICInfo()
		for id, info := range nicInfos {
			if name == info.Name {
				return ep.SocketOptions().SetBindToDevice(int32(id))
			}
		}
		return &tcpip.ErrUnknownDevice{}

	case C.SO_BROADCAST:
		if len(optVal) < sizeOfInt32 {
			return &tcpip.ErrInvalidOptionValue{}
		}

		v := binary.LittleEndian.Uint32(optVal)
		ep.SocketOptions().SetBroadcast(v != 0)
		return nil

	case C.SO_PASSCRED:
		if len(optVal) < sizeOfInt32 {
			return &tcpip.ErrInvalidOptionValue{}
		}

		v := binary.LittleEndian.Uint32(optVal)
		ep.SocketOptions().SetPassCred(v != 0)
		return nil

	case C.SO_KEEPALIVE:
		if len(optVal) < sizeOfInt32 {
			return &tcpip.ErrInvalidOptionValue{}
		}

		v := binary.LittleEndian.Uint32(optVal)
		ep.SocketOptions().SetKeepAlive(v != 0)
		return nil

	case C.SO_LINGER:
		var linger C.struct_linger
		if err := linger.Unmarshal(optVal); err != nil {
			return &tcpip.ErrInvalidOptionValue{}
		}
		ep.SocketOptions().SetLinger(tcpip.LingerOption{
			Enabled: linger.l_onoff != 0,
			Timeout: time.Second * time.Duration(linger.l_linger),
		})
		return nil

	case C.SO_SNDTIMEO:
		return &tcpip.ErrNotSupported{}

	case C.SO_RCVTIMEO:
		return &tcpip.ErrNotSupported{}

	case C.SO_OOBINLINE:
		if len(optVal) < sizeOfInt32 {
			return &tcpip.ErrInvalidOptionValue{}
		}

		v := binary.LittleEndian.Uint32(optVal)
		ep.SocketOptions().SetOutOfBandInline(v != 0)
		return nil

	case C.SO_NO_CHECK:
		if len(optVal) < sizeOfInt32 {
			return &tcpip.ErrInvalidOptionValue{}
		}

		v := binary.LittleEndian.Uint32(optVal)
		ep.SocketOptions().SetNoChecksum(v != 0)
		return nil

	default:
		_ = syslog.Infof("unimplemented setsockopt: SOL_SOCKET name=%d optVal=%x", name, optVal)
		return &tcpip.ErrUnknownProtocolOption{}
	}
}

// setSockOptTCP implements SetSockOpt when level is SOL_TCP.
func setSockOptTCP(ep tcpip.Endpoint, name int16, optVal []byte) tcpip.Error {
	switch name {
	case C.TCP_NODELAY:
		if len(optVal) < sizeOfInt32 {
			return &tcpip.ErrInvalidOptionValue{}
		}

		ep.SocketOptions().SetDelayOption(binary.LittleEndian.Uint32(optVal) == 0)
		return nil

	case C.TCP_CORK:
		if len(optVal) < sizeOfInt32 {
			return &tcpip.ErrInvalidOptionValue{}
		}

		ep.SocketOptions().SetCorkOption(binary.LittleEndian.Uint32(optVal) != 0)
		return nil

	case C.TCP_QUICKACK:
		if len(optVal) < sizeOfInt32 {
			return &tcpip.ErrInvalidOptionValue{}
		}

		ep.SocketOptions().SetQuickAck(binary.LittleEndian.Uint32(optVal) != 0)
		return nil

	case C.TCP_MAXSEG:
		if len(optVal) < sizeOfInt32 {
			return &tcpip.ErrInvalidOptionValue{}
		}

		v := binary.LittleEndian.Uint32(optVal)
		return ep.SetSockOptInt(tcpip.MaxSegOption, int(v))

	case C.TCP_KEEPIDLE:
		if len(optVal) < sizeOfInt32 {
			return &tcpip.ErrInvalidOptionValue{}
		}

		v := binary.LittleEndian.Uint32(optVal)
		// https://github.com/torvalds/linux/blob/f2850dd5ee015bd7b77043f731632888887689c7/net/ipv4/tcp.c#L2991
		if v < 1 || v > maxTCPKeepIdle {
			return &tcpip.ErrInvalidOptionValue{}
		}
		opt := tcpip.KeepaliveIdleOption(time.Second * time.Duration(v))
		return ep.SetSockOpt(&opt)

	case C.TCP_KEEPINTVL:
		if len(optVal) < sizeOfInt32 {
			return &tcpip.ErrInvalidOptionValue{}
		}

		v := binary.LittleEndian.Uint32(optVal)
		// https://github.com/torvalds/linux/blob/f2850dd5ee015bd7b77043f731632888887689c7/net/ipv4/tcp.c#L3008
		if v < 1 || v > maxTCPKeepIntvl {
			return &tcpip.ErrInvalidOptionValue{}
		}
		opt := tcpip.KeepaliveIntervalOption(time.Second * time.Duration(v))
		return ep.SetSockOpt(&opt)

	case C.TCP_KEEPCNT:
		if len(optVal) < sizeOfInt32 {
			return &tcpip.ErrInvalidOptionValue{}
		}

		v := binary.LittleEndian.Uint32(optVal)
		// https://github.com/torvalds/linux/blob/f2850dd5ee015bd7b77043f731632888887689c7/net/ipv4/tcp.c#L3014
		if v < 1 || v > maxTCPKeepCnt {
			return &tcpip.ErrInvalidOptionValue{}
		}
		return ep.SetSockOptInt(tcpip.KeepaliveCountOption, int(v))

	case C.TCP_USER_TIMEOUT:
		if len(optVal) < sizeOfInt32 {
			return &tcpip.ErrInvalidOptionValue{}
		}
		v := int32(binary.LittleEndian.Uint32(optVal))
		// https://github.com/torvalds/linux/blob/33b40134e5cfbbccad7f3040d1919889537a3df7/net/ipv4/tcp.c#L3086-L3094
		if v < 0 {
			return &tcpip.ErrInvalidOptionValue{}
		}
		opt := tcpip.TCPUserTimeoutOption(time.Millisecond * time.Duration(v))
		return ep.SetSockOpt(&opt)

	case C.TCP_CONGESTION:
		opt := tcpip.CongestionControlOption(optVal)
		return ep.SetSockOpt(&opt)

	case C.TCP_DEFER_ACCEPT:
		if len(optVal) < sizeOfInt32 {
			return &tcpip.ErrInvalidOptionValue{}
		}
		v := int32(binary.LittleEndian.Uint32(optVal))
		// Use 0 if negative to match Linux.
		//
		// https://github.com/torvalds/linux/blob/33b40134e5cfbbccad7f3040d1919889537a3df7/net/ipv4/tcp.c#L3045
		if v < 0 {
			v = 0
		}
		opt := tcpip.TCPDeferAcceptOption(time.Second * time.Duration(v))
		return ep.SetSockOpt(&opt)

	case C.TCP_SYNCNT:
		if len(optVal) < sizeOfInt32 {
			return &tcpip.ErrInvalidOptionValue{}
		}
		return ep.SetSockOptInt(tcpip.TCPSynCountOption, int(binary.LittleEndian.Uint32(optVal)))

	case C.TCP_WINDOW_CLAMP:
		if len(optVal) < sizeOfInt32 {
			return &tcpip.ErrInvalidOptionValue{}
		}
		return ep.SetSockOptInt(tcpip.TCPWindowClampOption, int(binary.LittleEndian.Uint32(optVal)))

	case C.TCP_LINGER2:
		if len(optVal) < sizeOfInt32 {
			return &tcpip.ErrInvalidOptionValue{}
		}
		// Linux uses this socket option to override `tcp_fin_timeout`, which is in
		// seconds.
		//
		// See the man page for details: https://man7.org/linux/man-pages/man7/tcp.7.html
		opt := tcpip.TCPLingerTimeoutOption(time.Second * time.Duration(
			int32(binary.LittleEndian.Uint32(optVal)),
		))
		return ep.SetSockOpt(&opt)

	case C.TCP_REPAIR_OPTIONS:

	default:
		_ = syslog.Infof("unimplemented setsockopt: SOL_TCP name=%d optVal=%x", name, optVal)
	}

	return &tcpip.ErrUnknownProtocolOption{}
}

// setSockOptIPv6 implements SetSockOpt when level is SOL_IPV6.
func setSockOptIPv6(ep tcpip.Endpoint, name int16, optVal []byte) tcpip.Error {
	switch name {
	case C.IPV6_V6ONLY:
		if len(optVal) < sizeOfInt32 {
			return &tcpip.ErrInvalidOptionValue{}
		}

		ep.SocketOptions().SetV6Only(binary.LittleEndian.Uint32(optVal) != 0)
		return nil

	case C.IPV6_ADD_MEMBERSHIP, C.IPV6_DROP_MEMBERSHIP:
		var ipv6_mreq C.struct_ipv6_mreq
		if err := ipv6_mreq.Unmarshal(optVal); err != nil {
			return &tcpip.ErrInvalidOptionValue{}
		}

		o := tcpip.MembershipOption{
			NIC:           tcpip.NICID(ipv6_mreq.ipv6mr_interface),
			MulticastAddr: tcpip.Address(ipv6_mreq.ipv6mr_multiaddr.Bytes()),
		}
		switch name {
		case C.IPV6_ADD_MEMBERSHIP:
			opt := tcpip.AddMembershipOption(o)
			return ep.SetSockOpt(&opt)
		case C.IPV6_DROP_MEMBERSHIP:
			opt := tcpip.RemoveMembershipOption(o)
			return ep.SetSockOpt(&opt)
		default:
			panic("unreachable")
		}

	case C.IPV6_MULTICAST_IF:
		v, err := parseIntOrChar(optVal)
		if err != nil {
			return err
		}
		opt := tcpip.MulticastInterfaceOption{
			NIC: tcpip.NICID(v),
		}
		return ep.SetSockOpt(&opt)

	case C.IPV6_MULTICAST_HOPS:
		if len(optVal) < sizeOfInt32 {
			return &tcpip.ErrInvalidOptionValue{}
		}

		v, err := parseIntOrChar(optVal)
		if err != nil {
			return err
		}

		if v == -1 {
			// Linux translates -1 to 1.
			v = 1
		}

		if v < 0 || v > 255 {
			return &tcpip.ErrInvalidOptionValue{}
		}

		return ep.SetSockOptInt(tcpip.MulticastTTLOption, int(v))

	case C.IPV6_MULTICAST_LOOP:
		if len(optVal) < sizeOfInt32 {
			return &tcpip.ErrInvalidOptionValue{}
		}

		v, err := parseIntOrChar(optVal)
		if err != nil {
			return err
		}
		ep.SocketOptions().SetMulticastLoop(v != 0)
		return nil

	case
		C.IPV6_IPSEC_POLICY,
		C.IPV6_JOIN_ANYCAST,
		C.IPV6_LEAVE_ANYCAST,
		C.IPV6_PKTINFO,
		C.IPV6_ROUTER_ALERT,
		C.IPV6_XFRM_POLICY,
		C.MCAST_BLOCK_SOURCE,
		C.MCAST_JOIN_GROUP,
		C.MCAST_JOIN_SOURCE_GROUP,
		C.MCAST_LEAVE_GROUP,
		C.MCAST_LEAVE_SOURCE_GROUP,
		C.MCAST_UNBLOCK_SOURCE:

	case C.IPV6_TCLASS:
		if len(optVal) < sizeOfInt32 {
			return &tcpip.ErrInvalidOptionValue{}
		}
		v := int32(binary.LittleEndian.Uint32(optVal))
		if v < -1 || v > 255 {
			return &tcpip.ErrInvalidOptionValue{}
		}
		if v == -1 {
			v = 0
		}
		return ep.SetSockOptInt(tcpip.IPv6TrafficClassOption, int(v))

	case C.IPV6_RECVTCLASS:
		// Although this is a boolean int flag, linux enforces that it is not
		// a char. This is a departure for how this is handled for the
		// comparable IPv4 option.
		// https://github.com/torvalds/linux/blob/f2850dd5ee015bd7b77043f731632888887689c7/net/ipv6/ipv6_sockglue.c#L345
		if len(optVal) < sizeOfInt32 {
			return &tcpip.ErrInvalidOptionValue{}
		}
		v, err := parseIntOrChar(optVal)
		if err != nil {
			return err
		}
		ep.SocketOptions().SetReceiveTClass(v != 0)
		return nil

	default:
		func() {
			var optName string
			switch name {
			case C.IPV6_UNICAST_HOPS:
				optName = "IPV6_UNICAST_HOPS"
			case C.IPV6_HOPLIMIT:
				optName = "IPV6_HOPLIMIT"
			default:
				_ = syslog.Infof("unimplemented setsockopt: SOL_IPV6 name=%d optVal=%x", name, optVal)
				return
			}
			v, _ := parseIntOrChar(optVal)
			_ = syslog.Infof("unimplemented setsockopt(SOL_IPV6,%s,%d)", optName, v)
		}()
	}

	return &tcpip.ErrUnknownProtocolOption{}
}

// parseIntOrChar copies either a 32-bit int or an 8-bit uint out of buf.
//
// net/ipv4/ip_sockglue.c:do_ip_setsockopt does this for its socket options.
func parseIntOrChar(buf []byte) (int32, tcpip.Error) {
	if len(buf) == 0 {
		return 0, &tcpip.ErrInvalidOptionValue{}
	}

	if len(buf) >= sizeOfInt32 {
		return int32(binary.LittleEndian.Uint32(buf)), nil
	}

	return int32(buf[0]), nil
}

// setSockOptIP implements SetSockOpt when level is SOL_IP.
func setSockOptIP(ep tcpip.Endpoint, name int16, optVal []byte) tcpip.Error {
	switch name {
	case C.IP_MULTICAST_TTL:
		v, err := parseIntOrChar(optVal)
		if err != nil {
			return err
		}

		if v == -1 {
			// Linux translates -1 to 1.
			v = 1
		}

		if v < 0 || v > 255 {
			return &tcpip.ErrInvalidOptionValue{}
		}

		return ep.SetSockOptInt(tcpip.MulticastTTLOption, int(v))

	case C.IP_ADD_MEMBERSHIP, C.IP_DROP_MEMBERSHIP, C.IP_MULTICAST_IF:
		var mreqn C.struct_ip_mreqn

		switch len(optVal) {
		case C.sizeof_struct_ip_mreq:
			var mreq C.struct_ip_mreq
			if err := mreq.Unmarshal(optVal); err != nil {
				return &tcpip.ErrInvalidOptionValue{}
			}

			mreqn.imr_multiaddr = mreq.imr_multiaddr
			mreqn.imr_address = mreq.imr_interface

		case C.sizeof_struct_ip_mreqn:
			if err := mreqn.Unmarshal(optVal); err != nil {
				return &tcpip.ErrInvalidOptionValue{}
			}

		case C.sizeof_struct_in_addr:
			if name == C.IP_MULTICAST_IF {
				copy(mreqn.imr_address.Bytes(), optVal)
				break
			}
			fallthrough

		default:
			return &tcpip.ErrInvalidOptionValue{}
		}

		switch name {
		case C.IP_ADD_MEMBERSHIP, C.IP_DROP_MEMBERSHIP:
			o := tcpip.MembershipOption{
				NIC:           tcpip.NICID(mreqn.imr_ifindex),
				MulticastAddr: tcpip.Address(mreqn.imr_multiaddr.Bytes()),
				InterfaceAddr: tcpip.Address(mreqn.imr_address.Bytes()),
			}

			switch name {
			case C.IP_ADD_MEMBERSHIP:
				opt := tcpip.AddMembershipOption(o)
				return ep.SetSockOpt(&opt)

			case C.IP_DROP_MEMBERSHIP:
				opt := tcpip.RemoveMembershipOption(o)
				return ep.SetSockOpt(&opt)

			default:
				panic("unreachable")
			}

		case C.IP_MULTICAST_IF:
			interfaceAddr := mreqn.imr_address.Bytes()
			if isZeros(interfaceAddr) {
				interfaceAddr = nil
			}

			return ep.SetSockOpt(&tcpip.MulticastInterfaceOption{
				NIC:           tcpip.NICID(mreqn.imr_ifindex),
				InterfaceAddr: tcpip.Address(interfaceAddr),
			})

		default:
			panic("unreachable")
		}

	case C.IP_MULTICAST_LOOP:
		v, err := parseIntOrChar(optVal)
		if err != nil {
			return err
		}

		ep.SocketOptions().SetMulticastLoop(v != 0)
		return nil

	case C.MCAST_JOIN_GROUP:
		// FIXME: Disallow IP-level multicast group options by
		// default. These will need to be supported by appropriately plumbing
		// the level through to the network stack (if at all). However, we
		// still allow setting TTL, and multicast-enable/disable type options.
		return &tcpip.ErrNotSupported{}

	case C.IP_TTL:
		v, err := parseIntOrChar(optVal)
		if err != nil {
			return err
		}
		// -1 means default TTL.
		if v == -1 {
			v = 0
		} else if v < 1 || v > 255 {
			return &tcpip.ErrInvalidOptionValue{}
		}
		return ep.SetSockOptInt(tcpip.TTLOption, int(v))

	case C.IP_TOS:
		if len(optVal) == 0 {
			return nil
		}
		v, err := parseIntOrChar(optVal)
		if err != nil {
			return err
		}
		return ep.SetSockOptInt(tcpip.IPv4TOSOption, int(v))

	case C.IP_RECVTOS:
		v, err := parseIntOrChar(optVal)
		if err != nil {
			return err
		}
		ep.SocketOptions().SetReceiveTOS(v != 0)
		return nil

	case C.IP_PKTINFO:
		if len(optVal) == 0 {
			return nil
		}
		v, err := parseIntOrChar(optVal)
		if err != nil {
			return err
		}
		ep.SocketOptions().SetReceivePacketInfo(v != 0)
		return nil

	case
		C.IP_ADD_SOURCE_MEMBERSHIP,
		C.IP_BIND_ADDRESS_NO_PORT,
		C.IP_BLOCK_SOURCE,
		C.IP_CHECKSUM,
		C.IP_DROP_SOURCE_MEMBERSHIP,
		C.IP_FREEBIND,
		C.IP_HDRINCL,
		C.IP_IPSEC_POLICY,
		C.IP_MINTTL,
		C.IP_MSFILTER,
		C.IP_MTU_DISCOVER,
		C.IP_MULTICAST_ALL,
		C.IP_NODEFRAG,
		C.IP_OPTIONS,
		C.IP_PASSSEC,
		C.IP_RECVERR,
		C.IP_RECVOPTS,
		C.IP_RECVORIGDSTADDR,
		C.IP_RECVTTL,
		C.IP_RETOPTS,
		C.IP_TRANSPARENT,
		C.IP_UNBLOCK_SOURCE,
		C.IP_UNICAST_IF,
		C.IP_XFRM_POLICY,
		C.MCAST_BLOCK_SOURCE,
		C.MCAST_JOIN_SOURCE_GROUP,
		C.MCAST_LEAVE_GROUP,
		C.MCAST_LEAVE_SOURCE_GROUP,
		C.MCAST_MSFILTER,
		C.MCAST_UNBLOCK_SOURCE:

	default:
		_ = syslog.Infof("unimplemented setsockopt: SOL_IP name=%d optVal=%x", name, optVal)
	}

	return &tcpip.ErrUnknownProtocolOption{}
}

// isLinkLocal determines if the given IPv6 address is link-local. This is the
// case when it has the fe80::/10 prefix. This check is used to determine when
// the NICID is relevant for a given IPv6 address.
func isLinkLocal(addr fidlnet.Ipv6Address) bool {
	return addr.Addr[0] == 0xfe && addr.Addr[1]&0xc0 == 0x80
}

// toNetSocketAddress converts a tcpip.FullAddress into a fidlnet.SocketAddress
// taking the protocol into consideration. If addr is unspecified, the
// unspecified address for the provided protocol is returned.
//
// Panics if protocol is neither IPv4 nor IPv6.
func toNetSocketAddress(protocol tcpip.NetworkProtocolNumber, addr tcpip.FullAddress) fidlnet.SocketAddress {
	switch protocol {
	case ipv4.ProtocolNumber:
		out := fidlnet.Ipv4SocketAddress{
			Port: addr.Port,
		}
		copy(out.Address.Addr[:], addr.Addr)
		return fidlnet.SocketAddressWithIpv4(out)
	case ipv6.ProtocolNumber:
		out := fidlnet.Ipv6SocketAddress{
			Port: addr.Port,
		}
		if len(addr.Addr) == header.IPv4AddressSize {
			// Copy address in v4-mapped format.
			copy(out.Address.Addr[header.IPv6AddressSize-header.IPv4AddressSize:], addr.Addr)
			out.Address.Addr[header.IPv6AddressSize-header.IPv4AddressSize-1] = 0xff
			out.Address.Addr[header.IPv6AddressSize-header.IPv4AddressSize-2] = 0xff
		} else {
			copy(out.Address.Addr[:], addr.Addr)
			if isLinkLocal(out.Address) {
				out.ZoneIndex = uint64(addr.NIC)
			}
		}
		return fidlnet.SocketAddressWithIpv6(out)
	default:
		panic(fmt.Sprintf("invalid protocol for conversion: %d", protocol))
	}
}

func bytesToAddressDroppingUnspecified(b []uint8) tcpip.Address {
	if isZeros(b) {
		return ""
	}
	return tcpip.Address(b)
}

func toTCPIPFullAddress(addr fidlnet.SocketAddress) (tcpip.FullAddress, error) {
	switch addr.Which() {
	case fidlnet.SocketAddressIpv4:
		return tcpip.FullAddress{
			NIC:  0,
			Addr: bytesToAddressDroppingUnspecified(addr.Ipv4.Address.Addr[:]),
			Port: addr.Ipv4.Port,
		}, nil
	case fidlnet.SocketAddressIpv6:
		return tcpip.FullAddress{
			NIC:  tcpip.NICID(addr.Ipv6.ZoneIndex),
			Addr: bytesToAddressDroppingUnspecified(addr.Ipv6.Address.Addr[:]),
			Port: addr.Ipv6.Port,
		}, nil
	default:
		return tcpip.FullAddress{}, fmt.Errorf("invalid fuchsia.net/SocketAddress variant: %d", addr.Which())
	}
}

func toTcpIpAddressDroppingUnspecifiedv4(fidl fidlnet.Ipv4Address) tcpip.Address {
	return bytesToAddressDroppingUnspecified(fidl.Addr[:])
}

func toTcpIpAddressDroppingUnspecifiedv6(fidl fidlnet.Ipv6Address) tcpip.Address {
	return bytesToAddressDroppingUnspecified(fidl.Addr[:])
}
