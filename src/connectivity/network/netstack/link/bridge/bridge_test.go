// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bridge_test

import (
	"bytes"
	"errors"
	"fmt"
	"strings"
	"testing"
	"time"

	"go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/link/bridge"
	"go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/util"

	"gvisor.dev/gvisor/pkg/tcpip"
	"gvisor.dev/gvisor/pkg/tcpip/buffer"
	"gvisor.dev/gvisor/pkg/tcpip/header"
	"gvisor.dev/gvisor/pkg/tcpip/link/loopback"
	"gvisor.dev/gvisor/pkg/tcpip/link/pipe"
	"gvisor.dev/gvisor/pkg/tcpip/link/sniffer"
	"gvisor.dev/gvisor/pkg/tcpip/network/arp"
	"gvisor.dev/gvisor/pkg/tcpip/network/ipv4"
	"gvisor.dev/gvisor/pkg/tcpip/network/ipv6"
	"gvisor.dev/gvisor/pkg/tcpip/stack"
	"gvisor.dev/gvisor/pkg/tcpip/transport/tcp"
	"gvisor.dev/gvisor/pkg/waiter"
)

const (
	linkAddr1 = tcpip.LinkAddress("\x02\x03\x04\x05\x06\x07")
	linkAddr2 = tcpip.LinkAddress("\x02\x03\x04\x05\x06\x08")
	linkAddr3 = tcpip.LinkAddress("\x02\x03\x04\x05\x06\x09")
	linkAddr4 = tcpip.LinkAddress("\x02\x03\x04\x05\x06\x0a")
	linkAddr5 = tcpip.LinkAddress("\x02\x03\x04\x05\x06\x0b")
	linkAddr6 = tcpip.LinkAddress("\x02\x03\x04\x05\x06\x0c")
)

var (
	timeoutReceiveReady    = errors.New("receiveready")
	timeoutSendReady       = errors.New("sendready")
	timeoutPayloadReceived = errors.New("payloadreceived")
)

type endpointWithAttributes struct {
	stack.LinkEndpoint
	capabilities    stack.LinkEndpointCapabilities
	maxHeaderLength uint16
}

func (ep *endpointWithAttributes) Capabilities() stack.LinkEndpointCapabilities {
	return ep.LinkEndpoint.Capabilities() | ep.capabilities
}

func (ep *endpointWithAttributes) MaxHeaderLength() uint16 {
	return ep.LinkEndpoint.MaxHeaderLength() + ep.maxHeaderLength
}

func TestEndpointAttributes(t *testing.T) {
	ep1 := bridge.NewEndpoint(&endpointWithAttributes{
		LinkEndpoint:    loopback.New(),
		capabilities:    stack.CapabilityLoopback,
		maxHeaderLength: 5,
	})
	ep2 := bridge.NewEndpoint(&endpointWithAttributes{
		LinkEndpoint:    loopback.New(),
		capabilities:    stack.CapabilityLoopback | stack.CapabilityResolutionRequired,
		maxHeaderLength: 10,
	})
	bridgeEP := bridge.New([]*bridge.BridgeableEndpoint{ep1, ep2})

	if got, want := bridgeEP.Capabilities(), stack.CapabilityResolutionRequired; got != want {
		t.Errorf("got Capabilities = %b, want = %b", got, want)
	}

	if got, want := bridgeEP.MaxHeaderLength(), ep2.MaxHeaderLength(); got != want {
		t.Errorf("got MaxHeaderLength = %d, want = %d", got, want)
	}

	if got, want := bridgeEP.MTU(), ep2.MTU(); got != want {
		t.Errorf("got MTU = %d, want = %d", got, want)
	}

	if linkAddr := bridgeEP.LinkAddress(); linkAddr[0]&0x2 == 0 {
		t.Errorf("bridge.LinkAddress() expected to be locally administered MAC address, got: %s", linkAddr)
	}
}

type waitingEndpoint struct {
	stack.LinkEndpoint
	ch chan struct{}
}

func (we *waitingEndpoint) Wait() {
	<-we.ch
}

func TestEndpoint_Wait(t *testing.T) {
	ep := loopback.New()
	ep1 := waitingEndpoint{
		LinkEndpoint: ep,
		ch:           make(chan struct{}),
	}
	ep2 := waitingEndpoint{
		LinkEndpoint: ep,
		ch:           make(chan struct{}),
	}
	bridgeEP := bridge.New([]*bridge.BridgeableEndpoint{
		bridge.NewEndpoint(&ep1),
		bridge.NewEndpoint(&ep2),
	})
	ch := make(chan struct{})
	go func() {
		bridgeEP.Wait()
		close(ch)
	}()

	for _, ep := range []waitingEndpoint{ep1, ep2} {
		select {
		case <-ch:
			t.Fatal("bridge wait completed before constituent links")
		case <-time.After(100 * time.Millisecond):
		}
		close(ep.ch)
	}

	select {
	case <-ch:
	case <-time.After(100 * time.Millisecond):
		t.Fatal("bridge wait pending after constituent links completed")
	}
}

var _ stack.NetworkDispatcher = (*testNetworkDispatcher)(nil)

type testNetworkDispatcher struct {
	pkt   *stack.PacketBuffer
	count int
}

func (t *testNetworkDispatcher) DeliverNetworkPacket(_, _ tcpip.LinkAddress, _ tcpip.NetworkProtocolNumber, pkt *stack.PacketBuffer) {
	t.count++
	t.pkt = pkt
}

const channelEndpointHeaderLen = 1

var _ stack.LinkEndpoint = (*channelEndpoint)(nil)

type channelEndpoint struct {
	stack.LinkEndpoint
	linkAddr tcpip.LinkAddress
	c        chan *stack.PacketBuffer
}

func (*channelEndpoint) MaxHeaderLength() uint16 {
	return channelEndpointHeaderLen
}

func (e *channelEndpoint) LinkAddress() tcpip.LinkAddress {
	return e.linkAddr
}

func (e *channelEndpoint) WritePacket(_ stack.RouteInfo, _ tcpip.NetworkProtocolNumber, pkt *stack.PacketBuffer) tcpip.Error {
	_ = pkt.LinkHeader().Push(channelEndpointHeaderLen)
	select {
	case e.c <- pkt:
	default:
		return &tcpip.ErrWouldBlock{}
	}

	return nil
}

func (e *channelEndpoint) WritePackets(_ stack.RouteInfo, pkts stack.PacketBufferList, _ tcpip.NetworkProtocolNumber) (int, tcpip.Error) {
	i := 0
	for pkt := pkts.Front(); pkt != nil; i, pkt = i+1, pkt.Next() {
		_ = pkt.LinkHeader().Push(channelEndpointHeaderLen)
		select {
		case e.c <- pkt:
		default:
			return i, &tcpip.ErrWouldBlock{}
		}
	}

	return i, nil
}

func (e *channelEndpoint) getPacket() *stack.PacketBuffer {
	select {
	case pkt := <-e.c:
		return pkt
	default:
		return nil
	}
}

func makeChannelEndpoint(linkAddr tcpip.LinkAddress, size int) channelEndpoint {
	return channelEndpoint{
		LinkEndpoint: loopback.New(),
		linkAddr:     linkAddr,
		c:            make(chan *stack.PacketBuffer, size),
	}
}

func TestBridgeWithoutDispatcher(t *testing.T) {
	ep := makeChannelEndpoint(linkAddr1, 0)
	bep := bridge.NewEndpoint(&ep)
	bridgeEP := bridge.New([]*bridge.BridgeableEndpoint{bep})

	tests := []struct {
		name        string
		dstLinkAddr tcpip.LinkAddress
	}{
		{
			name:        "To bridge",
			dstLinkAddr: bridgeEP.LinkAddress(),
		},
		{
			name:        "Flood",
			dstLinkAddr: header.EthernetBroadcastAddress,
		},
	}

	pkt := stack.NewPacketBuffer(stack.PacketBufferOptions{
		ReserveHeaderBytes: int(bridgeEP.MaxHeaderLength()),
	})

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			bridgeEP.DeliverNetworkPacketToBridge(nil /* rxEP */, linkAddr2 /* srcLinkAddr */, test.dstLinkAddr, 0 /* protocol */, pkt)
		})
	}
}

// TestBridgeWritePackets tests that writing to a bridge writes the packets to
// all bridged endpoints.
func TestBridgeWritePackets(t *testing.T) {
	data := [][]byte{{1, 2, 3, 4}, {5, 6, 7, 8}, {9, 10, 11, 12}}

	ep1 := makeChannelEndpoint(linkAddr1, len(data))
	ep2 := makeChannelEndpoint(linkAddr2, len(data))
	ep3 := makeChannelEndpoint(linkAddr3, len(data))

	bep1 := bridge.NewEndpoint(&ep1)
	bep2 := bridge.NewEndpoint(&ep2)
	bep3 := bridge.NewEndpoint(&ep3)

	bridgeEP := bridge.New([]*bridge.BridgeableEndpoint{bep1, bep2, bep3})

	t.Run("DeliverNetworkPacketToBridge", func(t *testing.T) {
		bridgeEP.DeliverNetworkPacketToBridge(nil /* rxEP */, linkAddr4, linkAddr5, 0 /* protocol */, stack.NewPacketBuffer(stack.PacketBufferOptions{
			ReserveHeaderBytes: int(bridgeEP.MaxHeaderLength()),
			Data:               buffer.View(data[0]).ToVectorisedView(),
		}))

		// The first byte in the data from the endpoints is expected to be the link header
		// byte which we ignore.
		if pkt := ep1.getPacket(); pkt == nil {
			t.Error("expected a packet on ep1")
		} else if got := pkt.Data().AsRange().ToOwnedView(); !bytes.Equal(got, data[0]) {
			t.Errorf("got ep1 data = %x, want = %x", got, data[0])
		}

		if pkt := ep2.getPacket(); pkt == nil {
			t.Error("expected a packet on ep2")
		} else if got := pkt.Data().AsRange().ToOwnedView(); !bytes.Equal(got, data[0]) {
			t.Errorf("got ep2 data = %x, want = %x", got, data[0])
		}

		if pkt := ep3.getPacket(); pkt == nil {
			t.Error("expected a packet on ep3")
		} else if got := pkt.Data().AsRange().ToOwnedView(); !bytes.Equal(got, data[0]) {
			t.Errorf("got ep3 data = %x, want = %x", got, data[0])
		}
	})

	t.Run("WritePacket", func(t *testing.T) {
		// The bridge and channel endpoints do not care about the route or network
		// protocol number when writing packets.
		if err := bridgeEP.WritePacket(stack.RouteInfo{}, 0 /* protocol */, stack.NewPacketBuffer(stack.PacketBufferOptions{
			ReserveHeaderBytes: int(bridgeEP.MaxHeaderLength()),
			Data:               buffer.View(data[0]).ToVectorisedView(),
		})); err != nil {
			t.Errorf("bridgeEP.WritePacket({}, 0, _): %s", err)
		}

		if pkt := ep1.getPacket(); pkt == nil {
			t.Error("expected a packet on ep1")
		} else if got := pkt.Data().AsRange().ToOwnedView(); !bytes.Equal(got, data[0]) {
			t.Errorf("got ep1 data = %x, want = %x", got, data[0])
		}

		if pkt := ep2.getPacket(); pkt == nil {
			t.Error("expected a packet on ep2")
		} else if got := pkt.Data().AsRange().ToOwnedView(); !bytes.Equal(got, data[0]) {
			t.Errorf("got ep2 data = %x, want = %x", got, data[0])
		}

		if pkt := ep3.getPacket(); pkt == nil {
			t.Error("expected a packet on ep3")
		} else if got := pkt.Data().AsRange().ToOwnedView(); !bytes.Equal(got, data[0]) {
			t.Errorf("got ep3 data = %x, want = %x", got, data[0])
		}
	})

	for i := 1; i <= len(data); i++ {
		t.Run(fmt.Sprintf("WritePackets(N=%d)", i), func(t *testing.T) {
			var pkts stack.PacketBufferList
			for j := 0; j < i; j++ {
				pkts.PushBack(stack.NewPacketBuffer(stack.PacketBufferOptions{
					ReserveHeaderBytes: int(bridgeEP.MaxHeaderLength()),
					Data:               buffer.View(data[j]).ToVectorisedView(),
				}))
			}

			// The bridge and channel endpoints do not care about the route or
			// network protocol number when writing packets.
			n, err := bridgeEP.WritePackets(stack.RouteInfo{}, pkts, 0 /* protocol */)
			if err != nil {
				t.Errorf("bridgeEP.WritePackets(nil, nil, _, 0): %s", err)
			}
			if n != i {
				t.Errorf("got bridgeEP.WritePackets(nil, nil, _, 0) = %d, want = %d", n, i)
			}

			for j := 0; j < i; j++ {
				if pkt := ep1.getPacket(); pkt == nil {
					t.Errorf("(j=%d) expected a packet on ep1", j)
				} else if got := pkt.Data().AsRange().ToOwnedView(); !bytes.Equal(got, data[j]) {
					t.Errorf("(j=%d) got ep1 data = %x, want = %x", j, got, data[j])
				}

				if pkt := ep2.getPacket(); pkt == nil {
					t.Errorf("(j=%d) expected a packet on ep2", j)
				} else if got := pkt.Data().AsRange().ToOwnedView(); !bytes.Equal(got, data[j]) {
					t.Errorf("(j=%d) got ep2 data = %x, want = %x", j, got, data[j])
				}

				if pkt := ep3.getPacket(); pkt == nil {
					t.Errorf("(j=%d) expected a packet on ep3", j)
				} else if got := pkt.Data().AsRange().ToOwnedView(); !bytes.Equal(got, data[j]) {
					t.Errorf("(j=%d) got ep3 data = %x, want = %x", j, got, data[j])
				}
			}
		})
	}
}

// TestBridgeRouting makes sure that frames are directed to the right unicast
// endpoint or floods all endpoints for multicast and broadcast frames.
func TestBridgeRouting(t *testing.T) {
	type rxEPKind int
	const (
		rxEPNil rxEPKind = iota
		rxEP1
		rxEP2
	)

	data := []byte{1, 2, 3, 4}

	tests := []struct {
		name               string
		dstAddr            tcpip.LinkAddress
		ep1ShouldGetPacket bool
		nd1ShouldGetPacket bool
		ep2ShouldGetPacket bool
		nd2ShouldGetPacket bool
		ndbShouldGetPacket bool
	}{
		{
			name:               "ToMulticast",
			dstAddr:            "\x01\x03\x04\x05\x06\x07",
			ep1ShouldGetPacket: true,
			nd1ShouldGetPacket: true,
			ep2ShouldGetPacket: true,
			nd2ShouldGetPacket: true,
			ndbShouldGetPacket: true,
		},
		{
			name:               "ToBroadcast",
			dstAddr:            "\xff\xff\xff\xff\xff\xff",
			ep1ShouldGetPacket: true,
			nd1ShouldGetPacket: true,
			ep2ShouldGetPacket: true,
			nd2ShouldGetPacket: true,
			ndbShouldGetPacket: true,
		},
		{
			name:               "ToEP1",
			dstAddr:            linkAddr1,
			nd1ShouldGetPacket: true,
		},
		{
			name:               "ToEP2",
			dstAddr:            linkAddr2,
			nd2ShouldGetPacket: true,
		},
		{
			name:               "ToOther",
			dstAddr:            linkAddr4,
			ep1ShouldGetPacket: true,
			ep2ShouldGetPacket: true,
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {

			subtests := []struct {
				name               string
				rxEP               rxEPKind
				ep1ShouldGetPacket bool
				ep2ShouldGetPacket bool
			}{
				{
					name:               "Delivered from nil EP",
					rxEP:               rxEPNil,
					ep1ShouldGetPacket: test.ep1ShouldGetPacket,
					ep2ShouldGetPacket: test.ep2ShouldGetPacket,
				},
				{
					name:               "Delivered from EP1",
					rxEP:               rxEP1,
					ep1ShouldGetPacket: false,
					ep2ShouldGetPacket: test.ep2ShouldGetPacket,
				},
				{
					name:               "Delivered from EP2",
					rxEP:               rxEP2,
					ep1ShouldGetPacket: test.ep1ShouldGetPacket,
					ep2ShouldGetPacket: false,
				},
			}

			for _, subtest := range subtests {
				t.Run(test.name, func(t *testing.T) {
					ep1 := makeChannelEndpoint(linkAddr1, 1)
					ep2 := makeChannelEndpoint(linkAddr2, 1)

					bep1 := bridge.NewEndpoint(&ep1)
					bep2 := bridge.NewEndpoint(&ep2)

					var nd1, nd2, ndb testNetworkDispatcher

					bridgeEP := bridge.New([]*bridge.BridgeableEndpoint{bep1, bep2})

					bep1.Attach(&nd1)
					bep2.Attach(&nd2)
					bridgeEP.Attach(&ndb)

					var rxEP *bridge.BridgeableEndpoint
					switch subtest.rxEP {
					case rxEPNil:
					case rxEP1:
						rxEP = bep1
					case rxEP2:
						rxEP = bep2
					default:
						t.Fatalf("unrecognized rxEPKind = %d", subtest.rxEP)
					}

					bridgeEP.DeliverNetworkPacketToBridge(rxEP, linkAddr3, test.dstAddr, 0, stack.NewPacketBuffer(stack.PacketBufferOptions{
						ReserveHeaderBytes: int(bridgeEP.MaxHeaderLength()),
						Data:               buffer.View(data).ToVectorisedView(),
					}))

					if pkt := ep1.getPacket(); subtest.ep1ShouldGetPacket {
						if pkt == nil {
							t.Error("expected a packet on ep1")
						} else if got := pkt.Data().AsRange().ToOwnedView(); !bytes.Equal(got, data) {
							t.Errorf("got ep1 data = %x, want = %x", got, data)
						}
					} else if pkt != nil {
						t.Errorf("ep1 unexpectedly got a packet = %+v", pkt)
					}

					if test.nd1ShouldGetPacket {
						if nd1.count != 1 {
							t.Errorf("got nd1.count = %d, want = 1", nd1.count)
						}
						if got := nd1.pkt.Data().AsRange().ToOwnedView(); !bytes.Equal(got, data) {
							t.Errorf("got nd1 data = %x, want = %x", got, data)
						}
					} else if nd1.count != 0 {
						t.Errorf("got nd1.count = %d, want = 0", nd1.count)
					}

					if pkt := ep2.getPacket(); subtest.ep2ShouldGetPacket {
						if pkt == nil {
							t.Error("expected a packet on ep2")
						} else if got := pkt.Data().AsRange().ToOwnedView(); !bytes.Equal(got, data) {
							t.Errorf("got ep2 data = %x, want = %x", got, data)
						}
					} else if pkt != nil {
						t.Errorf("ep2 unexpectedly got a packet = %+v", pkt)
					}

					if test.nd2ShouldGetPacket {
						if nd2.count != 1 {
							t.Errorf("got nd2.count = %d, want = 1", nd2.count)
						}
						if got := nd2.pkt.Data().AsRange().ToOwnedView(); !bytes.Equal(got, data) {
							t.Errorf("got nd2 data = %x, want = %x", got, data)
						}
					} else if nd2.count != 0 {
						t.Errorf("got nd2.count = %d, want = 0", nd2.count)
					}

					if test.ndbShouldGetPacket {
						if ndb.count != 1 {
							t.Errorf("got ndb.count = %d, want = 1", ndb.count)
						}
						if got := ndb.pkt.Data().AsRange().ToOwnedView(); !bytes.Equal(got, data) {
							t.Errorf("got ndb data = %x, want = %x", got, data)
						}
					} else if ndb.count != 0 {
						t.Errorf("got ndb.count = %d, want = 0", ndb.count)
					}
				})
			}
		})
	}
}

func TestBridge(t *testing.T) {
	const (
		s1NICID = 1
		s2NICID = 10

		sbEP2NICID   = 2
		sbOtherNICID = 9000
	)

	for _, testCase := range []struct {
		name            string
		protocolFactory stack.NetworkProtocolFactory
		protocolNumber  tcpip.NetworkProtocolNumber
		addressSize     int
	}{
		{name: "ipv4", protocolFactory: ipv4.NewProtocol, protocolNumber: ipv4.ProtocolNumber, addressSize: header.IPv4AddressSize},
		{name: "ipv6", protocolFactory: ipv6.NewProtocol, protocolNumber: ipv6.ProtocolNumber, addressSize: header.IPv6AddressSize},
	} {
		t.Run(testCase.name, func(t *testing.T) {
			// payload should be unique enough that it won't accidentally appear
			// in TCP/IP packets.
			const payload = "hello"

			// Connection diagram:
			//
			// <---> ep1 <--pipe--> ep2 <--bridge--> ep3 <--pipe--> ep4
			//
			// Included are several additional endpoints to ensure bridging N > 2
			// endpoints works.
			ep1, ep2 := makePipe(linkAddr1, linkAddr2)
			ep3, ep4 := makePipe(linkAddr3, linkAddr4)
			ep5, ep6 := makePipe(linkAddr5, linkAddr6)
			s1addr := tcpip.Address(bytes.Repeat([]byte{1}, testCase.addressSize))
			s1subnet := util.PointSubnet(s1addr)
			s1, err := makeStackWithEndpoint(s1NICID, ep1, testCase.protocolFactory, testCase.protocolNumber, s1addr)
			if err != nil {
				t.Fatal(err)
			}

			baddr := tcpip.Address(bytes.Repeat([]byte{2}, testCase.addressSize))
			bsubnet := util.PointSubnet(baddr)
			sb, b, bridgeNICID := makeStackWithBridgedEndpoints(t, testCase.protocolFactory, testCase.protocolNumber, baddr, ep5, ep2, ep3)

			if err := sb.CreateNIC(sbOtherNICID, ep6); err != nil {
				t.Fatal(err)
			}

			if err := b.Up(); err != nil {
				t.Fatal(err)
			}

			s2addr := tcpip.Address(bytes.Repeat([]byte{3}, testCase.addressSize))
			s2subnet := util.PointSubnet(s2addr)
			s2, err := makeStackWithEndpoint(s2NICID, ep4, testCase.protocolFactory, testCase.protocolNumber, s2addr)
			if err != nil {
				t.Fatal(err)
			}

			// Add an address to one of the constituent links of the bridge (in addition
			// to the address on the virtual NIC representing the bridge itself), to test
			// that constituent links are still routable.
			bcaddr := tcpip.Address(bytes.Repeat([]byte{4}, testCase.addressSize))
			bcsubnet := util.PointSubnet(bcaddr)
			if err := sb.AddAddress(sbEP2NICID, testCase.protocolNumber, bcaddr); err != nil {
				t.Fatal(fmt.Errorf("AddAddress failed: %s", err))
			}

			// Make sure s1 can communicate with all the addresses we configured
			// above.
			s1.SetRouteTable([]tcpip.Route{
				{
					Destination: s2subnet,
					NIC:         s1NICID,
				},
				{
					Destination: bsubnet,
					NIC:         s1NICID,
				},
				{
					Destination: bcsubnet,
					NIC:         s1NICID,
				},
			})
			sb.SetRouteTable([]tcpip.Route{
				{
					Destination: s1subnet,
					NIC:         sbEP2NICID,
				},
				{
					Destination: s1subnet,
					NIC:         bridgeNICID,
				},
			})
			s2.SetRouteTable(
				[]tcpip.Route{
					{
						Destination: s1subnet,
						NIC:         s2NICID,
					},
				},
			)

			addrs := map[tcpip.Address]*stack.Stack{
				s2addr: s2,
				baddr:  sb,
				bcaddr: sb,
			}

			stacks := map[string]*stack.Stack{
				"s1": s1, "s2": s2, "sb": sb,
			}

			ep2.onWritePacket = func(pkt *stack.PacketBuffer) {
				for i, view := range pkt.Data().Views() {
					if bytes.Contains(view, []byte(payload)) {
						t.Errorf("did not expect payload %x to be sent back to ep1 in view %d: %x", payload, i, view)
					}
				}
			}

			for addr, toStack := range addrs {
				t.Run(fmt.Sprintf("ConnectAndWrite_%s", addr), func(t *testing.T) {
					recvd, err := connectAndWrite(s1, toStack, testCase.protocolNumber, addr, payload)
					if err != nil {
						t.Fatal(err)
					}

					if !bytes.Equal(recvd, []byte(payload)) {
						t.Errorf("got Read(...) = %x, want = %x", recvd, payload)
					}

					for name, s := range stacks {
						stats := s.Stats()
						if n := stats.NICs.UnknownL3ProtocolRcvdPackets.Value(); n != 0 {
							t.Errorf("stack %s received %d UnknownL3ProtocolRcvdPackets", name, n)
						}
						if n := stats.NICs.UnknownL4ProtocolRcvdPackets.Value(); n != 0 {
							t.Errorf("stack %s received %d UnknownL4ProtocolRcvdPackets", name, n)
						}
						if n := stats.NICs.MalformedL4RcvdPackets.Value(); n != 0 {
							t.Errorf("stack %s received %d MalformedL4RcvdPackets", name, n)
						}
						if n := stats.DroppedPackets.Value(); n != 0 {
							t.Errorf("stack %s received %d DroppedPackets", name, n)
						}

						// The invalid address counter counts packets that have been received
						// by a stack correctly addressed at the link layer but incorrectly
						// addressed at the network layer (e.g. no network interface has the
						// address listed in the packet). This usually happens because
						// the stack is being sent packets for an IP address that it used to
						// have but doesn't have anymore.  In this case, the bridge will
						// forward a packet to all constituent links when the link address that
						// the packet is addressed to isn't found on the bridge.
						//
						// TODO(fxbug.dev/20778): When we implement learning, we should be able to
						// modify this test setup to get to zero invalid addresses received.
						// With the current test setup, once learning is implemented, the
						// bridge would indiscriminately forward the first packet addressed to
						// a link address to all constituent links (causing #links - 1 invalid
						// addresses received), observe which link the response packet came
						// from, and then remember which link to forward to when the next
						// packet addressed to that link address was received. We might be able
						// to get to zero invalid addresses received by learning which links a
						// given address is on via the broadcast packets sent during ARP.
						// if n := stats.IP.InvalidAddressesReceived.Value(); n != 0 {
						//   t.Errorf("stack %s received %d InvalidAddressesReceived", name, n)
						// }
						if n := stats.IP.OutgoingPacketErrors.Value(); n != 0 {
							t.Errorf("stack %s received %d OutgoingPacketErrors", name, n)
						}
						if n := stats.TCP.FailedConnectionAttempts.Value(); n != 0 {
							t.Errorf("stack %s received %d FailedConnectionAttempts", name, n)
						}
						if n := stats.TCP.InvalidSegmentsReceived.Value(); n != 0 {
							t.Errorf("stack %s received %d InvalidSegmentsReceived", name, n)
						}
						if n := stats.TCP.ResetsSent.Value(); n != 0 {
							t.Errorf("stack %s received %d ResetsSent", name, n)
						}
						if n := stats.TCP.ResetsReceived.Value(); n != 0 {
							t.Errorf("stack %s received %d ResetsReceived", name, n)
						}
					}
				})
			}

			b.Attach(nil)

			// verify that the endpoint from the constituent link on sb is still accessible
			// and the bridge endpoint and endpoint on s2 are no longer accessible from s1
			noLongerConnectable := map[tcpip.Address]*stack.Stack{
				s2addr: s2,
				baddr:  sb,
			}

			stillConnectable := map[tcpip.Address]*stack.Stack{
				bcaddr: sb,
			}

			for addr, toStack := range noLongerConnectable {
				t.Run(addr.String(), func(t *testing.T) {
					senderWaitQueue := new(waiter.Queue)
					sender, err := s1.NewEndpoint(tcp.ProtocolNumber, testCase.protocolNumber, senderWaitQueue)
					if err != nil {
						t.Fatalf("NewEndpoint failed: %s", err)
					}
					defer sender.Close()

					receiverWaitQueue := new(waiter.Queue)
					receiver, err := toStack.NewEndpoint(tcp.ProtocolNumber, testCase.protocolNumber, receiverWaitQueue)
					if err != nil {
						t.Fatalf("NewEndpoint failed: %s", err)
					}
					defer receiver.Close()

					if err := receiver.Bind(tcpip.FullAddress{Addr: addr}); err != nil {
						t.Fatalf("bind failed: %s", err)
					}
					if err := receiver.Listen(1); err != nil {
						t.Fatalf("listen failed: %s", err)
					}
					addr, err := receiver.GetLocalAddress()
					if err != nil {
						t.Fatalf("getlocaladdress failed: %s", err)
					}
					addr.NIC = 0

					if err := connect(sender, addr, senderWaitQueue, receiverWaitQueue); err != timeoutSendReady {
						t.Errorf("expected timeout sendready, got %v connecting to addr %+v", err, addr)
					}
				})
			}

			for addr, toStack := range stillConnectable {
				recvd, err := connectAndWrite(s1, toStack, testCase.protocolNumber, addr, payload)
				if err != nil {
					t.Fatal(err)
				}

				if !bytes.Equal(recvd, []byte(payload)) {
					t.Errorf("got Read(...) = %x, want = %x", recvd, payload)
				}
			}
		})
	}
}

// TestBridgeableEndpointDetach tests that bridgeable endpoints don't cause
// panics after attaching to a nil dispatcher.
func TestBridgeableEndpointDetach(t *testing.T) {
	ep1 := makeChannelEndpoint(linkAddr1, 1)
	bep1 := bridge.NewEndpoint(&ep1)
	var disp testNetworkDispatcher

	if ep1.IsAttached() {
		t.Fatal("ep1.IsAttached() = true, want = false")
	}
	if bep1.IsAttached() {
		t.Fatal("bep1.IsAttached() = true, want = false")
	}

	bep1.Attach(&disp)
	if disp.count != 0 {
		t.Fatalf("got disp.count = %d, want = 0", disp.count)
	}
	if !ep1.IsAttached() {
		t.Fatal("ep1.IsAttached() = false, want = true")
	}
	if !bep1.IsAttached() {
		t.Fatal("bep1.IsAttached() = false, want = true")
	}

	bep1.DeliverNetworkPacket(linkAddr1, linkAddr2, header.IPv4ProtocolNumber, &stack.PacketBuffer{})
	if disp.count != 1 {
		t.Fatalf("got disp.count = %d, want = 1", disp.count)
	}

	bep1.Attach(nil)
	if ep1.IsAttached() {
		t.Fatal("ep1.IsAttached() = true, want = false")
	}
	if bep1.IsAttached() {
		t.Fatal("bep1.IsAttached() = true, want = false")
	}

	bep1.DeliverNetworkPacket(linkAddr1, linkAddr2, header.IPv4ProtocolNumber, &stack.PacketBuffer{})
	if disp.count != 1 {
		t.Fatalf("got disp.count = %d, want = 1", disp.count)
	}
}

// makePipe mints two linked endpoints with the given link addresses.
func makePipe(addr1, addr2 tcpip.LinkAddress) (*endpoint, *endpoint) {
	ep1, ep2 := pipe.New(addr1, addr2)
	return &endpoint{LinkEndpoint: ep1}, &endpoint{LinkEndpoint: ep2}
}

var _ stack.LinkEndpoint = (*endpoint)(nil)

// Use our own endpoint fake because we'd like to report
// CapabilityResolutionRequired and trigger link address resolution.
//
// `endpoint` cannot be copied.
//
// Make endpoints using `makePipe()`, not using endpoint literals.
type endpoint struct {
	stack.LinkEndpoint
	onWritePacket func(*stack.PacketBuffer)
}

func (e *endpoint) WritePacket(r stack.RouteInfo, protocol tcpip.NetworkProtocolNumber, pkt *stack.PacketBuffer) tcpip.Error {
	if fn := e.onWritePacket; fn != nil {
		fn(pkt)
	}
	return e.LinkEndpoint.WritePacket(r, protocol, pkt)
}

func (e *endpoint) Capabilities() stack.LinkEndpointCapabilities {
	return stack.CapabilityResolutionRequired | e.LinkEndpoint.Capabilities()
}

func makeStackWithEndpoint(nicID tcpip.NICID, ep stack.LinkEndpoint, protocolFactory stack.NetworkProtocolFactory, protocolNumber tcpip.NetworkProtocolNumber, addr tcpip.Address) (*stack.Stack, error) {
	if testing.Verbose() {
		ep = sniffer.New(ep)
	}

	s := stack.New(stack.Options{
		NetworkProtocols: []stack.NetworkProtocolFactory{
			arp.NewProtocol,
			protocolFactory,
		},
		TransportProtocols: []stack.TransportProtocolFactory{
			tcp.NewProtocol,
		},
	})
	if err := s.CreateNIC(nicID, ep); err != nil {
		return nil, fmt.Errorf("CreateNIC failed: %s", err)
	}
	if err := s.AddAddress(nicID, protocolNumber, addr); err != nil {
		return nil, fmt.Errorf("AddAddress failed: %s", err)
	}
	return s, nil
}

func makeStackWithBridgedEndpoints(t *testing.T, protocolFactory stack.NetworkProtocolFactory, protocolNumber tcpip.NetworkProtocolNumber, baddr tcpip.Address, eps ...stack.LinkEndpoint) (*stack.Stack, *bridge.Endpoint, tcpip.NICID) {
	t.Helper()
	if testing.Verbose() {
		for i := range eps {
			eps[i] = sniffer.New(eps[i])
		}
	}

	stk := stack.New(stack.Options{
		NetworkProtocols: []stack.NetworkProtocolFactory{
			arp.NewProtocol,
			protocolFactory,
		},
		TransportProtocols: []stack.TransportProtocolFactory{
			tcp.NewProtocol,
		},
	})

	beps := make([]*bridge.BridgeableEndpoint, len(eps))
	for i, ep := range eps {
		bep := bridge.NewEndpoint(ep)
		if err := stk.CreateNIC(tcpip.NICID(i+1), bep); err != nil {
			t.Fatalf("CreateNIC failed: %s", err)
		}
		beps[i] = bep
	}

	bridgeEP := bridge.New(beps)
	var bridgeLinkEP stack.LinkEndpoint = bridgeEP
	if testing.Verbose() {
		bridgeLinkEP = sniffer.New(bridgeLinkEP)
	}
	bID := tcpip.NICID(len(beps) + 1)
	if err := stk.CreateNIC(bID, bridgeLinkEP); err != nil {
		t.Fatalf("CreateNIC failed: %s", err)
	}
	if err := stk.AddAddress(bID, protocolNumber, baddr); err != nil {
		t.Fatalf("AddAddress failed: %s", err)
	}

	return stk, bridgeEP, bID
}

func connectAndWrite(fromStack *stack.Stack, toStack *stack.Stack, protocolNumber tcpip.NetworkProtocolNumber, addr tcpip.Address, payload string) ([]byte, error) {
	senderWaitQueue := new(waiter.Queue)
	sender, err := fromStack.NewEndpoint(tcp.ProtocolNumber, protocolNumber, senderWaitQueue)
	if err != nil {
		return nil, fmt.Errorf("NewEndpoint failed: %s", err)
	}
	defer sender.Close()

	receiverWaitQueue := new(waiter.Queue)
	receiver, err := toStack.NewEndpoint(tcp.ProtocolNumber, protocolNumber, receiverWaitQueue)
	if err != nil {
		return nil, fmt.Errorf("NewEndpoint failed: %s", err)
	}
	defer receiver.Close()

	if err := receiver.Bind(tcpip.FullAddress{Addr: addr}); err != nil {
		return nil, fmt.Errorf("bind failed: %s", err)
	}
	if err := receiver.Listen(1); err != nil {
		return nil, fmt.Errorf("listen failed: %s", err)
	}
	{
		addr, err := receiver.GetLocalAddress()
		if err != nil {
			return nil, fmt.Errorf("getlocaladdress failed: %s", err)
		}
		addr.NIC = 0

		if err := connect(sender, addr, senderWaitQueue, receiverWaitQueue); err != nil {
			return nil, fmt.Errorf("connect failed: %s\n\n%+v\n\n%+v", err, fromStack.Stats(), toStack.Stats())
		}

		ep, wq, err := receiver.Accept(nil)
		if err != nil {
			return nil, fmt.Errorf("accept failed: %s", err)
		}

		if err := write(sender, addr, payload, wq); err != nil {
			return nil, err
		}

		var recvd bytes.Buffer
		if _, err := ep.Read(&recvd, tcpip.ReadOptions{}); err != nil {
			return nil, fmt.Errorf("read failed: %s", err)
		}
		return recvd.Bytes(), nil
	}
}

func write(sender tcpip.Endpoint, s2fulladdr tcpip.FullAddress, payload string, wq *waiter.Queue) error {
	payloadReceivedWaitEntry, payloadReceivedNotifyCh := waiter.NewChannelEntry(nil)
	wq.EventRegister(&payloadReceivedWaitEntry, waiter.EventIn)
	defer wq.EventUnregister(&payloadReceivedWaitEntry)
	var r strings.Reader
	r.Reset(payload)
	if _, err := sender.Write(&r, tcpip.WriteOptions{To: &s2fulladdr}); err != nil {
		return fmt.Errorf("write failed: %s", err)
	}
	select {
	case <-payloadReceivedNotifyCh:
	case <-time.After(1 * time.Second):
		return timeoutPayloadReceived
	}
	return nil
}

func connect(sender tcpip.Endpoint, addr tcpip.FullAddress, senderWaitQueue, receiverWaitQueue *waiter.Queue) error {
	sendReadyWaitEntry, sendReadyNotifyCh := waiter.NewChannelEntry(nil)
	senderWaitQueue.EventRegister(&sendReadyWaitEntry, waiter.EventOut)
	defer senderWaitQueue.EventUnregister(&sendReadyWaitEntry)

	receiveReadyWaitEntry, receiveReadyNotifyCh := waiter.NewChannelEntry(nil)
	receiverWaitQueue.EventRegister(&receiveReadyWaitEntry, waiter.EventIn)
	defer receiverWaitQueue.EventUnregister(&receiveReadyWaitEntry)

	switch err := sender.Connect(addr); err.(type) {
	case *tcpip.ErrConnectStarted:
	default:
		return fmt.Errorf("connect failed: %s", err)
	}

	select {
	case <-sendReadyNotifyCh:
	case <-time.After(1 * time.Second):
		return timeoutSendReady
	}
	select {
	case <-receiveReadyNotifyCh:
	case <-time.After(1 * time.Second):
		return timeoutReceiveReady
	}

	return nil
}
