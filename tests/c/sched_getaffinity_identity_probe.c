/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Backend-parity identity fixture: CPU-affinity round-trip.
 *
 * The fixture pins the calling thread to one CPU with sched_setaffinity and
 * reads the mask back with sched_getaffinity. An affinity mask a process sets
 * on itself is a deterministic guest property -- independent of the host's
 * online CPU set -- so every backend must observe the same one-CPU mask. The
 * observed population count is threaded through the shared mutation seam.
 *
 * The CPU it pins to is the lowest one in the mask it inherited, not a fixed
 * number. A host may run the fixture inside a cgroup cpuset that leaves CPU 0
 * out -- dev-hermit validation keeps cells off the CPUs the PMU drivers are
 * bound to (https://github.com/rrnewton/hermit/issues/3265) -- and there the
 * kernel refuses sched_setaffinity({0}) with EINVAL. Hermit reports the
 * virtual mask {CPU 0} to every guest, so under Hermit the fixture still pins
 * to CPU 0 and prints the same line as before. fixture_mutation.py runs the
 * clean contract natively with only one CPU allowed to hold this to it.
 *
 * This is the "second fixture" for the shared harness: it reuses parity_probe.h
 * and registers in fixture_mutation.py with its field name. It contains zero
 * bespoke both-direction verification -- the harness supplies all of it.
 */

#include <sched.h>

#include "parity_probe.h"

int main(void) {
  /* Pick the CPU to pin to from the CPUs this process may already use. */
  cpu_set_t inherited;
  CPU_ZERO(&inherited);
  parity_check(
      sched_getaffinity(0, sizeof(inherited), &inherited) == 0,
      "sched_getaffinity(inherited)");
  int target = -1;
  for (int cpu = 0; cpu < CPU_SETSIZE; cpu++) {
    if (CPU_ISSET(cpu, &inherited)) {
      target = cpu;
      break;
    }
  }
  parity_check(target >= 0, "inherited mask names at least one CPU");
  if (target < 0) {
    target = 0;
  }

  cpu_set_t want;
  CPU_ZERO(&want);
  CPU_SET(target, &want);
  parity_check(
      sched_setaffinity(0, sizeof(want), &want) == 0,
      "sched_setaffinity(lowest inherited cpu)");

  cpu_set_t got;
  CPU_ZERO(&got);
  parity_check(
      sched_getaffinity(0, sizeof(got), &got) == 0,
      "sched_getaffinity(readback)");

  /* Observe the population count through the mutation seam, then assert on and
   * emit it: "affinity_count" is the load-bearing field. */
  uint64_t count =
      parity_mutate_u64("affinity_count", (uint64_t)CPU_COUNT(&got));
  parity_check(count == 1, "affinity is exactly one cpu");
  parity_check(CPU_ISSET(target, &got), "pinned cpu present in mask");

  parity_emit(
      "sched-getaffinity-identity cpu%d=%d count=%llu\n",
      target,
      CPU_ISSET(target, &got) ? 1 : 0,
      (unsigned long long)count);
  return parity_finish();
}
