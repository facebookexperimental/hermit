/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * The main thread reads a pipe whose only writer first waits on a futex that
 * no thread ever wakes: natively both threads wait forever.
 */

#define _GNU_SOURCE
#include <linux/futex.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

static uint32_t never_woken;
static int pipe_fds[2];

static void close_handler_progress(void) {
  /* pthread_create installs libc's caught RT handlers too. This fixture's
   * deadlock requires that none can interrupt either wait. Use the kernel
   * mask: glibc's sigprocmask deliberately leaves signals 32 and 33 alone.
   * Linux v6.12 kernel/signal.c:3202-3221 removes only SIGKILL and SIGSTOP.
   * SIGTERM stays default and unblocked for the native timeout control.
   * SIGSTKFLT stays default too: Hermit reserves it for preemption and
   * always leaves it unblocked. Neither default action can write the pipe. */
  struct sigaction terminate = {0};
  terminate.sa_handler = SIG_DFL;
  uint64_t mask = UINT64_MAX & ~(UINT64_C(1) << (SIGKILL - 1)) &
                  ~(UINT64_C(1) << (SIGSTOP - 1)) &
                  ~(UINT64_C(1) << (SIGSTKFLT - 1)) &
                  ~(UINT64_C(1) << (SIGTERM - 1));
  uint64_t observed = 0;
  if (sigaction(SIGTERM, &terminate, NULL) != 0 ||
      sigaction(SIGSTKFLT, &terminate, NULL) != 0 ||
      syscall(SYS_rt_sigprocmask, SIG_SETMASK, &mask, NULL, sizeof(mask)) != 0 ||
      syscall(SYS_rt_sigprocmask, SIG_SETMASK, NULL, &observed,
              sizeof(observed)) != 0 ||
      observed != mask) {
    fprintf(stderr, "could not close handler progress: wanted=%016llx got=%016llx\n",
            (unsigned long long)mask, (unsigned long long)observed);
    _exit(2);
  }
}

static void* writer(void* unused) {
  (void)unused;
  close_handler_progress();
  /* Waits while the word is 0; nothing changes it or wakes it. */
  syscall(SYS_futex, &never_woken, FUTEX_WAIT_PRIVATE, 0, NULL, NULL, 0);
  if (write(pipe_fds[1], "x", 1) != 1) {
    perror("write");
  }
  return NULL;
}

int main(void) {
  pthread_t thread;
  char byte;

  if (pipe(pipe_fds) != 0) {
    perror("pipe");
    return 1;
  }
  if (pthread_create(&thread, NULL, writer, NULL) != 0) {
    fputs("pthread_create failed\n", stderr);
    return 1;
  }
  close_handler_progress();
  printf("ready\n");
  fflush(stdout);
  ssize_t got = read(pipe_fds[0], &byte, 1);
  printf("UNREACHABLE: read returned %zd\n", got);
  return 0;
}
