/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Backend-parity contract: resource-limit round-trips via
 * getrlimit/setrlimit/prlimit64 are deterministic and independent of host
 * process state.
 *
 * The contract exercises RLIMIT_NOFILE, the open-file-descriptor limit that
 * Detcore virtualizes to a fixed per-process value. Every checked value is
 * either one the guest itself just installed or is derived from the hard limit
 * the guest just observed -- never a raw host-inherited default. The guest:
 *
 *   - observes its own RLIMIT_NOFILE hard limit and lowers the soft limit to a
 *     value clamped to that observed hard limit (lowering the soft limit is
 *     always permitted for an unprivileged process on Linux), then reads it
 *     straight back;
 *   - confirms a query-only prlimit64(pid=0, self) agrees with the value it just
 *     installed;
 *   - performs a prlimit64 atomic set+get round-trip and confirms the reported
 *     old value equals what it had installed a moment earlier;
 *   - raises the soft limit back up to the hard limit (raising the soft limit up
 *     to the hard limit is always permitted) and reads it back;
 *   - confirms the two faithful Linux refusals that do not depend on host state:
 *     a request with soft > hard fails with EINVAL, and an unprivileged attempt
 *     to raise the hard limit fails with EPERM.
 *
 * No raw host-dependent number is ever printed, so the observable result depends
 * only on the program, not on the host process's inherited limits. That makes
 * the contract byte-identical across repeated runs and across the ptrace, DBI,
 * and KVM backends. It uses no threads, no blocking I/O, and no signal delivery,
 * so it is safe under the DBI no-preemption scheduler.
 */

#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/time.h>
#include <unistd.h>

static int fail(const char *what) {
  fprintf(stderr, "rlimit-identity: %s failed: %s\n", what, strerror(errno));
  return 1;
}

int main(void) {
  /* Observe our own RLIMIT_NOFILE limits. Under Detcore this is a fixed virtual
   * value; the checks below only compare against values we derive from it or set
   * ourselves, so nothing host-specific escapes to stdout. */
  struct rlimit nofile;
  memset(&nofile, 0xff, sizeof(nofile));
  if (getrlimit(RLIMIT_NOFILE, &nofile) != 0)
    return fail("getrlimit RLIMIT_NOFILE");
  const rlim_t hard = nofile.rlim_max;

  /* Lower the soft limit to a fixed value clamped to the observed hard limit.
   * Lowering the soft limit is always permitted for an unprivileged process, and
   * clamping to the observed hard limit keeps the target host-independent. The
   * hard limit is left unchanged. */
  const rlim_t soft1 = (hard >= 64) ? (rlim_t)64 : hard;
  struct rlimit set1 = {.rlim_cur = soft1, .rlim_max = hard};
  if (setrlimit(RLIMIT_NOFILE, &set1) != 0)
    return fail("setrlimit RLIMIT_NOFILE soft1");
  struct rlimit read1;
  memset(&read1, 0xff, sizeof(read1));
  if (getrlimit(RLIMIT_NOFILE, &read1) != 0)
    return fail("getrlimit RLIMIT_NOFILE read1");
  if (read1.rlim_cur != soft1 || read1.rlim_max != hard) {
    fprintf(stderr, "rlimit-identity: soft1 round-trip mismatch\n");
    return 1;
  }

  /* Query-only prlimit64(pid=0=self) must agree with what we just installed. */
  struct rlimit query;
  memset(&query, 0xff, sizeof(query));
  if (prlimit(0, RLIMIT_NOFILE, NULL, &query) != 0)
    return fail("prlimit RLIMIT_NOFILE query");
  if (query.rlim_cur != soft1 || query.rlim_max != hard) {
    fprintf(stderr, "rlimit-identity: prlimit query disagrees with set value\n");
    return 1;
  }

  /* Atomic prlimit64 set+get: install a lower soft limit and confirm the
   * reported old value is exactly what we had a moment earlier. */
  const rlim_t soft2 = (hard >= 32) ? (rlim_t)32 : hard;
  struct rlimit set2 = {.rlim_cur = soft2, .rlim_max = hard};
  struct rlimit old2;
  memset(&old2, 0xff, sizeof(old2));
  if (prlimit(0, RLIMIT_NOFILE, &set2, &old2) != 0)
    return fail("prlimit RLIMIT_NOFILE set+get");
  if (old2.rlim_cur != soft1 || old2.rlim_max != hard) {
    fprintf(stderr, "rlimit-identity: prlimit old value mismatch\n");
    return 1;
  }
  struct rlimit read2;
  memset(&read2, 0xff, sizeof(read2));
  if (getrlimit(RLIMIT_NOFILE, &read2) != 0)
    return fail("getrlimit RLIMIT_NOFILE read2");
  if (read2.rlim_cur != soft2 || read2.rlim_max != hard) {
    fprintf(stderr, "rlimit-identity: soft2 round-trip mismatch\n");
    return 1;
  }

  /* Raise the soft limit back up to the hard limit; raising the soft limit up to
   * (but not above) the hard limit is always permitted. */
  struct rlimit restore = {.rlim_cur = hard, .rlim_max = hard};
  if (setrlimit(RLIMIT_NOFILE, &restore) != 0)
    return fail("setrlimit RLIMIT_NOFILE restore");
  struct rlimit read3;
  memset(&read3, 0xff, sizeof(read3));
  if (getrlimit(RLIMIT_NOFILE, &read3) != 0)
    return fail("getrlimit RLIMIT_NOFILE read3");
  if (read3.rlim_cur != hard || read3.rlim_max != hard) {
    fprintf(stderr, "rlimit-identity: restore round-trip mismatch\n");
    return 1;
  }

  /* Faithful Linux refusal 1: a request with soft > hard fails with EINVAL. Only
   * meaningful when the hard limit leaves room for an invalid soft value. */
  if (hard != RLIM_INFINITY && hard > 0) {
    struct rlimit bad = {.rlim_cur = hard, .rlim_max = hard - 1};
    errno = 0;
    if (setrlimit(RLIMIT_NOFILE, &bad) == 0) {
      fprintf(stderr, "rlimit-identity: soft>hard was not rejected\n");
      return 1;
    }
    if (errno != EINVAL) {
      fprintf(stderr, "rlimit-identity: soft>hard errno not EINVAL: %s\n",
              strerror(errno));
      return 1;
    }
  }

  /* Faithful Linux refusal 2: an unprivileged process may not raise its hard
   * limit; requesting a larger hard limit fails with EPERM. Only meaningful when
   * the current hard limit is not already infinite. */
  if (hard != RLIM_INFINITY) {
    struct rlimit raise_hard = {.rlim_cur = hard, .rlim_max = hard + 1};
    errno = 0;
    if (setrlimit(RLIMIT_NOFILE, &raise_hard) == 0) {
      fprintf(stderr, "rlimit-identity: raising hard limit was permitted\n");
      return 1;
    }
    if (errno != EPERM) {
      fprintf(stderr, "rlimit-identity: raise-hard errno not EPERM: %s\n",
              strerror(errno));
      return 1;
    }
  }

  puts("rlimit-identity-ok");
  return 0;
}
