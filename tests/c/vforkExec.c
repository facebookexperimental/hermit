/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

int main(int argc, char* argv[], char* envp[]) {
  if (argc == 2 && strcmp(argv[1], "child") == 0) {
    printf("exec pid: %u\n", getpid());
    _exit(0);
  }

  /* A CLONE_VFORK call the kernel refuses (CLONE_THREAD requires
   * CLONE_SIGHAND, kernel/fork.c copy_process): no child is created, and the
   * parent goes on with EINVAL rather than waiting for a child. */
  errno = 0;
  long refused =
      syscall(SYS_clone, CLONE_VFORK | CLONE_THREAD | SIGCHLD, 0, 0, 0, 0);
  int refused_errno = errno;
  if (refused == 0) {
    _exit(2);
  }
  if (refused != -1 || refused_errno != EINVAL) {
    printf(
        "refused vfork: expected -1 EINVAL, got %ld %s\n",
        refused,
        strerror(refused_errno));
    return 3;
  }
  printf("refused vfork: -1 %s\n", strerror(refused_errno));

  pid_t pid = vfork();

  if (pid < 0) {
    perror("vfork failed: ");
    exit(1);
  } else if (pid == 0) {
    char* prog = argv[0];
    char* const newArgv[] = {prog, "child", NULL};
    printf("child pid: %u\n", getpid());
    execve(prog, newArgv, envp);
    printf("exec failed: %s\n", strerror(errno));
  } else {
    int status;
    printf("parent pid: %u\n", getpid());
    waitpid(pid, &status, 0);
    if (WIFSIGNALED(status)) {
      printf("%u terminated by signal: %u\n", pid, WTERMSIG(status));
    }
  }
}
