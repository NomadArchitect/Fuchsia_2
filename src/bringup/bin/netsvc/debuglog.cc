// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/bringup/bin/netsvc/debuglog.h"

#include <fidl/fuchsia.boot/cpp/wire.h>
#include <inttypes.h>
#include <lib/async/cpp/task.h>
#include <lib/fit/defer.h>
#include <lib/service/llcpp/service.h>
#include <lib/zx/clock.h>
#include <lib/zx/debuglog.h>
#include <stdio.h>
#include <string.h>
#include <zircon/assert.h>
#include <zircon/boot/netboot.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/log.h>

#include "src/bringup/bin/netsvc/netsvc.h"
#include "src/bringup/bin/netsvc/tftp.h"

#define MAX_LOG_LINE (ZX_LOG_RECORD_MAX + 32)

static zx::debuglog debuglog;
static logpacket_t pkt;
static size_t pkt_len;

static volatile uint32_t seqno = 1;
static volatile uint32_t pending = 0;

constexpr zx::duration kSendDelayShort = zx::msec(100);
constexpr zx::duration kSendDelayLong = zx::sec(4);

// Number of consecutive unacknowledged packets we will send before reducing send rate.
static const unsigned kUnackedThreshold = 5;

// Number of consecutive packets that went unacknowledged. Is reset on acknowledgment.
static unsigned num_unacked = 0;

// How long to wait between sending.
static zx::duration send_delay = kSendDelayShort;

static void debuglog_send(async_dispatcher_t* dispatcher);

async::Task timeout_task([](async_dispatcher_t* dispatcher, async::Task* task, zx_status_t status) {
  if (status == ZX_ERR_CANCELED) {
    return;
  }
  ZX_ASSERT_MSG(status == ZX_OK, "unexpected task status %s", zx_status_get_string(status));
  if (pending) {
    // No reply. If no one is listening, reduce send rate.
    if (++num_unacked >= kUnackedThreshold) {
      send_delay = kSendDelayLong;
    }
  }
  debuglog_send(dispatcher);
});

static size_t get_log_line(char* out) {
  char buf[ZX_LOG_RECORD_MAX + 1];
  zx_log_record_t* rec = reinterpret_cast<zx_log_record_t*>(buf);
  for (;;) {
    if (debuglog.read(0, rec, ZX_LOG_RECORD_MAX) > 0) {
      if (rec->datalen && (rec->data[rec->datalen - 1] == '\n')) {
        rec->datalen--;
      }
      // records flagged for local display are ignored
      if (rec->flags & ZX_LOG_LOCAL) {
        continue;
      }
      rec->data[rec->datalen] = 0;
      snprintf(out, MAX_LOG_LINE, "[%05d.%03d] %05" PRIu64 ".%05" PRIu64 "> %s\n",
               static_cast<int>(rec->timestamp / 1000000000ULL),
               static_cast<int>((rec->timestamp / 1000000ULL) % 1000ULL), rec->pid, rec->tid,
               rec->data);
      return strlen(out);
    } else {
      return 0;
    }
  }
}

zx_status_t debuglog_init(async_dispatcher_t* dispatcher) {
  zx::status client_end = service::Connect<fuchsia_boot::ReadOnlyLog>();
  if (client_end.is_error()) {
    return client_end.status_value();
  }
  fidl::WireResult result = fidl::WireCall(client_end.value())->Get();
  if (!result.ok()) {
    return result.status();
  }
  debuglog = std::move(result->log);

  // Set up our timeout to expire immediately, so that we check for pending log
  // messages.
  if (zx_status_t status = timeout_task.Post(dispatcher); status != ZX_OK) {
    return status;
  }

  seqno = 1;
  pending = 0;

  return ZX_OK;
}

// If we have an outstanding (unacknowledged) log, resend it. Otherwise, send new logs, if we
// have any.
static void debuglog_send(async_dispatcher_t* dispatcher) {
  auto reschedule = fit::defer([dispatcher]() {
    zx_status_t status = timeout_task.Cancel();
    ZX_ASSERT_MSG(status == ZX_OK || status == ZX_ERR_NOT_FOUND, "failed to cancel task %s",
                  zx_status_get_string(status));
    status = timeout_task.PostDelayed(dispatcher, send_delay);
    ZX_ASSERT_MSG(status == ZX_OK, "failed to schedule timeout task %s",
                  zx_status_get_string(status));
  });

  if (pending == 0) {
    pkt.magic = NB_DEBUGLOG_MAGIC;
    pkt.seqno = seqno;
    strncpy(pkt.nodename, nodename(), sizeof(pkt.nodename) - 1);
    pkt_len = 0;
    while (pkt_len < (MAX_LOG_DATA - MAX_LOG_LINE)) {
      size_t r = get_log_line(pkt.data + pkt_len);
      if (r > 0) {
        pkt_len += r;
      } else {
        break;
      }
    }
    if (pkt_len) {
      // include header and nodename in length
      pkt_len += MAX_NODENAME_LENGTH + sizeof(uint32_t) * 2;
      pending = 1;
    } else {
      return;
    }
  }
  udp6_send(&pkt, pkt_len, &ip6_ll_all_nodes, DEBUGLOG_PORT, DEBUGLOG_ACK_PORT, false);
}

void debuglog_recv(async_dispatcher_t* dispatcher, void* data, size_t len, bool is_mcast) {
  // The only message we should be receiving is acknowledgement of our last transmission
  if (!pending) {
    return;
  }
  if ((len != 8) || is_mcast) {
    return;
  }
  // Copied not cast in-place to satisfy alignment requirements flagged by ubsan (see
  // fxbug.dev/45798).
  logpacket_t pkt;
  memcpy(&pkt, data, sizeof(logpacket_t));
  if ((pkt.magic != NB_DEBUGLOG_MAGIC) || (pkt.seqno != seqno)) {
    return;
  }

  // Received an ack. We have an active listener. Don't delay.
  num_unacked = 0;
  send_delay = kSendDelayShort;

  seqno = seqno + 1;
  pending = 0;
  debuglog_send(dispatcher);
}
