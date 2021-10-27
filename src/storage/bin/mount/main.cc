// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <errno.h>
#include <fcntl.h>
#include <getopt.h>
#include <lib/fdio/directory.h>
#include <lib/fdio/fd.h>
#include <lib/fdio/fdio.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>
#include <zircon/processargs.h>
#include <zircon/syscalls.h>

#include <fs-management/mount.h>

int usage(void) {
  fprintf(stderr,
          "usage: mount [ <option>* ] devicepath mountpath\n"
          "options: \n"
          " -r|--readonly  : Open the filesystem as read-only\n"
          " -m|--metrics   : Collect filesystem metrics\n"
          " -v|--verbose   : Verbose mode\n"
          " -h|--help      : Display this message\n");
  return -1;
}

int parse_args(int argc, char** argv, MountOptions* options, char** devicepath, char** mountpath) {
  while (1) {
    static struct option opts[] = {
        {"readonly", no_argument, NULL, 'r'}, {"metrics", no_argument, NULL, 'm'},
        {"verbose", no_argument, NULL, 'v'},  {"compression", required_argument, NULL, 'c'},
        {"help", no_argument, NULL, 'h'},     {NULL, 0, NULL, 0},
    };
    int opt_index;
    int c = getopt_long(argc, argv, "rmvc:h", opts, &opt_index);
    if (c < 0) {
      break;
    }
    switch (c) {
      case 'r':
        options->readonly = true;
        break;
      case 'm':
        options->collect_metrics = true;
        break;
      case 'v':
        options->verbose_mount = true;
        break;
      case 'c':
        options->write_compression_algorithm = optarg;
        break;
      case 'h':
      default:
        return usage();
    }
  }

  argc -= optind;
  argv += optind;

  if (argc < 2) {
    return usage();
  }
  *devicepath = argv[0];
  *mountpath = argv[1];
  return 0;
}

bool should_use_admin_protocol(disk_format_t df) {
  // Newer filesystems (esp. written in rust) don't support the admin protocol, so we won't open it
  // with O_ADMIN.
  switch (df) {
    case DISK_FORMAT_FXFS:
    case DISK_FORMAT_FAT:
      return false;
    default:
      return true;
  }
}

int main(int argc, char** argv) {
  char* devicepath;
  char* mountpath;
  MountOptions options;
  if (int r = parse_args(argc, argv, &options, &devicepath, &mountpath)) {
    return r;
  }

  if (options.verbose_mount) {
    printf("fs_mount: Mounting device [%s] on path [%s]\n", devicepath, mountpath);
  }

  int fd;
  if ((fd = open(devicepath, O_RDWR)) < 0) {
    fprintf(stderr, "Error opening block device\n");
    return -1;
  }
  disk_format_t df = detect_disk_format(fd);
  options.admin = should_use_admin_protocol(df);
  zx_status_t status = mount(fd, mountpath, df, options, launch_logs_async);
  if (status != ZX_OK) {
    fprintf(stderr, "fs_mount: Error while mounting: %d\n", status);
  }
  return status;
}
