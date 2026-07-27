/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <unistd.h>

#ifndef SYS_process_vm_writev
#define SYS_process_vm_writev 311
#endif

int main(void) {
  unsigned char source = 0x5a;
  unsigned char destination = 0xa5;
  struct iovec local = {
      .iov_base = &source,
      .iov_len = sizeof(source),
  };
  struct iovec remote = {
      .iov_base = &destination,
      .iov_len = sizeof(destination),
  };

  errno = 0;
  long result = syscall(SYS_process_vm_writev, getpid(), &local, 1, &remote, 1, 0);
  if (result == -1 && errno == EPERM && destination == 0xa5) {
    puts("process-vm-writev-refused-ok");
    return 0;
  }

  fprintf(stderr,
          "process_vm_writev: expected EPERM/no copy, got result=%ld errno=%d "
          "destination=%#x\n",
          result, errno, destination);
  return 1;
}
