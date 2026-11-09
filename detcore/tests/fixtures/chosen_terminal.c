/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Actual terminal paths followed by a later process clone. The CLI harness
 * must run these under strict Chosen and compare native stdout as well as
 * requiring the Chosen summary/no host-ID warning. A plain successful run
 * alone does not establish that the later clone retained Chosen authority.
 */
#define _GNU_SOURCE
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static _Atomic int departed;
static int zombie_mode;
static const size_t STACK_BYTES = 128 * 1024;

static void fail(void) {
  static const char error[] = "terminal fixture failed\n";
  (void)write(2, error, sizeof(error) - 1);
  syscall(SYS_exit_group, 90);
  __builtin_unreachable();
}

static void later_clone(void) {
  /* No clone3 unknown-input route: this actual register-only fork operation
   * must use a fresh Chosen rewrite, not silently fall back to host IDs. */
#if defined(__x86_64__)
  long child = syscall(SYS_clone, (unsigned long)SIGCHLD, 0, 0, 0, 0);
#elif defined(__aarch64__)
  long child = syscall(SYS_clone, (unsigned long)SIGCHLD, 0, 0, 0, 0);
#else
#error "chosen terminal fixture needs a supported Linux ABI"
#endif
  if (child < 0) fail();
  if (child == 0) syscall(SYS_exit, 0);
  int status = -1;
  if (waitpid((pid_t)child, &status, 0) != child || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) fail();
  static const char ordinary[] = "ordinary exit: later clone completed\n";
  static const char zombie[] = "zombie leader: later clone completed\n";
  const char *text = zombie_mode ? zombie : ordinary;
  size_t length = zombie_mode ? sizeof(zombie) - 1 : sizeof(ordinary) - 1;
  if (write(1, text, length) != (ssize_t)length) fail();
}

static int sibling(void *unused) {
  (void)unused;
  if (!zombie_mode) return 0; /* libc clone trampoline executes raw exit. */
  while (atomic_load_explicit(&departed, memory_order_seq_cst) != 0)
    syscall(SYS_sched_yield);
  later_clone();
  return 0;
}

int main(int argc, char **argv) {
  if (argc != 2) return 64;
  zombie_mode = strcmp(argv[1], "zombie-leader") == 0;
  if (!zombie_mode && strcmp(argv[1], "ordinary-thread-exit") != 0) return 64;
  void *stack = mmap(NULL, STACK_BYTES, PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS | MAP_STACK, -1, 0);
  if (stack == MAP_FAILED) fail();
  int flags = CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD;
  if (zombie_mode) {
    atomic_store_explicit(&departed, (int)syscall(SYS_gettid), memory_order_seq_cst);
    if (syscall(SYS_set_tid_address, &departed) <= 0) fail();
  } else {
    /* PARENT_SETTID is complete before clone returns; CLEARTID is the native
     * exit effect this control waits for, not an exit callback observation. */
    flags |= CLONE_PARENT_SETTID | CLONE_CHILD_CLEARTID;
    atomic_store_explicit(&departed, -1, memory_order_seq_cst);
  }
  int tid = clone(sibling, (char *)stack + STACK_BYTES, flags, NULL,
                  &departed, NULL, &departed);
  if (tid <= 0) fail();
  if (zombie_mode) {
    syscall(SYS_exit, 0); /* Leader is a zombie while sibling keeps running. */
    __builtin_unreachable();
  }
  while (atomic_load_explicit(&departed, memory_order_seq_cst) != 0)
    syscall(SYS_sched_yield);
  later_clone();
  if (munmap(stack, STACK_BYTES) != 0) fail();
  return 0;
}
