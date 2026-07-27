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
  unsigned char byte = 0;
  struct iovec local = {.iov_base = &byte, .iov_len = sizeof(byte)};
  struct iovec remote = {.iov_base = (void *)(uintptr_t)1,
                         .iov_len = sizeof(byte)};

  errno = 0;
  long result = syscall(SYS_process_vm_writev, 1, &local, 1, &remote, 1, 0);
  if (result == -1 && errno == EPERM) {
    puts("process-vm-writev-refused-ok");
    return 0;
  }

  fprintf(stderr,
          "process_vm_writev: expected EPERM, got result=%ld errno=%d\n",
          result, errno);
  return 1;
}
