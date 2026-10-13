/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * epoll_pwait sleeps under its temporary signal mask for the whole call, as
 * Linux installs it (https://github.com/rrnewton/hermit/issues/4041).
 *
 * The main thread catches SIGALRM and blocks in epoll_pwait, with no timeout,
 * on an epoll instance that watches an empty pipe. A 100 ms ITIMER_REAL fires
 * during the wait. A second thread, which inherits a mask that blocks SIGALRM,
 * so that only the waiter can take the signal, fills the pipe at 300 ms.
 *
 *   blocks    SIGALRM is unblocked in the waiter's own mask and blocked by the
 *             call's temporary mask. Linux does not end the call for it: it
 *             returns 1 when the pipe is filled, and the handler runs after,
 *             when the pipe is ready.
 *   blocks-early
 *             blocks, with a 10 us timer, which fires as the call starts.
 *             Linux still keeps it pending for the whole call.
 *   unblocks  SIGALRM is blocked in the waiter's own mask and unblocked only by
 *             the call's temporary mask, the usual way to wait for a signal
 *             without a race. Linux ends the call with EINTR and runs the
 *             handler before epoll_pwait returns, while the pipe is still
 *             empty.
 *   unblocks-timed
 *             unblocks, with a 5 s timeout instead of none.
 *   unblocks-rewritten
 *             unblocks, with a second thread, which blocks SIGALRM, adding
 *             SIGALRM to the call's mask buffer at 50 ms, while the call
 *             sleeps. Linux copied the mask at entry, so the rewrite changes
 *             nothing: EINTR with the handler run before return.
 *   admits    SIGALRM is unblocked in both the waiter's own mask and the
 *             call's. Linux ends the call with EINTR after the handler ran.
 *   pending-unblocks
 *             unblocks, with SIGALRM raised before the call, while the
 *             waiter's own mask blocks it, instead of a timer. Linux ends the
 *             call with EINTR and runs the handler before it returns.
 *   chld-unblocks
 *             unblocks, for SIGCHLD instead of SIGALRM: a child exits at
 *             100 ms, and only the call's mask lets its SIGCHLD in. Linux
 *             ends the call with EINTR and runs the handler before it returns.
 *   timeout   no signal and a 50 ms timeout: Linux returns 0 after at least
 *             50 ms.
 *
 * The guest checks its result against Linux's and exits nonzero on a
 * mismatch.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t alarms;
static volatile sig_atomic_t children;
/* Whether the pipe was ready when the handler ran, or -1 before it ran. */
static volatile sig_atomic_t ready_in_handler = -1;
static int pipe_fds[2];
static sigset_t during;

static void count_alarm(int signal) {
  (void)signal;
  struct pollfd read_end = {pipe_fds[0], POLLIN, 0};
  ready_in_handler = poll(&read_end, 1, 0);
  alarms++;
}

static void count_child(int signal) {
  (void)signal;
  children++;
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
      (strcmp(argv[1], "blocks") != 0 && strcmp(argv[1], "blocks-early") != 0 &&
       strcmp(argv[1], "unblocks") != 0 &&
       strcmp(argv[1], "unblocks-timed") != 0 &&
       strcmp(argv[1], "unblocks-rewritten") != 0 &&
       strcmp(argv[1], "admits") != 0 &&
       strcmp(argv[1], "pending-unblocks") != 0 &&
       strcmp(argv[1], "chld-unblocks") != 0 &&
       strcmp(argv[1], "timeout") != 0)) {
    fprintf(
        stderr,
        "usage: %s blocks|blocks-early|unblocks|unblocks-timed|"
        "unblocks-rewritten|admits|pending-unblocks|chld-unblocks|timeout\n",
        argv[0]);
    return 2;
  }
  int early = strcmp(argv[1], "blocks-early") == 0;
  int blocks = early || strcmp(argv[1], "blocks") == 0;
  int timed = strcmp(argv[1], "unblocks-timed") == 0;
  int rewritten = strcmp(argv[1], "unblocks-rewritten") == 0;
  int admits = strcmp(argv[1], "admits") == 0;
  int pending = strcmp(argv[1], "pending-unblocks") == 0;
  int chld = strcmp(argv[1], "chld-unblocks") == 0;
  int timeout = strcmp(argv[1], "timeout") == 0;
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = count_alarm;
  struct sigaction child_action;
  memset(&child_action, 0, sizeof child_action);
  child_action.sa_handler = count_child;
  if (sigaction(SIGALRM, &action, NULL) != 0 ||
      sigaction(SIGCHLD, &child_action, NULL) != 0 || pipe(pipe_fds) != 0) {
    perror("setup");
    return 1;
  }
  int epoll_fd = epoll_create1(0);
  struct epoll_event watch = {.events = EPOLLIN, .data.fd = pipe_fds[0]};
  if (epoll_fd < 0 ||
      epoll_ctl(epoll_fd, EPOLL_CTL_ADD, pipe_fds[0], &watch) != 0) {
    perror("epoll setup");
    return 1;
  }
  sigset_t alarm_set;
  sigemptyset(&alarm_set);
  sigaddset(&alarm_set, SIGALRM);
  /* SIGCHLD is process-directed, so the writer thread must block it too for
   * only the waiter to take it. */
  sigset_t child_set;
  sigemptyset(&child_set);
  sigaddset(&child_set, SIGCHLD);
  if (pthread_sigmask(SIG_BLOCK, &alarm_set, NULL) != 0 ||
      pthread_sigmask(SIG_BLOCK, &child_set, NULL) != 0) {
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
  if (rewritten &&
      pthread_create(&rewriter, NULL, rewrite_mask_later, NULL) != 0) {
    perror("pthread_create");
    return 1;
  }
  if ((blocks || admits) &&
      pthread_sigmask(SIG_UNBLOCK, &alarm_set, NULL) != 0) {
    perror("pthread_sigmask");
    return 1;
  }
  /* Under Hermit, virtual time places the 10 us expiration in the call's
   * first turn. Natively it usually fires inside the call too, but Linux does
   * not promise that: fired before the call, the handler would see an empty
   * pipe. */
  struct itimerval alarm_in = {{0, 0}, {0, early ? 10 : 100000}};
  pid_t child = -1;
  if (pending) {
    raise(SIGALRM);
  } else if (chld) {
    child = fork();
    if (child == 0) {
      usleep(100000);
      _exit(0);
    }
    if (child < 0) {
      perror("fork");
      return 1;
    }
  } else if (!timeout && setitimer(ITIMER_REAL, &alarm_in, NULL) != 0) {
    perror("setitimer");
    return 1;
  }
  struct timespec started;
  clock_gettime(CLOCK_MONOTONIC, &started);
  struct epoll_event ready_event;
  errno = 0;
  int result = epoll_pwait(
      epoll_fd, &ready_event, 1, timed ? 5000 : (timeout ? 50 : -1), &during);
  struct timespec ended;
  clock_gettime(CLOCK_MONOTONIC, &ended);
  long elapsed_ms = (ended.tv_sec - started.tv_sec) * 1000 +
      (ended.tv_nsec - started.tv_nsec) / 1000000;
  int children_at_return = (int)children;
  int result_errno = errno;
  int ready = result == 1 && ready_event.data.fd == pipe_fds[0];
  int alarms_at_return = (int)alarms;
  if (pthread_join(writer, NULL) != 0 ||
      (rewritten && pthread_join(rewriter, NULL) != 0)) {
    perror("pthread_join");
    return 1;
  }
  if (!blocks && !admits) {
    /* Any SIGALRM still pending is delivered here, after the call. */
    pthread_sigmask(SIG_UNBLOCK, &alarm_set, NULL);
  }
  if (chld && waitpid(child, NULL, 0) != child) {
    perror("waitpid");
    return 1;
  }
  printf(
      "epoll-pwait-temporary-mask mode=%s result=%d errno=%d ready=%d "
      "alarms_at_return=%d alarms=%d ready_in_handler=%d "
      "children_at_return=%d\n",
      argv[1],
      result,
      result < 0 ? result_errno : 0,
      ready,
      alarms_at_return,
      (int)alarms,
      (int)ready_in_handler,
      children_at_return);
  int ok;
  if (blocks) {
    ok = result == 1 && ready && alarms == 1 && ready_in_handler == 1;
  } else if (chld) {
    ok = result == -1 && result_errno == EINTR && children_at_return == 1 &&
        alarms == 0;
  } else if (timeout) {
    /* The elapsed time is not printed: natively it varies. */
    ok = result == 0 && alarms == 0 && elapsed_ms >= 50 && elapsed_ms < 5000;
    if (!ok) {
      fprintf(stderr, "timeout after %ld ms\n", elapsed_ms);
    }
  } else {
    ok = result == -1 && result_errno == EINTR && alarms_at_return == 1 &&
        alarms == 1 && ready_in_handler == 0;
  }
  if (!ok) {
    fprintf(stderr, "epoll-pwait-temporary-mask %s mismatch\n", argv[1]);
    return 1;
  }
  return 0;
}
