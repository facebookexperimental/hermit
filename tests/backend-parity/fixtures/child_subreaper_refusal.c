/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <stdio.h>
#include <sys/prctl.h>

/*
 * PR_SET_CHILD_SUBREAPER / PR_GET_CHILD_SUBREAPER reconfigure how orphaned
 * descendants are re-parented: a subreaper adopts the orphaned grandchildren of
 * the processes below it instead of letting them reparent to init. That rewires
 * the process-reaping hierarchy, which Hermit's deterministic container owns and
 * models directly, so Hermit refuses to let a guest mutate or query the
 * subreaper attribute: both prctl requests fail with a deterministic ENOSYS on
 * every backend, exactly as io_uring, kernel AIO, and System V IPC are refused.
 * Outside Hermit the same calls succeed.
 *
 * This contract asserts that refusal only -- PR_SET_CHILD_SUBREAPER and
 * PR_GET_CHILD_SUBREAPER must each return -1 with errno == ENOSYS. It never
 * asserts a subreaper value (the native PR_GET result is not part of the
 * contract) and prints only a check count.
 */

int main(void) {
  int ok = 0;

  errno = 0;
  int set_rc = prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
  if (set_rc == -1 && errno == ENOSYS) {
    ok++;
  } else {
    fprintf(
        stderr,
        "PR_SET_CHILD_SUBREAPER rc=%d errno=%d (want -1/ENOSYS)\n",
        set_rc,
        errno);
    return 1;
  }

  int value = -1;
  errno = 0;
  int get_rc = prctl(PR_GET_CHILD_SUBREAPER, &value, 0, 0, 0);
  if (get_rc == -1 && errno == ENOSYS) {
    ok++;
  } else {
    fprintf(
        stderr,
        "PR_GET_CHILD_SUBREAPER rc=%d errno=%d (want -1/ENOSYS)\n",
        get_rc,
        errno);
    return 1;
  }

  printf("subreaper ok=%d\n", ok);
  return 0;
}
