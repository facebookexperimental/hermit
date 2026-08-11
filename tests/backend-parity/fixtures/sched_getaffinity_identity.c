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
 * The fixture pins the calling thread to CPU 0 with sched_setaffinity and reads
 * the mask back with sched_getaffinity. An affinity mask a process sets on
 * itself is a deterministic guest property -- independent of the host's online
 * CPU set -- so every backend must observe the same {CPU 0} mask. The observed
 * population count is threaded through the shared mutation seam.
 *
 * This is the "second fixture" for the shared harness: it reuses parity_probe.h
 * and registers in parity_mutation.py with its field name. It contains zero
 * bespoke both-direction verification -- the harness supplies all of it.
 */

#include <sched.h>

#include "parity_probe.h"

int main(void) {
  cpu_set_t want;
  CPU_ZERO(&want);
  CPU_SET(0, &want);
  parity_check(sched_setaffinity(0, sizeof(want), &want) == 0,
               "sched_setaffinity(cpu0)");

  cpu_set_t got;
  CPU_ZERO(&got);
  parity_check(sched_getaffinity(0, sizeof(got), &got) == 0,
               "sched_getaffinity(readback)");

  /* Observe the population count through the mutation seam, then assert on and
   * emit it: "affinity_count" is the load-bearing field. */
  uint64_t count = parity_mutate_u64("affinity_count", (uint64_t)CPU_COUNT(&got));
  parity_check(count == 1, "affinity is exactly {cpu0}");
  parity_check(CPU_ISSET(0, &got), "cpu0 present in mask");

  parity_emit("sched-getaffinity-identity cpu0=%d count=%llu\n",
              CPU_ISSET(0, &got) ? 1 : 0, (unsigned long long)count);
  return parity_finish();
}
