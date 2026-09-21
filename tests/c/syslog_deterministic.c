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
#include <unistd.h>

enum { SYSLOG_ACTION_SIZE_BUFFER = 10 };

int main(void) {
  long result = syscall(SYS_syslog, SYSLOG_ACTION_SIZE_BUFFER, NULL, 0);
  if (result < 0) {
    fprintf(stderr, "syslog failed: %s\n", strerror(errno));
    return 1;
  }

  errno = 0;
  long invalid_result = syscall(SYS_syslog, 11, NULL, 0);
  if (invalid_result != -1 || errno != EINVAL) {
    fprintf(stderr, "invalid syslog returned %ld/%d, expected EINVAL\n",
            invalid_result, errno);
    return 1;
  }
  printf("syslog-ok size=%ld\n", result);
  return 0;
}
