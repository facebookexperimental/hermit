/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <limits.h>
#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

/* Preserve the shared Tool's existing raw-20 model, including its absent-target
 * limitation. This mode is not a native Linux target-existence oracle. */
static int ordinary_targets(void) {
  int ready[2], release[2];
  if (pipe(ready) != 0 || pipe(release) != 0)
    return 10;
  pid_t parent = getpid();
  pid_t child = fork();
  if (child < 0)
    return 11;
  if (child == 0) {
    close(ready[0]);
    close(release[1]);
    pid_t identity[2] = {getpid(), getppid()};
    char token = 0;
    if (write(ready[1], identity, sizeof(identity)) != (ssize_t)sizeof(identity) ||
        read(release[0], &token, 1) != 1 || token != 'x')
      _exit(12);
    _exit(37);
  }
  close(ready[1]);
  close(release[0]);
  pid_t identity[2] = {0, 0};
  ssize_t count = read(ready[0], identity, sizeof(identity));
  close(ready[0]);
  int child_matches = count == (ssize_t)sizeof(identity) && child > 0 &&
      child != parent && identity[0] == child;
  int parent_matches = identity[1] == parent;
  int failed = !child_matches || !parent_matches;
  printf("targets identities: child=%d parent=%d\n", child_matches, parent_matches);

  const int which[] = {PRIO_PROCESS, PRIO_PROCESS, PRIO_PROCESS, PRIO_PROCESS,
                       PRIO_PROCESS, -1, 3, 999};
  const long who[] = {0, parent, child, INT_MAX, -1, 0, child, INT_MAX};
  const char* labels[] = {"zero", "self", "live", "absent", "negative",
                          "invalid-negative", "invalid-live", "invalid-absent"};
  for (unsigned int i = 0; i < sizeof(which) / sizeof(which[0]); ++i) {
    errno = 0;
    long value = syscall(SYS_getpriority, which[i], who[i]);
    int error = errno;
    printf("getpriority %s: raw=%ld errno=%d\n", labels[i], value, error);
    failed |= i < 5 ? value != 20 || error != 0 : value != -1 || error != EINVAL;
  }

  /* The child cannot exit37 until this release, after every target query. */
  if (write(release[1], "x", 1) != 1)
    failed = 1;
  close(release[1]);
  int status = 0;
  pid_t waited = waitpid(child, &status, 0);
  int child_exit = waited == child && WIFEXITED(status) ? WEXITSTATUS(status) : -1;
  printf("targets wait: exit=%d\n", child_exit);
  failed |= child_exit != 37;
  if (failed)
    return 13;
  puts("getpriority-targets=ok");
  return 0;
}

int main(int argc, char** argv) {
  if (argc != 2)
    return 2;
  if (strcmp(argv[1], "ordinary-targets") == 0)
    return ordinary_targets();
  if (strcmp(argv[1], "ordinary") == 0) {
    errno = 0;
    long value = syscall(SYS_getpriority, PRIO_PROCESS, 0);
    int error = errno;
    printf("ordinary getpriority: raw=%ld errno=%d\n", value, error);
    return value < 0 || error != 0;
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
    observation[0] = syscall(SYS_getpriority, PRIO_PROCESS, 0);
    observation[1] = errno;
    static const char after[] = "after getpriority\n";
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
  printf("vfork getpriority: raw=%ld errno=%ld exit=0\n", observation[0], observation[1]);
  return observation[0] < 0 || observation[1] != 0;
}
