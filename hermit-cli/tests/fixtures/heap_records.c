/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* A heap for --detlog-heap to record: the first brk grows the break to an
 * unaligned end, an mprotect splits the heap, the break goes back to a page
 * boundary, and the guest unmaps the heap. */

#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>
extern char end;
int main(void) {
  /* The first brk grows the break, to an unaligned end: no query comes
     before it. */
  unsigned long start = ((unsigned long)&end + 4095) & ~4095UL;
  long grown = syscall(SYS_brk, start + 4 * 4096 + 1);
  memset((void *)start, 0x5a, 4 * 4096 + 1);
  /* Split the heap: its second page read-only. */
  int split = mprotect((void *)(start + 4096), 4096, PROT_READ);
  long query = syscall(SYS_brk, 0);
  /* Back to a page boundary, then unmap it all; the break stays. */
  long aligned = syscall(SYS_brk, start + 4 * 4096);
  int gone = munmap((void *)start, 4 * 4096);
  long after = syscall(SYS_brk, 0);
  printf("grown %d split %d query %d gone %d after %d\n", grown == (long)(start + 4 * 4096 + 1), split,
         query == grown, gone, after == aligned && aligned == (long)(start + 4 * 4096));
  return 0;
}
