/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Regression guest for BOTH `epoll_pwait` scheduler defects in #1850. It takes
 * one optional argument selecting the scenario:
 *
 *   (none)     zero-timeout polling loop must not STARVE its producer.
 *   blocking   infinite-timeout wait must not DEADLOCK the scheduler.
 *
 * The two are different failure modes with different causes and they need
 * separate brackets. The zero-timeout case spins and is rescued by ordinary
 * timer preemption unless preemption is disabled, so its test must pass
 * `--max-timeslice=disabled` to detect anything. The blocking case is stuck
 * inside the kernel where no preemption can reach it, so it reproduces under
 * plain `--strict` and needs no special flag.
 *
 * The blocking scenario is the defect this PR is named for -- routing a
 * NULL-sigmask `epoll_pwait` away from inject-and-block. It was previously
 * unbracketed: reverting `handle_epoll_pwait` to its pre-#1850 body left the
 * entire suite green, including this file's zero-timeout tests, so nothing in
 * the repository detected the headline regression.
 *
 * A zero-timeout `epoll_pwait` cannot block, so it is tempting to inject it
 * straight into Linux with no scheduler interaction. Under sequentialized
 * threads that is a starvation bug: the polling thread never requests a
 * resource, so it is never descheduled between preemptions, and the worker
 * thread it is waiting on never gets the single logical CPU. `handle_poll`
 * has always taken a scheduler turn for zero-timeout calls for exactly this
 * reason; `handle_internal_epoll_pwait` must do the same.
 *
 * The main thread polls an eventfd with a zero timeout and spins until the
 * worker publishes. The worker does a little work, publishes a value, then
 * signals the eventfd. If the poller yields a turn, the worker runs and the
 * program prints its success line. If the poller monopolizes the CPU, this
 * hangs and the harness's `timeout` kills it (exit 124) -- which is the
 * regression signal, not a flaky failure.
 *
 * The syscall is issued directly rather than through glibc so the test pins
 * `epoll_pwait` with a NULL sigmask specifically: that is the exact routing
 * this regression concerns, and glibc is free to implement `epoll_wait` via
 * `epoll_pwait` or `epoll_pwait2` depending on version and kernel.
 *
 * Success is printing "epoll-pwait-zero-timeout-progress-ok <value>" and
 * exiting 0. Failure is a hang.
 */

#include <errno.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#define PUBLISHED_VALUE 0x5eed1234ULL

static atomic_ullong g_value = 0;
static int g_event_fd = -1;

/* Raw epoll_pwait with an explicitly NULL sigmask. */
static int epoll_pwait_zero_timeout(int epfd, struct epoll_event* events, int maxevents) {
  return (int)syscall(SYS_epoll_pwait, epfd, events, maxevents, /*timeout=*/0, /*sigmask=*/NULL,
                      /*sigsetsize=*/(size_t)8);
}

static void* worker(void* arg) {
  (void)arg;
  /* A little work, so the poller genuinely has to yield rather than winning a
   * race by luck on the very first iteration. */
  unsigned long long acc = 0;
  for (unsigned long long i = 0; i < 100000ULL; i++) {
    acc += i;
  }
  atomic_store(&g_value, PUBLISHED_VALUE + (acc & 0ULL));

  uint64_t one = 1;
  if (write(g_event_fd, &one, sizeof(one)) != (ssize_t)sizeof(one)) {
    perror("write eventfd");
    _exit(1);
  }
  return NULL;
}

/* ---- Mode 2: the INFINITE-timeout deadlock, i.e. the defect #1850 is named
 * after. ----
 *
 * A blocking `epoll_pwait` that is injected raw and waited on holds the
 * scheduler turn inside the kernel, while the only thread that could satisfy
 * the wait sits in the run queue waiting for a turn that never comes. Nothing
 * breaks the cycle: unlike the zero-timeout case, timer preemption cannot
 * rescue it, because the waiter is blocked in the kernel rather than spinning.
 *
 * THE WORKER'S SLEEP IS LOAD-BEARING, not padding. Without it the worker wins
 * the race, writes before the main thread ever reaches `epoll_pwait`, and the
 * call returns immediately without blocking -- so the guest completes even with
 * the fix reverted and the test guards nothing. Measured while building this:
 * two earlier drafts without the sleep passed against the pre-#1850 handler.
 * Do not "simplify" it away.
 */
static void* sleeping_worker(void* arg) {
  (void)arg;
  struct timespec delay = {0, 50 * 1000 * 1000}; /* 50ms: the waiter must block first. */
  nanosleep(&delay, NULL);

  atomic_store(&g_value, PUBLISHED_VALUE);
  uint64_t one = 1;
  if (write(g_event_fd, &one, sizeof(one)) != (ssize_t)sizeof(one)) {
    perror("write eventfd");
    _exit(1);
  }
  return NULL;
}

static int run_blocking_mode(int epfd) {
  pthread_t tid;
  if (pthread_create(&tid, NULL, sleeping_worker, NULL) != 0) {
    perror("pthread_create");
    return 1;
  }

  /* Infinite timeout, NULL sigmask: the exact call `cmake` issues through
   * libuv, and the one that used to wedge the whole guest. */
  struct epoll_event got;
  for (;;) {
    int n = (int)syscall(SYS_epoll_pwait, epfd, &got, 1, /*timeout=*/-1, /*sigmask=*/NULL,
                         /*sigsetsize=*/(size_t)8);
    if (n < 0) {
      if (errno == EINTR) {
        continue;
      }
      perror("epoll_pwait(-1)");
      return 1;
    }
    if (n > 0) {
      break;
    }
  }

  uint64_t drained = 0;
  if (read(g_event_fd, &drained, sizeof(drained)) != (ssize_t)sizeof(drained)) {
    perror("read eventfd");
    return 1;
  }
  if (pthread_join(tid, NULL) != 0) {
    perror("pthread_join");
    return 1;
  }
  unsigned long long value = atomic_load(&g_value);
  if (value != PUBLISHED_VALUE) {
    fprintf(stderr, "unexpected published value: %llu\n", value);
    return 1;
  }
  printf("epoll-pwait-blocking-progress-ok %llu\n", value);
  return 0;
}

int main(int argc, char** argv) {
  const int blocking_mode = argc > 1 && strcmp(argv[1], "blocking") == 0;

  g_event_fd = eventfd(0, 0);
  if (g_event_fd < 0) {
    perror("eventfd");
    return 1;
  }

  int epfd = epoll_create1(0);
  if (epfd < 0) {
    perror("epoll_create1");
    return 1;
  }

  struct epoll_event ev;
  memset(&ev, 0, sizeof(ev));
  ev.events = EPOLLIN;
  ev.data.fd = g_event_fd;
  if (epoll_ctl(epfd, EPOLL_CTL_ADD, g_event_fd, &ev) != 0) {
    perror("epoll_ctl");
    return 1;
  }

  if (blocking_mode) {
    int rc = run_blocking_mode(epfd);
    close(epfd);
    close(g_event_fd);
    return rc;
  }

  pthread_t tid;
  if (pthread_create(&tid, NULL, worker, NULL) != 0) {
    perror("pthread_create");
    return 1;
  }

  /* Poll with a zero timeout until the worker publishes. No sleep, no yield:
   * the scheduler turn taken inside the zero-timeout epoll_pwait path is the
   * only thing that lets the worker run. */
  struct epoll_event got;
  for (;;) {
    int n = epoll_pwait_zero_timeout(epfd, &got, 1);
    if (n < 0) {
      if (errno == EINTR) {
        continue;
      }
      perror("epoll_pwait");
      return 1;
    }
    if (n > 0) {
      break;
    }
  }

  uint64_t drained = 0;
  if (read(g_event_fd, &drained, sizeof(drained)) != (ssize_t)sizeof(drained)) {
    perror("read eventfd");
    return 1;
  }

  if (pthread_join(tid, NULL) != 0) {
    perror("pthread_join");
    return 1;
  }

  unsigned long long value = atomic_load(&g_value);
  if (value != PUBLISHED_VALUE) {
    fprintf(stderr, "unexpected published value: %llu\n", value);
    return 1;
  }

  printf("epoll-pwait-zero-timeout-progress-ok %llu\n", value);
  close(epfd);
  close(g_event_fd);
  return 0;
}
