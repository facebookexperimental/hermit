/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A guest thread sends SIGCHLD to a sibling asleep in nanosleep or pause
 * while a third thread spins on getpid and sched_yield
 * (https://github.com/rrnewton/hermit/issues/4132). The main thread blocks
 * SIGCHLD; the sleeper unblocks it unless the mode says otherwise.
 *
 *   tgkill-nano, tgkill-pause
 *                  tgkill(sleeper, SIGCHLD), caught: the sleep ends with EINTR
 *                  and the handler runs in the sleeper.
 *   sigqueue-nano  The same with pthread_sigqueue (rt_tgsigqueueinfo).
 *   blocked-nano   The sleeper blocks SIGCHLD: the 300 ms sleep returns 0,
 *                  and the handler runs once it unblocks.
 *   default-nano   SIGCHLD has its default (ignore) action: the sleep
 *                  returns 0, and no handler runs.
 *   flush-nano     The sleeper blocks SIGCHLD; once it is sent, the main
 *                  thread sets SIGCHLD to SIG_IGN and back to the handler,
 *                  which discards the pending copy: the sleep returns 0, and
 *                  no handler runs after the sleeper unblocks.
 *   futex-cond, futex-timed, futex-raw, futex-restart, futex-bitset
 *                  The sibling waits on a futex instead: pthread_cond_wait;
 *                  pthread_cond_timedwait with a 300 ms deadline nothing
 *                  signals (ETIMEDOUT); one raw FUTEX_WAIT (EINTR); the same
 *                  with an SA_RESTART handler (the wait restarts and returns
 *                  0 at the wake); one raw FUTEX_WAIT_BITSET (EINTR). Main
 *                  waits up to 1 s for the handler (100 ms in the three modes
 *                  below, where it does not run first) before it wakes the
 *                  waiter, and prints whether the handler ran in it first.
 *   futex-blocked, futex-default, futex-flush
 *                  As blocked-nano, default-nano and flush-nano, for a
 *                  pthread_cond_wait.
 *   futex-dfl-then-handler
 *                  A raw FUTEX_WAIT; SIGCHLD keeps its default action when
 *                  main sends it, and main installs the handler only after:
 *                  Linux discards the signal at generation, so the wait
 *                  returns 0 at the wake and no handler runs.
 *   futex-cond-busy, futex-restart-busy
 *                  futex-cond and futex-restart with a sibling that spins
 *                  with no syscall, which Hermit can only preempt.
 *   stale, stale-nopriv
 *                  Can a private SIGCHLD record outlive its copy and end a
 *                  later sleep? SIGCHLD caught; a sibling waits in
 *                  sigsuspend(empty mask), so it may take a shared SIGCHLD
 *                  outside the schedule. Unless -nopriv, a helper tgkills
 *                  the main thread with SIGCHLD, and main takes it. Then main
 *                  forks a child that exits at once and sleeps 2 s. The sleep
 *                  ends with EINTR only if main's handler ran for the child's
 *                  SIGCHLD during it (the Claude review of
 *                  https://github.com/rrnewton/hermit/pull/4177). Prints each
 *                  handler run as taker:code[:during].
 */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <linux/futex.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static _Atomic int done, got, ready, sent, in_sleeper;
static _Atomic pid_t sleeper_tid;
static const char* mode;
static int ret, err, got_at_return;

static void on_chld(int sig) {
  (void)sig;
  if ((pid_t)syscall(SYS_gettid) == atomic_load(&sleeper_tid)) {
    atomic_fetch_add(&in_sleeper, 1);
  }
  atomic_fetch_add(&got, 1);
}

static void set_handler(void (*handler)(int)) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = handler;
  sigaction(SIGCHLD, &action, NULL);
}

static void mask(int how) {
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGCHLD);
  pthread_sigmask(how, &set, NULL);
}

static void* spin(void* arg) {
  (void)arg;
  while (!atomic_load(&done)) {
    syscall(SYS_getpid);
    syscall(SYS_sched_yield);
  }
  return NULL;
}

static void* sleeper(void* arg) {
  (void)arg;
  atomic_store(&sleeper_tid, (pid_t)syscall(SYS_gettid));
  int blocked = !strcmp(mode, "blocked-nano") || !strcmp(mode, "flush-nano");
  if (!blocked) {
    mask(SIG_UNBLOCK);
  }
  atomic_store(&ready, 1);
  if (!strcmp(mode, "tgkill-pause")) {
    ret = pause();
  } else {
    struct timespec request = {0, 300 * 1000 * 1000};
    ret = nanosleep(&request, NULL);
  }
  err = errno;
  got_at_return = atomic_load(&in_sleeper);
  if (blocked) {
    while (!atomic_load(&sent)) {
      syscall(SYS_sched_yield);
    }
    mask(SIG_UNBLOCK);
  }
  return NULL;
}

#define STALE_EVENTS 8
static _Atomic int stale_events, stale_stop, stale_ready, stale_sleeping;
static int stale_who[STALE_EVENTS], stale_code[STALE_EVENTS], stale_during[STALE_EVENTS];
static _Atomic pid_t stale_main, stale_sibling;

static void stale_on_chld(int sig, siginfo_t* info, void* context) {
  (void)sig;
  (void)context;
  int k = atomic_fetch_add(&stale_events, 1);
  if (k >= STALE_EVENTS) {
    return;
  }
  pid_t me = (pid_t)syscall(SYS_gettid);
  stale_who[k] = me == atomic_load(&stale_main) ? 'M' : me == atomic_load(&stale_sibling) ? 'U' : '?';
  stale_code[k] = info->si_code;
  stale_during[k] = atomic_load(&stale_sleeping);
}

static void stale_on_usr1(int sig) {
  (void)sig;
}

static void* stale_suspender(void* arg) {
  (void)arg;
  atomic_store(&stale_sibling, (pid_t)syscall(SYS_gettid));
  sigset_t empty;
  sigemptyset(&empty);
  atomic_store(&stale_ready, 1);
  while (!atomic_load(&stale_stop)) {
    sigsuspend(&empty);
  }
  return NULL;
}

static void* stale_helper(void* arg) {
  pthread_kill(*(pthread_t*)arg, SIGCHLD);
  return NULL;
}

static int stale(const char* label) {
  int private_copy = strcmp(label, "stale-nopriv");
  atomic_store(&stale_main, (pid_t)syscall(SYS_gettid));
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_sigaction = stale_on_chld;
  action.sa_flags = SA_SIGINFO;
  sigaction(SIGCHLD, &action, NULL);
  memset(&action, 0, sizeof(action));
  action.sa_handler = stale_on_usr1;
  sigaction(SIGUSR1, &action, NULL);
  // The sibling starts with SIGCHLD blocked, so the private send below can
  // reach only main; its sigsuspend unblocks it.
  mask(SIG_BLOCK);
  pthread_t sibling;
  pthread_create(&sibling, NULL, stale_suspender, NULL);
  mask(SIG_UNBLOCK);
  while (!atomic_load(&stale_ready)) {
    syscall(SYS_sched_yield);
  }
  struct timespec delay = {0, 20 * 1000 * 1000};
  nanosleep(&delay, NULL);
  if (private_copy) {
    pthread_t self = pthread_self(), helper;
    pthread_create(&helper, NULL, stale_helper, &self);
    pthread_join(helper, NULL);
    while (atomic_load(&stale_events) == 0) {
      syscall(SYS_sched_yield);
    }
  }
  pid_t child = fork();
  if (child == 0) {
    _exit(0);
  }
  atomic_store(&stale_sleeping, 1);
  struct timespec request = {2, 0};
  int result = nanosleep(&request, NULL);
  int error = errno;
  atomic_store(&stale_sleeping, 0);
  waitpid(child, NULL, 0);
  nanosleep(&delay, NULL);
  atomic_store(&stale_stop, 1);
  pthread_kill(sibling, SIGUSR1);
  pthread_join(sibling, NULL);
  printf("%s: nanosleep %s |", label, result == 0 ? "0" : error == EINTR ? "EINTR" : strerror(error));
  int events = atomic_load(&stale_events);
  for (int k = 0; k < events && k < STALE_EVENTS; k++) {
    printf(" %c:%s%s", stale_who[k],
           stale_code[k] == SI_TKILL     ? "tkill"
               : stale_code[k] == CLD_EXITED ? "exited"
                                             : "other",
           stale_during[k] ? ":during" : "");
  }
  printf("\n");
  return 0;
}

static pthread_mutex_t fx_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t fx_cond = PTHREAD_COND_INITIALIZER;
static int fx_flag;
static _Atomic int fx_word;
static long fx_ret;

static int is_mode(const char* name) {
  return !strcmp(mode, name);
}

static void* busy(void* arg) {
  (void)arg;
  while (!atomic_load_explicit(&done, memory_order_relaxed)) {
  }
  return NULL;
}

static void* futex_waiter(void* arg) {
  (void)arg;
  atomic_store(&sleeper_tid, (pid_t)syscall(SYS_gettid));
  int blocked = is_mode("futex-blocked") || is_mode("futex-flush");
  if (!blocked) {
    mask(SIG_UNBLOCK);
  }
  if (is_mode("futex-raw") || is_mode("futex-restart") || is_mode("futex-restart-busy") ||
      is_mode("futex-bitset") || is_mode("futex-dfl-then-handler")) {
    atomic_store(&ready, 1);
    int op = is_mode("futex-bitset") ? FUTEX_WAIT_BITSET_PRIVATE : FUTEX_WAIT_PRIVATE;
    fx_ret = syscall(SYS_futex, &fx_word, op, 0, NULL, NULL, FUTEX_BITSET_MATCH_ANY);
    err = errno;
  } else {
    pthread_mutex_lock(&fx_lock);
    atomic_store(&ready, 1);
    if (is_mode("futex-timed")) {
      struct timespec deadline;
      clock_gettime(CLOCK_REALTIME, &deadline);
      deadline.tv_nsec += 300 * 1000 * 1000;
      if (deadline.tv_nsec >= 1000 * 1000 * 1000) {
        deadline.tv_sec += 1;
        deadline.tv_nsec -= 1000 * 1000 * 1000;
      }
      while (!fx_flag && ret != ETIMEDOUT) {
        ret = pthread_cond_timedwait(&fx_cond, &fx_lock, &deadline);
      }
    } else {
      while (!fx_flag) {
        pthread_cond_wait(&fx_cond, &fx_lock);
      }
    }
    pthread_mutex_unlock(&fx_lock);
  }
  if (blocked) {
    while (!atomic_load(&sent)) {
      syscall(SYS_sched_yield);
    }
    mask(SIG_UNBLOCK);
  }
  return NULL;
}

static int futex_main(void) {
  int restart = is_mode("futex-restart") || is_mode("futex-restart-busy");
  int dfl_then_handler = is_mode("futex-dfl-then-handler");
  int raw = restart || dfl_then_handler || is_mode("futex-raw") || is_mode("futex-bitset");
  if (!is_mode("futex-default") && !dfl_then_handler) {
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_handler = on_chld;
    action.sa_flags = restart ? SA_RESTART : 0;
    sigaction(SIGCHLD, &action, NULL);
  }
  mask(SIG_BLOCK);
  pthread_t spinner, thread;
  int cpu_bound = is_mode("futex-cond-busy") || is_mode("futex-restart-busy");
  pthread_create(&spinner, NULL, cpu_bound ? busy : spin, NULL);
  pthread_create(&thread, NULL, futex_waiter, NULL);
  while (!atomic_load(&ready)) {
    syscall(SYS_sched_yield);
  }
  struct timespec delay = {0, 50 * 1000 * 1000};
  nanosleep(&delay, NULL);
  pthread_kill(thread, SIGCHLD);
  if (is_mode("futex-flush")) {
    set_handler(SIG_IGN);
    set_handler(on_chld);
  }
  if (dfl_then_handler) {
    set_handler(on_chld);
  }
  atomic_store(&sent, 1);
  // Up to 1 s where the handler runs before the wake, 100 ms where it
  // does not.
  int handler_expected = !is_mode("futex-blocked") && !is_mode("futex-default") &&
      !is_mode("futex-flush") && !dfl_then_handler;
  struct timespec step = {0, 10 * 1000 * 1000};
  for (int i = 0; i < (handler_expected ? 100 : 10) && !atomic_load(&in_sleeper); i++) {
    nanosleep(&step, NULL);
  }
  int before_wake = atomic_load(&in_sleeper);
  if (restart) {
    // Let the restarted wait park again before the word changes.
    nanosleep(&delay, NULL);
  }
  if (raw) {
    atomic_store(&fx_word, 1);
    syscall(SYS_futex, &fx_word, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0);
  } else if (!is_mode("futex-timed")) {
    pthread_mutex_lock(&fx_lock);
    fx_flag = 1;
    pthread_cond_signal(&fx_cond);
    pthread_mutex_unlock(&fx_lock);
  }
  pthread_join(thread, NULL);
  atomic_store(&done, 1);
  pthread_join(spinner, NULL);
  char result[64];
  if (raw) {
    snprintf(result, sizeof(result), "futex %s",
             fx_ret == 0 ? "0" : err == EINTR ? "-1 EINTR" : err == EAGAIN ? "-1 EAGAIN" : strerror(err));
  } else if (is_mode("futex-timed")) {
    snprintf(result, sizeof(result), "timedwait %s", ret == ETIMEDOUT ? "ETIMEDOUT" : "other");
  } else {
    snprintf(result, sizeof(result), "cond_wait");
  }
  printf("%s: %s, handler in waiter before the wake %d, in waiter %d, in all %d\n", mode, result,
         before_wake, atomic_load(&in_sleeper), atomic_load(&got));
  return 0;
}

int main(int argc, char** argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  mode = argc > 1 ? argv[1] : "tgkill-nano";
  if (!strncmp(mode, "stale", 5)) {
    return stale(mode);
  }
  if (!strncmp(mode, "futex-", 6)) {
    return futex_main();
  }
  if (strcmp(mode, "default-nano")) {
    set_handler(on_chld);
  }
  mask(SIG_BLOCK);
  pthread_t spinner, thread;
  pthread_create(&spinner, NULL, spin, NULL);
  pthread_create(&thread, NULL, sleeper, NULL);
  while (!atomic_load(&ready)) {
    syscall(SYS_sched_yield);
  }
  struct timespec delay = {0, 50 * 1000 * 1000};
  nanosleep(&delay, NULL);
  if (!strcmp(mode, "sigqueue-nano")) {
    union sigval value = {.sival_int = 7};
    pthread_sigqueue(thread, SIGCHLD, value);
  } else {
    pthread_kill(thread, SIGCHLD);
  }
  if (!strcmp(mode, "flush-nano")) {
    set_handler(SIG_IGN);
    set_handler(on_chld);
  }
  atomic_store(&sent, 1);
  pthread_join(thread, NULL);
  atomic_store(&done, 1);
  pthread_join(spinner, NULL);
  printf("%s: %s %s, in sleeper before return %d, in all %d\n", mode,
         strcmp(mode, "tgkill-pause") ? "nanosleep" : "pause",
         ret == 0 ? "0" : (err == EINTR ? "-1 EINTR" : strerror(err)), got_at_return,
         atomic_load(&got));
  return 0;
}
