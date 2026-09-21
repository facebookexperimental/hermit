/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
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

/*
 * THE OBSERVED errno IS EMITTED. This fixture asserts a specific refusal, and
 * "subreaper ok=2" carried neither the errno nor which entry point produced it.
 * The diagnostics went to stderr, which the cell observation EXCLUDES
 * (observation = {status, stdout}), so a backend refusing with the wrong errno
 * reached the oracle as an empty stdout and a bare non-zero status -- the same
 * observation as any other failure. Both entry points are now probed
 * unconditionally and each reports its rc and errno, so a wrong refusal is
 * legible in the byte stream and names itself. ENOSYS is a fixed Linux ABI
 * constant, so emitting it adds no host state.
 */
int main(void) {
  enum { EXPECTED_CHECKS = 2 };

  errno = 0;
  int set_rc = prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
  int set_errno = errno;
  int set_refused = set_rc == -1 && set_errno == ENOSYS;

  int value = -1;
  errno = 0;
  int get_rc = prctl(PR_GET_CHILD_SUBREAPER, &value, 0, 0, 0);
  int get_errno = errno;
  int get_refused = get_rc == -1 && get_errno == ENOSYS;

  int ok = set_refused + get_refused;
  printf(
      "subreaper ok=%d set_rc=%d set_errno=%d get_rc=%d get_errno=%d\n",
      ok,
      set_rc,
      set_errno,
      get_rc,
      get_errno);
  return ok == EXPECTED_CHECKS ? 0 : 1;
}
