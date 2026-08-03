/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Backend-parity contract: signal-disposition state round-trips are
 * deterministic and independent of host process state.
 *
 * The guest installs and queries signal handlers (rt_sigaction) and the blocked
 * signal mask (rt_sigprocmask) and reads back exactly what it set. Every checked
 * value depends only on the program's own prior calls, not on the host's
 * inherited disposition or mask, so the observable result is byte-identical
 * across repeated runs and across the ptrace, DBI, and KVM backends.
 *
 * This is deliberately pure signal *state*: no signal is ever raised or
 * delivered, so it exercises no handler frame, no cross-thread delivery, and no
 * scheduler wakeup. It uses a single thread and no blocking I/O, so it is safe
 * under the DBI no-preemption scheduler. It also avoids rt_sigpending, which the
 * KVM ElfExecutor personality does not implement.
 *
 * Feature-test macros come from the compiler flags (-D_GNU_SOURCE); do not
 * redefine them here or -Werror rejects the redefinition.
 */

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>

static volatile sig_atomic_t unused_marker;
static void handler(int signo) {
  unused_marker = signo;
}

static int fail(const char *what) {
  fprintf(stderr, "signal-disposition: %s failed: %s\n", what, strerror(errno));
  return 1;
}

int main(void) {
  /* SIG_IGN disposition round-trip on SIGUSR1. */
  struct sigaction want;
  memset(&want, 0, sizeof(want));
  want.sa_handler = SIG_IGN;
  sigemptyset(&want.sa_mask);
  if (sigaction(SIGUSR1, &want, NULL) != 0)
    return fail("sigaction set SIG_IGN");
  struct sigaction got;
  memset(&got, 0, sizeof(got));
  if (sigaction(SIGUSR1, NULL, &got) != 0)
    return fail("sigaction get SIG_IGN");
  if (got.sa_handler != SIG_IGN) {
    fprintf(stderr, "signal-disposition: SIGUSR1 not SIG_IGN\n");
    return 1;
  }

  /* Real handler + SA_RESTART flag round-trip on SIGUSR2, including the
   * signal added to the handler's own during-delivery block mask. */
  memset(&want, 0, sizeof(want));
  want.sa_handler = handler;
  want.sa_flags = SA_RESTART;
  sigemptyset(&want.sa_mask);
  sigaddset(&want.sa_mask, SIGHUP);
  if (sigaction(SIGUSR2, &want, NULL) != 0)
    return fail("sigaction set handler");
  memset(&got, 0, sizeof(got));
  if (sigaction(SIGUSR2, NULL, &got) != 0)
    return fail("sigaction get handler");
  if (got.sa_handler != handler) {
    fprintf(stderr, "signal-disposition: SIGUSR2 handler mismatch\n");
    return 1;
  }
  if ((got.sa_flags & SA_RESTART) == 0) {
    fprintf(stderr, "signal-disposition: SA_RESTART not preserved\n");
    return 1;
  }
  if (!sigismember(&got.sa_mask, SIGHUP)) {
    fprintf(stderr, "signal-disposition: handler mask missing SIGHUP\n");
    return 1;
  }

  /* Blocked-mask round-trip: block SIGUSR2, confirm, then unblock and confirm. */
  sigset_t block, current;
  sigemptyset(&block);
  sigaddset(&block, SIGUSR2);
  if (sigprocmask(SIG_BLOCK, &block, NULL) != 0)
    return fail("sigprocmask block");
  if (sigprocmask(SIG_BLOCK, NULL, &current) != 0)
    return fail("sigprocmask query blocked");
  if (!sigismember(&current, SIGUSR2)) {
    fprintf(stderr, "signal-disposition: SIGUSR2 not blocked\n");
    return 1;
  }
  if (sigprocmask(SIG_UNBLOCK, &block, NULL) != 0)
    return fail("sigprocmask unblock");
  if (sigprocmask(SIG_BLOCK, NULL, &current) != 0)
    return fail("sigprocmask query unblocked");
  if (sigismember(&current, SIGUSR2)) {
    fprintf(stderr, "signal-disposition: SIGUSR2 still blocked\n");
    return 1;
  }

  /* Restore SIGUSR1 to default and read it back. */
  memset(&want, 0, sizeof(want));
  want.sa_handler = SIG_DFL;
  sigemptyset(&want.sa_mask);
  if (sigaction(SIGUSR1, &want, NULL) != 0)
    return fail("sigaction restore SIG_DFL");
  memset(&got, 0, sizeof(got));
  if (sigaction(SIGUSR1, NULL, &got) != 0)
    return fail("sigaction get SIG_DFL");
  if (got.sa_handler != SIG_DFL) {
    fprintf(stderr, "signal-disposition: SIGUSR1 not SIG_DFL\n");
    return 1;
  }

  puts("signal-disposition-ok");
  return 0;
}
