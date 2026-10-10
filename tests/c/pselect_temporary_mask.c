/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * pselect6 sleeps under its temporary signal mask for the whole call, as
 * Linux installs it (https://github.com/rrnewton/hermit/issues/3991).
 *
 * The main thread catches SIGALRM and blocks in pselect6 on an empty pipe with
 * a NULL timeout. A 100 ms ITIMER_REAL fires during the wait. A second thread,
 * which inherits a mask that blocks SIGALRM, so that only the waiter can take
 * the signal, fills the pipe at 300 ms.
 *
 *   blocks    SIGALRM is unblocked in the waiter's own mask and blocked by the
 *             call's temporary mask. Linux does not end the call for it: it
 *             returns 1 when the pipe is filled, and the handler runs after.
 *   unblocks  SIGALRM is blocked in the waiter's own mask and unblocked only by
 *             the call's temporary mask, the usual way to wait for a signal
 *             without a race. Linux ends the call with EINTR and runs the
 *             handler before pselect returns.
 *   unblocks-timed
 *             unblocks, through the raw system call with a 5 s timeout, which
 *             Linux updates to the time remaining when the signal ends the
 *             call. Hermit writes the remaining virtual time; the guest prints
 *             it, so strict verification sees any host time written there.
 *   unblocks-rewritten
 *             unblocks, with a second thread, which blocks SIGALRM, adding
 *             SIGALRM to the call's mask buffer at 50 ms, while the call
 *             sleeps. Linux copied the mask at entry, so the rewrite changes
 *             nothing: EINTR with the handler run before return (review of
 *             https://github.com/rrnewton/hermit/pull/4051).
 *
 * The guest checks its result against Linux's and exits nonzero on a
 * mismatch.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <sys/select.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t alarms;
static int pipe_fds[2];
static sigset_t during;

static void count_alarm(int signal) {
  (void)signal;
  alarms++;
}

static void* rewrite_mask_later(void* arg) {
  (void)arg;
  usleep(50000);
  sigaddset(&during, SIGALRM);
  return NULL;
}

static void* fill_pipe_later(void* arg) {
  (void)arg;
  for (int i = 0; i < 1000; i++) {
    sched_yield();
  }
  usleep(300000);
  if (write(pipe_fds[1], "x", 1) != 1) {
    perror("write");
  }
  return NULL;
}

int main(int argc, char** argv) {
  if (argc != 2 ||
      (strcmp(argv[1], "blocks") != 0 && strcmp(argv[1], "unblocks") != 0 &&
       strcmp(argv[1], "unblocks-timed") != 0 &&
       strcmp(argv[1], "unblocks-rewritten") != 0)) {
    fprintf(
        stderr, "usage: %s blocks|unblocks|unblocks-timed|unblocks-rewritten\n",
        argv[0]);
    return 2;
  }
  int blocks = strcmp(argv[1], "blocks") == 0;
  int timed = strcmp(argv[1], "unblocks-timed") == 0;
  int rewritten = strcmp(argv[1], "unblocks-rewritten") == 0;
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = count_alarm;
  if (sigaction(SIGALRM, &action, NULL) != 0 || pipe(pipe_fds) != 0) {
    perror("setup");
    return 1;
  }
  sigset_t alarm_set;
  sigemptyset(&alarm_set);
  sigaddset(&alarm_set, SIGALRM);
  if (pthread_sigmask(SIG_BLOCK, &alarm_set, NULL) != 0) {
    perror("pthread_sigmask");
    return 1;
  }
  sigemptyset(&during);
  if (blocks) {
    sigaddset(&during, SIGALRM);
  }
  pthread_t writer;
  if (pthread_create(&writer, NULL, fill_pipe_later, NULL) != 0) {
    perror("pthread_create");
    return 1;
  }
  pthread_t rewriter;
  if (rewritten && pthread_create(&rewriter, NULL, rewrite_mask_later, NULL) != 0) {
    perror("pthread_create");
    return 1;
  }
  if (blocks && pthread_sigmask(SIG_UNBLOCK, &alarm_set, NULL) != 0) {
    perror("pthread_sigmask");
    return 1;
  }
  struct itimerval alarm_in = {{0, 0}, {0, 100000}};
  if (setitimer(ITIMER_REAL, &alarm_in, NULL) != 0) {
    perror("setitimer");
    return 1;
  }
  fd_set read_set;
  FD_ZERO(&read_set);
  FD_SET(pipe_fds[0], &read_set);
  struct timespec remaining = {5, 0};
  errno = 0;
  int result;
  if (timed) {
    uint64_t empty_mask = 0;
    struct {
      const void* mask;
      size_t size;
    } wrapper = {&empty_mask, sizeof empty_mask};
    result = (int)syscall(
        SYS_pselect6, pipe_fds[0] + 1, &read_set, NULL, NULL, &remaining,
        &wrapper);
  } else {
    result = pselect(pipe_fds[0] + 1, &read_set, NULL, NULL, NULL, &during);
  }
  int result_errno = errno;
  int ready = result > 0 && FD_ISSET(pipe_fds[0], &read_set);
  int alarms_at_return = (int)alarms;
  if (pthread_join(writer, NULL) != 0 ||
      (rewritten && pthread_join(rewriter, NULL) != 0)) {
    perror("pthread_join");
    return 1;
  }
  if (!blocks) {
    /* Any SIGALRM still pending is delivered here, after the call. */
    pthread_sigmask(SIG_UNBLOCK, &alarm_set, NULL);
  }
  printf(
      "pselect-temporary-mask mode=%s result=%d errno=%d ready=%d "
      "alarms_at_return=%d alarms=%d remaining=%ld.%09ld\n",
      argv[1],
      result,
      result < 0 ? result_errno : 0,
      ready,
      alarms_at_return,
      (int)alarms,
      (long)remaining.tv_sec,
      remaining.tv_nsec);
  int remaining_ok = !timed ||
      (remaining.tv_sec >= 0 && remaining.tv_sec < 5 && remaining.tv_nsec >= 0 &&
       remaining.tv_nsec < 1000000000);
  int ok = blocks
      ? (result == 1 && ready && alarms == 1)
      : (result == -1 && result_errno == EINTR && alarms_at_return == 1 &&
         alarms == 1 && remaining_ok);
  if (!ok) {
    fprintf(stderr, "pselect-temporary-mask %s mismatch\n", argv[1]);
    return 1;
  }
  return 0;
}
