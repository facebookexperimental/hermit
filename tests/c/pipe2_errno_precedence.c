/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Linux validates pipe2's arguments in a fixed order, and a guest doing its own
 * argument checking depends on that order:
 *
 *   flags are checked FIRST, before pipefd is touched at all
 *     -> a bad pointer with BAD flags is EINVAL, not EFAULT
 *   only then is pipefd written
 *     -> a bad pointer with GOOD flags is EFAULT
 *
 * Detcore pins the capacity of scheduler-managed pipes. It used to read the
 * caller's pipefd bytes BEFORE injecting pipe2, so that a failed capacity pin
 * could restore them and fabricate a pipe2 error. That pre-call read was the
 * only access on this path to an address Linux had not yet validated, and it
 * broke twice over: it replaced both answers above with the tool's own memory
 * error (measured: EIO for both, so a guest could not tell a bad pointer from
 * bad flags), and on the backends whose guest memory is `LocalMemory` -- an
 * unsafe copy_nonoverlapping that always reports success -- a bad pointer was a
 * hardware fault no error check could catch (measured: DBT SIGSEGV, rc 255).
 *
 * The pre-call read is now GONE, not made best-effort. Nothing in the tool
 * touches pipefd before the kernel does, so Linux alone decides the errno and
 * its ordering above holds unchanged. Everything the tool reads afterwards runs
 * only when pipe2 SUCCEEDED, which proves the address valid because the kernel
 * itself just wrote to it. With no snapshot there is nothing to restore. A pin
 * that cannot be applied is a tool error and the guest does not resume, because
 * an unpinnable pipe means determinism is unavailable. Descriptor cleanup on
 * that failure path is tracked separately by hermit#2533.
 *
 * This probe asserts the precedence directly rather than only asserting that
 * two runs agree: a deterministic WRONG errno is still wrong, and a pure
 * determinism check would pass it.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <unistd.h>

/* Deliberately raw: glibc's pipe2 wrapper may pre-screen arguments, and the
 * point here is what the kernel/tool boundary returns. */
static long raw_pipe2(void* pipefd, int flags) {
  errno = 0;
  return syscall(SYS_pipe2, pipefd, (long)flags);
}

static int failures = 0;

static void expect_errno(
    const char* what,
    void* pipefd,
    int flags,
    int want_errno) {
  long rc = raw_pipe2(pipefd, flags);
  int got = errno;
  if (rc == -1 && got == want_errno) {
    printf("%s: errno=%d ok\n", what, got);
    return;
  }
  fprintf(
      stderr,
      "%s: expected rc=-1 errno=%d, got rc=%ld errno=%d\n",
      what,
      want_errno,
      rc,
      got);
  failures++;
}

int main(int argc, char** argv) {
  /* Optional expected capacity. Passed as a guest arg under Detcore, where the
   * pin makes the value knowable; omitted for a native run, whose capacity is
   * whatever the host default happens to be. Without this the positive control
   * only PRINTED F_GETPIPE_SZ, so deleting the pin outright still passed every
   * registered cell: two runs of an unpinned pipe agree with each other and the
   * errno cases are unaffected. The mechanism this guest exists to protect was
   * therefore uncovered. */
  long expected_capacity = -1;
  if (argc > 1) {
    errno = 0;
    char* end = NULL;
    expected_capacity = strtol(argv[1], &end, 10);
    if (errno != 0 || end == argv[1] || *end != '\0' ||
        expected_capacity <= 0) {
      fprintf(stderr, "usage: %s [expected-capacity-bytes]\n", argv[0]);
      return 2;
    }
  }

  /* A non-null pointer that cannot be written. This is the case that
   * regressed; NULL never did, because a null pipefd was filtered out before
   * the tool touched it at all. Both are asserted so the fix cannot be
   * narrowed to only the null case. */
  void* bad = (void*)1;

  expect_errno("badptr_validflags", bad, 0, EFAULT);
  expect_errno("nullptr_validflags", NULL, 0, EFAULT);

  /* Flags are checked before the pointer, so these stay EINVAL even though the
   * pointer is also unusable. This is the precedence assertion. */
  expect_errno("badptr_allflags", bad, -1, EINVAL);
  expect_errno("badptr_oappend", bad, O_APPEND, EINVAL);

  /* Positive control. When an expected capacity is supplied this ASSERTS the
   * pinned value, so removing or bypassing F_SETPIPE_SZ fails here instead of
   * looking clean. With no argument it only reports, which keeps the native
   * errno bracket runnable on a host with any default. */
  int fds[2] = {-1, -1};
  long rc = raw_pipe2(fds, 0);
  if (rc != 0) {
    fprintf(stderr, "valid_pipe2: expected success, got rc=%ld errno=%d\n", rc,
            errno);
    failures++;
  } else {
    int capacity = fcntl(fds[1], F_GETPIPE_SZ);
    printf("valid_pipe2: ok capacity=%d\n", capacity);
    if (expected_capacity > 0 && (long)capacity != expected_capacity) {
      fprintf(stderr,
              "valid_pipe2: expected pinned capacity %ld, got %d -- the "
              "deterministic pipe-capacity pin is not in effect\n",
              expected_capacity, capacity);
      failures++;
    }
    /* The pair must be usable, not merely returned. */
    if (write(fds[1], "x", 1) != 1) {
      perror("valid_pipe2 write");
      failures++;
    }
    char byte = 0;
    if (read(fds[0], &byte, 1) != 1 || byte != 'x') {
      perror("valid_pipe2 read");
      failures++;
    }
    close(fds[0]);
    close(fds[1]);
  }

  if (failures != 0) {
    fprintf(stderr, "pipe2-errno-precedence: %d case(s) failed\n", failures);
    return 1;
  }
  puts("pipe2-errno-precedence-ok");
  return 0;
}
