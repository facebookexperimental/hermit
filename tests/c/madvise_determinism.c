/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdbool.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#ifndef MADV_FREE
#define MADV_FREE 8
#endif
#ifndef MADV_POPULATE_READ
#define MADV_POPULATE_READ 22
#endif

static int expect_errno(void *address, size_t length, int advice, int expected) {
  errno = 0;
  if (madvise(address, length, advice) != -1 || errno != expected) {
    fprintf(stderr, "madvise(%d) expected errno %d, got %d\n", advice,
            expected, errno);
    return 1;
  }
  return 0;
}

int main(int argc, char **argv) {
  const bool kvm = argc == 2 && strcmp(argv[1], "--kvm") == 0;
  const long page_size_raw = sysconf(_SC_PAGESIZE);
  const bool recording = argc == 2 && strcmp(argv[1], "--record") == 0;
  if (page_size_raw <= 0) {
    return 10;
  }
  const size_t page_size = (size_t)page_size_raw;

  unsigned char *anonymous =
      mmap(NULL, page_size, PROT_READ | PROT_WRITE,
           MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (anonymous == MAP_FAILED) {
    return 11;
  }
  anonymous[0] = 0x5a;

  if (madvise(anonymous, page_size, MADV_WILLNEED) != 0 ||
      madvise(anonymous, page_size, MADV_FREE) != 0 ||
      anonymous[0] != 0x5a) {
    return 12;
  }
  if (expect_errno(anonymous, page_size, MADV_POPULATE_READ, EINVAL) ||
      expect_errno(anonymous, page_size, INT_MAX, EINVAL) ||
      expect_errno(anonymous + 1, 0, MADV_FREE, EINVAL)) {
    return 13;
  }

  if (kvm) {
    if (expect_errno(anonymous, page_size, MADV_DONTNEED, ENOSYS)) {
      return 14;
    }
  } else {
    int fd = open(argv[0], O_RDONLY);
    if (fd < 0) {
      return 15;
    }
    unsigned char *file_mapping =
        mmap(NULL, page_size, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    if (file_mapping == MAP_FAILED) {
      return 16;
    }
    const unsigned char original = file_mapping[0];
    const unsigned char modified = file_mapping[0] ^ 0xff;
    file_mapping[0] = modified;
    if (file_mapping[0] == original) {
      return 17;
    }
    if (recording) {
      if (expect_errno(file_mapping, page_size, MADV_DONTNEED, ENOSYS) ||
          file_mapping[0] != modified) {
        return 17;
      }
    } else if (madvise(file_mapping, page_size, MADV_DONTNEED) != 0 ||
               file_mapping[0] != original) {
      return 17;
    }
    if (munmap(file_mapping, page_size) != 0 || close(fd) != 0) {
      return 18;
    }
  }

  if (munmap(anonymous, page_size) != 0) {
    return 19;
  }
  puts("madvise-ok");
  return 0;
}
