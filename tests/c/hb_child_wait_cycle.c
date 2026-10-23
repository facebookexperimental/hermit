/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * The parent forks a child and waits for it, then writes "parent\n". The child
 * writes "child\n" and exits. hermit-cli/tests/cli.rs holds the child's write
 * until the parent's write: a cycle through the child wait, since the parent
 * cannot write before the child exits and the child cannot exit before the
 * parent writes (https://github.com/rrnewton/hermit/issues/3904).
 *
 * In "joined" mode the child first creates and joins a helper thread, so the
 * child process has had a thread that is gone by the time it is held. In
 * "nested" mode the root first forks the parent, waits for it and writes
 * "root\n": an ordinary wait above the cycle (the Codex design re-check of
 * https://github.com/rrnewton/hermit/issues/3929).
 */
#include <pthread.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static void *helper(void *arg) {
  (void)arg;
  return 0;
}

int main(int argc, char **argv) {
  int joined = argc > 1 && strcmp(argv[1], "joined") == 0;
  if (argc > 1 && strcmp(argv[1], "nested") == 0) {
    pid_t parent = fork();
    if (parent < 0) {
      return 3;
    }
    if (parent > 0) {
      int status;
      if (waitpid(parent, &status, 0) != parent) {
        return 4;
      }
      if (write(1, "root\n", 5) != 5) {
        return 2;
      }
      return 0;
    }
  }
  pid_t pid = fork();
  if (pid < 0) {
    return 3;
  }
  if (pid == 0) {
    if (joined) {
      pthread_t thread;
      if (pthread_create(&thread, 0, helper, 0) != 0) {
        _exit(5);
      }
      pthread_join(thread, 0);
    }
    if (write(1, "child\n", 6) != 6) {
      _exit(2);
    }
    _exit(0);
  }
  int status;
  if (waitpid(pid, &status, 0) != pid) {
    return 4;
  }
  if (write(1, "parent\n", 7) != 7) {
    return 2;
  }
  return 0;
}
