/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <stdio.h>
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

int main(void) {
  int ok = 0;

  errno = 0;
  int sem = semget(IPC_PRIVATE, 1, IPC_CREAT | 0600);
  if (sem == -1 && errno == ENOSYS) {
    ok++;
  } else {
    fprintf(stderr, "semget rc=%d errno=%d (want -1/ENOSYS)\n", sem, errno);
    return 1;
  }

  errno = 0;
  int shm = shmget(IPC_PRIVATE, 4096, IPC_CREAT | 0600);
  if (shm == -1 && errno == ENOSYS) {
    ok++;
  } else {
    fprintf(stderr, "shmget rc=%d errno=%d (want -1/ENOSYS)\n", shm, errno);
    return 1;
  }

  errno = 0;
  int msg = msgget(IPC_PRIVATE, IPC_CREAT | 0600);
  if (msg == -1 && errno == ENOSYS) {
    ok++;
  } else {
    fprintf(stderr, "msgget rc=%d errno=%d (want -1/ENOSYS)\n", msg, errno);
    return 1;
  }

  printf("sysvipc ok=%d\n", ok);
  return 0;
}
