/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <unistd.h>

static int invalid_tv(void) {
  errno = 0;
  long rc = syscall(SYS_gettimeofday, (void*)-1, NULL);
  if (rc != -1 || errno != EFAULT) {
    fprintf(stderr, "invalid tv: rc=%ld errno=%d\n", rc, errno);
    return 1;
  }
  puts("invalid-tv: EFAULT");
  return 0;
}

static int faulting_tz(void) {
  long page_size = sysconf(_SC_PAGESIZE);
  if (page_size <= 0) {
    perror("sysconf");
    return 2;
  }
  void* page = mmap(
      NULL,
      (size_t)page_size,
      PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS,
      -1,
      0);
  if (page == MAP_FAILED) {
    perror("mmap");
    return 3;
  }
  if (munmap(page, (size_t)page_size) != 0) {
    perror("munmap");
    return 4;
  }

  struct timeval tv = {.tv_sec = 123, .tv_usec = 456};
  errno = 0;
  long rc = syscall(SYS_gettimeofday, &tv, page);
  if (rc != -1 || errno != EFAULT) {
    fprintf(stderr, "faulting tz: rc=%ld errno=%d\n", rc, errno);
    return 5;
  }
  if (tv.tv_sec != 0 || tv.tv_usec != 0) {
    fprintf(
        stderr,
        "faulting tz: tv=%ld.%06ld\n",
        (long)tv.tv_sec,
        (long)tv.tv_usec);
    return 6;
  }
  printf(
      "faulting-tz: EFAULT tv=%ld.%06ld\n", (long)tv.tv_sec, (long)tv.tv_usec);
  return 0;
}

int main(int argc, char** argv) {
  if (argc != 2) {
    return 64;
  }
  if (strcmp(argv[1], "invalid-tv") == 0) {
    return invalid_tv();
  }
  if (strcmp(argv[1], "faulting-tz") == 0) {
    return faulting_tz();
  }
  return 64;
}
