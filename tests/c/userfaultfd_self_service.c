/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * One process serves its own userfaultfd: it registers two anonymous pages
 * for missing-page faults, resolves both ahead of use with UFFDIO_COPY, reads
 * them, and exits while they are still registered. This is the common
 * single-threaded userfaultfd shape. A backend whose process exits complete
 * asynchronously holds the schedule until each exit is physically complete,
 * and still supports it (coord ruling D.2).
 *
 * Prints "userfaultfd-unavailable <errno name>" and exits 0 when the kernel
 * refuses an unprivileged userfaultfd (EPERM, ENOSYS, or EINVAL for a kernel
 * without UFFD_USER_MODE_ONLY), so the caller can skip. Any other failure is
 * a failure.
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <errno.h>
#include <fcntl.h>
#include <linux/userfaultfd.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef UFFD_USER_MODE_ONLY
#define UFFD_USER_MODE_ONLY 1
#endif

static void fail(const char* what) {
  fprintf(stderr, "%s: %s\n", what, strerror(errno));
  exit(1);
}

int main(void) {
  long page = sysconf(_SC_PAGESIZE);
  /* User-mode-only faults need no privilege (Linux 5.11 and later). */
  int uffd = (int)syscall(
      SYS_userfaultfd, O_CLOEXEC | O_NONBLOCK | UFFD_USER_MODE_ONLY);
  if (uffd < 0) {
    if (errno == EPERM || errno == ENOSYS || errno == EINVAL) {
      printf("userfaultfd-unavailable %s\n", strerrorname_np(errno));
      return 0;
    }
    fail("userfaultfd");
  }
  struct uffdio_api api = {.api = UFFD_API, .features = 0};
  if (ioctl(uffd, UFFDIO_API, &api) != 0) {
    fail("UFFDIO_API");
  }

  size_t length = 2 * (size_t)page;
  char* region =
      mmap(NULL, length, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS,
           -1, 0);
  if (region == MAP_FAILED) {
    fail("mmap");
  }
  struct uffdio_register registration = {
      .range = {.start = (uintptr_t)region, .len = length},
      .mode = UFFDIO_REGISTER_MODE_MISSING,
  };
  if (ioctl(uffd, UFFDIO_REGISTER, &registration) != 0) {
    fail("UFFDIO_REGISTER");
  }

  char* source = malloc((size_t)page);
  if (source == NULL) {
    fail("malloc");
  }
  for (int i = 0; i < 2; i++) {
    memset(source, 'a' + i, (size_t)page);
    struct uffdio_copy copy = {
        .dst = (uintptr_t)(region + i * page),
        .src = (uintptr_t)source,
        .len = (size_t)page,
        .mode = 0,
    };
    if (ioctl(uffd, UFFDIO_COPY, &copy) != 0) {
      fail("UFFDIO_COPY");
    }
    if (copy.copy != page) {
      fprintf(stderr, "UFFDIO_COPY copied %lld of %ld bytes\n",
              (long long)copy.copy, page);
      return 1;
    }
  }

  /* Every page is present, so these reads never fault to the descriptor. */
  unsigned long sum = 0;
  for (size_t i = 0; i < length; i++) {
    sum += (unsigned char)region[i];
  }
  printf("userfaultfd-served pages=2 sum=%lu\n", sum);
  fflush(stdout);
  /* Exit still registered: the kernel tears the registration down. */
  return 0;
}
