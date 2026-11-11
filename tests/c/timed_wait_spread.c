/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Timed waits raced against a worker's progress, for
 * --chaos-timed-wait-spread.
 *
 * usage: timed_wait_spread <mode>
 *
 * A worker takes 40 turns of a little work and a sched_yield, then sets a
 * flag (and wakes the futex word and condition variable). Meanwhile the main
 * thread makes one 50 ms timed wait, by mode:
 *
 *   sleep       nanosleep, relative
 *   clock-rel   clock_nanosleep(CLOCK_MONOTONIC), relative
 *   clock-abs   clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME)
 *   futex       a raw FUTEX_WAIT on the worker's word, relative timeout
 *   condwait    pthread_cond_timedwait on the flag, absolute CLOCK_MONOTONIC
 *   multi       three condwait waiters with timeouts of 50, 51 and 52 ms
 *
 * It prints which came first and how long the wait took on its own
 * CLOCK_MONOTONIC, sampled immediately before and after the call itself:
 *
 *   first=worker waited_ns=N     the worker finished before the wait ended
 *   first=sleeper waited_ns=N    the sleep ended before the worker finished
 *   first=timeout waited_ns=N    the timed wait expired before the worker
 *
 * Linux never ends these waits early: a completed sleep, and every timeout
 * (ETIMEDOUT), must be observed at or after its deadline on the guest's own
 * clock, whether or not the worker has finished. Such a wait that ends early
 * prints SHORT and exits 1. So does any unexpected return (no signals are
 * sent, so EINTR is unexpected too) and a futex or condition wake with
 * nothing to wake for.
 */

#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#define WAIT_NS 50000000LL
#define TURNS 40
#define WAITERS 3

static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv;
static int done;
static int word;
static volatile uint64_t sink;

static int64_t now_ns(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

static struct timespec to_timespec(int64_t ns) {
  struct timespec ts = {ns / 1000000000, ns % 1000000000};
  return ts;
}

static void *worker(void *arg) {
  (void)arg;
  uint64_t x = 1;
  for (int turn = 0; turn < TURNS; turn++) {
    for (int i = 0; i < 1000; i++) {
      x = x * 6364136223846793005ULL + 1442695040888963407ULL;
    }
    sink += x;
    sched_yield();
  }
  pthread_mutex_lock(&mu);
  done = 1;
  __atomic_store_n(&word, 1, __ATOMIC_SEQ_CST);
  pthread_cond_broadcast(&cv);
  pthread_mutex_unlock(&mu);
  syscall(SYS_futex, &word, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0);
  return NULL;
}

static int read_done(void) {
  pthread_mutex_lock(&mu);
  int value = done;
  pthread_mutex_unlock(&mu);
  return value;
}

/* One wait's outcome: which came first, and the clock around the call. */
struct outcome {
  const char *first;
  int64_t start;
  int64_t end;
  int64_t deadline; /* the wait may not end before this, or 0 */
  int failed;
};

static void fail(struct outcome *out, const char *what, int error) {
  printf("UNEXPECTED: %s returned %s\n", what, strerror(error));
  out->failed = 1;
}

/* A relative or absolute clock_nanosleep, or nanosleep (`clock` < 0). */
static struct outcome do_sleep(clockid_t clock, int absolute) {
  struct outcome out = {0};
  struct timespec request;
  out.start = now_ns();
  out.deadline = out.start + WAIT_NS;
  request = absolute ? to_timespec(out.deadline) : to_timespec(WAIT_NS);
  int error;
  if ((int)clock < 0) {
    error = nanosleep(&request, NULL) ? errno : 0;
  } else {
    error = clock_nanosleep(clock, absolute ? TIMER_ABSTIME : 0, &request, NULL);
  }
  out.end = now_ns();
  if (error) {
    fail(&out, "the sleep", error);
  }
  out.first = read_done() ? "worker" : "sleeper";
  return out;
}

static struct outcome do_futex(void) {
  struct outcome out = {0};
  struct timespec timeout = to_timespec(WAIT_NS);
  out.start = now_ns();
  long ret = syscall(SYS_futex, &word, FUTEX_WAIT_PRIVATE, 0, &timeout, NULL, 0);
  out.end = now_ns();
  int error = ret ? errno : 0;
  if (error == ETIMEDOUT) {
    out.deadline = out.start + WAIT_NS;
    out.first = "timeout";
  } else if (error == EAGAIN || (!error && __atomic_load_n(&word, __ATOMIC_SEQ_CST))) {
    /* The word had changed already (EAGAIN), or the worker woke it. */
    out.first = "worker";
  } else {
    fail(&out, error ? "FUTEX_WAIT" : "FUTEX_WAIT (a wake with the word unchanged)", error);
    out.first = "error";
  }
  return out;
}

/* A pthread_cond_timedwait of `wait_ns`. */
static struct outcome do_condwait(int64_t wait_ns) {
  struct outcome out = {0};
  pthread_mutex_lock(&mu);
  out.start = now_ns();
  int64_t deadline = out.start + wait_ns;
  struct timespec abs = to_timespec(deadline);
  while (!done) {
    int error = pthread_cond_timedwait(&cv, &mu, &abs);
    out.end = now_ns();
    if (error == ETIMEDOUT) {
      out.deadline = deadline;
      out.first = "timeout";
      pthread_mutex_unlock(&mu);
      return out;
    }
    if (error) {
      fail(&out, "pthread_cond_timedwait", error);
      out.first = "error";
      pthread_mutex_unlock(&mu);
      return out;
    }
  }
  if (!out.end) {
    out.end = now_ns();
  }
  out.first = "worker";
  pthread_mutex_unlock(&mu);
  return out;
}

/* Prints `out`; returns whether it broke a rule. */
static int report(const char *prefix, const struct outcome *out) {
  long long waited = (long long)(out->end - out->start);
  printf("%sfirst=%s waited_ns=%lld\n", prefix, out->first, waited);
  if (out->deadline && out->end < out->deadline) {
    printf(
        "SHORT: the wait ended %lld ns before its deadline\n",
        (long long)(out->deadline - out->end));
    return 1;
  }
  return out->failed;
}

static struct outcome waiter_outcomes[WAITERS];

static void *waiter(void *arg) {
  long index = (long)arg;
  waiter_outcomes[index] = do_condwait(WAIT_NS + index * 1000000LL);
  return NULL;
}

int main(int argc, char **argv) {
  const char *mode = argc == 2 ? argv[1] : "";
  const char *modes[] = {"sleep", "clock-rel", "clock-abs", "futex", "condwait", "multi"};
  int known = 0;
  for (size_t i = 0; i < sizeof(modes) / sizeof(modes[0]); i++) {
    known |= !strcmp(mode, modes[i]);
  }
  if (!known) {
    fprintf(
        stderr,
        "usage: %s <sleep|clock-rel|clock-abs|futex|condwait|multi>\n",
        argv[0]);
    return 2;
  }
  pthread_condattr_t attr;
  pthread_condattr_init(&attr);
  pthread_condattr_setclock(&attr, CLOCK_MONOTONIC);
  pthread_cond_init(&cv, &attr);
  pthread_t waiters[WAITERS];
  if (!strcmp(mode, "multi")) {
    for (long i = 0; i < WAITERS; i++) {
      if (pthread_create(&waiters[i], NULL, waiter, (void *)i)) {
        perror("pthread_create");
        return 2;
      }
    }
  }
  pthread_t thread;
  if (pthread_create(&thread, NULL, worker, NULL)) {
    perror("pthread_create");
    return 2;
  }
  int broken = 0;
  if (!strcmp(mode, "multi")) {
    for (long i = 0; i < WAITERS; i++) {
      pthread_join(waiters[i], NULL);
    }
    pthread_join(thread, NULL);
    for (long i = 0; i < WAITERS; i++) {
      char prefix[32];
      snprintf(prefix, sizeof(prefix), "waiter=%ld ", i);
      broken |= report(prefix, &waiter_outcomes[i]);
    }
    return broken;
  }
  struct outcome out;
  if (!strcmp(mode, "sleep")) {
    out = do_sleep((clockid_t)-1, 0);
  } else if (!strcmp(mode, "clock-rel")) {
    out = do_sleep(CLOCK_MONOTONIC, 0);
  } else if (!strcmp(mode, "clock-abs")) {
    out = do_sleep(CLOCK_MONOTONIC, 1);
  } else if (!strcmp(mode, "futex")) {
    out = do_futex();
  } else {
    out = do_condwait(WAIT_NS);
  }
  pthread_join(thread, NULL);
  return report("", &out);
}
