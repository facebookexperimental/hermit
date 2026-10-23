/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <limits.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

_Static_assert(sizeof(struct sched_param) == 4, "x86_64 sched_param oracle");

struct parameter_observation {
  uint32_t before;
  struct sched_param parameter;
  uint32_t after;
};

static struct parameter_observation parameter_sentinel(void) {
  struct parameter_observation value = {
      .before = UINT32_C(0x2468ace0),
      .parameter = {.sched_priority = 0x13579bdf},
      .after = UINT32_C(0xfdb97531),
  };
  return value;
}

static int parameter_canaries(const struct parameter_observation* value) {
  return value->before == UINT32_C(0x2468ace0) &&
      value->after == UINT32_C(0xfdb97531);
}

/* Characterize the existing shared Tool: PID existence is ignored and null
 * succeeds without writing. These are not native Linux error semantics. */
static int parameter_targets(void) {
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
  printf("parameter identities: child=%d parent=%d\n", child_matches, parent_matches);

  const pid_t targets[] = {0, parent, child, INT_MAX, -1, 0, 0};
  const char* labels[] = {"zero", "self", "live", "absent", "negative", "null", "bad"};
  for (unsigned int i = 0; i < sizeof(targets) / sizeof(targets[0]); ++i) {
    struct parameter_observation output = parameter_sentinel();
    struct sched_param* address =
        i == 5 ? NULL : i == 6 ? (struct sched_param*)(uintptr_t)1 : &output.parameter;
    errno = 0;
    long value = syscall(SYS_sched_getparam, targets[i], address);
    int error = errno;
    int canaries = parameter_canaries(&output);
    printf("sched_getparam %s: result=%ld errno=%d priority=%d canaries=%d\n",
           labels[i], value, error, output.parameter.sched_priority, canaries);
    failed |= !canaries;
    if (i < 5)
      failed |= value != 0 || error != 0 || output.parameter.sched_priority != 0;
    else
      failed |= value != (i == 5 ? 0 : -1) || error != (i == 5 ? 0 : EFAULT) ||
          output.parameter.sched_priority != 0x13579bdf;
  }

  /* The child remains held until every query has completed. */
  if (write(release[1], "x", 1) != 1)
    failed = 1;
  close(release[1]);
  int status = 0;
  pid_t waited = waitpid(child, &status, 0);
  int child_exit = waited == child && WIFEXITED(status) ? WEXITSTATUS(status) : -1;
  printf("parameter wait: exit=%d\n", child_exit);
  failed |= child_exit != 37;
  if (failed)
    return 13;
  puts("sched-getparam-targets=ok");
  return 0;
}

static int parameter_vfork(void) {
  int observation_pipe[2];
  if (pipe(observation_pipe) != 0)
    return 20;
  pid_t child = vfork();
  if (child < 0)
    return 21;
  if (child == 0) {
    struct parameter_observation output = parameter_sentinel();
    errno = 0;
    long observation[4];
    observation[0] = syscall(SYS_sched_getparam, 0, &output.parameter);
    observation[1] = errno;
    observation[2] = output.parameter.sched_priority;
    observation[3] = parameter_canaries(&output);
    static const char after[] = "after sched_getparam\n";
    if (syscall(SYS_write, STDOUT_FILENO, after, sizeof(after) - 1) != (long)sizeof(after) - 1 ||
        syscall(SYS_write, observation_pipe[1], observation, sizeof(observation)) !=
            (long)sizeof(observation))
      syscall(SYS_exit, 22);
    syscall(SYS_exit, 0);
    __builtin_unreachable();
  }

  close(observation_pipe[1]);
  int status = 0;
  if (waitpid(child, &status, 0) != child)
    return 23;
  long observation[4];
  ssize_t count = read(observation_pipe[0], observation, sizeof(observation));
  close(observation_pipe[0]);
  if (!WIFEXITED(status) || WEXITSTATUS(status) != 0 || count != (ssize_t)sizeof(observation))
    return 24;
  printf("vfork sched_getparam: result=%ld errno=%ld priority=%ld canaries=%ld exit=0\n",
         observation[0], observation[1], observation[2], observation[3]);
  return observation[0] != 0 || observation[1] != 0 ||
      observation[2] != 0 || observation[3] != 1;
}


#define AFFINITY_BYTES 128
#define AFFINITY_SENTINEL 0x7f

struct affinity_observation {
  uint64_t before;
  unsigned char mask[AFFINITY_BYTES];
  uint64_t after;
};

static struct affinity_observation affinity_sentinel(void) {
  struct affinity_observation value;
  value.before = UINT64_C(0x2468ace02468ace0);
  memset(value.mask, AFFINITY_SENTINEL, sizeof(value.mask));
  value.after = UINT64_C(0xfdb97531fdb97531);
  return value;
}

static int affinity_canaries(const struct affinity_observation* value) {
  return value->before == UINT64_C(0x2468ace02468ace0) &&
      value->after == UINT64_C(0xfdb97531fdb97531);
}

/* The raw native byte count and mask are observations, not the shared CPU0
 * oracle. A host needing more than this bounded buffer fails the control. */
static int affinity_success(long result, long error,
                            const struct affinity_observation* value) {
  if (result <= 0 || result > AFFINITY_BYTES || error != 0 || !affinity_canaries(value))
    return 0;
  int nonempty = 0;
  for (long i = 0; i < result; ++i)
    nonempty |= value->mask[i];
  for (long i = result; i < AFFINITY_BYTES; ++i)
    if (value->mask[i] != AFFINITY_SENTINEL)
      return 0;
  return nonempty != 0;
}

static void print_affinity(const char* mode, long result, long error,
                           const struct affinity_observation* value) {
  printf("%s sched_getaffinity: result=%ld errno=%ld mask=", mode, result, error);
  for (unsigned int i = 0; i < sizeof(value->mask); ++i)
    printf("%02x", value->mask[i]);
  printf(" canaries=%d\n", affinity_canaries(value));
}

static int affinity_ordinary(void) {
  struct affinity_observation output = affinity_sentinel();
  errno = 0;
  long result = syscall(SYS_sched_getaffinity, 0, sizeof(output.mask), output.mask);
  int error = errno;
  print_affinity("ordinary", result, error, &output);
  return !affinity_success(result, error, &output);
}

static int affinity_vfork(void) {
  int observation_pipe[2];
  if (pipe(observation_pipe) != 0)
    return 30;
  pid_t child = vfork();
  if (child < 0)
    return 31;
  if (child == 0) {
    struct {
      long result;
      long error;
      struct affinity_observation output;
    } observation;
    observation.output = affinity_sentinel();
    errno = 0;
    observation.result = syscall(
        SYS_sched_getaffinity, 0, sizeof(observation.output.mask), observation.output.mask);
    observation.error = errno;
    static const char after[] = "after sched_getaffinity\n";
    if (syscall(SYS_write, STDOUT_FILENO, after, sizeof(after) - 1) != (long)sizeof(after) - 1 ||
        syscall(SYS_write, observation_pipe[1], &observation, sizeof(observation)) !=
            (long)sizeof(observation))
      syscall(SYS_exit, 32);
    syscall(SYS_exit, 0);
    __builtin_unreachable();
  }

  close(observation_pipe[1]);
  int status = 0;
  if (waitpid(child, &status, 0) != child)
    return 33;
  struct {
    long result;
    long error;
    struct affinity_observation output;
  } observation;
  ssize_t count = read(observation_pipe[0], &observation, sizeof(observation));
  close(observation_pipe[0]);
  if (!WIFEXITED(status) || WEXITSTATUS(status) != 0 || count != (ssize_t)sizeof(observation))
    return 34;
  print_affinity("vfork", observation.result, observation.error, &observation.output);
  return !affinity_success(observation.result, observation.error, &observation.output);
}

int main(int argc, char** argv) {
  if (argc != 2)
    return 2;
  if (strcmp(argv[1], "affinity-ordinary") == 0)
    return affinity_ordinary();
  if (strcmp(argv[1], "affinity-vfork") == 0)
    return affinity_vfork();
  if (strcmp(argv[1], "param-targets") == 0)
    return parameter_targets();
  if (strcmp(argv[1], "param-vfork") == 0)
    return parameter_vfork();
  if (strcmp(argv[1], "param-ordinary") == 0) {
    struct parameter_observation output = parameter_sentinel();
    errno = 0;
    long value = syscall(SYS_sched_getparam, 0, &output.parameter);
    int error = errno;
    int canaries = parameter_canaries(&output);
    printf("ordinary sched_getparam: result=%ld errno=%d priority=%d canaries=%d\n",
           value, error, output.parameter.sched_priority, canaries);
    return value != 0 || error != 0 || output.parameter.sched_priority != 0 || !canaries;
  }
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
