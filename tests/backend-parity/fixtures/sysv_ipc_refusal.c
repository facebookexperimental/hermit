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
#include <sys/ipc.h>
#include <sys/msg.h>
#include <sys/sem.h>
#include <sys/shm.h>

/*
 * System V IPC (semaphores, shared memory, message queues) is a global,
 * kernel-namespaced coordination facility: objects created with semget/shmget/
 * msgget outlive the creating process, are keyed into a host-wide namespace,
 * and expose cross-process shared state that Hermit's deterministic container
 * does not model. Hermit therefore refuses every SysV IPC "get" entry point
 * with a deterministic ENOSYS on all three backends, exactly as it refuses
 * io_uring and Linux kernel AIO. Outside Hermit the same calls succeed.
 *
 * This contract asserts that refusal uniformly: each of semget, shmget, and
 * msgget must fail with rc == -1 && errno == ENOSYS. It prints only a check
 * count so the golden output is backend-independent.
 */

/*
 * THE OBSERVED errno IS EMITTED PER ENTRY POINT. Three separate refusals summed
 * into "sysvipc ok=3", so a backend that admitted semget and one that admitted
 * msgget were indistinguishable, and the errno -- the actual content of the
 * contract -- went only to stderr, which the cell observation excludes.
 *
 * Note the fail-fast structure also meant a leaked semget masked whether shmget
 * and msgget were refused at all: the fixture returned before probing them. All
 * three are now probed unconditionally, so one leak no longer hides two.
 */
int main(void) {
  enum { EXPECTED_CHECKS = 3 };

  errno = 0;
  int sem = semget(IPC_PRIVATE, 1, IPC_CREAT | 0600);
  int sem_errno = errno;
  int sem_refused = sem == -1 && sem_errno == ENOSYS;

  errno = 0;
  int shm = shmget(IPC_PRIVATE, 4096, IPC_CREAT | 0600);
  int shm_errno = errno;
  int shm_refused = shm == -1 && shm_errno == ENOSYS;

  errno = 0;
  int msg = msgget(IPC_PRIVATE, IPC_CREAT | 0600);
  int msg_errno = errno;
  int msg_refused = msg == -1 && msg_errno == ENOSYS;

  int ok = sem_refused + shm_refused + msg_refused;
  printf(
      "sysvipc ok=%d sem_rc=%d sem_errno=%d shm_rc=%d shm_errno=%d "
      "msg_rc=%d msg_errno=%d\n",
      ok,
      sem,
      sem_errno,
      shm,
      shm_errno,
      msg,
      msg_errno);
  return ok == EXPECTED_CHECKS ? 0 : 1;
}
