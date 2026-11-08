/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * numa_node_identity — backend-parity contract for Detcore's no-NUMA
 * platform (https://github.com/rrnewton/hermit/issues/5269).
 *
 * Detcore presents a kernel built without NUMA. Linux compiles
 * mm/mempolicy.o only under CONFIG_NUMA and the memory-policy entry
 * points are cond_syscall stubs (kernel/sys_ni.c), so get_mempolicy(2)
 * and move_pages(2) fail with ENOSYS before any argument is looked at,
 * and write nothing through their output pointers. That fixed answer is
 * bitwise-identical across --verify repeat runs and under record/replay,
 * and must match across backends: DBT and KVM have to mirror the golden
 * ptrace reference exactly.
 *
 * This fixture uses the RAW syscalls (no libnuma) so it observes precisely
 * what Detcore's dispatch returns. It calls get_mempolicy and a
 * move_pages location query repeatedly (over a resident and a
 * not-present page, the pair whose answers used to expose the host's
 * residency), asserting -1/ENOSYS every time and that the mode and status
 * buffers keep their poison values. On a real NUMA host both calls
 * succeed, so a native run diverges — proving this pins a genuine
 * platform policy rather than a tautology.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

/* Poison sentinel: a value neither Detcore nor the kernel writes. */
#define SENTINEL 0x7f

static int query_move_pages_enosys(int iter, void* const pages[2]) {
  int status[2] = {SENTINEL, SENTINEL};

  /* nodes == NULL selects location-query mode; pid 0 targets this process. */
  errno = 0;
  long ret = syscall(
      SYS_move_pages,
      0,
      (unsigned long)2,
      pages,
      (int*)NULL,
      status,
      (unsigned long)0);
  if (ret != -1 || errno != ENOSYS) {
    fprintf(
        stderr,
        "iter %d: move_pages query returned %ld errno %d, expected -1/ENOSYS\n",
        iter,
        ret,
        errno);
    return 1;
  }
  if (status[0] != SENTINEL || status[1] != SENTINEL) {
    fprintf(
        stderr,
        "iter %d: move_pages wrote status %d,%d on its ENOSYS answer\n",
        iter,
        status[0],
        status[1]);
    return 1;
  }
  return 0;
}

int main(void) {
  /* get_mempolicy: no such syscall on a kernel without NUMA. */
  int mode = SENTINEL;
  errno = 0;
  long pol = syscall(
      SYS_get_mempolicy,
      &mode,
      (unsigned long*)NULL,
      (unsigned long)0,
      (void*)NULL,
      (unsigned long)0);
  if (pol != -1 || errno != ENOSYS || mode != SENTINEL) {
    fprintf(
        stderr,
        "get_mempolicy returned %ld errno %d mode %d, expected -1/ENOSYS "
        "with the mode untouched\n",
        pol,
        errno,
        mode);
    return 1;
  }

  long page_size = sysconf(_SC_PAGESIZE);
  if (page_size <= 0) {
    perror("sysconf");
    return 1;
  }

  unsigned char* region = mmap(
      NULL,
      (size_t)page_size * 2,
      PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS,
      -1,
      0);
  if (region == MAP_FAILED) {
    perror("mmap");
    return 1;
  }

  /* Fault in only the first page; the second stays not-present. Neither
   * matters now: the stub answers before it looks at either. */
  region[0] = 1;

  void* const pages[2] = {region, region + page_size};

  /* Repeat to prove the answer is stable, not incidental. */
  for (int i = 0; i < 4; i++) {
    if (query_move_pages_enosys(i, pages)) {
      return 1;
    }
  }

  /* Emit the observations rather than a success token: ENOSYS from both
   * calls is the !CONFIG_NUMA answer this contract exists to pin, and a
   * backend that still emulated a one-node machine (as Detcore did:
   * MPOL_DEFAULT and node 0) prints different bytes. */
  printf("numa-node-identity get_mempolicy=ENOSYS move_pages=ENOSYS\n");
  return 0;
}
