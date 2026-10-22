/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
  long pid = syscall(SYS_getpid);
  long tid = syscall(SYS_gettid);
  if (pid <= 0 || pid != tid || syscall(SYS_gettid) != tid ||
      syscall(SYS_getpid) != pid) {
    return 1;
  }
  printf("own pid=%ld tid=%ld\n", pid, tid);
  return 0;
}
