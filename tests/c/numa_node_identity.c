/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * numa_node_identity — backend-parity contract for Detcore's single-virtual-node
 * NUMA determinization of get_mempolicy(2) and move_pages(2).
 *
 * Detcore presents exactly one virtual NUMA node. get_mempolicy(2) reports the
 * default policy MPOL_DEFAULT (0), and move_pages(2) in query mode
 * (nodes == NULL) reports node 0 for EVERY page it is asked about — including
 * pages that are not resident, which a real kernel reports as -ENOENT. Those
 * constants are what make the values bitwise-identical across --verify repeat
 * runs and under record/replay, and they must match across backends: DBI and
 * KVM have to mirror the golden ptrace reference exactly.
 *
 * This fixture uses the RAW syscalls (no libnuma) so it observes precisely what
 * Detcore's handlers return. It (1) queries get_mempolicy and asserts the mode
 * is MPOL_DEFAULT, and (2) maps two anonymous pages, faults in only the first,
 * and runs a move_pages location query repeatedly, asserting node 0 for both the
 * resident and the not-present page. On a real host the not-present page's status
 * comes back as -ENOENT (a negative value), so a native run diverges — proving
 * this pins a genuine determinization rather than a tautology.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

/* MPOL_DEFAULT is 0 on Linux; spell it out to avoid a libnuma dependency. */
#define VIRTUAL_MEMPOLICY 0
/* Poison sentinel: a value neither Detcore nor the kernel writes on success. */
#define SENTINEL 0x7f

static int query_move_pages(int iter, void *const pages[2]) {
  int status[2] = {SENTINEL, SENTINEL};

  /* nodes == NULL selects location-query mode; pid 0 targets this process. */
  long ret = syscall(SYS_move_pages, 0, (unsigned long)2, pages, (int *)NULL,
                     status, (unsigned long)0);
  if (ret != 0) {
    fprintf(stderr, "iter %d: move_pages query returned %ld, expected 0\n", iter,
            ret);
    return 1;
  }
  for (int p = 0; p < 2; p++) {
    if (status[p] != 0) {
      fprintf(stderr,
              "iter %d: move_pages status[%d] = %d, expected 0 (virtual node 0)\n",
              iter, p, status[p]);
      return 1;
    }
  }
  return 0;
}

int main(void) {
  /* get_mempolicy: the current policy must be the virtualized MPOL_DEFAULT. */
  int mode = SENTINEL;
  long pol = syscall(SYS_get_mempolicy, &mode, (unsigned long *)NULL,
                     (unsigned long)0, (void *)NULL, (unsigned long)0);
  if (pol != 0 || mode != VIRTUAL_MEMPOLICY) {
    fprintf(stderr,
            "get_mempolicy returned %ld mode %d, expected 0 and MPOL_DEFAULT %d\n",
            pol, mode, VIRTUAL_MEMPOLICY);
    return 1;
  }

  long page_size = sysconf(_SC_PAGESIZE);
  if (page_size <= 0) {
    perror("sysconf");
    return 1;
  }

  unsigned char *region = mmap(NULL, (size_t)page_size * 2,
                               PROT_READ | PROT_WRITE,
                               MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (region == MAP_FAILED) {
    perror("mmap");
    return 1;
  }

  /* Fault in only the first page; the second stays not-present on purpose so
   * that a native kernel would report -ENOENT for it while Detcore reports 0. */
  region[0] = 1;

  void *const pages[2] = {region, region + page_size};

  /* Repeat to prove the determinized answer is stable, not incidental. */
  for (int i = 0; i < 4; i++) {
    if (query_move_pages(i, pages)) {
      return 1;
    }
  }

  puts("numa-node-identity-ok");
  return 0;
}
