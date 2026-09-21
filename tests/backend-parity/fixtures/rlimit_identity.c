/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Backend-parity identity fixture: RLIMIT_NOFILE soft-limit round-trip.
 *
 * The fixture lowers its own RLIMIT_NOFILE soft limit to a fixed, host-
 * independent value and reads it back. The soft limit a process sets on itself
 * is a deterministic guest property -- independent of the host's real ulimit --
 * so the ptrace, DBT, and KVM backends must all observe the same value. The
 * observed value is threaded through the shared mutation seam so
 * parity_mutation.py can prove the round-trip is load-bearing.
 *
 * This fixture carries NO bespoke pass/fail logic: it uses only the shared
 * contract in parity_probe.h. Adding a family member means supplying its syscall
 * and its mutable field, nothing more.
 */

#include <sys/resource.h>

#include "parity_probe.h"

/* Fixed soft limit below any realistic host hard limit; host-independent. */
#define PARITY_NOFILE_SOFT 64u

int main(void) {
  struct rlimit initial;
  parity_check(getrlimit(RLIMIT_NOFILE, &initial) == 0, "getrlimit(initial)");

  /* Lower only the soft limit; keep the hard limit so the call always succeeds
   * for an unprivileged guest. */
  struct rlimit want = {
      .rlim_cur = PARITY_NOFILE_SOFT,
      .rlim_max = initial.rlim_max,
  };
  parity_check(setrlimit(RLIMIT_NOFILE, &want) == 0, "setrlimit(nofile soft)");

  struct rlimit got;
  parity_check(getrlimit(RLIMIT_NOFILE, &got) == 0, "getrlimit(readback)");

  /* Observe the round-tripped value through the mutation seam, then both assert
   * on it and emit it: that is what makes "nofile" a load-bearing field. */
  uint64_t soft = parity_mutate_u64("nofile", (uint64_t)got.rlim_cur);
  parity_check(soft == PARITY_NOFILE_SOFT, "nofile soft round-trip");

  parity_emit("rlimit-identity nofile_soft=%llu\n", (unsigned long long)soft);
  return parity_finish();
}
