/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A parent SIGKILLs a forked child that shares a process-shared mutex with
 * it in MAP_SHARED memory, while the child takes and releases the mutex in a
 * loop and a contender thread in the parent competes for it.
 *
 * Usage: sigkill_robust_pshared <robust|plain> [delay_us] [hold_spins]
 *
 * robust: the mutex is PTHREAD_MUTEX_ROBUST. A child killed while it holds
 *   the mutex leaves it marked FUTEX_OWNER_DIED by the kernel's robust-list
 *   walk, and the next locker gets EOWNERDEAD. A contender blocked in
 *   pthread_mutex_lock at the death is woken by that walk.
 * plain: the mutex is not robust. A child killed while it holds the mutex
 *   leaves it held forever, so every lock is bounded by an absolute
 *   CLOCK_REALTIME deadline and reports ETIMEDOUT instead of hanging.
 *
 * The parent prints what it observed after reaping the child: whether the
 * child died inside its critical section, how many cycles it completed, the
 * owner-died and waiters bits of the mutex word, the results of the contender,
 * which keeps locking until the parent has reaped the child, and the parent's
 * own final lock. Natively these depend on where the kill lands;
 * under Hermit's strict mode they must be a pure function of the seed.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

struct shared {
  pthread_mutex_t mutex;
  _Atomic long child_cycles;
  _Atomic int child_holding;
  _Atomic int child_ready;
  long guarded;
};

struct contender_result {
  int locked;
  int owner_dead;
  int timed_out;
  int not_recoverable;
};

static struct shared* state;
static int robust;
static long hold_spins = 2000;
static atomic_int contender_started;
static atomic_int contender_stop;

static void die(const char* what) {
  perror(what);
  _exit(2);
}

static void spin(long n) {
  for (volatile long i = 0; i < n; ++i) {
  }
}

/* An absolute CLOCK_REALTIME deadline `ms` milliseconds from now. */
static struct timespec deadline_ms(long ms) {
  struct timespec ts;
  clock_gettime(CLOCK_REALTIME, &ts);
  ts.tv_sec += ms / 1000;
  ts.tv_nsec += (ms % 1000) * 1000000L;
  if (ts.tv_nsec >= 1000000000L) {
    ts.tv_sec += 1;
    ts.tv_nsec -= 1000000000L;
  }
  return ts;
}

/* Lock the shared mutex. Robust mode blocks; plain mode waits at most
 * `ms` milliseconds. EOWNERDEAD is made consistent so the next lock works. */
static int lock_shared(long ms) {
  int rc;
  if (robust) {
    rc = pthread_mutex_lock(&state->mutex);
  } else {
    struct timespec ts = deadline_ms(ms);
    rc = pthread_mutex_timedlock(&state->mutex, &ts);
  }
  if (rc == EOWNERDEAD && pthread_mutex_consistent(&state->mutex) != 0) {
    die("pthread_mutex_consistent");
  }
  return rc;
}

static void child_loop(void) {
  atomic_store(&state->child_ready, 1);
  for (;;) {
    int rc = lock_shared(1);
    if (rc != 0 && rc != EOWNERDEAD) {
      continue;
    }
    atomic_store(&state->child_holding, 1);
    state->guarded++;
    spin(hold_spins);
    atomic_store(&state->child_holding, 0);
    if (pthread_mutex_unlock(&state->mutex) != 0) {
      die("child unlock");
    }
    atomic_fetch_add(&state->child_cycles, 1);
    spin(hold_spins / 4);
  }
}

static void* contender(void* arg) {
  struct contender_result* r = arg;
  atomic_store(&contender_started, 1);
  while (!atomic_load(&contender_stop)) {
    int rc = lock_shared(20);
    if (rc == ETIMEDOUT) {
      r->timed_out++;
      continue;
    }
    if (rc == ENOTRECOVERABLE) {
      r->not_recoverable++;
      continue;
    }
    if (rc == EOWNERDEAD) {
      r->owner_dead++;
    } else if (rc != 0) {
      fprintf(stderr, "contender lock: %s\n", strerror(rc));
      _exit(2);
    }
    r->locked++;
    state->guarded++;
    spin(hold_spins / 2);
    if (pthread_mutex_unlock(&state->mutex) != 0) {
      die("contender unlock");
    }
  }
  return NULL;
}

int main(int argc, char** argv) {
  if (argc < 2 ||
      (strcmp(argv[1], "robust") != 0 && strcmp(argv[1], "plain") != 0)) {
    fprintf(stderr, "usage: %s <robust|plain> [delay_us] [hold_spins]\n",
            argv[0]);
    return 2;
  }
  robust = strcmp(argv[1], "robust") == 0;
  long delay_us = argc > 2 ? atol(argv[2]) : 3000;
  if (argc > 3) {
    hold_spins = atol(argv[3]);
  }

  state = mmap(NULL, sizeof(*state), PROT_READ | PROT_WRITE,
               MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  if (state == MAP_FAILED) {
    die("mmap");
  }
  pthread_mutexattr_t attr;
  if (pthread_mutexattr_init(&attr) != 0 ||
      pthread_mutexattr_setpshared(&attr, PTHREAD_PROCESS_SHARED) != 0 ||
      (robust &&
       pthread_mutexattr_setrobust(&attr, PTHREAD_MUTEX_ROBUST) != 0) ||
      pthread_mutex_init(&state->mutex, &attr) != 0) {
    die("mutex init");
  }
  pthread_mutexattr_destroy(&attr);

  pid_t child = fork();
  if (child < 0) {
    die("fork");
  }
  if (child == 0) {
    child_loop();
  }
  while (!atomic_load(&state->child_ready)) {
    sched_yield();
  }

  struct contender_result result = {0};
  pthread_t thread;
  if (pthread_create(&thread, NULL, contender, &result) != 0) {
    die("pthread_create");
  }
  while (!atomic_load(&contender_started)) {
    sched_yield();
  }
  usleep(delay_us);

  if (kill(child, SIGKILL) != 0) {
    die("kill");
  }
  int status = 0;
  if (waitpid(child, &status, 0) != child) {
    die("waitpid");
  }
  /* Read right after the reap: the kernel's robust-list walk ran before the
   * child became a zombie, so the word already shows it. */
  unsigned word = atomic_load((_Atomic unsigned*)&state->mutex.__data.__lock);
  int died_holding = atomic_load(&state->child_holding);
  long cycles = atomic_load(&state->child_cycles);

  atomic_store(&contender_stop, 1);
  if (pthread_join(thread, NULL) != 0) {
    die("pthread_join");
  }
  int final_rc = lock_shared(20);
  printf(
      "mode=%s signaled=%d sig=%d died_holding=%d child_cycles=%ld "
      "word_owner_died=%d word_waiters=%d word_tid_set=%d "
      "contender_locked=%d contender_owner_dead=%d contender_timed_out=%d "
      "contender_not_recoverable=%d final_lock=%s guarded=%ld\n",
      argv[1], WIFSIGNALED(status), WIFSIGNALED(status) ? WTERMSIG(status) : 0,
      died_holding, cycles, (word & FUTEX_OWNER_DIED) != 0,
      (word & FUTEX_WAITERS) != 0, (word & FUTEX_TID_MASK) != 0,
      result.locked, result.owner_dead, result.timed_out,
      result.not_recoverable,
      final_rc == 0             ? "ok"
          : final_rc == EOWNERDEAD ? "EOWNERDEAD"
          : final_rc == ETIMEDOUT  ? "ETIMEDOUT"
                                   : strerror(final_rc),
      state->guarded);
  return 0;
}
