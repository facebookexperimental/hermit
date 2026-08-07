/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Real fork+execve pipeline with output capture and exit-status propagation.
 *
 * The parent creates a pipe, forks, and the child REPLACES ITSELF via execve
 * with /bin/echo writing a fixed string to the inherited pipe write end. The
 * parent drains to EOF, checksums the bytes, and reaps, reporting the child's
 * exit status.
 *
 * Distinct from the existing coverage: pipe_ipc.c forks but never execs (so the
 * child keeps the parent's image), and vforkExec.c execs but does not capture
 * output through a pipe. This guest is the composition -- fd inheritance ACROSS
 * an execve image replacement, which is where a backend that rebuilds the fd
 * table on exec would diverge.
 *
 * Deterministic by construction: the only observables are a byte count, a sum
 * checksum, and an exit status. No pid, timestamp, address, or ordering.
 */

/* The e2e harness compiles with -std=c11, which hides POSIX declarations. */
#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

static void fail(const char *message) {
  fprintf(stderr, "%s: %s\n", message, strerror(errno));
  exit(1);
}

/*
 * The e2e harness has no golden-output field: its verify oracle is exit status
 * plus cross-attempt determinism. A deterministically wrong stdout therefore
 * passes unnoticed unless the guest checks itself, so every invariant below is
 * asserted rather than merely printed.
 */
static int violations;

static void expect(const char *name, long long observed, long long wanted) {
  if (observed != wanted) {
    fprintf(stderr, "invariant %s: observed %lld, wanted %lld\n", name, observed,
            wanted);
    violations++;
  }
}

int main(void) {
  int fds[2];
  if (pipe(fds) != 0)
    fail("pipe");

  pid_t child = fork();
  if (child < 0)
    fail("fork");

  if (child == 0) {
    if (close(fds[0]) != 0)
      _exit(101);
    /* Redirect stdout onto the pipe, then replace the image entirely. */
    if (dup2(fds[1], STDOUT_FILENO) < 0)
      _exit(102);
    if (close(fds[1]) != 0)
      _exit(103);
    char *const argv[] = {(char *)"/bin/echo", (char *)"-n", (char *)"hermit-fork-exec", NULL};
    char *const envp[] = {NULL};
    execve("/bin/echo", argv, envp);
    _exit(104); /* only reached if execve failed */
  }

  if (close(fds[1]) != 0)
    fail("close write end");

  size_t bytes = 0;
  unsigned long checksum = 0;
  uint8_t buffer[64];
  for (;;) {
    ssize_t n = read(fds[0], buffer, sizeof(buffer));
    if (n < 0) {
      if (errno == EINTR)
        continue;
      fail("read");
    }
    if (n == 0)
      break;
    for (ssize_t i = 0; i < n; ++i)
      checksum += buffer[i];
    bytes += (size_t)n;
  }
  if (close(fds[0]) != 0)
    fail("close read end");

  int status = 0;
  if (waitpid(child, &status, 0) < 0)
    fail("waitpid");

  expect("bytes", (long long)bytes, 16);
  expect("checksum", (long long)checksum, 1594);
  expect("exited", WIFEXITED(status) ? 1 : 0, 1);
  expect("code", WIFEXITED(status) ? WEXITSTATUS(status) : -1, 0);
  printf("forkexec bytes=%zu checksum=%lu exited=%d code=%d\n", bytes, checksum,
         WIFEXITED(status) ? 1 : 0, WIFEXITED(status) ? WEXITSTATUS(status) : -1);
  return violations == 0 ? 0 : 1;
}
