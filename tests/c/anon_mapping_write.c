/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Maps two private anonymous pages between two PROT_NONE guard pages, so the
 * kernel merges them with no neighbouring mapping, prints their address, and writes
 * one byte into its second page between two marker calls (`getppid`), so a
 * run with `--detlog-anon` shows that mapping's digest unchanged across the
 * first marker and changed at the second, and with `--detlog-anon=pages`
 * shows page 1 listed only after the write.
 */

#define _GNU_SOURCE

#include <stdio.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
  const size_t page = 4096;
  unsigned char* guarded = mmap(NULL, 4 * page, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (guarded == MAP_FAILED) {
    perror("mmap");
    return 1;
  }
  unsigned char* map = guarded + page;
  if (mprotect(map, 2 * page, PROT_READ | PROT_WRITE) != 0) {
    perror("mprotect");
    return 1;
  }
  printf("mapping %#lx-%#lx\n", (unsigned long)map, (unsigned long)(map + 2 * page));
  fflush(stdout);
  syscall(SYS_getppid);
  syscall(SYS_getppid);
  map[page + 7] = 1;
  syscall(SYS_getppid);
  return 0;
}
