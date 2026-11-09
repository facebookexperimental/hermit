/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A death test as googletest's "fast" style runs one. The parent opens a status
// pipe and forks. The child runs the statement, which must kill it, and the
// parent reads the pipe (end-of-file: the child died without reporting) and
// waits for the child. The statement is a fatal assertion that looks for a
// debugger: if /proc/self/status names a tracer, it breaks into the debugger
// with raise(SIGTRAP); otherwise it calls abort(). Run outside a debugger, the
// child dies of SIGABRT.
//
// Under Hermit the child read TracerPid 1 and raised SIGTRAP. The ptrace
// backend used not to deliver a SIGTRAP a guest sends itself, so a statement
// that went on to wait hung its parent for good; this child writes
// googletest's "lived" byte and exits instead, so that failure shows at once
// rather than as a hang. Now that the backend delivers it, a child that reads
// TracerPid 1 dies of SIGTRAP (signal 5) rather than SIGABRT. Either way the
// output differs from a native run's.

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <unistd.h>

static long tracer_pid(void) {
  FILE* file = fopen("/proc/self/status", "r");
  char line[256];
  long tracer = -1;
  while (file != NULL && fgets(line, sizeof(line), file) != NULL) {
    if (sscanf(line, "TracerPid:\t%ld", &tracer) == 1) {
      break;
    }
  }
  if (file != NULL) {
    fclose(file);
  }
  return tracer;
}

// The statement: a fatal assertion that breaks into a debugger when there is
// one.
static void fatal_assertion(void) {
  if (tracer_pid() != 0) {
    raise(SIGTRAP);
    return;
  }
  abort();
}

int main(void) {
  printf("TracerPid %ld\n", tracer_pid());
  fflush(stdout);

  // A soft core limit of 1 writes no core, even through a core_pattern pipe.
  struct rlimit core;
  if (getrlimit(RLIMIT_CORE, &core) == 0) {
    core.rlim_cur = core.rlim_max < 1 ? core.rlim_max : 1;
    (void)setrlimit(RLIMIT_CORE, &core);
  }

  int status_pipe[2];
  if (pipe(status_pipe) != 0) {
    return 1;
  }
  pid_t child = fork();
  if (child < 0) {
    return 1;
  }
  if (child == 0) {
    close(status_pipe[0]);
    fatal_assertion();
    // The statement returned: googletest's DeathTest::Abort writes 'L'.
    (void)!write(status_pipe[1], "L", 1);
    _exit(1);
  }
  close(status_pipe[1]);
  char byte;
  ssize_t read_bytes;
  do {
    read_bytes = read(status_pipe[0], &byte, 1);
  } while (read_bytes < 0 && errno == EINTR);
  int status = 0;
  if (waitpid(child, &status, 0) != child) {
    return 1;
  }
  printf("status byte: %s\n", read_bytes == 0 ? "none" : "L");
  if (WIFSIGNALED(status)) {
    printf("child: killed by signal %d\n", WTERMSIG(status));
  } else {
    printf("child: exited %d\n", WEXITSTATUS(status));
  }
  return read_bytes == 0 && WIFSIGNALED(status) && WTERMSIG(status) == SIGABRT
      ? 0
      : 1;
}
