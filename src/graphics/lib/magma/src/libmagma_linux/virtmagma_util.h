// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_GRAPHICS_LIB_MAGMA_SRC_LIBMAGMA_LINUX_VIRTMAGMA_UTIL_H_
#define SRC_GRAPHICS_LIB_MAGMA_SRC_LIBMAGMA_LINUX_VIRTMAGMA_UTIL_H_

#include <errno.h>
#include <sys/fcntl.h>
#include <sys/ioctl.h>
#include <sys/unistd.h>

#include "src/graphics/lib/magma/src/magma_util/macros.h"
#include "virtmagma.h"

inline bool virtmagma_handshake(int32_t file_descriptor) {
  if (fcntl(file_descriptor, F_GETFD) == -1) {
    DMESSAGE("Invalid file descriptor: %d\n", errno);
    return false;
  }

  virtmagma_ioctl_args_handshake handshake{};
  handshake.handshake_inout = VIRTMAGMA_HANDSHAKE_SEND;
  if (ioctl(file_descriptor, VIRTMAGMA_IOCTL_HANDSHAKE, &handshake)) {
    DMESSAGE("ioctl(HANDSHAKE) failed: %d\n", errno);
    return false;
  }

  if (handshake.handshake_inout != VIRTMAGMA_HANDSHAKE_RECV) {
    DMESSAGE("Handshake failed: 0x%08X\n", handshake.handshake_inout);
    return false;
  }

  uint32_t version_major = 0;
  uint32_t version_minor = 0;
  uint32_t version_patch = 0;
  VIRTMAGMA_GET_VERSION(handshake.version_out, version_major, version_minor, version_patch);
  DMESSAGE("Successfully connected to virtio-magma driver (version %d.%d.%d)\n", version_major,
           version_minor, version_patch);

  return true;
}

inline bool virtmagma_send_command(int32_t file_descriptor, void* request, size_t request_size,
                                   void* response, size_t response_size) {
  virtmagma_ioctl_args_magma_command command{};
  command.request_address = (uint64_t)request;
  command.request_size = request_size;
  command.response_address = (uint64_t)response;
  command.response_size = response_size;
  if (ioctl(file_descriptor, VIRTMAGMA_IOCTL_MAGMA_COMMAND, &command)) {
    DMESSAGE("ioctl(MAGMA_COMMAND) failed: %d\n", errno);
    return false;
  }
  return true;
}

class OwnedFd {
 public:
  OwnedFd(int fd) : fd_(fd) {}
  ~OwnedFd() {
    if (fd_ >= 0)
      close(fd_);
  }

  OwnedFd(const OwnedFd&) = delete;
  OwnedFd(OwnedFd&& other) {
    fd_ = other.fd_;
    other.fd_ = -1;
  }

  int fd() const { return fd_; }

 private:
  int fd_;
};

template <class T, class U, int magic>
class VirtmagmaObject {
 public:
  static VirtmagmaObject* Create(T object, U parent) {
    return new VirtmagmaObject(object, std::move(parent));
  }
  static VirtmagmaObject* Get(T object) {
    auto p = reinterpret_cast<VirtmagmaObject*>(object);
    DASSERT(p->magic_ == magic);
    return p;
  }
  T Wrap() { return reinterpret_cast<T>(this); }
  T& Object() { return object_; }
  U& Parent() { return parent_; }

 private:
  VirtmagmaObject(T object, U parent)
      : object_{object}, parent_{std::move(parent)}, magic_{magic} {}
  T object_;
  U parent_;
  int magic_;
};

typedef VirtmagmaObject<magma_connection_t, int32_t, 0x1111> virtmagma_connection_t;
typedef VirtmagmaObject<magma_buffer_t, magma_connection_t, 0x2222> virtmagma_buffer_t;
typedef VirtmagmaObject<magma_semaphore_t, magma_connection_t, 0x3333> virtmagma_semaphore_t;
typedef VirtmagmaObject<magma_device_t, OwnedFd, 0x5555> virtmagma_device_t;

#endif  // SRC_GRAPHICS_LIB_MAGMA_SRC_LIBMAGMA_LINUX_VIRTMAGMA_UTIL_H_
