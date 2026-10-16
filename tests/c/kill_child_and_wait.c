/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A parent kills its own pausing child with SIGKILL and waits for it.
 * Natively, and under Hermit's ptrace backend, the kill succeeds and the wait
 * reports the child signaled by SIGKILL. The KVM backend cannot deliver a
 * signal to another process; it must refuse the kill by name rather than
 * return a bare ENOSYS that leaves the parent waiting forever
 * (https://github.com/rrnewton/hermit/issues/4485).
 */
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  int ready[2];
  if (pipe(ready) != 0) {
    return 2;
  }
  pid_t child = fork();
  if (child < 0) {
    return 2;
  }
  if (child == 0) {
    close(ready[0]);
    if (write(ready[1], "r", 1) != 1) {
      _exit(2);
    }
    pause();
    _exit(3);
  }
  close(ready[1]);
  char c;
  if (read(ready[0], &c, 1) != 1) {
    return 2;
  }
  int killed = kill(child, SIGKILL);
  printf("kill=%d errno=%s\n", killed, killed ? strerrorname_np(errno) : "-");
  int status = 0;
  pid_t waited = waitpid(child, &status, 0);
  printf(
      "waited=%s signaled=%d sig=%d\n",
      waited == child ? "child" : "other",
      WIFSIGNALED(status),
      WIFSIGNALED(status) ? WTERMSIG(status) : 0);
  return 0;
}
