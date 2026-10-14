/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Scenarios for the coalescing periodic ITIMER_REAL
 * (https://github.com/rrnewton/hermit/issues/4139). Linux re-arms an expired
 * ITIMER_REAL only when a SIGALRM is dequeued from the process's shared queue,
 * so at most one timer SIGALRM is outstanding and missed periods are dropped.
 * Each mode prints key=value results for the test to judge; the same binary
 * runs natively to check that the oracles hold on Linux.
 *
 *   storm <period_us> <millions> <threads>
 *       A syscall-free loop in each of 1 or 2 threads under the periodic
 *       timer, recording the loop index at each delivery.
 *   blocked <period_us>
 *       Block SIGALRM, wait for an expiry, read getitimer, unblock: counts the
 *       deliveries inside the unblocking sigprocmask, then proves a re-arm by a
 *       fresh pending copy after blocking again.
 *   sfd <period_us> <read|fault|preadv2|badset> <work_periods>
 *       Block SIGALRM, wait for an expiry, consume it through a non-blocking
 *       signalfd (or rt_sigtimedwait for badset), compute without syscalls for
 *       work_periods, then read getitimer and prove a re-arm causally.
 *   ignored <period_us>
 *       SIG_IGN for 3 periods, then a handler for 3 periods, then a kill.
 *   flush <period_us>
 *       Block, let it expire, set SIG_IGN (flushing the copy), unblock.
 *   midturn <period_us> <total_ms>
 *       SIGALRM blocked; loops of about 1 ms of syscall-free work followed by a
 *       non-blocking signalfd read, counting the records read.
 *   stw <period_us> <ok|fault> <work_periods>
 *       As sfd, consuming through rt_sigtimedwait (fault: a siginfo pointer
 *       that faults after the dequeue).
 *   late <period_us>
 *       Consume 10 periods of syscall-free work after the expiry.
 *   longhandler <period_us>
 *       A handler that runs for 3 periods, inside a window of 30.
 *   mixed <period_us> / mixedsample <period_us>
 *       SIG_IGN, the leader blocks SIGALRM and a worker does not: the
 *       acknowledged protocol, and getitimer sampled every half period.
 *   ignblocked <period_us>
 *       SIG_IGN set while blocked; unblocking discards the queued copy.
 *   private <period_us>
 *       A private tgkill copy taken before the timer's shared copy.
 *   sigsuspend <period_us> <n>
 *       n timer deliveries through sigsuspend.
 *   live <period_us>
 *       As blocked, then 10 periods of live recurrence after unblocking.
 *   privfd <period_us>
 *       A thread created without CLONE_FILES consumes the copy through a
 *       signalfd only its own descriptor table holds, works 2 periods, and
 *       queries.
 *   posix <period_us>
 *       SIG_IGN: the timer stops at its first expiry; then a one-shot POSIX
 *       SIGALRM timer's copy, dequeued through a signalfd, restarts it.
 *   pollmask <period_us> <kind>
 *       SIG_IGN, unblocked: an 80 ms wait of `kind` (poll, select,
 *       pselect6_null, or a ppoll or pselect6 whose temporary mask leaves
 *       SIGALRM unblocked or blocks it), then 60 ms of samples.
 *   stale <kind>
 *       SIG_IGN, unblocked, during a 30 ms wait of `kind`; afterwards the
 *       thread blocks SIGALRM and arms a 10 ms timer: the expiry must queue.
 *   pause <period_us> <millions>
 *       The leader waits in pause while a worker runs the storm loop.
 *   polling <period_us> <millions>
 *       The storm loop beside a sibling polling an empty pipe every 1 ms.
 *
 * Every wait is bounded; a missed bound prints outcome=INCONCLUSIVE.
 */

#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/signalfd.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/uio.h>
#include <time.h>
#include <unistd.h>

#define MAX_SAMPLES 4096

static long period_us;
static long iterations;
static volatile long loop_index[2];
static volatile sig_atomic_t delivered[2];
static long samples[2][MAX_SAMPLES];
static long sample_clock[2][MAX_SAMPLES];
static long armed_at;
static volatile sig_atomic_t overflow;
static __thread int my_slot = -1;

static long now_us(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return t.tv_sec * 1000000L + t.tv_nsec / 1000;
}

static void arm(long value_us, long interval_us) {
  struct itimerval timer = {
      {interval_us / 1000000, interval_us % 1000000},
      {value_us / 1000000, value_us % 1000000}};
  setitimer(ITIMER_REAL, &timer, NULL);
}

static long remaining_us(void) {
  struct itimerval value;
  getitimer(ITIMER_REAL, &value);
  return value.it_value.tv_sec * 1000000L + value.it_value.tv_usec;
}

static int pending(void) {
  sigset_t set;
  sigpending(&set);
  return sigismember(&set, SIGALRM);
}

static void block(int how) {
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGALRM);
  sigprocmask(how, &set, NULL);
}

static void on_alarm(int signal) {
  (void)signal;
  int slot = my_slot < 0 ? 0 : my_slot;
  long n = delivered[slot];
  if (n < MAX_SAMPLES) {
    samples[slot][n] = loop_index[slot];
    sample_clock[slot][n] = now_us();
  } else {
    overflow = 1;
  }
  delivered[slot] = n + 1;
}

static void install(void (*handler)(int)) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = handler;
  action.sa_flags = SA_RESTART;
  sigaction(SIGALRM, &action, NULL);
}

/* Spin on getppid until `deadline`, a CLOCK_MONOTONIC microsecond value. */
static void spin_until(long deadline) {
  while (now_us() < deadline) {
    syscall(SYS_getppid);
  }
}

/* Wait (at most `bound_us`) for a pending SIGALRM; returns whether it came. */
static int await_pending(long bound_us) {
  long deadline = now_us() + bound_us;
  while (now_us() < deadline) {
    if (pending()) {
      return 1;
    }
    syscall(SYS_getppid);
  }
  return pending();
}

static void* spin_thread(void* arg) {
  my_slot = (int)(intptr_t)arg;
  for (long i = 0; i < iterations; i++) {
    loop_index[my_slot] = i;
  }
  return NULL;
}

static int storm(int argc, char** argv) {
  if (argc != 5) {
    return 2;
  }
  period_us = atol(argv[2]);
  iterations = atol(argv[3]) * 1000000L;
  int threads = atoi(argv[4]);
  install(on_alarm);
  long start = now_us();
  armed_at = now_us();
  arm(period_us, period_us);
  pthread_t worker;
  if (threads == 2) {
    pthread_create(&worker, NULL, spin_thread, (void*)(intptr_t)1);
  }
  spin_thread((void*)(intptr_t)0);
  if (threads == 2) {
    pthread_join(worker, NULL);
  }
  /* The drain boundary: block, disarm, drop a pending copy, then mark the end. */
  block(SIG_BLOCK);
  arm(0, 0);
  struct timespec zero = {0, 0};
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGALRM);
  sigtimedwait(&set, NULL, &zero);
  long work_us = now_us() - start;
  for (int t = 0; t < threads; t++) {
    long n = delivered[t] < MAX_SAMPLES ? delivered[t] : MAX_SAMPLES;
    long first = n ? samples[t][0] : -1;
    long last = n ? samples[t][n - 1] : -1;
    long min_gap = -1, max_gap = -1;
    for (long i = 1; i < n; i++) {
      long gap = samples[t][i] - samples[t][i - 1];
      if (min_gap < 0 || gap < min_gap) {
        min_gap = gap;
      }
      if (gap > max_gap) {
        max_gap = gap;
      }
    }
    printf("t%d_signals=%ld t%d_first=%ld t%d_last=%ld t%d_min_gap=%ld t%d_max_gap=%ld ",
           t, (long)delivered[t], t, first, t, last, t, min_gap, t, max_gap);
  }
  /* Each delivery comes no earlier than the first grid point after the
   * previous one: the timer re-armed at the previous dequeue, on the grid
   * that starts at arming. */
  long grid_violations = 0;
  for (int t = 0; t < threads; t++) {
    long n = delivered[t] < MAX_SAMPLES ? delivered[t] : MAX_SAMPLES;
    for (long i = 1; i < n; i++) {
      long prev = sample_clock[t][i - 1];
      long next = armed_at + ((prev - armed_at) / period_us + 1) * period_us;
      if (sample_clock[t][i] < next) {
        grid_violations++;
      }
    }
  }
  printf("grid_violations=%ld overflow=%d work_us=%ld\n", grid_violations, (int)overflow, work_us);
  return 0;
}

/* The causal re-arm proof: SIGALRM is blocked; after at least two periods
 * past `consumed`, a fresh pending copy must appear before the deadline. */
static const char* rearm_proof(long consumed) {
  spin_until(consumed + 2 * period_us);
  return await_pending(1000000 + 2 * period_us) ? "PASS" : "INCONCLUSIVE";
}

/* Acknowledge a consumption that started at `before` (SIGALRM blocked, the
 * timer armed at `armed_at`): the copy must have left the queue. NULL if it
 * did. A copy still pending before the first grid point after `before` is the
 * old one: UNCONSUMED. One seen later may be a fresh generation, after host
 * descheduling: INCONCLUSIVE. */
static const char* acknowledge(long before) {
  int still = pending();
  long checked = now_us();
  if (!still) {
    return NULL;
  }
  long next = armed_at + ((before - armed_at) / period_us + 1) * period_us;
  return checked >= next ? "INCONCLUSIVE" : "UNCONSUMED";
}

/* The causal re-arm proof, after an acknowledged consumption. */
static const char* acknowledged_rearm(const char* ack, long consumed) {
  return ack ? ack : rearm_proof(consumed);
}

static int blocked(int argc, char** argv) {
  if (argc != 3) {
    return 2;
  }
  period_us = atol(argv[2]);
  install(on_alarm);
  block(SIG_BLOCK);
  long start = now_us();
  arm(period_us, period_us);
  if (!await_pending(1000000 + 3 * period_us)) {
    printf("outcome=INCONCLUSIVE\n");
    return 0;
  }
  spin_until(start + 3 * period_us);
  long value = remaining_us();
  /* Disarmed first, so no later expiry can add a delivery. */
  arm(0, 0);
  block(SIG_UNBLOCK);
  long in_unblock = delivered[0];
  spin_until(now_us() + 3 * period_us);
  printf("blocked_remaining_us=%ld in_unblock=%ld after=%ld\n", value, in_unblock,
         (long)delivered[0] - in_unblock);
  return 0;
}

static int sfd(int argc, char** argv) {
  if (argc != 5) {
    return 2;
  }
  period_us = atol(argv[2]);
  const char* path = argv[3];
  long work_periods = atol(argv[4]);
  block(SIG_BLOCK);
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGALRM);
  armed_at = now_us();
  arm(period_us, period_us);
  if (!await_pending(1000000 + 3 * period_us)) {
    printf("outcome=INCONCLUSIVE\n");
    return 0;
  }
  /* Created after the expiry: late registration. */
  int fd = signalfd(-1, &set, SFD_NONBLOCK);
  struct signalfd_siginfo record;
  memset(&record, 0, sizeof record);
  long result;
  long before = now_us();
  if (strcmp(path, "read") == 0) {
    result = read(fd, &record, sizeof record);
  } else if (strcmp(path, "fault") == 0) {
    /* An unmapped buffer: the kernel dequeues, then fails the copy-out. */
    result = syscall(SYS_read, fd, (void*)8, sizeof record);
  } else if (strcmp(path, "readv_fault") == 0) {
    struct iovec iov = {(void*)8, sizeof record};
    result = syscall(SYS_readv, fd, &iov, 1);
  } else if (strcmp(path, "preadv2") == 0) {
    struct iovec iov = {&record, sizeof record};
    result = syscall(SYS_preadv2, fd, &iov, 1, -1L, -1L, 0);
  } else if (strcmp(path, "badset") == 0) {
    struct timespec zero = {0, 0};
    result = syscall(SYS_rt_sigtimedwait, (void*)8, NULL, &zero, 8);
  } else if (strcmp(path, "ebadf") == 0) {
    /* Fails before any dequeue. */
    result = syscall(SYS_read, -1, &record, sizeof record);
  } else {
    return 2;
  }
  int error = result < 0 ? errno : 0;
  long consumed = now_us();
  const char* ack = acknowledge(before);
  /* Syscall-free work: no scheduler turn under Hermit. */
  volatile long acc = 0;
  long target = consumed + work_periods * period_us;
  while (work_periods > 0 && now_us() < target) {
    for (int i = 0; i < 1000; i++) {
      acc += i;
    }
  }
  long value = remaining_us();
  int still_pending = pending();
  const char* proof = acknowledged_rearm(ack, consumed);
  printf("result=%ld errno=%d signo=%u cleared=%d remaining_us=%ld pending_after=%d rearm=%s\n",
         result, error, record.ssi_signo, ack == NULL, value, still_pending, proof);
  return 0;
}

static int ignored(int argc, char** argv) {
  if (argc != 3) {
    return 2;
  }
  period_us = atol(argv[2]);
  install(SIG_IGN);
  long start = now_us();
  arm(period_us, period_us);
  spin_until(start + 3 * period_us);
  long ignored_remaining = remaining_us();
  install(on_alarm);
  spin_until(start + 6 * period_us);
  long handled_remaining = remaining_us();
  long before_kill = delivered[0];
  kill(getpid(), SIGALRM);
  long after_kill = delivered[0];
  /* The kill's dequeue restarted the timer: an expiry falls in this window,
   * which starts at the kill whenever the host resumes the process. */
  spin_until(now_us() + 3 * period_us);
  printf("ignored_remaining_us=%ld handled_remaining_us=%ld before_kill=%ld kill_deliveries=%ld "
         "after=%ld\n",
         ignored_remaining, handled_remaining, before_kill, after_kill - before_kill,
         (long)delivered[0] - after_kill);
  return 0;
}

static int flush(int argc, char** argv) {
  if (argc != 3) {
    return 2;
  }
  period_us = atol(argv[2]);
  install(on_alarm);
  block(SIG_BLOCK);
  arm(period_us, period_us);
  if (!await_pending(1000000 + 3 * period_us)) {
    printf("outcome=INCONCLUSIVE\n");
    return 0;
  }
  install(SIG_IGN);
  int flushed = !pending();
  block(SIG_UNBLOCK);
  install(on_alarm);
  long start = now_us();
  spin_until(start + 3 * period_us);
  printf("flushed=%d remaining_us=%ld deliveries=%ld\n", flushed, remaining_us(),
         (long)delivered[0]);
  return 0;
}

static int midturn(int argc, char** argv) {
  if (argc != 4) {
    return 2;
  }
  period_us = atol(argv[2]);
  long total_us = atol(argv[3]) * 1000L;
  block(SIG_BLOCK);
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGALRM);
  int fd = signalfd(-1, &set, SFD_NONBLOCK);
  long start = now_us();
  arm(period_us, period_us);
  long records = 0;
  volatile long acc = 0;
  while (now_us() - start < total_us) {
    long until = now_us() + 1000;
    while (now_us() < until) {
      for (int i = 0; i < 1000; i++) {
        acc += i;
      }
    }
    struct signalfd_siginfo record;
    if (read(fd, &record, sizeof record) == sizeof record) {
      records++;
    }
  }
  printf("records=%ld elapsed_us=%ld\n", records, now_us() - start);
  return 0;
}


/* Consume the blocked copy with rt_sigtimedwait (ok) or with a siginfo
 * pointer that faults after the dequeue (fault), compute without syscalls,
 * then query. */
static int stw(int argc, char** argv) {
  if (argc != 5) {
    return 2;
  }
  period_us = atol(argv[2]);
  const char* path = argv[3];
  long work_periods = atol(argv[4]);
  block(SIG_BLOCK);
  armed_at = now_us();
  arm(period_us, period_us);
  if (!await_pending(1000000 + 3 * period_us)) {
    printf("outcome=INCONCLUSIVE\n");
    return 0;
  }
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGALRM);
  struct timespec zero = {0, 0};
  long result;
  long before = now_us();
  if (strcmp(path, "ok") == 0) {
    result = syscall(SYS_rt_sigtimedwait, &set, NULL, &zero, 8);
  } else if (strcmp(path, "fault") == 0) {
    result = syscall(SYS_rt_sigtimedwait, &set, (void*)8, &zero, 8);
  } else {
    return 2;
  }
  int error = result < 0 ? errno : 0;
  long consumed = now_us();
  const char* ack = acknowledge(before);
  volatile long acc = 0;
  long target = consumed + work_periods * period_us;
  while (work_periods > 0 && now_us() < target) {
    for (int i = 0; i < 1000; i++) {
      acc += i;
    }
  }
  long value = remaining_us();
  int still_pending = pending();
  const char* proof = acknowledged_rearm(ack, consumed);
  printf("result=%ld errno=%d cleared=%d remaining_us=%ld pending_after=%d rearm=%s\n", result,
         error, ack == NULL, value, still_pending, proof);
  return 0;
}

/* Late in-turn consumption: compute 10 periods without syscalls after the
 * expiry, then consume through a non-blocking signalfd and query at once. */
static int late(int argc, char** argv) {
  if (argc != 3) {
    return 2;
  }
  period_us = atol(argv[2]);
  block(SIG_BLOCK);
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGALRM);
  int fd = signalfd(-1, &set, SFD_NONBLOCK);
  arm(period_us, period_us);
  if (!await_pending(1000000 + 3 * period_us)) {
    printf("outcome=INCONCLUSIVE\n");
    return 0;
  }
  volatile long acc = 0;
  long target = now_us() + 10 * period_us;
  while (now_us() < target) {
    for (int i = 0; i < 1000; i++) {
      acc += i;
    }
  }
  struct signalfd_siginfo record;
  long result = read(fd, &record, sizeof record);
  long read_clock = now_us();
  long value = remaining_us();
  /* The next expiry comes after the read. */
  long waited = await_pending(1000000 + 2 * period_us) ? now_us() - read_clock : -1;
  printf("result=%ld remaining_us=%ld next_after_us=%ld\n", result, value, waited);
  return 0;
}

static volatile sig_atomic_t stop_handler;
static long handler_entries[MAX_SAMPLES];
static volatile long entries;
static long window_end;

static void long_handler(int signal) {
  (void)signal;
  long n = entries;
  if (n < MAX_SAMPLES) {
    handler_entries[n] = now_us();
  }
  entries = n + 1;
  if (now_us() >= window_end) {
    arm(0, 0);
    stop_handler = 1;
    return;
  }
  long until = now_us() + 3 * period_us;
  if (until > window_end) {
    until = window_end;
  }
  spin_until(until);
}

/* A handler that spins for 3 periods inside a closed window of 30. */
static int longhandler(int argc, char** argv) {
  if (argc != 3) {
    return 2;
  }
  period_us = atol(argv[2]);
  install(long_handler);
  long start = now_us();
  window_end = start + 30 * period_us;
  arm(period_us, period_us);
  while (!stop_handler && now_us() < window_end + 1000000) {
    syscall(SYS_getppid);
  }
  block(SIG_BLOCK);
  arm(0, 0);
  long n = entries < MAX_SAMPLES ? entries : MAX_SAMPLES;
  /* The last entry's handler is cut short by the window end. */
  long min_spacing = -1;
  for (long i = 1; i + 1 < n; i++) {
    long spacing = handler_entries[i] - handler_entries[i - 1];
    if (min_spacing < 0 || spacing < min_spacing) {
      min_spacing = spacing;
    }
  }
  printf("entries=%ld min_spacing_us=%ld stopped=%d\n", (long)entries, min_spacing,
         (int)stop_handler);
  return 0;
}

static volatile long progress;

/* Computes between calls, so a slice costs few traced calls. Its call
 * returns are where it dequeues. */
static void* mixed_worker(void* arg) {
  (void)arg;
  volatile long acc = 0;
  for (;;) {
    for (int i = 0; i < 1000; i++) {
      acc += i;
    }
    syscall(SYS_getppid);
    progress++;
  }
  return NULL;
}

/* SIG_IGN, the leader blocks SIGALRM and the worker does not. The leader
 * acknowledges each generation (a pending copy), waits for the worker to make
 * two calls (whose return dequeued it), and classifies its getitimer. */
static int mixed(int argc, char** argv) {
  if (argc != 3) {
    return 2;
  }
  period_us = atol(argv[2]);
  install(SIG_IGN);
  /* The worker starts first, so it does not inherit the leader's mask. */
  pthread_t worker;
  pthread_create(&worker, NULL, mixed_worker, NULL);
  block(SIG_BLOCK);
  long armed = now_us();
  arm(period_us, period_us);
  long positive = 0, negative = 0, ambiguous = 0, unobserved = 0;
  long deadline = now_us() + 3000000;
  for (int cycle = 0; cycle < 20 && now_us() < deadline; cycle++) {
    if (!await_pending(3 * period_us)) {
      unobserved++;
      continue;
    }
    long w = progress;
    long ack_deadline = now_us() + 1000000;
    while (progress < w + 2 && now_us() < ack_deadline) {
      syscall(SYS_getppid);
    }
    if (progress < w + 2) {
      unobserved++;
      continue;
    }
    long before = now_us();
    long value = remaining_us();
    long after = now_us();
    long next_grid = armed + ((before - armed) / period_us + 1) * period_us;
    if (value >= 1) {
      positive++;
    } else if (after + 1 < next_grid - 1000) {
      negative++;
    } else {
      ambiguous++;
    }
  }
  printf("positive=%ld negative=%ld ambiguous=%ld unobserved=%ld\n", positive, negative, ambiguous,
         unobserved);
  if (positive == 0) {
    printf("outcome=INCONCLUSIVE\n");
  }
  return 0;
}

/* The mixed-mask case sampled: the leader reads getitimer every half period
 * for 10 periods. Under Hermit the worker, the first unblocked thread
 * resumed after each expiry, always dequeues before the leader can see the
 * copy, so the acknowledged protocol of `mixed` never observes a generation;
 * a timer that keeps running reads positive at some sample, a dead one never
 * does. */
static int mixedsample(int argc, char** argv) {
  if (argc != 3) {
    return 2;
  }
  period_us = atol(argv[2]);
  install(SIG_IGN);
  pthread_t worker;
  pthread_create(&worker, NULL, mixed_worker, NULL);
  block(SIG_BLOCK);
  long start = now_us();
  arm(period_us, period_us);
  long positive = 0, samples_taken = 0;
  for (int i = 1; i <= 20; i++) {
    spin_until(start + i * period_us / 2);
    samples_taken++;
    if (remaining_us() >= 1) {
      positive++;
    }
  }
  printf("positive=%ld samples=%ld progress=%ld\n", positive, samples_taken, (long)progress);
  return 0;
}

/* SIG_IGN set while blocked, an expiry queues (the leader blocks it), then
 * unblocking dequeues and discards the ignored copy, which re-arms. */
static int ignblocked(int argc, char** argv) {
  if (argc != 3) {
    return 2;
  }
  period_us = atol(argv[2]);
  block(SIG_BLOCK);
  install(SIG_IGN);
  armed_at = now_us();
  arm(period_us, period_us);
  if (!await_pending(1000000 + 3 * period_us)) {
    printf("outcome=INCONCLUSIVE\n");
    return 0;
  }
  long before = now_us();
  block(SIG_UNBLOCK);
  long consumed = now_us();
  long value = remaining_us();
  block(SIG_BLOCK);
  const char* proof = acknowledged_rearm(acknowledge(before), consumed);
  printf("remaining_us=%ld rearm=%s\n", value, proof);
  return 0;
}

static long handler_remaining[8];
static volatile long handler_count;

static void remaining_handler(int signal) {
  (void)signal;
  long n = handler_count;
  if (n < 8) {
    handler_remaining[n] = remaining_us();
  }
  handler_count = n + 1;
}

/* A private copy and the timer's shared copy: the private one is taken
 * first and must not re-arm; the shared one does. */
static int private_shared(int argc, char** argv) {
  if (argc != 3) {
    return 2;
  }
  period_us = atol(argv[2]);
  install(remaining_handler);
  block(SIG_BLOCK);
  arm(period_us, period_us);
  if (!await_pending(1000000 + 3 * period_us)) {
    printf("outcome=INCONCLUSIVE\n");
    return 0;
  }
  syscall(SYS_tgkill, getpid(), syscall(SYS_gettid), SIGALRM);
  block(SIG_UNBLOCK);
  block(SIG_BLOCK);
  printf("handled=%ld first_remaining_us=%ld second_remaining_us=%ld\n", (long)handler_count,
         handler_remaining[0], handler_remaining[1]);
  return 0;
}

/* A timer loop on sigsuspend, the fallback (rule 2) end to end. */
static int sigsuspend_loop(int argc, char** argv) {
  if (argc != 4) {
    return 2;
  }
  period_us = atol(argv[2]);
  long n = atol(argv[3]);
  install(on_alarm);
  block(SIG_BLOCK);
  arm(period_us, period_us);
  sigset_t empty;
  sigemptyset(&empty);
  for (long i = 0; i < n; i++) {
    sigsuspend(&empty);
  }
  arm(0, 0);
  printf("deliveries=%ld\n", (long)delivered[0]);
  return 0;
}

/* Live recurrence after unblocking: as `blocked`, then a window of 10
 * periods of getppid calls, ended by the drain boundary. */
static int live(int argc, char** argv) {
  if (argc != 3) {
    return 2;
  }
  period_us = atol(argv[2]);
  install(on_alarm);
  block(SIG_BLOCK);
  armed_at = now_us();
  arm(period_us, period_us);
  if (!await_pending(1000000 + 3 * period_us)) {
    printf("outcome=INCONCLUSIVE\n");
    return 0;
  }
  spin_until(armed_at + 3 * period_us);
  long value = remaining_us();
  block(SIG_UNBLOCK);
  spin_until(now_us() + 10 * period_us);
  block(SIG_BLOCK);
  arm(0, 0);
  struct timespec zero = {0, 0};
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGALRM);
  sigtimedwait(&set, NULL, &zero);
  long n = delivered[0] < MAX_SAMPLES ? delivered[0] : MAX_SAMPLES;
  long grid_violations = 0;
  for (long i = 1; i < n; i++) {
    long prev = sample_clock[0][i - 1];
    long next = armed_at + ((prev - armed_at) / period_us + 1) * period_us;
    if (sample_clock[0][i] < next) {
      grid_violations++;
    }
  }
  printf("blocked_remaining_us=%ld deliveries=%ld grid_violations=%ld\n", value,
         (long)delivered[0], grid_violations);
  return 0;
}

static char child_stack[65536] __attribute__((aligned(16)));
static int private_fd;
static volatile int child_done;
static long child_result;
static long child_remaining;
static int child_pending;
static int child_cleared;

/* SIGALRM's bit in the calling thread's pending set, by raw system call. */
static int raw_pending(void) {
  uint64_t set = 0;
  syscall(SYS_rt_sigpending, &set, sizeof set);
  return (int)((set >> (SIGALRM - 1)) & 1);
}

/* Runs without its own TLS: raw system calls and the vDSO clock only. */
static int private_fd_child(void* arg) {
  (void)arg;
  struct signalfd_siginfo record;
  child_result = syscall(SYS_read, private_fd, &record, sizeof record);
  child_cleared = !raw_pending();
  volatile long acc = 0;
  long target = now_us() + 2 * period_us;
  while (now_us() < target) {
    for (int i = 0; i < 1000; i++) {
      acc += i;
    }
  }
  struct itimerval value;
  syscall(SYS_getitimer, ITIMER_REAL, &value);
  child_remaining = value.it_value.tv_sec * 1000000L + value.it_value.tv_usec;
  child_pending = raw_pending();
  __atomic_store_n(&child_done, 1, __ATOMIC_RELEASE);
  syscall(SYS_exit, 0);
  return 0;
}

/* The consumer's descriptor table is private: the thread is created without
 * CLONE_FILES, and the creator closes its own copy of the signalfd. */
static int privfd(int argc, char** argv) {
  if (argc != 3) {
    return 2;
  }
  period_us = atol(argv[2]);
  block(SIG_BLOCK);
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGALRM);
  arm(period_us, period_us);
  if (!await_pending(1000000 + 3 * period_us)) {
    printf("outcome=INCONCLUSIVE\n");
    return 0;
  }
  private_fd = signalfd(-1, &set, SFD_NONBLOCK);
  int flags = CLONE_VM | CLONE_SIGHAND | CLONE_THREAD | CLONE_SYSVSEM;
  if (clone(private_fd_child, child_stack + sizeof child_stack, flags, NULL) < 0) {
    return 2;
  }
  close(private_fd);
  long deadline = now_us() + 1000000 + 4 * period_us;
  while (!__atomic_load_n(&child_done, __ATOMIC_ACQUIRE) && now_us() < deadline) {
    syscall(SYS_getppid);
  }
  if (!__atomic_load_n(&child_done, __ATOMIC_ACQUIRE)) {
    printf("outcome=INCONCLUSIVE\n");
    return 0;
  }
  printf("result=%ld cleared=%d remaining_us=%ld pending_after=%d\n", child_result, child_cleared,
         child_remaining, child_pending);
  return 0;
}

/* Linux restarts a stopped ITIMER_REAL on any shared-queue SIGALRM dequeue,
 * whatever produced the copy: here a POSIX timer's. */
static int posix_revival(int argc, char** argv) {
  if (argc != 3) {
    return 2;
  }
  period_us = atol(argv[2]);
  install(SIG_IGN);
  armed_at = now_us();
  arm(period_us, period_us);
  spin_until(armed_at + 4 * period_us);
  long stopped = remaining_us();
  block(SIG_BLOCK);
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGALRM);
  int fd = signalfd(-1, &set, SFD_NONBLOCK);
  timer_t timer;
  struct sigevent event;
  memset(&event, 0, sizeof event);
  event.sigev_notify = SIGEV_SIGNAL;
  event.sigev_signo = SIGALRM;
  if (fd < 0 || timer_create(CLOCK_MONOTONIC, &event, &timer) != 0) {
    return 2;
  }
  struct itimerspec once = {{0, 0}, {0, period_us * 250}};
  timer_settime(timer, 0, &once, NULL);
  if (!await_pending(1000000 + period_us)) {
    printf("outcome=INCONCLUSIVE\n");
    return 0;
  }
  struct signalfd_siginfo record;
  memset(&record, 0, sizeof record);
  long result = read(fd, &record, sizeof record);
  int cleared = !pending();
  long consumed = now_us();
  long immediate = remaining_us();
  const char* proof = cleared ? rearm_proof(consumed) : "UNCONSUMED";
  arm(0, 0);
  timer_delete(timer);
  printf("stopped_us=%ld result=%ld signo=%u cleared=%d immediate_us=%ld rearm=%s\n", stopped,
         result, record.ssi_signo, cleared, immediate, proof);
  return 0;
}

/* Generation discard follows the mask Linux's `blocked` holds during a wait:
 * the guest's own mask in poll, the temporary one in ppoll. */
static int pollmask(int argc, char** argv) {
  if (argc != 4) {
    return 2;
  }
  period_us = atol(argv[2]);
  const char* kind = argv[3];
  install(SIG_IGN);
  block(SIG_UNBLOCK);
  arm(period_us, period_us);
  struct timespec timeout = {0, 80000000};
  sigset_t temporary;
  sigemptyset(&temporary);
  int result;
  struct timeval tv = {0, 80000};
  struct {
    const sigset_t* set;
    size_t size;
  } pselect_mask = {&temporary, 8};
  if (strcmp(kind, "poll") == 0) {
    result = poll(NULL, 0, 80);
  } else if (strcmp(kind, "select") == 0) {
    result = syscall(SYS_select, 0, NULL, NULL, NULL, &tv);
  } else if (strcmp(kind, "pselect6_null") == 0) {
    result = syscall(SYS_pselect6, 0, NULL, NULL, NULL, &timeout, NULL);
  } else if (strcmp(kind, "pselect6_block") == 0) {
    sigaddset(&temporary, SIGALRM);
    result = syscall(SYS_pselect6, 0, NULL, NULL, NULL, &timeout, &pselect_mask);
  } else if (strcmp(kind, "ppoll_unblock") == 0) {
    result = ppoll(NULL, 0, &timeout, &temporary);
  } else if (strcmp(kind, "ppoll_block") == 0) {
    sigaddset(&temporary, SIGALRM);
    result = ppoll(NULL, 0, &timeout, &temporary);
  } else {
    return 2;
  }
  long returned = now_us();
  long first = remaining_us();
  long first_clock = now_us();
  long positive = first > 0;
  long end = now_us() + 60000;
  while (now_us() < end) {
    if (remaining_us() > 0) {
      positive++;
    }
    syscall(SYS_getppid);
  }
  arm(0, 0);
  printf("result=%d first_us=%ld positive=%ld\n", result, first, positive);
  /* *_block: the copy queued under the temporary mask is discarded as
   * the call returns, which restarts the timer; a first sample taken a whole
   * period after the return can legally miss that. */
  if (strstr(kind, "_block") != NULL && positive == 0 && first_clock - returned >= period_us) {
    printf("outcome=INCONCLUSIVE\n");
  }
  return 0;
}

/* A wait publishes the mask Linux's `blocked` holds while it lasts; once it
 * returns, the thread's own mask decides again. Linux: blocked means not
 * ignored, so the expiry queues (pending=1). A published mask left behind
 * (SIGALRM unblocked) discards it. */
static int stale(int argc, char** argv) {
  if (argc != 3) {
    return 2;
  }
  const char* kind = argv[2];
  install(SIG_IGN);
  block(SIG_UNBLOCK);
  sigset_t empty;
  sigemptyset(&empty);
  struct timespec ts = {0, 30000000};
  struct timeval tv = {0, 30000};
  struct {
    const sigset_t* set;
    size_t size;
  } pselect_mask = {&empty, 8};
  int epoll = epoll_create1(0);
  struct epoll_event event;
  sigset_t wait_set;
  sigemptyset(&wait_set);
  sigaddset(&wait_set, SIGUSR2);
  long result;
  if (strcmp(kind, "poll") == 0) {
    result = poll(NULL, 0, 30);
  } else if (strcmp(kind, "select") == 0) {
    result = syscall(SYS_select, 0, NULL, NULL, NULL, &tv);
  } else if (strcmp(kind, "pselect6_null") == 0) {
    result = syscall(SYS_pselect6, 0, NULL, NULL, NULL, &ts, NULL);
  } else if (strcmp(kind, "pselect6_mask") == 0) {
    result = syscall(SYS_pselect6, 0, NULL, NULL, NULL, &ts, &pselect_mask);
  } else if (strcmp(kind, "ppoll_mask") == 0) {
    result = ppoll(NULL, 0, &ts, &empty);
  } else if (strcmp(kind, "epoll_pwait") == 0) {
    result = epoll_pwait(epoll, &event, 1, 30, &empty);
  } else if (strcmp(kind, "sigtimedwait") == 0) {
    sigprocmask(SIG_BLOCK, &wait_set, NULL);
    result = sigtimedwait(&wait_set, NULL, &ts);
  } else {
    return 2;
  }
  block(SIG_BLOCK);
  period_us = 10000;
  arm(period_us, period_us);
  struct timespec sleep = {0, 45000000};
  nanosleep(&sleep, NULL);
  int still = pending();
  arm(0, 0);
  printf("result=%ld pending=%d\n", result, still);
  return 0;
}

static volatile sig_atomic_t worker_done;

static void on_usr1(int signal) {
  (void)signal;
}

static void* pause_worker(void* arg) {
  pthread_t leader = *(pthread_t*)arg;
  spin_thread((void*)(intptr_t)1);
  worker_done = 1;
  pthread_kill(leader, SIGUSR1);
  return NULL;
}

/* The leader waits in pause, woken by its SIGALRM deliveries, while a worker
 * computes; the worker's final SIGUSR1 ends the wait. */
static int pause_leader(int argc, char** argv) {
  if (argc != 4) {
    return 2;
  }
  period_us = atol(argv[2]);
  iterations = atol(argv[3]) * 1000000L;
  install(on_alarm);
  signal(SIGUSR1, on_usr1);
  pthread_t leader = pthread_self();
  long start = now_us();
  arm(period_us, period_us);
  pthread_t worker;
  pthread_create(&worker, NULL, pause_worker, &leader);
  while (!worker_done) {
    pause();
  }
  pthread_join(worker, NULL);
  block(SIG_BLOCK);
  arm(0, 0);
  printf("t0_signals=%ld t1_signals=%ld work_us=%ld\n", (long)delivered[0], (long)delivered[1],
         now_us() - start);
  return 0;
}

static void* polling_thread(void* arg) {
  int fd = *(int*)arg;
  struct pollfd pfd = {fd, POLLIN, 0};
  for (;;) {
    poll(&pfd, 1, 1);
  }
  return NULL;
}

/* A sibling polls an empty pipe with a 1 ms timeout (host-timed retries)
 * while the main thread runs the storm loop. */
static int polling(int argc, char** argv) {
  if (argc != 4) {
    return 2;
  }
  period_us = atol(argv[2]);
  iterations = atol(argv[3]) * 1000000L;
  int fds[2];
  if (pipe(fds) != 0) {
    return 2;
  }
  install(on_alarm);
  pthread_t poller;
  pthread_create(&poller, NULL, polling_thread, &fds[0]);
  arm(period_us, period_us);
  spin_thread((void*)(intptr_t)0);
  block(SIG_BLOCK);
  arm(0, 0);
  printf("signals=%ld\n", (long)delivered[0]);
  return 0;
}

int main(int argc, char** argv) {
  if (argc < 2) {
    return 2;
  }
  if (strcmp(argv[1], "storm") == 0) {
    return storm(argc, argv);
  }
  if (strcmp(argv[1], "blocked") == 0) {
    return blocked(argc, argv);
  }
  if (strcmp(argv[1], "sfd") == 0) {
    return sfd(argc, argv);
  }
  if (strcmp(argv[1], "ignored") == 0) {
    return ignored(argc, argv);
  }
  if (strcmp(argv[1], "flush") == 0) {
    return flush(argc, argv);
  }
  if (strcmp(argv[1], "midturn") == 0) {
    return midturn(argc, argv);
  }
  if (strcmp(argv[1], "stw") == 0) {
    return stw(argc, argv);
  }
  if (strcmp(argv[1], "late") == 0) {
    return late(argc, argv);
  }
  if (strcmp(argv[1], "longhandler") == 0) {
    return longhandler(argc, argv);
  }
  if (strcmp(argv[1], "mixed") == 0) {
    return mixed(argc, argv);
  }
  if (strcmp(argv[1], "mixedsample") == 0) {
    return mixedsample(argc, argv);
  }
  if (strcmp(argv[1], "ignblocked") == 0) {
    return ignblocked(argc, argv);
  }
  if (strcmp(argv[1], "private") == 0) {
    return private_shared(argc, argv);
  }
  if (strcmp(argv[1], "sigsuspend") == 0) {
    return sigsuspend_loop(argc, argv);
  }
  if (strcmp(argv[1], "live") == 0) {
    return live(argc, argv);
  }
  if (strcmp(argv[1], "privfd") == 0) {
    return privfd(argc, argv);
  }
  if (strcmp(argv[1], "posix") == 0) {
    return posix_revival(argc, argv);
  }
  if (strcmp(argv[1], "pollmask") == 0) {
    return pollmask(argc, argv);
  }
  if (strcmp(argv[1], "stale") == 0) {
    return stale(argc, argv);
  }
  if (strcmp(argv[1], "pause") == 0) {
    return pause_leader(argc, argv);
  }
  if (strcmp(argv[1], "polling") == 0) {
    return polling(argc, argv);
  }
  return 2;
}
