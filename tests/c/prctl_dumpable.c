/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <stdio.h>
#include <sys/prctl.h>

static int expect_dumpable(int expected) {
  int observed = prctl(PR_GET_DUMPABLE);
  if (observed < 0) {
    perror("PR_GET_DUMPABLE");
    return 1;
  }
  if (observed != expected) {
    fprintf(stderr, "dumpable: expected %d, observed %d\n", expected, observed);
    return 1;
  }
  return 0;
}

int main(void) {
  if (expect_dumpable(1) != 0) {
    return 1;
  }
  if (prctl(PR_SET_DUMPABLE, 0) != 0 || expect_dumpable(0) != 0) {
    perror("clear dumpable");
    return 2;
  }
  if (prctl(PR_SET_DUMPABLE, 1) != 0 || expect_dumpable(1) != 0) {
    perror("restore dumpable");
    return 3;
  }

  errno = 0;
  if (prctl(PR_SET_DUMPABLE, 2) != -1 || errno != EINVAL) {
    fprintf(stderr, "invalid dumpable value: result/errno mismatch (%d)\n",
            errno);
    return 4;
  }
  puts("dumpable=1->0->1 invalid=EINVAL");
  return 0;
}
