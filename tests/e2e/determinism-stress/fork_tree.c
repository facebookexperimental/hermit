/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <stdbool.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

enum {
  CHILD_COUNT = 4,
  GRANDCHILDREN_PER_CHILD = 2,
  SYSCALL_ROUNDS = 25,
  SYSCALLS_PER_PROCESS = SYSCALL_ROUNDS * 4,
  CHILD_EXIT_BASE = 20,
  GRANDCHILD_EXIT_BASE = 40,
};

static void fail(const char *operation) {
  fprintf(stderr, "%s: %s\n", operation, strerror(errno));
  exit(EXIT_FAILURE);
}

static int run_syscalls(bool allow_namespace_init_parent) {
  for (unsigned round = 0; round < SYSCALL_ROUNDS; ++round) {
    const long pid = syscall(SYS_getpid);
    const long parent = syscall(SYS_getppid);
    const long tid = syscall(SYS_gettid);
    const long uid = syscall(SYS_getuid);
    // Linux reports PPID 0 for PID 1 in a PID namespace. Child processes must
    // still identify a positive parent inside the namespace.
    if (pid <= 0 || parent < 0 || (!allow_namespace_init_parent && parent == 0) ||
        tid != pid || uid < 0) {
      return -1;
    }
  }
  return 0;
}

static int wait_for_exit(pid_t process, int expected_exit) {
  int status;
  pid_t waited;
  do {
    waited = waitpid(process, &status, 0);
  } while (waited < 0 && errno == EINTR);

  return waited == process && WIFEXITED(status) &&
         WEXITSTATUS(status) == expected_exit;
}

static void run_child(unsigned child_index) {
  pid_t grandchildren[GRANDCHILDREN_PER_CHILD];

  for (unsigned grandchild_index = 0;
       grandchild_index < GRANDCHILDREN_PER_CHILD; ++grandchild_index) {
    const pid_t grandchild = fork();
    if (grandchild < 0) {
      _exit(200 + child_index);
    }
    if (grandchild == 0) {
      const int exit_code =
          GRANDCHILD_EXIT_BASE + child_index * GRANDCHILDREN_PER_CHILD +
          grandchild_index;
      const int failure_code = 240 + (int)grandchild_index;
      _exit(run_syscalls(false) == 0 ? exit_code : failure_code);
    }
    grandchildren[grandchild_index] = grandchild;
  }

  if (run_syscalls(false) != 0) {
    _exit(210 + child_index);
  }
  for (unsigned grandchild_index = 0;
       grandchild_index < GRANDCHILDREN_PER_CHILD; ++grandchild_index) {
    const int expected_exit =
        GRANDCHILD_EXIT_BASE + child_index * GRANDCHILDREN_PER_CHILD +
        grandchild_index;
    if (!wait_for_exit(grandchildren[grandchild_index], expected_exit)) {
      _exit(220 + child_index);
    }
  }

  _exit(CHILD_EXIT_BASE + child_index);
}

int main(void) {
  pid_t children[CHILD_COUNT];

  for (unsigned child_index = 0; child_index < CHILD_COUNT; ++child_index) {
    const pid_t child = fork();
    if (child < 0) {
      fail("fork(child)");
    }
    if (child == 0) {
      run_child(child_index);
    }
    children[child_index] = child;
  }

  if (run_syscalls(true) != 0) {
    fputs("parent syscall invariant failed\n", stderr);
    return EXIT_FAILURE;
  }
  for (unsigned child_index = 0; child_index < CHILD_COUNT; ++child_index) {
    if (!wait_for_exit(children[child_index], CHILD_EXIT_BASE + child_index)) {
      fprintf(stderr, "child %u exit mismatch\n", child_index);
      return EXIT_FAILURE;
    }
  }

  printf("fork-tree processes=13 syscalls-per-process=%u child-exits=",
         SYSCALLS_PER_PROCESS);
  for (unsigned child_index = 0; child_index < CHILD_COUNT; ++child_index) {
    printf("%s%u", child_index == 0 ? "" : ",",
           CHILD_EXIT_BASE + child_index);
  }
  printf(" grandchild-exits=");
  for (unsigned index = 0; index < CHILD_COUNT * GRANDCHILDREN_PER_CHILD;
       ++index) {
    printf("%s%u", index == 0 ? "" : ",", GRANDCHILD_EXIT_BASE + index);
  }
  putchar('\n');
  return EXIT_SUCCESS;
}
