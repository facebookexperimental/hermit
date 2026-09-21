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
#include <sys/syscall.h>
#include <sys/timex.h>
#include <time.h>
#include <unistd.h>

int main(void) {
  struct timex tx = {0};
  long result = syscall(SYS_clock_adjtime, CLOCK_REALTIME, &tx);
  if (result < 0) {
    fprintf(stderr, "clock_adjtime failed: %s\n", strerror(errno));
    return 1;
  }

  struct timex mutation = {.modes = ADJ_OFFSET, .offset = 1};
  errno = 0;
  long mutation_result =
      syscall(SYS_clock_adjtime, CLOCK_REALTIME, &mutation);
  if (mutation_result != -1 || errno != EPERM) {
    fprintf(stderr,
            "clock_adjtime mutation returned %ld/%d, expected EPERM\n",
            mutation_result, errno);
    return 1;
  }
  printf("clock-adjtime-ok state=%ld status=%d tick=%ld\n", result,
         tx.status, tx.tick);
  return 0;
}
