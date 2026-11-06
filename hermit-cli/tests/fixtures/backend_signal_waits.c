/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Waits under a temporary signal mask, ended the ways Linux ends them
 * (SIGSHARE S2b). Every mode prints one line whose content is the same
 * natively and under Hermit, on every backend; nothing prints a time.
 *
 *   timer:         SIGALRM is caught and blocked outside the call; a one-shot
 *                  alarm ends sigsuspend(empty): -1 EINTR, one handler run.
 *   posix-timer:   a process-directed POSIX timer (SIGEV_SIGNAL, SIGUSR1)
 *                  ends sigsuspend(empty) the same way.
 *   two-waiters:   two threads wait in sigsuspend(empty) with SIGUSR1 and
 *                  SIGUSR2 blocked outside the call; a process-directed POSIX
 *                  timer sends SIGUSR1 once, then the main thread sends SIGUSR2
 *                  to each waiter. SIGUSR1's handler runs exactly once, and
 *                  both waits end with EINTR.
 *   ignored-let-in: SIGWINCH (default-ignored) is pending while blocked; the
 *                  temporary mask lets it in. Linux discards it and the wait
 *                  goes on until a sibling's SIGUSR1 ends it; SIGWINCH is no
 *                  longer pending afterwards.
 *   blocked-by-mask: SIGUSR1 is caught and unblocked outside the call, but the
 *                  temporary mask blocks it. A sibling's SIGUSR1 stays pending
 *                  without ending the wait; its SIGUSR2 ends it.
 *   fatal:         a forked child waits in sigsuspend(empty) with SIGTERM at
 *                  its default; its sibling thread sends it SIGTERM, which
 *                  ends the child.
 *   sigchld:       SIGCHLD is caught; the process waits in sigsuspend(empty)
 *                  until its handler saw a forked child's exit.
 *   ppoll-timer:   ppoll(NULL, 0, NULL, &empty) with SIGALRM caught and
 *                  blocked outside: a one-shot alarm ends it, -1 EINTR.
 *   ppoll-sibling: the same ppoll, ended by a sibling's SIGUSR1.
 *   ppoll-ignored: a 20 ms ppoll whose temporary mask lets in a pending,
 *                  default-ignored SIGWINCH: it times out (0), and SIGWINCH is
 *                  no longer pending.
 *   mixed:         SIGWINCH (default-ignored) and SIGUSR1 are queued on the
 *                  thread while blocked; SIGUSR1's handler blocks SIGWINCH
 *                  (sa_mask). sigsuspend(empty): Linux dequeues SIGUSR1 (10)
 *                  before SIGWINCH (28) and runs the handler, under whose mask
 *                  SIGWINCH stays pending, and it is still pending after the
 *                  call restores the thread's mask.
 *   ignored-first: the same with SIGPWR (30) for SIGUSR1: Linux dequeues
 *                  SIGWINCH first and discards it, then runs SIGPWR's handler.
 *   late-ignored:  a sibling sends SIGWINCH (default-ignored) while the main
 *                  thread waits in sigsuspend(empty), later installs a
 *                  SIGWINCH handler, then sends SIGUSR1. Linux discarded
 *                  SIGWINCH when it was sent (the temporary mask let it in),
 *                  so only SIGUSR1's handler runs.
 *   late-ignored-same-turn: the same, with the handler installed and SIGUSR1
 *                  sent at once after SIGWINCH.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t usr1_runs;
static volatile sig_atomic_t usr2_runs;
static volatile sig_atomic_t alarm_runs;
static volatile sig_atomic_t child_exited;
static pid_t waiter_tid;
static pid_t waiter_tids[2];
static atomic_int waiting;

static void on_usr1(int signo) {
  (void)signo;
  usr1_runs++;
}

static void on_usr2(int signo) {
  (void)signo;
  usr2_runs++;
}

static void on_alarm(int signo) {
  (void)signo;
  alarm_runs++;
}

static void on_chld(int signo) {
  (void)signo;
  child_exited = 1;
}

static void install(int signo, void (*handler)(int)) {
  struct sigaction sa;
  memset(&sa, 0, sizeof(sa));
  sigemptyset(&sa.sa_mask);
  sa.sa_handler = handler;
  sigaction(signo, &sa, NULL);
}

static void block(int how, int signo) {
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, signo);
  sigprocmask(how, &set, NULL);
}

static void sleep_ms(long ms) {
  struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
  nanosleep(&ts, NULL);
}

static int pending(int signo) {
  sigset_t set;
  sigemptyset(&set);
  sigpending(&set);
  return sigismember(&set, signo);
}

static int suspend_empty(void) {
  sigset_t empty;
  sigemptyset(&empty);
  errno = 0;
  int rc = sigsuspend(&empty);
  return rc == -1 && errno == EINTR;
}

static void* send_to_waiter(void* arg) {
  int signo = (int)(long)arg;
  sleep_ms(50);
  syscall(SYS_tgkill, getpid(), waiter_tid, signo);
  return NULL;
}

static void* usr1_then_usr2(void* arg) {
  (void)arg;
  sleep_ms(50);
  syscall(SYS_tgkill, getpid(), waiter_tid, SIGUSR1);
  sleep_ms(50);
  syscall(SYS_tgkill, getpid(), waiter_tid, SIGUSR2);
  return NULL;
}

static int timer(void) {
  install(SIGALRM, on_alarm);
  block(SIG_BLOCK, SIGALRM);
  struct itimerval it = {{0, 0}, {0, 20000}};
  setitimer(ITIMER_REAL, &it, NULL);
  int eintr = suspend_empty();
  printf("timer eintr=%d alarm_runs=%d\n", eintr, (int)alarm_runs);
  return 0;
}

static int posix_timer(void) {
  install(SIGUSR1, on_usr1);
  block(SIG_BLOCK, SIGUSR1);
  struct sigevent sev;
  memset(&sev, 0, sizeof(sev));
  sev.sigev_notify = SIGEV_SIGNAL;
  sev.sigev_signo = SIGUSR1;
  timer_t id;
  if (timer_create(CLOCK_MONOTONIC, &sev, &id) != 0) {
    puts("posix-timer TIMER_CREATE_FAILED");
    return 1;
  }
  struct itimerspec its = {{0, 0}, {0, 20000000}};
  timer_settime(id, 0, &its, NULL);
  int eintr = suspend_empty();
  timer_delete(id);
  printf("posix-timer eintr=%d usr1_runs=%d\n", eintr, (int)usr1_runs);
  return 0;
}

static int two_eintr[2];

static void* two_waiter(void* arg) {
  long index = (long)arg;
  waiter_tids[index] = (pid_t)syscall(SYS_gettid);
  atomic_fetch_add(&waiting, 1);
  two_eintr[index] = suspend_empty();
  return NULL;
}

static int two_waiters(void) {
  install(SIGUSR1, on_usr1);
  install(SIGUSR2, on_usr2);
  block(SIG_BLOCK, SIGUSR1);
  block(SIG_BLOCK, SIGUSR2);
  pthread_t threads[2];
  for (long i = 0; i < 2; i++) {
    pthread_create(&threads[i], NULL, two_waiter, (void*)i);
  }
  while (atomic_load(&waiting) < 2) {
    sleep_ms(1);
  }
  struct sigevent sev;
  memset(&sev, 0, sizeof(sev));
  sev.sigev_notify = SIGEV_SIGNAL;
  sev.sigev_signo = SIGUSR1;
  timer_t id;
  if (timer_create(CLOCK_MONOTONIC, &sev, &id) != 0) {
    puts("two-waiters TIMER_CREATE_FAILED");
    return 1;
  }
  struct itimerspec its = {{0, 0}, {0, 50000000}};
  timer_settime(id, 0, &its, NULL);
  sleep_ms(100);
  timer_delete(id);
  for (int i = 0; i < 2; i++) {
    syscall(SYS_tgkill, getpid(), waiter_tids[i], SIGUSR2);
  }
  for (int i = 0; i < 2; i++) {
    pthread_join(threads[i], NULL);
  }
  printf(
      "two-waiters eintr=%d,%d usr1_runs=%d\n",
      two_eintr[0],
      two_eintr[1],
      (int)usr1_runs);
  return 0;
}

static int ignored_let_in(void) {
  install(SIGUSR1, on_usr1);
  block(SIG_BLOCK, SIGUSR1);
  block(SIG_BLOCK, SIGWINCH);
  raise(SIGWINCH);
  int before = pending(SIGWINCH);
  waiter_tid = (pid_t)syscall(SYS_gettid);
  pthread_t thread;
  pthread_create(&thread, NULL, send_to_waiter, (void*)(long)SIGUSR1);
  int eintr = suspend_empty();
  pthread_join(thread, NULL);
  printf(
      "ignored-let-in before=%d eintr=%d usr1_runs=%d winch_after=%d\n",
      before,
      eintr,
      (int)usr1_runs,
      pending(SIGWINCH));
  return 0;
}

static int blocked_by_mask(void) {
  install(SIGUSR1, on_usr1);
  install(SIGUSR2, on_usr2);
  block(SIG_BLOCK, SIGUSR2);
  waiter_tid = (pid_t)syscall(SYS_gettid);
  pthread_t thread;
  pthread_create(&thread, NULL, usr1_then_usr2, NULL);
  sigset_t mask;
  sigemptyset(&mask);
  sigaddset(&mask, SIGUSR1);
  errno = 0;
  int rc = sigsuspend(&mask);
  int eintr = rc == -1 && errno == EINTR;
  int usr1_in_wait = usr1_runs;
  int usr2_in_wait = usr2_runs;
  pthread_join(thread, NULL);
  printf(
      "blocked-by-mask eintr=%d usr1_in_wait=%d usr2_in_wait=%d usr1_runs=%d\n",
      eintr,
      usr1_in_wait,
      usr2_in_wait,
      (int)usr1_runs);
  return 0;
}

static int fatal(void) {
  pid_t child = fork();
  if (child == 0) {
    waiter_tid = (pid_t)syscall(SYS_gettid);
    pthread_t thread;
    pthread_create(&thread, NULL, send_to_waiter, (void*)(long)SIGTERM);
    suspend_empty();
    _exit(9);
  }
  int status = 0;
  waitpid(child, &status, 0);
  printf(
      "fatal signaled=%d termsig=%d\n",
      WIFSIGNALED(status),
      WIFSIGNALED(status) ? WTERMSIG(status) : 0);
  return 0;
}

static int sigchld(void) {
  install(SIGCHLD, on_chld);
  block(SIG_BLOCK, SIGCHLD);
  pid_t child = fork();
  if (child == 0) {
    sleep_ms(20);
    _exit(7);
  }
  sigset_t empty;
  sigemptyset(&empty);
  while (!child_exited) {
    sigsuspend(&empty);
  }
  int status = 0;
  waitpid(child, &status, 0);
  printf("sigchld exited=%d status=%d\n", (int)child_exited, WEXITSTATUS(status));
  return 0;
}

static int masked_ppoll(const struct timespec* timeout) {
  sigset_t empty;
  sigemptyset(&empty);
  errno = 0;
  int rc = ppoll(NULL, 0, timeout, &empty);
  return rc == -1 ? (errno == EINTR ? -4 : -1) : rc;
}

static int ppoll_timer(void) {
  install(SIGALRM, on_alarm);
  block(SIG_BLOCK, SIGALRM);
  struct itimerval it = {{0, 0}, {0, 20000}};
  setitimer(ITIMER_REAL, &it, NULL);
  int rc = masked_ppoll(NULL);
  printf("ppoll-timer rc=%d alarm_runs=%d\n", rc, (int)alarm_runs);
  return 0;
}

static int ppoll_sibling(void) {
  install(SIGUSR1, on_usr1);
  block(SIG_BLOCK, SIGUSR1);
  waiter_tid = (pid_t)syscall(SYS_gettid);
  pthread_t thread;
  pthread_create(&thread, NULL, send_to_waiter, (void*)(long)SIGUSR1);
  int rc = masked_ppoll(NULL);
  pthread_join(thread, NULL);
  printf("ppoll-sibling rc=%d usr1_runs=%d\n", rc, (int)usr1_runs);
  return 0;
}

static int ppoll_ignored(void) {
  block(SIG_BLOCK, SIGWINCH);
  raise(SIGWINCH);
  int before = pending(SIGWINCH);
  struct timespec timeout = {0, 20000000};
  int rc = masked_ppoll(&timeout);
  printf(
      "ppoll-ignored before=%d rc=%d winch_after=%d\n",
      before,
      rc,
      pending(SIGWINCH));
  return 0;
}

static volatile sig_atomic_t winch_runs;
static volatile sig_atomic_t winch_in_handler = -1;
static volatile sig_atomic_t winch_blocked_in_handler = -1;

static void note_winch(int signo) {
  (void)signo;
  sigset_t set;
  sigemptyset(&set);
  sigpending(&set);
  winch_in_handler = sigismember(&set, SIGWINCH);
  sigprocmask(SIG_SETMASK, NULL, &set);
  winch_blocked_in_handler = sigismember(&set, SIGWINCH);
  if (signo == SIGUSR1) {
    usr1_runs++;
  }
}

static void on_winch(int signo) {
  (void)signo;
  winch_runs++;
}

/* SIGWINCH and `caught` queued on the thread while both are blocked;
   `caught`'s handler blocks SIGWINCH; then sigsuspend(empty). */
static int queued_pair(const char* name, int caught) {
  block(SIG_BLOCK, SIGWINCH);
  block(SIG_BLOCK, caught);
  struct sigaction sa;
  memset(&sa, 0, sizeof(sa));
  sigemptyset(&sa.sa_mask);
  sigaddset(&sa.sa_mask, SIGWINCH);
  sa.sa_handler = note_winch;
  sigaction(caught, &sa, NULL);
  raise(SIGWINCH);
  raise(caught);
  int before = pending(SIGWINCH);
  int eintr = suspend_empty();
  sigset_t now;
  sigprocmask(SIG_SETMASK, NULL, &now);
  printf(
      "%s before=%d eintr=%d winch_in_handler=%d winch_blocked_in_handler=%d "
      "winch_after=%d mask_restored=%d\n",
      name,
      before,
      eintr,
      (int)winch_in_handler,
      (int)winch_blocked_in_handler,
      pending(SIGWINCH),
      sigismember(&now, SIGWINCH) == 1 && sigismember(&now, caught) == 1);
  return 0;
}

static int late_same_turn;

static void* late_sender(void* arg) {
  (void)arg;
  while (!atomic_load(&waiting)) {
    sched_yield();
  }
  sleep_ms(50);
  syscall(SYS_tgkill, getpid(), waiter_tid, SIGWINCH);
  if (!late_same_turn) {
    sleep_ms(50);
  }
  install(SIGWINCH, on_winch);
  syscall(SYS_tgkill, getpid(), waiter_tid, SIGUSR1);
  return NULL;
}

static int late_ignored(const char* name, int same_turn) {
  late_same_turn = same_turn;
  install(SIGUSR1, on_usr1);
  install(SIGWINCH, SIG_DFL);
  block(SIG_BLOCK, SIGUSR1);
  block(SIG_BLOCK, SIGWINCH);
  waiter_tid = (pid_t)syscall(SYS_gettid);
  pthread_t thread;
  pthread_create(&thread, NULL, late_sender, NULL);
  atomic_store(&waiting, 1);
  int eintr = suspend_empty();
  pthread_join(thread, NULL);
  printf(
      "%s eintr=%d usr1_runs=%d winch_runs=%d winch_after=%d\n",
      name,
      eintr,
      (int)usr1_runs,
      (int)winch_runs,
      pending(SIGWINCH));
  return 0;
}

int main(int argc, char** argv) {
  if (argc != 2) {
    puts("BACKEND_SIGNAL_WAITS_USAGE");
    return 2;
  }
  const char* mode = argv[1];
  if (strcmp(mode, "timer") == 0) {
    return timer();
  } else if (strcmp(mode, "posix-timer") == 0) {
    return posix_timer();
  } else if (strcmp(mode, "two-waiters") == 0) {
    return two_waiters();
  } else if (strcmp(mode, "ignored-let-in") == 0) {
    return ignored_let_in();
  } else if (strcmp(mode, "blocked-by-mask") == 0) {
    return blocked_by_mask();
  } else if (strcmp(mode, "fatal") == 0) {
    return fatal();
  } else if (strcmp(mode, "sigchld") == 0) {
    return sigchld();
  } else if (strcmp(mode, "ppoll-timer") == 0) {
    return ppoll_timer();
  } else if (strcmp(mode, "ppoll-sibling") == 0) {
    return ppoll_sibling();
  } else if (strcmp(mode, "ppoll-ignored") == 0) {
    return ppoll_ignored();
  } else if (strcmp(mode, "mixed") == 0) {
    return queued_pair("mixed", SIGUSR1);
  } else if (strcmp(mode, "ignored-first") == 0) {
    return queued_pair("ignored-first", SIGPWR);
  } else if (strcmp(mode, "late-ignored") == 0) {
    return late_ignored("late-ignored", 0);
  } else if (strcmp(mode, "late-ignored-same-turn") == 0) {
    return late_ignored("late-ignored-same-turn", 1);
  }
  puts("BACKEND_SIGNAL_WAITS_USAGE");
  return 2;
}
