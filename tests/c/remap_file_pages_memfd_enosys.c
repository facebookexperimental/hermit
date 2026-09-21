/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <linux/memfd.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
  long page_size = sysconf(_SC_PAGESIZE);
  if (page_size <= 0) {
    perror("sysconf");
    return 1;
  }

  int fd = (int)syscall(SYS_memfd_create, "hermit-remap", MFD_CLOEXEC);
  if (fd < 0 || ftruncate(fd, page_size * 2) != 0) {
    perror("memfd setup");
    if (fd >= 0) {
      close(fd);
    }
    return 1;
  }

  void *mapping = mmap(NULL, (size_t)page_size * 2, PROT_READ | PROT_WRITE,
                       MAP_SHARED, fd, 0);
  if (mapping == MAP_FAILED) {
    perror("mmap");
    close(fd);
    return 1;
  }

  errno = 0;
  long result = syscall(SYS_remap_file_pages, mapping, (size_t)page_size, 0, 1, 0);
  if (result != -1 || errno != ENOSYS) {
    fprintf(stderr,
            "memfd remap_file_pages returned %ld with errno %d (%s), "
            "expected ENOSYS\n",
            result, errno, strerror(errno));
    munmap(mapping, (size_t)page_size * 2);
    close(fd);
    return 1;
  }

  munmap(mapping, (size_t)page_size * 2);
  close(fd);
  puts("memfd nonlinear mappings deterministically unavailable");
  return 0;
}
