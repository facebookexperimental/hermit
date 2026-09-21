/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <linux/bpf.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
  union bpf_attr attr;
  memset(&attr, 0, sizeof(attr));
  attr.map_type = BPF_MAP_TYPE_ARRAY;
  attr.key_size = sizeof(unsigned int);
  attr.value_size = sizeof(unsigned int);
  attr.max_entries = 1;

  errno = 0;
  long result = syscall(SYS_bpf, BPF_MAP_CREATE, &attr, sizeof(attr));
  if (result != -1 || errno != ENOSYS) {
    fprintf(stderr, "bpf returned %ld with errno %d (%s), expected ENOSYS\n",
            result, errno, strerror(errno));
    return 1;
  }
  puts("bpf deterministically unavailable");
  return 0;
}
