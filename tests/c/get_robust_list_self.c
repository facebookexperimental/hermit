/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <linux/futex.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
  struct robust_list_head *head = NULL;
  size_t length = 0;

  if (syscall(SYS_get_robust_list, 0, &head, &length) != 0) {
    perror("get_robust_list self");
    return 1;
  }
  if (head == NULL || length != sizeof(*head)) {
    fprintf(stderr, "unexpected self robust-list shape\n");
    return 1;
  }

  puts("self robust list queried");
  return 0;
}
