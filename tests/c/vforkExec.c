/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

int main(int argc, char* argv[], char* envp[]) {
  if (argc == 2 && strcmp(argv[1], "child") == 0) {
    printf("exec pid: %u\n", getpid());
    _exit(0);
  }

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
