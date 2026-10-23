/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Takes an uncontended priority-inheritance futex with FUTEX_LOCK_PI and
 * prints the result. Natively it succeeds (0). Detcore does not emulate PI
 * futexes, so under Hermit the command is refused by name: a fail-closed run
 * stops with the policy-refusal status, and a run with
 * --allow-unsupported-syscalls returns ENOSYS, which is what Linux returns for
 * every PI command when built without CONFIG_FUTEX_PI
 * (https://github.com/rrnewton/hermit/issues/3958). It used to panic Detcore.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static uint32_t word;

int main(void) {
  long ret = syscall(SYS_futex, &word, FUTEX_LOCK_PI_PRIVATE, 0, NULL, NULL, 0);
  if (ret < 0) {
    printf("lock_pi: -1 %s\n", strerrorname_np(errno));
  } else {
    printf("lock_pi: %ld\n", ret);
  }
  return 0;
}
