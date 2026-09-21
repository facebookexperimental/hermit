/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * flock(2) probes for hermit-cli/tests/flock_exclusion.rs.
 *
 * Scenario is argv[1]; "exclusion" when absent. Every scenario prints a
 * scenario-specific `...-ok` marker on success and a line beginning `FAIL` on
 * failure, and exits 0 / 1 accordingly, so the Rust driver can distinguish a
 * product failure from a harness failure (exit 2).
 *
 *   exclusion  Mutual exclusion, the property the pre-#2373 no-op destroyed:
 *              a held LOCK_EX must exclude a second open file description in
 *              the same process AND a second process, and must become
 *              available again after LOCK_UN. Matches native Linux, so it is
 *              also meaningful under --verify and under record/replay.
 *
 *   upgrade    A CONTENDED BLOCKING LOCK_SH -> LOCK_EX conversion. Detcore
 *              cannot park a thread on a file lock deterministically, so it
 *              refuses; the point of this scenario is that the refusal must
 *              leave the caller's shared lock intact. Linux converts a lock
 *              non-atomically (the old lock is deleted before the conflict
 *              scan), so a naive LOCK_NB probe silently destroys it. A fresh
 *              open file description reports whether the original shared lock
 *              survived. HERMIT ONLY: natively the
 *              blocking conversion sleeps and this scenario deadlocks by
 *              construction, which is exactly why Detcore refuses instead.
 *
 *   received   Receive an already-locked open file description through
 *              SCM_RIGHTS, then attempt a contended blocking upgrade. Hermit
 *              did not observe the original lock acquisition, so it must
 *              refuse before a destructive LOCK_NB probe rather than drop a
 *              lock it cannot restore.
 *
 *   fork-blocking-refusal  A DBT copied child cannot enter the Rust Detcore
 *              handler, so a blocking flock must return ENOLCK instead of
 *              running natively and deadlocking its waiting parent.
 *
 *   fork-safe-operations  The same copied child still delegates malformed,
 *              nonblocking, and unlock operations to the kernel.
 *
 *   fork-vfork-blocking  A copied fork child's vfork child must route a
 *              blocking flock through the copied-syscall guard. Native Linux
 *              blocks; DBT returns ENOLCK before the kernel.
 *
 *   failed-clone  A rejected process-clone syscall must not discard known
 *              flock state; the next uncontended blocking lock succeeds.
 *
 *   pidfd-getfd  A successful self pidfd_getfd creates another alias of the
 *              source open file description. Unlocking through the returned
 *              fd must invalidate the source alias's cached flock authority;
 *              failed duplication and duplication of an unrelated fd must not
 *              discard still-authoritative state.
 *
 *   pidfd-getfd-record  Record/replay reissues a self pidfd_getfd alias and
 *              preserves both its shared OFD and reserved-flags errors.
 *
 *   sent-after-fork  Transfer a lock acquired after fork, let the receiver
 *              unlock it, and prove the sender does not restore stale state.
 *
 *   vfork-upgrade  A copied vfork child must not enter a blocking flock that
 *              bypasses Detcore while its parent is suspended.
 *
 *   holder     Take LOCK_EX|LOCK_NB, print, release. Deliberately minimal, so
 *              a record/replay driver can tell "replay re-took the kernel
 *              lock" from "replay only replayed the return value".
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/sched.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

/* Harness failure: something unrelated to flock semantics broke. */
#define HARNESS_FAILURE 2

/* argv[2], when present, overrides the lock file path. Hermit replaces guest
 * /tmp with a per-run isolated directory, so a driver that needs the host and
 * the guest to contend for ONE inode -- the record/replay side-effect probe --
 * must name a path outside /tmp. */
static const char *lock_override = NULL;

static const char *lock_path(const char *scenario) {
  static char buffer[512];
  if (lock_override != NULL) {
    return lock_override;
  }
  snprintf(buffer, sizeof(buffer), "/tmp/hermit-flock-%s.lock", scenario);
  return buffer;
}

static int open_lock(const char *path) {
  return open(path, O_CREAT | O_RDWR, 0600);
}


static int send_fd(int socket_fd, int fd) {
  char payload = 'f';
  struct iovec iov = {.iov_base = &payload, .iov_len = sizeof(payload)};
  char control[CMSG_SPACE(sizeof(fd))];
  memset(control, 0, sizeof(control));
  struct msghdr message = {
      .msg_iov = &iov,
      .msg_iovlen = 1,
      .msg_control = control,
      .msg_controllen = sizeof(control),
  };
  struct cmsghdr *header = CMSG_FIRSTHDR(&message);
  header->cmsg_level = SOL_SOCKET;
  header->cmsg_type = SCM_RIGHTS;
  header->cmsg_len = CMSG_LEN(sizeof(fd));
  memcpy(CMSG_DATA(header), &fd, sizeof(fd));
  return sendmsg(socket_fd, &message, 0) == 1 ? 0 : -1;
}

static int send_fd_mmsg(int socket_fd, int fd) {
  char payload = 'f';
  struct iovec iov = {.iov_base = &payload, .iov_len = sizeof(payload)};
  char control[CMSG_SPACE(sizeof(fd))];
  memset(control, 0, sizeof(control));
  struct mmsghdr message = {0};
  message.msg_hdr.msg_iov = &iov;
  message.msg_hdr.msg_iovlen = 1;
  message.msg_hdr.msg_control = control;
  message.msg_hdr.msg_controllen = sizeof(control);
  struct cmsghdr *header = CMSG_FIRSTHDR(&message.msg_hdr);
  header->cmsg_level = SOL_SOCKET;
  header->cmsg_type = SCM_RIGHTS;
  header->cmsg_len = CMSG_LEN(sizeof(fd));
  memcpy(CMSG_DATA(header), &fd, sizeof(fd));
  return sendmmsg(socket_fd, &message, 1, 0) == 1 ? 0 : -1;
}

static int receive_fd(int socket_fd) {
  char payload = 0;
  struct iovec iov = {.iov_base = &payload, .iov_len = sizeof(payload)};
  char control[CMSG_SPACE(sizeof(int))];
  memset(control, 0, sizeof(control));
  struct msghdr message = {
      .msg_iov = &iov,
      .msg_iovlen = 1,
      .msg_control = control,
      .msg_controllen = sizeof(control),
  };
  if (recvmsg(socket_fd, &message, 0) != 1) {
    return -1;
  }
  struct cmsghdr *header = CMSG_FIRSTHDR(&message);
  if (header == NULL || header->cmsg_level != SOL_SOCKET ||
      header->cmsg_type != SCM_RIGHTS ||
      header->cmsg_len != CMSG_LEN(sizeof(int))) {
    errno = EBADMSG;
    return -1;
  }
  int fd = -1;
  memcpy(&fd, CMSG_DATA(header), sizeof(fd));
  return fd;
}

static int scenario_exclusion(void) {
  const char *path = lock_path("exclusion");
  int first = open_lock(path);
  int second = open_lock(path);
  if (first < 0 || second < 0) {
    printf("FAIL: could not open %s (errno=%d)\n", path, errno);
    return HARNESS_FAILURE;
  }

  if (flock(first, LOCK_EX | LOCK_NB) != 0) {
    printf("FAIL: LOCK_EX on an unlocked file was refused (errno=%d)\n", errno);
    return 1;
  }
  printf("flock-first-holder-acquired\n");

  /* A second open of the same file is an independent flock contender, even
   * inside one process. */
  errno = 0;
  if (flock(second, LOCK_EX | LOCK_NB) == 0) {
    printf("FAIL: a second open file description acquired a held LOCK_EX\n");
    return 1;
  }
  if (errno != EWOULDBLOCK) {
    printf("FAIL: second open file description got errno=%d, wanted EWOULDBLOCK\n",
           errno);
    return 1;
  }
  printf("flock-second-open-excluded\n");

  fflush(stdout);
  pid_t child = fork();
  if (child < 0) {
    printf("FAIL: fork failed (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  if (child == 0) {
    int contender = open_lock(path);
    if (contender < 0) {
      _exit(HARNESS_FAILURE);
    }
    errno = 0;
    int taken = flock(contender, LOCK_EX | LOCK_NB);
    _exit((taken == -1 && errno == EWOULDBLOCK) ? 0 : 1);
  }
  int status = 0;
  if (waitpid(child, &status, 0) < 0) {
    printf("FAIL: waitpid failed (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  if (!WIFEXITED(status)) {
    printf("FAIL: contending process did not exit normally (status=%d)\n", status);
    return HARNESS_FAILURE;
  }
  if (WEXITSTATUS(status) == HARNESS_FAILURE) {
    printf("FAIL: contending process could not open %s\n", path);
    return HARNESS_FAILURE;
  }
  if (WEXITSTATUS(status) != 0) {
    printf("FAIL: a second process acquired a LOCK_EX this process holds\n");
    return 1;
  }
  printf("flock-second-process-excluded\n");

  if (flock(first, LOCK_UN) != 0) {
    printf("FAIL: LOCK_UN was refused (errno=%d)\n", errno);
    return 1;
  }
  if (flock(second, LOCK_EX | LOCK_NB) != 0) {
    printf("FAIL: LOCK_EX after LOCK_UN was refused (errno=%d)\n", errno);
    return 1;
  }
  printf("flock-released-and-reacquired\n");
  printf("flock-exclusion-ok\n");
  close(first);
  close(second);
  return 0;
}

static int scenario_upgrade(void) {
  const char *path = lock_path("upgrade");
  int held = open_lock(path);
  int contender = open_lock(path);
  if (held < 0 || contender < 0) {
    printf("FAIL: could not open %s (errno=%d)\n", path, errno);
    return HARNESS_FAILURE;
  }
  if (flock(held, LOCK_SH | LOCK_NB) != 0) {
    printf("FAIL: LOCK_SH on an unlocked file was refused (errno=%d)\n", errno);
    return 1;
  }
  printf("flock-upgrade-parent-holds-shared\n");
  if (flock(contender, LOCK_SH | LOCK_NB) != 0) {
    printf("FAIL: second LOCK_SH was refused (errno=%d)\n", errno);
    return 1;
  }
  printf("flock-upgrade-contender-holds-shared\n");

  /* The contended blocking conversion. Natively this sleeps forever here,
   * which is the deadlock Detcore refuses rather than reproduces. Both locks
   * are in one process so Detcore still knows the first descriptor's mode and
   * must exercise the restore path after its substituted LOCK_NB probe. */
  errno = 0;
  int upgraded = flock(held, LOCK_EX);
  int upgrade_errno = errno;
  if (upgraded == 0) {
    printf("FAIL: a contended blocking LOCK_EX upgrade reported success\n");
    return 1;
  }
  printf("flock-upgrade-refused errno=%d\n", upgrade_errno);

  flock(contender, LOCK_UN);
  close(contender);
  int probe = open_lock(path);
  if (probe < 0) {
    return HARNESS_FAILURE;
  }
  errno = 0;
  if (flock(probe, LOCK_EX | LOCK_NB) == 0) {
    printf("FAIL: the refused upgrade destroyed this process's shared lock\n");
    return 1;
  }
  if (errno != EWOULDBLOCK) {
    printf("FAIL: shared-lock survival probe got errno=%d\n", errno);
    return 1;
  }
  printf("flock-upgrade-preserved-shared-lock\n");
  printf("flock-upgrade-ok\n");
  close(probe);
  close(held);
  return 0;
}

static int scenario_received(void) {
  const char *path = lock_path("received");
  int holder = open_lock(path);
  if (holder < 0 || flock(holder, LOCK_SH | LOCK_NB) != 0) {
    printf("FAIL: could not establish the shared lock to transfer (errno=%d)\n",
           errno);
    return HARNESS_FAILURE;
  }

  int sockets[2];
  if (socketpair(AF_UNIX, SOCK_DGRAM, 0, sockets) != 0) {
    printf("FAIL: socketpair failed (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }

  fflush(stdout);
  pid_t child = fork();
  if (child < 0) {
    printf("FAIL: fork failed (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  if (child == 0) {
    close(sockets[0]);
    close(holder);
    int received = receive_fd(sockets[1]);
    if (received < 0) {
      _exit(HARNESS_FAILURE);
    }
    errno = 0;
    if (flock(received, 0) != -1 || errno != EINVAL) {
      printf("FAIL: invalid flock operation on received fd did not return EINVAL (errno=%d)\n",
             errno);
      _exit(1);
    }

    int contender = open_lock(path);
    if (contender < 0 || flock(contender, LOCK_SH | LOCK_NB) != 0) {
      _exit(HARNESS_FAILURE);
    }

    errno = 0;
    int upgraded = flock(received, LOCK_EX);
    int upgrade_errno = errno;
    if (upgraded == 0 || upgrade_errno != ENOLCK) {
      printf("FAIL: blocking upgrade on received fd returned %d errno=%d\n",
             upgraded, upgrade_errno);
      _exit(1);
    }
    printf("flock-received-upgrade-refused errno=%d\n", upgrade_errno);

    flock(contender, LOCK_UN);
    close(contender);
    int probe = open_lock(path);
    if (probe < 0) {
      _exit(HARNESS_FAILURE);
    }
    errno = 0;
    if (flock(probe, LOCK_EX | LOCK_NB) == 0) {
      printf("FAIL: probing an unknown received fd destroyed its shared lock\n");
      _exit(1);
    }
    if (errno != EWOULDBLOCK) {
      printf("FAIL: received-fd survival probe got errno=%d\n", errno);
      _exit(1);
    }
    printf("flock-received-upgrade-preserved-shared-lock\n");
    printf("flock-received-upgrade-ok\n");
    _exit(0);
  }

  close(sockets[1]);
  if (send_fd(sockets[0], holder) != 0) {
    printf("FAIL: sendmsg could not transfer the locked fd (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  close(sockets[0]);

  int status = 0;
  if (waitpid(child, &status, 0) < 0 || !WIFEXITED(status)) {
    printf("FAIL: received-fd child did not exit normally (status=%d errno=%d)\n",
           status, errno);
    return HARNESS_FAILURE;
  }
  close(holder);
  return WEXITSTATUS(status);
}

static int scenario_fork_blocking_refusal(void) {
  const char *path = lock_path("fork-blocking-refusal");
  int holder = open_lock(path);
  if (holder < 0 || flock(holder, LOCK_SH | LOCK_NB) != 0) {
    printf("FAIL: could not establish parent shared lock (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }

  fflush(stdout);
  pid_t child = fork();
  if (child < 0) {
    printf("FAIL: fork failed (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  if (child == 0) {
    int contender = open_lock(path);
    errno = 0;
    int result = contender < 0 ? -2 : flock(contender, LOCK_EX);
    if (result != -1 || errno != ENOLCK) {
      dprintf(STDOUT_FILENO,
              "FAIL: copied fork child blocking flock returned %d errno=%d\n",
              result, errno);
      _exit(1);
    }
    dprintf(STDOUT_FILENO, "flock-fork-child-blocking-refused errno=%d\n",
            errno);
    _exit(0);
  }

  int status = 0;
  if (waitpid(child, &status, 0) < 0 || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    printf("FAIL: copied fork child did not fail closed (status=%d errno=%d)\n",
           status, errno);
    return HARNESS_FAILURE;
  }
  printf("flock-fork-child-refusal-ok\n");
  flock(holder, LOCK_UN);
  close(holder);
  return 0;
}

static int scenario_fork_safe_operations(void) {
  const char *path = lock_path("fork-safe-operations");
  int inherited = open_lock(path);
  if (inherited < 0 || flock(inherited, LOCK_SH | LOCK_NB) != 0) {
    printf("FAIL: could not establish inherited shared lock (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }

  fflush(stdout);
  pid_t child = fork();
  if (child < 0) {
    printf("FAIL: fork failed (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  if (child == 0) {
    errno = 0;
    if (flock(inherited, 0) != -1 || errno != EINVAL) {
      dprintf(STDOUT_FILENO,
              "FAIL: copied child malformed flock returned errno=%d\n", errno);
      _exit(1);
    }
    dprintf(STDOUT_FILENO, "flock-fork-child-malformed-einval\n");
    int contender = open_lock(path);
    errno = 0;
    if (contender < 0 || flock(contender, LOCK_EX | LOCK_NB) != -1 ||
        (errno != EWOULDBLOCK && errno != EAGAIN)) {
      dprintf(STDOUT_FILENO,
              "FAIL: copied child nonblocking contention returned errno=%d\n",
              errno);
      _exit(1);
    }
    dprintf(STDOUT_FILENO,
            "flock-fork-child-nonblocking-contended errno=%d\n", errno);
    close(contender);
    if (flock(inherited, LOCK_SH | LOCK_NB) != 0) {
      dprintf(STDOUT_FILENO,
              "FAIL: copied child nonblocking flock failed errno=%d\n", errno);
      _exit(1);
    }
    dprintf(STDOUT_FILENO, "flock-fork-child-nonblocking-ok\n");
    if (flock(inherited, LOCK_UN) != 0) {
      dprintf(STDOUT_FILENO, "FAIL: copied child unlock failed errno=%d\n",
              errno);
      _exit(1);
    }
    dprintf(STDOUT_FILENO, "flock-fork-child-unlock-ok\n");
    _exit(0);
  }

  int status = 0;
  if (waitpid(child, &status, 0) < 0 || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    printf("FAIL: copied fork child safe-operation run failed (status=%d errno=%d)\n",
           status, errno);
    return HARNESS_FAILURE;
  }
  int probe = open_lock(path);
  if (probe < 0 || flock(probe, LOCK_EX | LOCK_NB) != 0) {
    printf("FAIL: copied child unlock did not release inherited lock (errno=%d)\n",
           errno);
    return 1;
  }
  printf("flock-fork-child-safe-operations-ok\n");
  flock(probe, LOCK_UN);
  close(probe);
  close(inherited);
  return 0;
}

static int scenario_fork_vfork_blocking(void) {
  const char *path = lock_path("fork-vfork-blocking");
  int inherited = open_lock(path);
  int contender = open_lock(path);
  if (inherited < 0 || contender < 0 ||
      flock(inherited, LOCK_SH | LOCK_NB) != 0 ||
      flock(contender, LOCK_SH | LOCK_NB) != 0) {
    printf("FAIL: could not establish nested vfork contention (errno=%d)\n",
           errno);
    return HARNESS_FAILURE;
  }

  fflush(stdout);
  pid_t fork_child = fork();
  if (fork_child < 0) {
    printf("FAIL: outer fork failed (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  if (fork_child == 0) {
    dprintf(STDOUT_FILENO, "flock-fork-vfork-child-entered\n");
    pid_t vfork_child = vfork();
    if (vfork_child < 0) {
      dprintf(STDOUT_FILENO,
              "FAIL: copied fork child vfork returned errno=%d\n", errno);
      _exit(1);
    }
    if (vfork_child == 0) {
      errno = 0;
      int flock_result = flock(inherited, LOCK_EX);
      if (flock_result != -1 || errno != ENOLCK) {
        static const char failure[] =
            "FAIL: nested vfork child blocking flock did not return ENOLCK\n";
        (void)write(STDOUT_FILENO, failure, sizeof(failure) - 1);
        _exit(1);
      }
      static const char refused[] =
          "flock-nested-vfork-blocking-refused errno=37\n";
      (void)write(STDOUT_FILENO, refused, sizeof(refused) - 1);
      _exit(0);
    }
    int vfork_status = 0;
    if (waitpid(vfork_child, &vfork_status, 0) != vfork_child ||
        !WIFEXITED(vfork_status) || WEXITSTATUS(vfork_status) != 0) {
      dprintf(STDOUT_FILENO,
              "FAIL: nested vfork child did not fail closed (status=%d errno=%d)\n",
              vfork_status, errno);
      _exit(1);
    }
    _exit(0);
  }

  int status = 0;
  if (waitpid(fork_child, &status, 0) != fork_child || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    printf("FAIL: copied fork child failed (status=%d errno=%d)\n", status,
           errno);
    return HARNESS_FAILURE;
  }
  printf("flock-fork-vfork-copied-policy-ok\n");
  flock(inherited, LOCK_UN);
  flock(contender, LOCK_UN);
  close(inherited);
  close(contender);
  return 0;
}

static int scenario_failed_clone(void) {
  const char *path = lock_path("failed-clone");
  int fd = open_lock(path);
  if (fd < 0) {
    printf("FAIL: could not open %s (errno=%d)\n", path, errno);
    return HARNESS_FAILURE;
  }

  errno = 0;
  long result = syscall(SYS_clone, CLONE_SIGHAND, NULL, NULL, NULL, 0);
  if (result != -1 || errno != EINVAL) {
    printf("FAIL: invalid process clone returned %ld errno=%d\n", result, errno);
    return 1;
  }
  printf("flock-failed-clone-rejected errno=%d\n", errno);

  errno = 0;
  if (flock(fd, LOCK_EX) != 0) {
    printf("FAIL: uncontended blocking LOCK_EX after failed clone was refused (errno=%d)\n", errno);
    return 1;
  }
  printf("flock-after-failed-clone-acquired\n");
  if (flock(fd, LOCK_UN) != 0) {
    printf("FAIL: LOCK_UN after failed clone was refused (errno=%d)\n", errno);
    return 1;
  }
  printf("flock-failed-clone-ok\n");
  close(fd);
  return 0;
}

static int scenario_pidfd_getfd(void) {
  const char *path = lock_path("pidfd-getfd");
  int source = open_lock(path);
  int contender = open_lock(path);
  if (source < 0 || contender < 0 ||
      flock(source, LOCK_SH | LOCK_NB) != 0) {
    printf("FAIL: could not establish pidfd_getfd source lock (errno=%d)\n",
           errno);
    return HARNESS_FAILURE;
  }

  int pidfd = (int)syscall(SYS_pidfd_open, getpid(), 0);
  if (pidfd < 0) {
    printf("FAIL: pidfd_open self failed (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  int duplicate = (int)syscall(SYS_pidfd_getfd, pidfd, source, 0);
  if (duplicate < 0) {
    printf("FAIL: pidfd_getfd self failed (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  if (flock(duplicate, LOCK_UN) != 0) {
    printf("FAIL: pidfd_getfd duplicate unlock failed (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  printf("flock-pidfd-duplicate-unlocked\n");

  if (flock(contender, LOCK_SH | LOCK_NB) != 0) {
    printf("FAIL: pidfd_getfd contender could not take LOCK_SH (errno=%d)\n",
           errno);
    return HARNESS_FAILURE;
  }
  errno = 0;
  if (flock(source, LOCK_EX) != -1 || errno != ENOLCK) {
    printf("FAIL: pidfd_getfd source blocking upgrade returned errno=%d\n",
           errno);
    return 1;
  }
  printf("flock-pidfd-source-upgrade-refused errno=%d\n", errno);

  if (flock(contender, LOCK_UN) != 0) {
    printf("FAIL: pidfd_getfd contender unlock failed (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  int fresh = open_lock(path);
  if (fresh < 0 || flock(fresh, LOCK_EX | LOCK_NB) != 0) {
    printf("FAIL: pidfd_getfd stale source cache restored a released lock (errno=%d)\n",
           errno);
    return 1;
  }
  printf("flock-pidfd-stale-restore-absent\n");
  flock(fresh, LOCK_UN);
  close(fresh);
  close(duplicate);
  close(contender);
  close(source);

  const char *failed_path = lock_path("pidfd-getfd-failed");
  int failed_source = open_lock(failed_path);
  if (failed_source < 0 || flock(failed_source, LOCK_SH | LOCK_NB) != 0) {
    printf("FAIL: could not establish failed-pidfd_getfd source lock (errno=%d)\n",
           errno);
    return HARNESS_FAILURE;
  }
  errno = 0;
  if (syscall(SYS_pidfd_getfd, pidfd, -1, 0) != -1 || errno != EBADF) {
    printf("FAIL: invalid pidfd_getfd did not return EBADF (errno=%d)\n", errno);
    return 1;
  }
  if (flock(failed_source, LOCK_EX) != 0) {
    printf("FAIL: failed pidfd_getfd discarded source authority (errno=%d)\n",
           errno);
    return 1;
  }
  printf("flock-pidfd-failed-getfd-preserved errno=%d\n", EBADF);
  flock(failed_source, LOCK_UN);

  if (flock(failed_source, LOCK_SH | LOCK_NB) != 0) {
    printf("FAIL: could not establish invalid-flags source lock (errno=%d)\n",
           errno);
    return HARNESS_FAILURE;
  }
  errno = 0;
  if (syscall(SYS_pidfd_getfd, pidfd, failed_source, 1) != -1 ||
      errno != EINVAL) {
    printf("FAIL: nonzero-flags valid-pidfd/valid-targetfd pidfd_getfd did not return EINVAL (errno=%d)\n",
           errno);
    return 1;
  }
  printf("flock-pidfd-valid-pidfd-valid-targetfd-flags-precedence errno=%d\n",
         EINVAL);

  errno = 0;
  if (syscall(SYS_pidfd_getfd, pidfd, -1, 1) != -1 || errno != EINVAL) {
    printf("FAIL: nonzero-flags valid-pidfd/invalid-targetfd pidfd_getfd did not return EINVAL (errno=%d)\n",
           errno);
    return 1;
  }
  printf("flock-pidfd-valid-pidfd-invalid-targetfd-flags-precedence errno=%d\n",
         EINVAL);

  errno = 0;
  if (syscall(SYS_pidfd_getfd, -1, failed_source, 1) != -1 ||
      errno != EINVAL) {
    printf("FAIL: nonzero-flags invalid-pidfd/valid-targetfd pidfd_getfd did not return EINVAL (errno=%d)\n",
           errno);
    return 1;
  }
  printf("flock-pidfd-invalid-pidfd-valid-targetfd-flags-precedence errno=%d\n",
         EINVAL);

  errno = 0;
  if (syscall(SYS_pidfd_getfd, -1, -1, 1) != -1 || errno != EINVAL) {
    printf("FAIL: nonzero-flags invalid-pidfd/invalid-targetfd pidfd_getfd did not preserve EINVAL precedence (errno=%d)\n",
           errno);
    return 1;
  }
  printf("flock-pidfd-invalid-pidfd-invalid-targetfd-flags-precedence errno=%d\n",
         EINVAL);

  if (flock(failed_source, LOCK_EX) != 0) {
    printf("FAIL: invalid-flags pidfd_getfd discarded source authority (errno=%d)\n",
           errno);
    return 1;
  }
  printf("flock-pidfd-recorded-failure-preserved errno=%d\n", EINVAL);
  flock(failed_source, LOCK_UN);
  close(failed_source);

  const char *unrelated_path = lock_path("pidfd-getfd-unrelated");
  char duplicated_path[512];
  snprintf(duplicated_path, sizeof(duplicated_path), "%s-duplicate",
           unrelated_path);
  int authoritative = open_lock(unrelated_path);
  int unrelated = open_lock(duplicated_path);
  if (authoritative < 0 || unrelated < 0 ||
      flock(authoritative, LOCK_SH | LOCK_NB) != 0) {
    printf("FAIL: could not establish unrelated-authority bracket (errno=%d)\n",
           errno);
    return HARNESS_FAILURE;
  }
  int unrelated_duplicate =
      (int)syscall(SYS_pidfd_getfd, pidfd, unrelated, 0);
  if (unrelated_duplicate < 0) {
    printf("FAIL: unrelated pidfd_getfd failed (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  if (flock(authoritative, LOCK_EX) != 0) {
    printf("FAIL: unrelated pidfd_getfd discarded authoritative flock state (errno=%d)\n",
           errno);
    return 1;
  }
  printf("flock-pidfd-unrelated-authority-preserved\n");
  flock(authoritative, LOCK_UN);
  close(unrelated_duplicate);
  close(unrelated);
  close(authoritative);

  int ready[2];
  int release[2];
  if (pipe(ready) != 0 || pipe(release) != 0) {
    printf("FAIL: could not create foreign-pidfd synchronization pipes (errno=%d)\n",
           errno);
    return HARNESS_FAILURE;
  }
  pid_t child = fork();
  if (child < 0) {
    printf("FAIL: foreign-pidfd fork failed (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  if (child == 0) {
    close(ready[0]);
    close(release[1]);
    int foreign = open_lock(lock_path("pidfd-getfd-foreign"));
    char token = 0;
    if (foreign < 0 ||
        write(ready[1], &foreign, sizeof(foreign)) != sizeof(foreign) ||
        read(release[0], &token, sizeof(token)) != sizeof(token)) {
      _exit(HARNESS_FAILURE);
    }
    close(foreign);
    _exit(0);
  }
  close(ready[1]);
  close(release[0]);
  int foreign = -1;
  if (read(ready[0], &foreign, sizeof(foreign)) != sizeof(foreign)) {
    printf("FAIL: child did not publish its foreign descriptor (errno=%d)\n",
           errno);
    return HARNESS_FAILURE;
  }
  int child_pidfd = (int)syscall(SYS_pidfd_open, child, 0);
  if (child_pidfd < 0) {
    printf("FAIL: pidfd_open child failed (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  errno = 0;
  if (syscall(SYS_pidfd_getfd, child_pidfd, foreign, 0) != -1 ||
      errno != EOPNOTSUPP) {
    printf("FAIL: foreign pidfd_getfd did not fail closed (errno=%d)\n",
           errno);
    return 1;
  }
  printf("flock-pidfd-foreign-source-refused errno=%d\n", errno);
  char token = 'x';
  if (write(release[1], &token, sizeof(token)) != sizeof(token)) {
    printf("FAIL: could not release foreign-pidfd child (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  int status = 0;
  if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    printf("FAIL: foreign-pidfd child failed (status=%d errno=%d)\n", status,
           errno);
    return HARNESS_FAILURE;
  }
  close(child_pidfd);
  close(ready[0]);
  close(release[1]);
  close(pidfd);
  printf("flock-pidfd-getfd-ok\n");
  return 0;
}

static int scenario_pidfd_getfd_record(void) {
  int source = open_lock(lock_path("pidfd-getfd-record"));
  if (source < 0 || lseek(source, 17, SEEK_SET) != 17) {
    printf("FAIL: could not prepare recorded pidfd_getfd source (errno=%d)\n",
           errno);
    return HARNESS_FAILURE;
  }

  int pidfd = (int)syscall(SYS_pidfd_open, getpid(), 0);
  if (pidfd < 0) {
    printf("FAIL: recorded pidfd_open self failed (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  int duplicate = (int)syscall(SYS_pidfd_getfd, pidfd, source, 0);
  if (duplicate < 0 || lseek(duplicate, 0, SEEK_CUR) != 17 ||
      lseek(duplicate, 31, SEEK_SET) != 31 ||
      lseek(source, 0, SEEK_CUR) != 31) {
    printf("FAIL: recorded pidfd_getfd did not reproduce one OFD (errno=%d)\n",
           errno);
    return HARNESS_FAILURE;
  }
  printf("flock-pidfd-record-success-shared-ofd\n");
  close(duplicate);

  errno = 0;
  if (syscall(SYS_pidfd_getfd, pidfd, -1, 0) != -1 || errno != EBADF) {
    printf("FAIL: recorded invalid targetfd did not return EBADF (errno=%d)\n",
           errno);
    return 1;
  }
  printf("flock-pidfd-record-validation-failure errno=%d\n", EBADF);

  const int pidfds[] = {pidfd, pidfd, -1, -1};
  const int targets[] = {source, -1, source, -1};
  for (size_t i = 0; i < sizeof(pidfds) / sizeof(pidfds[0]); ++i) {
    errno = 0;
    if (syscall(SYS_pidfd_getfd, pidfds[i], targets[i], 1) != -1 ||
        errno != EINVAL) {
      printf("FAIL: recorded reserved flags case %zu did not return EINVAL (errno=%d)\n",
             i, errno);
      return 1;
    }
  }
  printf("flock-pidfd-record-flags-failures errno=%d count=4\n", EINVAL);

  close(pidfd);
  close(source);
  printf("flock-pidfd-record-ok\n");
  return 0;
}

static int scenario_pidfd_getfd_relaxed_refusal(void) {
  errno = 0;
  if (syscall(SYS_pidfd_getfd, -1, -1, 0) != -1 || errno != EOPNOTSUPP) {
    printf("FAIL: relaxed pidfd_getfd did not fail before validation (errno=%d)\n",
           errno);
    return 1;
  }
  printf("flock-pidfd-relaxed-refused errno=%d\n", errno);
  return 0;
}

static int scenario_sent_after_fork(const char *scenario, int batched) {
  const char *path = lock_path(scenario);
  int sockets[2];
  if (socketpair(AF_UNIX, SOCK_DGRAM, 0, sockets) != 0) {
    printf("FAIL: socketpair failed (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }

  fflush(stdout);
  pid_t child = fork();
  if (child < 0) {
    printf("FAIL: fork failed (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  if (child == 0) {
    close(sockets[0]);
    int received = receive_fd(sockets[1]);
    if (received < 0 || flock(received, LOCK_UN) != 0) {
      _exit(HARNESS_FAILURE);
    }
    _exit(0);
  }

  close(sockets[1]);
  int sent = open_lock(path);
  if (sent < 0 || flock(sent, LOCK_SH | LOCK_NB) != 0) {
    printf("FAIL: sender could not establish shared lock (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  printf("flock-sender-locked-after-fork\n");
  if ((batched ? send_fd_mmsg(sockets[0], sent) : send_fd(sockets[0], sent)) != 0) {
    printf("FAIL: sendmsg could not transfer post-fork lock (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  close(sockets[0]);

  int status = 0;
  if (waitpid(child, &status, 0) < 0 || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    printf("FAIL: receiver could not unlock transferred fd (status=%d errno=%d)\n",
           status, errno);
    return HARNESS_FAILURE;
  }
  printf("flock-receiver-unlocked-transferred-lock\n");

  int contender = open_lock(path);
  if (contender < 0 || flock(contender, LOCK_SH | LOCK_NB) != 0) {
    printf("FAIL: could not establish post-transfer contender (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  errno = 0;
  if (flock(sent, LOCK_EX) != -1 || errno != ENOLCK) {
    printf("FAIL: post-transfer blocking upgrade returned errno=%d\n", errno);
    return 1;
  }
  printf("flock-sender-upgrade-refused errno=%d\n", errno);

  flock(contender, LOCK_UN);
  close(contender);
  int probe = open_lock(path);
  if (probe < 0 || flock(probe, LOCK_EX | LOCK_NB) != 0) {
    printf("FAIL: sender restored stale lock after receiver unlocked it (errno=%d)\n", errno);
    return 1;
  }
  printf("flock-transfer-release-remained-unlocked\n");
  printf("flock-sent-after-fork-ok\n");
  close(probe);
  close(sent);
  return 0;
}

static int scenario_failed_send_preserves_state(void) {
  const char *path = lock_path("failed-send");
  int fd = open_lock(path);
  if (fd < 0 || flock(fd, LOCK_SH | LOCK_NB) != 0) {
    printf("FAIL: could not establish failed-send lock (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }

  char payload = "f"[0];
  struct iovec iov = {.iov_base = &payload, .iov_len = sizeof(payload)};
  char control[CMSG_SPACE(sizeof(fd))];
  memset(control, 0, sizeof(control));
  struct msghdr message = {0};
  message.msg_iov = &iov;
  message.msg_iovlen = 1;
  message.msg_control = control;
  message.msg_controllen = sizeof(control);
  struct cmsghdr *header = CMSG_FIRSTHDR(&message);
  header->cmsg_level = SOL_SOCKET;
  header->cmsg_type = SCM_RIGHTS;
  header->cmsg_len = CMSG_LEN(sizeof(fd));
  memcpy(CMSG_DATA(header), &fd, sizeof(fd));

  errno = 0;
  if (sendmsg(-1, &message, 0) != -1 || errno != EBADF) {
    printf("FAIL: invalid sendmsg did not return EBADF (errno=%d)\n", errno);
    return 1;
  }
  printf("flock-failed-send-rejected errno=%d\n", errno);
  if (flock(fd, LOCK_EX) != 0) {
    printf("FAIL: failed sendmsg discarded known flock state (errno=%d)\n", errno);
    return 1;
  }
  printf("flock-after-failed-send-acquired\n");
  flock(fd, LOCK_UN);
  close(fd);
  return 0;
}

static int scenario_partial_sendmmsg(void) {
  const char *first_path = lock_path("partial-sendmmsg-first");
  char second_path[512];
  snprintf(second_path, sizeof(second_path), "%s-second", first_path);
  int sockets[2];
  if (socketpair(AF_UNIX, SOCK_DGRAM, 0, sockets) != 0) {
    printf("FAIL: socketpair failed (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }

  fflush(stdout);
  pid_t child = fork();
  if (child < 0) {
    printf("FAIL: fork failed (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  if (child == 0) {
    close(sockets[0]);
    int received = receive_fd(sockets[1]);
    if (received < 0 || flock(received, LOCK_UN) != 0)
      _exit(HARNESS_FAILURE);
    _exit(0);
  }

  close(sockets[1]);
  int first = open_lock(first_path);
  int second = open(second_path, O_CREAT | O_RDWR | O_TRUNC, 0600);
  if (first < 0 || second < 0 || flock(first, LOCK_SH | LOCK_NB) != 0 ||
      flock(second, LOCK_SH | LOCK_NB) != 0) {
    printf("FAIL: could not establish partial-sendmmsg locks (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }

  char payloads[2] = {"a"[0], "b"[0]};
  struct iovec iov[2] = {
      {.iov_base = &payloads[0], .iov_len = 1},
      {.iov_base = &payloads[1], .iov_len = 1},
  };
  char control[CMSG_SPACE(sizeof(first))];
  memset(control, 0, sizeof(control));
  struct mmsghdr messages[2] = {0};
  messages[0].msg_hdr.msg_iov = &iov[0];
  messages[0].msg_hdr.msg_iovlen = 1;
  messages[0].msg_hdr.msg_control = control;
  messages[0].msg_hdr.msg_controllen = sizeof(control);
  struct cmsghdr *header = CMSG_FIRSTHDR(&messages[0].msg_hdr);
  header->cmsg_level = SOL_SOCKET;
  header->cmsg_type = SCM_RIGHTS;
  header->cmsg_len = CMSG_LEN(sizeof(first));
  memcpy(CMSG_DATA(header), &first, sizeof(first));
  messages[1].msg_hdr.msg_iov = &iov[1];
  messages[1].msg_hdr.msg_iovlen = 1;
  messages[1].msg_hdr.msg_control = (void *)1;
  messages[1].msg_hdr.msg_controllen = CMSG_SPACE(sizeof(second));

  int sent = sendmmsg(sockets[0], messages, 2, 0);
  if (sent != 1) {
    printf("FAIL: partial sendmmsg returned %d (errno=%d)\n", sent, errno);
    return HARNESS_FAILURE;
  }
  printf("flock-partial-sendmmsg-sent-one\n");
  close(sockets[0]);
  int status = 0;
  if (waitpid(child, &status, 0) < 0 || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    printf("FAIL: partial receiver failed (status=%d errno=%d)\n", status, errno);
    return HARNESS_FAILURE;
  }

  errno = 0;
  if (flock(first, LOCK_EX) != -1 || errno != ENOLCK) {
    printf("FAIL: delivered fd retained stale state (errno=%d)\n", errno);
    return 1;
  }
  errno = 0;
  if (flock(second, LOCK_EX) != -1 || errno != ENOLCK) {
    printf("FAIL: partial sendmmsg did not conservatively invalidate unsent state (errno=%d)\n", errno);
    return 1;
  }
  printf("flock-partial-sendmmsg-invalidated-all\n");
  close(first);
  close(second);
  return 0;
}

static int scenario_vfork_no_flock_state(void) {
  close(STDIN_FILENO);
  close(STDOUT_FILENO);
  close(STDERR_FILENO);
  pid_t child = vfork();
  if (child < 0)
    return HARNESS_FAILURE;
  if (child == 0)
    _exit(0);
  int status = 0;
  return waitpid(child, &status, 0) == child && WIFEXITED(status) &&
                 WEXITSTATUS(status) == 0
             ? 0
             : HARNESS_FAILURE;
}

enum vfork_form {
  VFORK_SYSCALL,
  CLONE_VFORK_SYSCALL,
  CLONE3_VFORK_SYSCALL,
};

enum clone_files_form {
  CLONE_FILES_SYSCALL,
  CLONE3_FILES_SYSCALL,
};

static pid_t start_clone_files_process(enum clone_files_form form) {
  if (form == CLONE_FILES_SYSCALL)
    return (pid_t)syscall(SYS_clone, (unsigned long)(CLONE_FILES | SIGCHLD),
                          NULL, NULL, NULL, 0);
  struct clone_args args = {
      .flags = CLONE_FILES,
      .exit_signal = SIGCHLD,
  };
  return (pid_t)syscall(SYS_clone3, &args, sizeof(args));
}

static int scenario_clone_files_process(enum clone_files_form form) {
  const char *name = form == CLONE_FILES_SYSCALL ? "clone-files-process"
                                                 : "clone3-files-process";
  char target_path[256];
  char replacement_path[256];
  snprintf(target_path, sizeof(target_path), "/tmp/hermit-%s-target", name);
  snprintf(replacement_path, sizeof(replacement_path),
           "/tmp/hermit-%s-replacement", name);
  int target = open(target_path, O_CREAT | O_RDWR | O_TRUNC, 0600);
  int replacement = open(replacement_path, O_CREAT | O_RDWR | O_TRUNC, 0600);
  if (target < 0 || replacement < 0 || write(replacement, "R", 1) != 1 ||
      lseek(replacement, 0, SEEK_SET) != 0) {
    printf("FAIL: could not prepare %s descriptors (errno=%d)\n", name, errno);
    return HARNESS_FAILURE;
  }

  fflush(stdout);
  pid_t child = start_clone_files_process(form);
  if (child < 0) {
    printf("flock-%s-refused errno=%d\n", name, errno);
    return errno == EOPNOTSUPP ? 0 : HARNESS_FAILURE;
  }
  if (child == 0) {
    if (close(target) != 0 || dup2(replacement, target) != target)
      _exit(HARNESS_FAILURE);
    _exit(0);
  }

  int status = 0;
  if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    printf("FAIL: %s child failed (status=%d errno=%d)\n", name, status,
           errno);
    return HARNESS_FAILURE;
  }
  char observed = 0;
  if (read(target, &observed, 1) != 1 || observed != 'R') {
    printf("FAIL: %s did not share the replaced descriptor slot (byte=%d errno=%d)\n",
           name, observed, errno);
    return 1;
  }
  printf("flock-%s-shared-mutation-observed\n", name);
  close(target);
  close(replacement);
  return 0;
}

static pid_t start_vfork_form(enum vfork_form form) {
  if (form == VFORK_SYSCALL)
    return vfork();
  if (form == CLONE_VFORK_SYSCALL)
    return (pid_t)syscall(SYS_clone, (unsigned long)(CLONE_VFORK | SIGCHLD),
                          NULL, NULL, NULL, 0);
  struct clone_args args = {
      .flags = CLONE_VFORK,
      .exit_signal = SIGCHLD,
  };
  return (pid_t)syscall(SYS_clone3, &args, sizeof(args));
}

static int scenario_vfork_upgrade(int make_unknown, enum vfork_form form) {
  const char *path = lock_path("vfork-upgrade");
  int inherited = open_lock(path);
  int contender = open_lock(path);
  if (inherited < 0 || contender < 0 ||
      flock(inherited, LOCK_SH | LOCK_NB) != 0 ||
      flock(contender, LOCK_SH | LOCK_NB) != 0) {
    printf("FAIL: could not establish vfork upgrade contention (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }

  if (make_unknown) {
    fflush(stdout);
    pid_t fork_child = fork();
    if (fork_child < 0)
      return HARNESS_FAILURE;
    if (fork_child == 0)
      _exit(0);
    int fork_status = 0;
    if (waitpid(fork_child, &fork_status, 0) != fork_child ||
        !WIFEXITED(fork_status) || WEXITSTATUS(fork_status) != 0)
      return HARNESS_FAILURE;
  }

  fflush(stdout);
  pid_t child = start_vfork_form(form);
  if (child < 0) {
    printf("FAIL: vfork failed (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  if (child == 0) {
    (void)flock(inherited, LOCK_EX);
    _exit(1);
  }
  printf("FAIL: copied vfork child returned from blocking flock (status=%ld)\n",
         (long)child);
  return 1;
}

static int scenario_holder(void) {
  const char *path = lock_path("holder");
  int fd = lock_override != NULL ? open(path, O_RDONLY) : open_lock(path);
  if (fd < 0) {
    printf("FAIL: could not open %s (errno=%d)\n", path, errno);
    return HARNESS_FAILURE;
  }
  errno = 0;
  if (flock(fd, LOCK_EX | LOCK_NB) != 0) {
    printf("FAIL: LOCK_EX was refused (errno=%d)\n", errno);
    return 1;
  }
  printf("flock-holder-acquired\n");
  if (flock(fd, LOCK_UN) != 0) {
    printf("FAIL: LOCK_UN was refused (errno=%d)\n", errno);
    return 1;
  }
  printf("flock-holder-ok\n");
  close(fd);
  return 0;
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IOLBF, 0);
  const char *scenario = (argc > 1) ? argv[1] : "exclusion";
  if (argc > 2) {
    lock_override = argv[2];
  }
  if (strcmp(scenario, "exclusion") == 0) {
    return scenario_exclusion();
  }
  if (strcmp(scenario, "upgrade") == 0) {
    return scenario_upgrade();
  }
  if (strcmp(scenario, "received") == 0) {
    return scenario_received();
  }
  if (strcmp(scenario, "fork-blocking-refusal") == 0) {
    return scenario_fork_blocking_refusal();
  }
  if (strcmp(scenario, "fork-safe-operations") == 0) {
    return scenario_fork_safe_operations();
  }
  if (strcmp(scenario, "fork-vfork-blocking") == 0) {
    return scenario_fork_vfork_blocking();
  }
  if (strcmp(scenario, "failed-clone") == 0) {
    return scenario_failed_clone();
  }
  if (strcmp(scenario, "pidfd-getfd") == 0) {
    return scenario_pidfd_getfd();
  }
  if (strcmp(scenario, "pidfd-getfd-record") == 0) {
    return scenario_pidfd_getfd_record();
  }
  if (strcmp(scenario, "pidfd-getfd-relaxed-refusal") == 0) {
    return scenario_pidfd_getfd_relaxed_refusal();
  }
  if (strcmp(scenario, "sent-after-fork") == 0) {
    return scenario_sent_after_fork(scenario, 0);
  }
  if (strcmp(scenario, "sent-after-fork-mmsg") == 0) {
    return scenario_sent_after_fork(scenario, 1);
  }
  if (strcmp(scenario, "failed-send") == 0) {
    return scenario_failed_send_preserves_state();
  }
  if (strcmp(scenario, "partial-sendmmsg") == 0) {
    return scenario_partial_sendmmsg();
  }
  if (strcmp(scenario, "vfork-no-flock-state") == 0) {
    return scenario_vfork_no_flock_state();
  }
  if (strcmp(scenario, "vfork-upgrade") == 0) {
    return scenario_vfork_upgrade(0, VFORK_SYSCALL);
  }
  if (strcmp(scenario, "vfork-unknown-upgrade") == 0) {
    return scenario_vfork_upgrade(1, VFORK_SYSCALL);
  }
  if (strcmp(scenario, "clone-vfork-upgrade") == 0) {
    return scenario_vfork_upgrade(0, CLONE_VFORK_SYSCALL);
  }
  if (strcmp(scenario, "clone3-vfork-upgrade") == 0) {
    return scenario_vfork_upgrade(0, CLONE3_VFORK_SYSCALL);
  }
  if (strcmp(scenario, "clone-files-process") == 0) {
    return scenario_clone_files_process(CLONE_FILES_SYSCALL);
  }
  if (strcmp(scenario, "clone3-files-process") == 0) {
    return scenario_clone_files_process(CLONE3_FILES_SYSCALL);
  }
  if (strcmp(scenario, "holder") == 0) {
    return scenario_holder();
  }
  printf("FAIL: unknown scenario %s\n", scenario);
  return HARNESS_FAILURE;
}
