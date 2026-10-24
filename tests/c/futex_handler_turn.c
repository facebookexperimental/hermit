/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * futex_handler_turn: whether a signal handler's own scheduling point ends
 * the window in which the next delivery continues it (Codex's witness in the
 * review of https://github.com/rrnewton/hermit/pull/4510 at 9012f37e).
 *
 * A worker blocked in read() takes SIGUSR1; handler A blocks SIGUSR2 while it
 * runs. SIGUSR2 is pending when A returns, so it is delivered on that return.
 * The main thread loops in sched_yield() and publishes 1 once A has finished;
 * handler B records what it reads.
 *
 *   wait    A sleeps in FUTEX_WAIT until the main thread sends SIGUSR2 and
 *           wakes it: a real scheduler hand-off inside the handler. B's
 *           delivery is then an ordinary one, behind the main thread's older
 *           sched_yield, which publishes first: publication=1.
 *   nowait  The main thread sends both signals before the worker runs, and A
 *           makes no syscall. B's delivery continues A's, before the main
 *           thread's next turn: publication=0.
 *   sleep   As wait, but A waits with nanosleep(100 us) until the main thread
 *           has sent SIGUSR2 and set the word: on KVM a parked-wait site, on
 *           ptrace an ordinary sleep request. publication=1.
 *   alarm   A sleeps in nanosleep(1 s); the main thread, which blocks SIGALRM,
 *           arms a 1 us ITIMER_REAL, makes 100 getpid calls and loops in
 *           sched_yield, publishing once A has started. SIGALRM interrupts
 *           A's sleep and runs handler B nested in A (B handles SIGALRM, with
 *           no mask). On KVM the sleep is a parked wait that the scheduler
 *           resumes to observe the signal, which is a hand-off: B's delivery
 *           waits behind the main thread's older sched_yield, publication=1
 *           (codex-sol's witness at 576cf05f).
 *   fork    As nowait, but A forks (raw SYS_fork). The parent's continuation
 *           after the fork is a scheduler grant, so B's delivery is again an
 *           ordinary one: publication=1. The child waits on a pipe until B
 *           has made its observation, so its SIGCHLD comes later (Codex's
 *           held-child witness at ae195713).
 *
 * Output: "futex-handler: a=1 b=1 order=ok publication=<0|1>"
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
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <unistd.h>

static _Atomic int entered, word, finished;
static _Atomic int a_count, b_count, wrong_order;
static _Atomic int published, b_seen;
static int waits, sleeps, forks, alarms;
static long child = -1;
static int pipefd[2], forkfd[2];

static void signal_b(int signo) {
  (void)signo;
  atomic_store(&b_seen, atomic_load(&published));
  ++b_count;
  if (a_count != 1 && !alarms) {
    wrong_order = 1;
  }
}

static void signal_a(int signo) {
  (void)signo;
  atomic_store(&entered, 1);
  while (alarms && atomic_load(&word) == 0) {
    struct timespec pause = {1, 0};
    if (nanosleep(&pause, NULL) < 0 && errno != EINTR) {
      _exit(38);
    }
  }
  while (sleeps && atomic_load(&word) == 0) {
    struct timespec pause = {0, 100000};
    if (nanosleep(&pause, NULL) < 0 && errno != EINTR) {
      _exit(37);
    }
  }
  while (waits && atomic_load(&word) == 0) {
    int rc = syscall(SYS_futex, &word, FUTEX_WAIT_PRIVATE, 0, NULL, NULL, 0);
    if (rc < 0 && errno != EAGAIN && errno != EINTR) {
      _exit(31);
    }
  }
  if (forks) {
    child = syscall(SYS_fork);
    if (child < 0) {
      _exit(33);
    }
    if (child == 0) {
      char c;
      syscall(SYS_exit, syscall(SYS_read, forkfd[0], &c, 1) == 1 ? 0 : 36);
    }
  }
  ++a_count;
}

static void* worker(void* ignored) {
  (void)ignored;
  char c;
  while (!atomic_load(&finished)) {
    ssize_t n = read(pipefd[0], &c, 1);
    if (n == 1) {
      break;
    }
    if (n < 0 && errno == EINTR) {
      continue;
    }
    _exit(32);
  }
  return NULL;
}

int main(int argc, char** argv) {
  if (argc != 2 || (strcmp(argv[1], "wait") != 0 && strcmp(argv[1], "nowait") != 0 &&
                    strcmp(argv[1], "fork") != 0 && strcmp(argv[1], "sleep") != 0 &&
                    strcmp(argv[1], "alarm") != 0)) {
    fprintf(stderr, "usage: %s wait|sleep|alarm|nowait|fork\n", argv[0]);
    return 2;
  }
  waits = strcmp(argv[1], "wait") == 0;
  forks = strcmp(argv[1], "fork") == 0;
  sleeps = strcmp(argv[1], "sleep") == 0;
  alarms = strcmp(argv[1], "alarm") == 0;
  struct sigaction a = {.sa_handler = signal_a};
  struct sigaction b = {.sa_handler = signal_b};
  sigemptyset(&a.sa_mask);
  sigaddset(&a.sa_mask, SIGUSR2);
  sigemptyset(&b.sa_mask);
  if (sigaction(SIGUSR1, &a, NULL) || sigaction(alarms ? SIGALRM : SIGUSR2, &b, NULL) ||
      pipe(pipefd) || pipe(forkfd)) {
    return 10;
  }
  pthread_t t;
  if (pthread_create(&t, NULL, worker, NULL)) {
    return 11;
  }
  usleep(1000);
  if (pthread_kill(t, SIGUSR1)) {
    return 12;
  }
  if (alarms) {
    sigset_t blocked;
    sigemptyset(&blocked);
    sigaddset(&blocked, SIGALRM);
    if (pthread_sigmask(SIG_BLOCK, &blocked, NULL)) {
      return 21;
    }
  }
  if (waits || sleeps || alarms) {
    while (!atomic_load(&entered)) {
      sched_yield();
    }
    usleep(1000);
  }
  if (alarms) {
    struct itimerval timer = {.it_value = {.tv_sec = 0, .tv_usec = 1}};
    if (setitimer(ITIMER_REAL, &timer, NULL)) {
      return 22;
    }
    for (int i = 0; i < 100; ++i) {
      (void)syscall(SYS_getpid);
    }
    atomic_store(&word, 1);
  } else if (pthread_kill(t, SIGUSR2)) {
    return 13;
  }
  if (sleeps) {
    atomic_store(&word, 1);
  }
  if (waits) {
    atomic_store(&word, 1);
    if (syscall(SYS_futex, &word, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0) < 0) {
      return 14;
    }
  }
  while (!b_count) {
    sched_yield();
    if (alarms ? atomic_load(&entered) : atomic_load(&a_count)) {
      atomic_store(&published, 1);
    }
  }
  atomic_store(&finished, 1);
  if (forks && (write(forkfd[1], "y", 1) != 1 || waitpid((pid_t)child, NULL, 0) != child)) {
    return 18;
  }
  if (write(pipefd[1], "x", 1) != 1) {
    return 15;
  }
  if (pthread_join(t, NULL)) {
    return 16;
  }
  printf("futex-handler: a=%d b=%d order=%s publication=%d\n", (int)a_count, (int)b_count,
         wrong_order ? "BAD" : "ok", (int)b_seen);
  return a_count == 1 && b_count == 1 && !wrong_order ? 0 : 17;
}
