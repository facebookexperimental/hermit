/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <stdio.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
  struct rlimit original = {0};
  struct rlimit changed = {0};
  struct rlimit previous = {0};
  struct rlimit observed = {0};
  pid_t self = getpid();

  if (syscall(SYS_prlimit64, 0, RLIMIT_NOFILE, NULL, &original) != 0) {
    perror("prlimit64 query");
    return 1;
  }
  changed = original;
  if (changed.rlim_cur > 0) {
    changed.rlim_cur--;
  }
  if (syscall(SYS_prlimit64, self, RLIMIT_NOFILE, &changed, &previous) != 0) {
    perror("prlimit64 virtual-self mutation");
    return 2;
  }
  if (previous.rlim_cur != original.rlim_cur ||
      previous.rlim_max != original.rlim_max) {
    fputs("prlimit64 previous-limit mismatch\n", stderr);
    return 3;
  }
  if (syscall(SYS_prlimit64, 0, RLIMIT_NOFILE, NULL, &observed) != 0 ||
      observed.rlim_cur != changed.rlim_cur ||
      observed.rlim_max != changed.rlim_max) {
    fputs("prlimit64 pid-zero observation mismatch\n", stderr);
    return 4;
  }

  errno = 0;
  if (syscall(SYS_prlimit64, self + 1, RLIMIT_NOFILE, (void *)1, NULL) != -1 ||
      errno != EFAULT) {
    fprintf(stderr, "prlimit64 bad input returned errno=%d\n", errno);
    return 5;
  }
  errno = 0;
  if (syscall(SYS_prlimit64, self + 1, RLIMIT_NOFILE, NULL, NULL) != -1 ||
      errno != EPERM) {
    fprintf(stderr, "prlimit64 other process returned errno=%d\n", errno);
    return 6;
  }
  if (syscall(SYS_prlimit64, 0, RLIMIT_NOFILE, &original, NULL) != 0) {
    perror("prlimit64 restore");
    return 7;
  }

  puts("dbi-prlimit-self-ok");
  return 0;
}
