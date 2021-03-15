// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fuchsia/io/llcpp/fidl.h>
#include <lib/fdio/watcher.h>
#include <stdio.h>
#include <stdlib.h>
#include <zircon/syscalls.h>
#include <zircon/types.h>

#include "fdio_unistd.h"

namespace fio = fuchsia_io;

using fdio_watcher_t = struct fdio_watcher {
  zx_handle_t h;
  watchdir_func_t func;
  void* cookie;
  int fd;
};

static zx_status_t fdio_watcher_create(int dirfd, fdio_watcher_t** out) {
  fdio_ptr io = fd_to_io(dirfd);
  if (io == nullptr) {
    return ZX_ERR_INVALID_ARGS;
  }

  zx_handle_t handle;
  zx_status_t status = io->borrow_channel(&handle);
  if (status != ZX_OK) {
    return status;
  }
  auto directory = fidl::UnownedClientEnd<fio::Directory>(handle);
  if (!directory.is_valid()) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  zx::status endpoints = fidl::CreateEndpoints<fio::DirectoryWatcher>();
  if (endpoints.is_error()) {
    return endpoints.status_value();
  }

  auto result = fio::Directory::Call::Watch(directory, fio::wire::WATCH_MASK_ALL, 0,
                                            endpoints->server.TakeChannel());
  status = result.status();
  if (status != ZX_OK) {
    return status;
  }
  fio::Directory::WatchResponse* response = result.Unwrap();
  status = response->s;
  if (status != ZX_OK) {
    return status;
  }

  auto watcher = static_cast<fdio_watcher_t*>(malloc(sizeof(fdio_watcher_t)));
  watcher->h = endpoints->client.channel().release();
  *out = watcher;
  return ZX_OK;
}

// watcher process expects the msg buffer to be len + 1 in length
// as it drops temporary nuls in it while dispatching
static zx_status_t fdio_watcher_process(fdio_watcher_t* w, uint8_t* msg, size_t len) {
  // Message Format: { OP, LEN, DATA[LEN] }
  while (len >= 2) {
    unsigned event = *msg++;
    unsigned namelen = *msg++;

    if (len < (namelen + 2u)) {
      break;
    }

    switch (event) {
      case fio::wire::WATCH_EVENT_ADDED:
      case fio::wire::WATCH_EVENT_EXISTING:
        event = WATCH_EVENT_ADD_FILE;
        break;
      case fio::wire::WATCH_EVENT_REMOVED:
        event = WATCH_EVENT_REMOVE_FILE;
        break;
      case fio::wire::WATCH_EVENT_IDLE:
        event = WATCH_EVENT_WAITING;
        break;
      default:
        // unsupported event
        continue;
    }

    uint8_t tmp = msg[namelen];
    msg[namelen] = 0;

    zx_status_t status;
    if ((status = w->func(w->fd, event, (char*)msg, w->cookie)) != ZX_OK) {
      return status;
    }
    msg[namelen] = tmp;
    len -= (namelen + 2);
    msg += namelen;
  }

  return ZX_OK;
}

static zx_status_t fdio_watcher_loop(fdio_watcher_t* w, zx_time_t deadline) {
  for (;;) {
    // extra byte for watcher process use
    uint8_t msg[fio::wire::MAX_BUF + 1];
    uint32_t sz = fio::wire::MAX_BUF;
    zx_status_t status;
    if ((status = zx_channel_read(w->h, 0, msg, nullptr, sz, 0, &sz, nullptr)) < 0) {
      if (status != ZX_ERR_SHOULD_WAIT) {
        return status;
      }
      if ((status = zx_object_wait_one(w->h, ZX_CHANNEL_READABLE | ZX_CHANNEL_PEER_CLOSED, deadline,
                                       nullptr)) < 0) {
        return status;
      }
      continue;
    }

    if ((status = fdio_watcher_process(w, msg, sz)) != ZX_OK) {
      return status;
    }
  }
}

static void fdio_watcher_destroy(fdio_watcher_t* watcher) {
  zx_handle_close(watcher->h);
  free(watcher);
}

__EXPORT
zx_status_t fdio_watch_directory(int dirfd, watchdir_func_t cb, zx_time_t deadline, void* cookie) {
  fdio_watcher_t* watcher = nullptr;

  zx_status_t status;
  if ((status = fdio_watcher_create(dirfd, &watcher)) < 0) {
    return status;
  }

  watcher->func = cb;
  watcher->cookie = cookie;
  watcher->fd = dirfd;
  status = fdio_watcher_loop(watcher, deadline);

  fdio_watcher_destroy(watcher);
  return status;
}
