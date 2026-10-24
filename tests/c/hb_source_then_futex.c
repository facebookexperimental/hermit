/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A happens-before source thread that blocks right after its source fires.
 * The parent makes eight getpid calls and then FUTEX_WAITs on a shared word;
 * the forked child makes 256 getpid calls, stores 1 to the word and wakes the
 * parent. hermit-cli/tests/cli.rs anchors the edge "parent's last getpid"
 * before "a child getpid in the middle of its loop": the child is parked, the
 * parent's source fires and the parent blocks in FUTEX_WAIT, so only the
 * re-admitted child can make progress. The run must complete and print
 * "wake-completed" (https://github.com/rrnewton/hermit/issues/3149; the
 * program is the reproduction from the review of
 * https://github.com/rrnewton/hermit/pull/3897).
 */
#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <stdatomic.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
  _Atomic int *word =
      mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  if (word == MAP_FAILED) {
    return 10;
  }
  atomic_init(word, 0);
  pid_t child = fork();
  if (child < 0) {
    return 11;
  }
  if (!child) {
    for (unsigned i = 0; i != 256; ++i) {
      if (syscall(SYS_getpid) <= 0) {
        _exit(12);
      }
    }
    atomic_store_explicit(word, 1, memory_order_release);
    if (syscall(SYS_futex, word, FUTEX_WAKE, 1, 0, 0, 0) < 0) {
      _exit(13);
    }
    _exit(0);
  }
  /* The last getpid is the source anchor; the next syscall is FUTEX_WAIT. */
  for (unsigned i = 0; i != 8; ++i) {
    if (syscall(SYS_getpid) <= 0) {
      return 14;
    }
  }
  long result = syscall(SYS_futex, word, FUTEX_WAIT, 0, 0, 0, 0);
  if (result < 0 && errno != EAGAIN) {
    return 15;
  }
  if (atomic_load_explicit(word, memory_order_acquire) != 1) {
    return 16;
  }
  int status = 0;
  if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
      WEXITSTATUS(status)) {
    return 17;
  }
  if (write(STDOUT_FILENO, "wake-completed\n", 15) != 15) {
    return 18;
  }
  return munmap(word, 4096) == 0 ? 0 : 19;
}
