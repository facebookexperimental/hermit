/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Issues FUTEX_CMP_REQUEUE_PI, the requeue-PI command a PI condition variable
 * would use, with no waiter on either word, and prints the result. Natively it
 * succeeds (0). Detcore emulates the PI lock commands but not the requeue-PI
 * pair, so under Hermit the command is refused by name: a fail-closed run stops
 * with the policy-refusal status, and a run with --allow-unsupported-syscalls
 * returns ENOSYS, which is what Linux returns for every PI command when built
 * without CONFIG_FUTEX_PI (https://github.com/rrnewton/hermit/issues/3958).
 *
 * With the argument `nested-pid`, a child in a new PID namespace issues
 * FUTEX_LOCK_PI on a free word instead. Natively it takes the word (0). Its
 * TID is the nested namespace's, which Detcore does not translate into the
 * container's thread IDs it keys PI ownership by, so Hermit refuses that call
 * by name too.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static uint32_t condition;
static uint32_t mutex;

static void print_result(const char* what, long ret) {
  if (ret < 0) {
    printf("%s: -1 %s\n", what, strerrorname_np(errno));
  } else {
    printf("%s: %ld\n", what, ret);
  }
}

/* FUTEX_LOCK_PI from the init of a new PID namespace. */
static int lock_pi_in_a_nested_pid_namespace(void) {
  fflush(stdout);
  long child = syscall(SYS_clone, (long)(CLONE_NEWPID | SIGCHLD), 0L, 0L, 0L, 0L);
  if (child < 0) {
    printf("clone(CLONE_NEWPID): -1 %s\n", strerrorname_np(errno));
    return 1;
  }
  if (child == 0) {
    static uint32_t word;
    print_result(
        "lock_pi in a nested PID namespace",
        syscall(SYS_futex, &word, FUTEX_LOCK_PI_PRIVATE, 0, NULL, NULL, 0));
    fflush(stdout);
    _exit(0);
  }
  int status;
  if (waitpid((pid_t)child, &status, 0) != (pid_t)child) {
    perror("waitpid");
    return 1;
  }
  return 0;
}

int main(int argc, char** argv) {
  if (argc > 1 && strcmp(argv[1], "nested-pid") == 0) {
    return lock_pi_in_a_nested_pid_namespace();
  }
  long ret = syscall(
      SYS_futex, &condition, FUTEX_CMP_REQUEUE_PI_PRIVATE, 1, 0, &mutex, 0);
  print_result("cmp_requeue_pi", ret);
  return 0;
}
