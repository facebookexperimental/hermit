/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

int main(int argc, char** argv) {
  if (argc != 2)
    return 2;
  if (strcmp(argv[1], "ordinary") == 0) {
    errno = 0;
    long policy = syscall(SYS_sched_getscheduler, 0);
    int error = errno;
    printf("ordinary sched_getscheduler: policy=%ld errno=%d\n", policy, error);
    return policy < 0 || error != 0;
  }
  if (strcmp(argv[1], "vfork") != 0)
    return 2;

  int observation_pipe[2];
  if (pipe(observation_pipe) != 0)
    return 3;
  pid_t child = vfork();
  if (child < 0)
    return 4;
  if (child == 0) {
    errno = 0;
    long observation[2];
    observation[0] = syscall(SYS_sched_getscheduler, 0);
    observation[1] = errno;
    static const char after[] = "after sched_getscheduler\n";
    if (syscall(SYS_write, STDOUT_FILENO, after, sizeof(after) - 1) != (long)sizeof(after) - 1 ||
        syscall(SYS_write, observation_pipe[1], observation, sizeof(observation)) !=
            (long)sizeof(observation))
      syscall(SYS_exit, 5);
    syscall(SYS_exit, 0);
    __builtin_unreachable();
  }

  close(observation_pipe[1]);
  int status = 0;
  if (waitpid(child, &status, 0) != child)
    return 6;
  long observation[2];
  ssize_t count = read(observation_pipe[0], observation, sizeof(observation));
  close(observation_pipe[0]);
  if (!WIFEXITED(status) || WEXITSTATUS(status) != 0 || count != (ssize_t)sizeof(observation))
    return 7;
  printf("vfork sched_getscheduler: policy=%ld errno=%ld exit=0\n",
         observation[0], observation[1]);
  return observation[0] < 0 || observation[1] != 0;
}
