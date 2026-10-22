/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * FUTEX_WAKE_OP on a word in shared memory
 * (https://github.com/rrnewton/hermit/pull/4020). Linux changes the word with a
 * locked instruction, so an update by another process sharing the page is
 * never lost. Detcore applies the operation as a read and a write while it
 * holds the only running guest thread. That is atomic for an anonymous shared
 * mapping, which only guest processes can reach, but not for a file-backed one
 * that a process outside Hermit could map. So Hermit refuses the second case by
 * name: a fail-closed run stops with the policy-refusal status, and a run with
 * --allow-unsupported-syscalls gets ENOSYS. Both aliases left by an
 * MREMAP_DONTUNMAP of the file-backed word are refused the same way. Natively
 * every call succeeds.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

static void wake_op_add_one(const char* what, uint32_t* word) {
  static uint32_t first;
  long ret = syscall(
      SYS_futex,
      &first,
      FUTEX_WAKE_OP_PRIVATE,
      0,
      0,
      word,
      FUTEX_OP(FUTEX_OP_ADD, 1, FUTEX_OP_CMP_EQ, 0));
  if (ret < 0) {
    printf("%s: -1 %s, word %u\n", what, strerrorname_np(errno), *word);
  } else {
    printf("%s: %ld, word %u\n", what, ret, *word);
  }
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  uint32_t* anonymous = mmap(
      NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  wake_op_add_one("wake_op +1, anonymous shared word", anonymous);

  int fd = memfd_create("futex-word", 0);
  if (fd < 0 || ftruncate(fd, 4096) != 0) {
    perror("memfd");
    return 1;
  }
  uint32_t* file = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  wake_op_add_one("wake_op +1, file-backed shared word", file);

  /* MREMAP_DONTUNMAP keeps the old mapping, so both the old and the new
   * address alias the same file-backed word. */
  uint32_t* moved = mremap(file, 4096, 4096, MREMAP_MAYMOVE | MREMAP_DONTUNMAP, (void*)0);
  if (moved == MAP_FAILED) {
    perror("mremap");
    return 1;
  }
  wake_op_add_one("wake_op +1, old alias after MREMAP_DONTUNMAP", file);
  wake_op_add_one("wake_op +1, new alias after MREMAP_DONTUNMAP", moved);
  return 0;
}
