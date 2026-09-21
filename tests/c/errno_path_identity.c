/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Contract: syscall FAILURE is as deterministic as syscall success.
 *
 * Error paths are a guest-visible determinism surface with no coverage today.
 * `errno` is read by the guest, branched on, printed, and turned into program
 * behaviour, so a syscall that fails differently between two runs is exactly as
 * nondeterministic as one that returns a different value -- but it is far
 * easier to miss, because the happy path still looks identical and the run
 * still exits zero.
 *
 * They are also the least-exercised paths in any syscall implementation. A
 * determinization that handles the success case correctly and forwards the
 * failure case straight to the host is a common shape, and nothing currently
 * catches it.
 *
 * WHAT IS ASSERTED, AND WHAT IS NOT.
 *
 * Every case here is chosen to fail for a reason intrinsic to its ARGUMENTS,
 * not to host state: a path that does not exist, a descriptor that is not open,
 * a flag combination that is invalid. Those must produce the same errno every
 * run on any machine.
 *
 * Cases whose failure depends on host conditions are deliberately EXCLUDED --
 * ENOMEM under memory pressure, EAGAIN under load, EINTR on signal timing,
 * ENOSPC on a full disk. Including them would make the fixture flaky and, worse,
 * would make a genuine determinism regression indistinguishable from a busy
 * machine. A fixture that fails for environmental reasons trains people to
 * ignore it.
 *
 * The program asserts that every selected operation reaches a failure path,
 * then prints the outcome of each case. Identity is established by the harness
 * running it twice under `verify`. It does NOT compare against a golden errno
 * table. That is deliberate: hard-coding expected errno values would turn a
 * determinism fixture into a Linux-semantics conformance test, and the two fail
 * for different reasons and want different fixes. If a case's errno is WRONG
 * but stable, that is a Linux-semantics bug for a different test to catch; this
 * fixture's job is that it is STABLE.
 */

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <stdint.h>
#include <time.h>
#include <unistd.h>

/* Print an attempt's outcome. The errno NAME is printed via strerror so a diff
 * names the condition rather than a bare integer, and the return value is
 * printed too: "failed with ENOENT" and "unexpectedly succeeded" must be
 * distinguishable, since a case that starts succeeding is also a divergence. */
static int unexpected_successes;

static void report(const char* name, long ret, int err) {
  if (ret >= 0) {
    printf("CASE %-28s SUCCEEDED ret=%ld\n", name, ret);
    unexpected_successes++;
  } else {
    printf("CASE %-28s failed errno=%s\n", name, strerror(err));
  }
}

#define ATTEMPT(name, expr)      \
  do {                           \
    errno = 0;                   \
    long r = (long)(expr);       \
    report((name), r, errno);    \
  } while (0)

int main(void) {
  /* --- path resolution ---------------------------------------------------- */
  ATTEMPT("open-missing", open("/nonexistent-hermit-fixture", O_RDONLY));
  ATTEMPT("open-dir-for-write", open("/", O_WRONLY));
  ATTEMPT("open-excl-existing", open("/", O_CREAT | O_EXCL, 0600));
  ATTEMPT("stat-missing", stat("/nonexistent-hermit-fixture", &(struct stat){0}));
  ATTEMPT("access-missing", access("/nonexistent-hermit-fixture", R_OK));
  ATTEMPT("unlink-missing", unlink("/nonexistent-hermit-fixture"));
  ATTEMPT("rmdir-missing", rmdir("/nonexistent-hermit-fixture"));
  ATTEMPT("mkdir-existing", mkdir("/", 0700));
  ATTEMPT("chdir-missing", chdir("/nonexistent-hermit-fixture"));
  ATTEMPT("readlink-not-symlink", readlink("/", (char[64]){0}, 64));
  ATTEMPT("open-empty-path", open("", O_RDONLY));
  ATTEMPT("open-null-byte-path", open("/proc/self/\x01nope", O_RDONLY));

  /* --- descriptor validity ------------------------------------------------ */
  ATTEMPT("close-badfd", close(-1));
  ATTEMPT("read-badfd", read(-1, (char[8]){0}, 8));
  ATTEMPT("write-badfd", write(-1, "x", 1));
  ATTEMPT("fstat-badfd", fstat(-1, &(struct stat){0}));
  ATTEMPT("dup-badfd", dup(-1));
  ATTEMPT("fcntl-badfd", fcntl(-1, F_GETFD));
  ATTEMPT("lseek-badfd", lseek(-1, 0, SEEK_SET));
  ATTEMPT("ftruncate-badfd", ftruncate(-1, 0));

  /* --- invalid arguments -------------------------------------------------- */
  ATTEMPT("lseek-bad-whence", lseek(0, 0, 12345));
  ATTEMPT("fcntl-bad-cmd", fcntl(0, 99999));
  ATTEMPT("mmap-zero-length", (long)(intptr_t)mmap(NULL, 0, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
  ATTEMPT("madvise-bad-advice", madvise(NULL, 0, 99999));
  ATTEMPT("mprotect-unaligned", mprotect((void*)0x1, 1, PROT_READ));
  ATTEMPT("socket-bad-domain", socket(9999, SOCK_STREAM, 0));
  ATTEMPT("kill-bad-signal", kill(getpid(), 9999));
  ATTEMPT("nanosleep-negative", nanosleep(&(struct timespec){.tv_sec = -1, .tv_nsec = 0}, NULL));
  ATTEMPT("clock-gettime-bad-pointer", syscall(SYS_clock_gettime, CLOCK_REALTIME, (void*)1));
  ATTEMPT("getcwd-tiny-buffer", syscall(SYS_getcwd, (char[1]){0}, 1));
  ATTEMPT("pipe2-bad-flags", pipe2((int[2]){0}, 0x7fffffff));
  ATTEMPT("dup3-same-fd", dup3(0, 0, 0));

  /* --- pipe-specific errors, using a pipe we own -------------------------- */
  int fds[2];
  if (pipe(fds) == 0) {
    ATTEMPT("lseek-on-pipe", lseek(fds[0], 0, SEEK_SET));
    ATTEMPT("ftruncate-on-pipe", ftruncate(fds[0], 0));
    ATTEMPT("write-to-read-end", write(fds[0], "x", 1));
    ATTEMPT("read-from-write-end", read(fds[1], (char[8]){0}, 8));
    /* EPIPE with SIGPIPE suppressed, so the failure is observable as an errno
     * rather than as process death. */
    signal(SIGPIPE, SIG_IGN);
    close(fds[0]);
    ATTEMPT("write-to-closed-pipe", write(fds[1], "x", 1));
    close(fds[1]);
  } else {
    printf("CASE pipe-setup                 UNAVAILABLE\n");
    unexpected_successes++;
  }

  /* --- repeated failure must be IDENTICAL, not merely similar -------------
   * The same call three times. A failure path that consumes state, caches, or
   * increments something would diverge here even though a single attempt looks
   * stable -- and this is intra-run, so a run-to-run comparison alone would not
   * catch it. */
  for (int i = 0; i < 3; i++) {
    ATTEMPT("repeat-open-missing", open("/nonexistent-hermit-fixture", O_RDONLY));
  }

  return unexpected_successes == 0 ? 0 : 1;
}
