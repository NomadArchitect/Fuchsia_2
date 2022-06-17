// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <sys/epoll.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <unistd.h>

#include <gtest/gtest.h>

TEST(UnixSocket, HupEvent) {
  int fds[2];

  ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, fds));

  int epfd = epoll_create1(0);
  ASSERT_LT(-1, epfd);
  epoll_event ev = {EPOLLIN, {.u64 = 42}};
  ASSERT_EQ(0, epoll_ctl(epfd, EPOLL_CTL_ADD, fds[0], &ev));

  epoll_event outev = {0, {.u64 = 0}};

  int no_ready = epoll_wait(epfd, &outev, 1, 0);
  ASSERT_EQ(0, no_ready);

  close(fds[1]);

  no_ready = epoll_wait(epfd, &outev, 1, 0);
  ASSERT_EQ(1, no_ready);
  ASSERT_EQ(EPOLLIN | EPOLLHUP, outev.events);
  ASSERT_EQ(42ul, outev.data.u64);

  close(fds[0]);
  close(epfd);
}

struct read_info_spec {
  unsigned char* mem;
  size_t length;
  size_t bytes_read;
  int fd;
};

void* reader(void* arg) {
  read_info_spec* read_info = reinterpret_cast<read_info_spec*>(arg);
  while (read_info->bytes_read < read_info->length) {
    size_t to_read = read_info->length - read_info->bytes_read;
    fflush(stdout);
    ssize_t bytes_read = read(read_info->fd, read_info->mem + read_info->bytes_read, to_read);
    EXPECT_LT(-1, bytes_read);
    if (bytes_read < 0) {
      return nullptr;
    }
    read_info->bytes_read += bytes_read;
  }
  return nullptr;
}

TEST(UnixSocket, BigWrite) {
  const size_t write_size = 300000;
  unsigned char* send_mem = new unsigned char[write_size];
  ASSERT_TRUE(send_mem != nullptr);

  for (size_t i = 0; i < write_size; i++) {
    send_mem[i] = 0xff & random();
  }

  int fds[2];
  ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, fds));

  read_info_spec read_info;
  read_info.mem = new unsigned char[write_size];
  bzero(read_info.mem, sizeof(unsigned char) * write_size);
  ASSERT_TRUE(read_info.mem != nullptr);
  read_info.length = write_size;
  read_info.fd = fds[1];
  read_info.bytes_read = 0;

  pthread_t read_thread;
  ASSERT_EQ(0, pthread_create(&read_thread, nullptr, reader, &read_info));
  size_t write_count = 0;
  while (write_count < write_size) {
    size_t to_send = write_size - write_count;
    ssize_t bytes_read = write(fds[0], send_mem + write_count, to_send);
    ASSERT_LT(-1, bytes_read);
    write_count += bytes_read;
  }

  ASSERT_EQ(0, pthread_join(read_thread, nullptr));

  close(fds[0]);
  close(fds[1]);

  ASSERT_EQ(write_count, read_info.bytes_read);
  ASSERT_EQ(0, memcmp(send_mem, read_info.mem, sizeof(unsigned char) * write_size));

  delete[] send_mem;
  delete[] read_info.mem;
}
