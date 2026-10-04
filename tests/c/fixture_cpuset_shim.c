/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Link-time cpuset for the identity-fixture family's CPU-placement leg.
 *
 * A validation host may run a fixture inside a cgroup cpuset that leaves out
 * some CPUs. The dev-hermit validate launcher does this on purpose: it keeps
 * cells off the CPUs the host PMU drivers are bound to, CPU 0 among them
 * (https://github.com/rrnewton/hermit/issues/3265). Inside such a cpuset the
 * kernel intersects every sched_setaffinity(2) request with the cpuset and
 * fails the call with EINVAL when nothing is left. A fixture that asks for a
 * fixed CPU number therefore passes on one host and fails on another.
 *
 * Restricting only the inherited affinity mask does not reproduce that: the
 * kernel lets a process widen its own mask again to any CPU in its cpuset, so a
 * fixture that asks for CPU 0 still gets it. And an unprivileged harness cannot
 * create a cpuset. fixture_mutation.py instead links a fixture with
 * `-Wl,--wrap=sched_setaffinity` and this file, starts it with its affinity
 * restricted to one CPU, and this wrapper applies the kernel's cpuset rule:
 *
 *   the effective mask is the request intersected with the cpuset the process
 *   started in (its inherited affinity), and an empty intersection is EINVAL.
 *
 * A fixture that asks only for CPUs it may already use is unaffected. The
 * wrapper covers calls through the glibc symbol, which is how every family
 * member calls it; a raw syscall(SYS_sched_setaffinity, ...) would bypass it.
 *
 * Built with -DFIXTURE_CPUSET_SHIM_CONTROL, this file is also the leg's
 * control: a program that asks for each CPU named on its command line. The
 * harness names a CPU from its own affinity mask but outside the one CPU it
 * starts the control on; the kernel would accept that request, so an EINVAL can
 * only come from the wrapper. Without that negative, a wrapper that stopped
 * refusing would turn the placement leg into a second unrestricted run. The
 * harness also names the allowed CPU, which must succeed, so a wrapper that
 * refuses everything is caught by the control rather than by every fixture.
 */

#include <errno.h>
#include <sched.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <unistd.h>

int __real_sched_setaffinity(pid_t pid, size_t size, const cpu_set_t* mask);
int __wrap_sched_setaffinity(pid_t pid, size_t size, const cpu_set_t* mask);

/* The simulated cpuset: this process's affinity mask at startup. */
static cpu_set_t shim_cpuset;
/* Whether the startup read succeeded. A failed read refuses every request. */
static int shim_cpuset_known;

__attribute__((constructor)) static void shim_capture_cpuset(void) {
  CPU_ZERO(&shim_cpuset);
  /* The raw syscall returns the number of bytes the kernel wrote. */
  shim_cpuset_known =
      syscall(SYS_sched_getaffinity, 0, sizeof(shim_cpuset), &shim_cpuset) > 0;
}

int __wrap_sched_setaffinity(pid_t pid, size_t size, const cpu_set_t* mask) {
  if (mask == NULL) {
    errno = EFAULT;
    return -1;
  }
  cpu_set_t effective;
  CPU_ZERO(&effective);
  memcpy(&effective, mask, size < sizeof(effective) ? size : sizeof(effective));
  CPU_AND(&effective, &effective, &shim_cpuset);
  if (!shim_cpuset_known || CPU_COUNT(&effective) == 0) {
    errno = EINVAL;
    return -1;
  }
  return __real_sched_setaffinity(pid, sizeof(effective), &effective);
}

#ifdef FIXTURE_CPUSET_SHIM_CONTROL
#include <stdio.h>
#include <stdlib.h>

/* Ask for each CPU named on the command line, one at a time, and report the
 * result of every request. */
int main(int argc, char** argv) {
  if (argc < 2) {
    fprintf(stderr, "usage: %s CPU...\n", argv[0]);
    return 2;
  }
  for (int i = 1; i < argc; i++) {
    int cpu = atoi(argv[i]);
    cpu_set_t want;
    CPU_ZERO(&want);
    CPU_SET(cpu, &want);
    errno = 0;
    int rc = sched_setaffinity(0, sizeof(want), &want);
    int saved = errno;
    printf(
        "cpuset-shim-control cpu%d rc=%d errno=%d\n",
        cpu,
        rc,
        rc == 0 ? 0 : saved);
  }
  return 0;
}
#endif
