/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * The capability-set prctls answer as Linux does, on every backend.
 *
 * PR_CAPBSET_DROP and PR_CAP_AMBIENT reach the kernel on every backend (the
 * host's, or KVM's provider). Until Hermit 2026-10-16 they reached it on KVM
 * only and failed with ENOSYS everywhere else. This guest checks Linux's
 * answers that do not depend on the caller's credentials, and prints one
 * line per call, so the cell's stdout is the same natively and under every
 * backend:
 *
 * - PR_CAP_AMBIENT: CLEAR_ALL succeeds; afterwards IS_SET reports 0; LOWER
 *   of a capability not in the set succeeds; an unknown capability or
 *   operation fails with EINVAL.
 * - PR_CAPBSET_DROP needs CAP_SETPCAP, which a caller may lack. An unknown
 *   capability fails (EINVAL, or EPERM without CAP_SETPCAP). Dropping a known
 *   one succeeds or fails with EPERM, and after a success PR_CAPBSET_READ
 *   reports the capability gone. The guest prints only "rejected", and
 *   "dropped" or "not permitted".
 */

#include <errno.h>
#include <linux/capability.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>

static int failures;

static void expect(const char *what, long rc, long want_rc, int want_errno) {
  int err = errno;
  if (rc != want_rc || (rc < 0 && err != want_errno)) {
    fprintf(
        stderr,
        "%s returned %ld (errno %d: %s), expected %ld (errno %d)\n",
        what,
        rc,
        rc < 0 ? err : 0,
        rc < 0 ? strerror(err) : "none",
        want_rc,
        want_errno);
    failures += 1;
  }
  printf("%s: %ld%s%s\n", what, rc, rc < 0 ? " " : "", rc < 0 ? strerror(err) : "");
}

static long call(int option, unsigned long arg2, unsigned long arg3) {
  errno = 0;
  return prctl(option, arg2, arg3, 0, 0);
}

int main(void) {
  expect(
      "ambient clear all",
      call(PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL, 0),
      0,
      0);
  expect(
      "ambient is set CAP_NET_RAW",
      call(PR_CAP_AMBIENT, PR_CAP_AMBIENT_IS_SET, CAP_NET_RAW),
      0,
      0);
  expect(
      "ambient lower CAP_NET_RAW",
      call(PR_CAP_AMBIENT, PR_CAP_AMBIENT_LOWER, CAP_NET_RAW),
      0,
      0);
  expect(
      "ambient is set unknown capability",
      call(PR_CAP_AMBIENT, PR_CAP_AMBIENT_IS_SET, 99),
      -1,
      EINVAL);
  expect(
      "ambient unknown operation",
      call(PR_CAP_AMBIENT, 77, 0),
      -1,
      EINVAL);
  /*
   * Linux checks CAP_SETPCAP before the capability number, so an unknown
   * capability fails with EPERM for a caller without it and EINVAL otherwise.
   */
  long unknown = call(PR_CAPBSET_DROP, 99, 0);
  int unknown_errno = errno;
  if (unknown != -1 || (unknown_errno != EINVAL && unknown_errno != EPERM)) {
    fprintf(
        stderr,
        "PR_CAPBSET_DROP 99 returned %ld (errno %d: %s), expected EINVAL or "
        "EPERM\n",
        unknown,
        unknown_errno,
        strerror(unknown_errno));
    failures += 1;
  }
  printf("bounding set drop unknown capability: rejected\n");

  long dropped = call(PR_CAPBSET_DROP, CAP_SYS_BOOT, 0);
  int drop_errno = errno;
  if (dropped == 0) {
    long still = call(PR_CAPBSET_READ, CAP_SYS_BOOT, 0);
    if (still != 0) {
      fprintf(
          stderr,
          "PR_CAPBSET_READ CAP_SYS_BOOT returned %ld after a drop\n",
          still);
      failures += 1;
    }
    printf("bounding set drop CAP_SYS_BOOT: dropped\n");
  } else if (dropped == -1 && drop_errno == EPERM) {
    printf("bounding set drop CAP_SYS_BOOT: not permitted\n");
  } else {
    fprintf(
        stderr,
        "PR_CAPBSET_DROP CAP_SYS_BOOT returned %ld (errno %d: %s), expected 0 "
        "or EPERM\n",
        dropped,
        drop_errno,
        strerror(drop_errno));
    failures += 1;
  }
  return failures == 0 ? 0 : 1;
}
