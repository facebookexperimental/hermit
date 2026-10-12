/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Guest for hermit-cli/tests/cli.rs: exec discards a POSIX timer's queued
 * signal and keeps every other pending signal (flush_itimer_signals;
 * https://github.com/rrnewton/hermit/issues/4184).
 *
 * usage: timer_exec_flush <mode>
 *
 * The process blocks the signal, leaves a copy of it pending, and execs
 * itself; the new image reports whether the signal is pending, then installs a
 * handler and unblocks it.
 *
 *   thread:          a SIGEV_THREAD_ID timer's SIGUSR1 (flushed).
 *   process:         a SIGEV_SIGNAL timer's SIGUSR1 (flushed).
 *   kill:            the process's own kill(SIGUSR1) (kept).
 *   both:            a kill copy pending before a SIGEV_SIGNAL timer expires,
 *                    which Linux counts as an overrun: the kill copy is kept.
 *   taken_then_kill: the timer's copy is taken by a handler, then a kill copy
 *                    is left pending (kept).
 *   rt_process:      a SIGEV_SIGNAL timer's SIGRTMIN (flushed).
 *   sfd_process:     as process, with a signalfd covering SIGUSR1 open, which
 *                    could take the timer's copy unseen (Hermit refuses).
 *   worker_timer:    a worker's SIGEV_THREAD_ID copy pending on the worker,
 *                    which execs (a non-leader exec: flushed).
 *   leader_exec:     the main thread's SIGEV_THREAD_ID copy pending on it; a
 *                    worker tgkills itself and execs (the leader's queue goes
 *                    with it, the worker's own copy is kept).
 *   stw_taken:       a SIGEV_THREAD_ID copy taken by sigtimedwait, then the
 *                    thread's own tgkill copy (kept).
 *   sfd_thread:      a SIGEV_THREAD_ID copy read by a signalfd opened after it
 *                    expired, then the thread's own tgkill copy: Linux keeps
 *                    the tgkill copy (Hermit refuses).
 *   stw_other:       a SIGEV_THREAD_ID SIGUSR2 copy left pending by a
 *                    sigtimedwait for SIGUSR1 or SIGUSR2 that took the
 *                    thread's own SIGUSR1 first: Linux flushes it (Hermit
 *                    refuses: the call could have taken it).
 *   cont_flush:      a SIGEV_THREAD_ID SIGTSTP copy flushed by the process's
 *                    own SIGCONT, then its own tgkill SIGTSTP: Linux keeps it
 *                    (Hermit refuses).
 *   cont_flush_process: a SIGEV_SIGNAL SIGTSTP copy flushed by the process's
 *                    own SIGCONT, then its own kill(SIGTSTP): Linux keeps the
 *                    kill copy (Hermit refuses).
 *   shared_sfd:      a signalfd for SIGUSR2 is inherited by a child, which
 *                    changes the shared description to SIGUSR1; the parent
 *                    reads its SIGEV_SIGNAL copy through it, then sends its own
 *                    kill(SIGUSR1): Linux keeps the kill copy (Hermit refuses).
 *   shared_sfd_no_timer: the same without the timer (kept).
 *   dead_armer:      a worker arms a SIGEV_SIGNAL timer and exits before it
 *                    expires (checked: the worker is gone and the signal not
 *                    yet pending); the leader checks the copy is pending and
 *                    execs (flushed).
 */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/signalfd.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t hits;
static void on_signal(int signo) {
  (void)signo;
  hits++;
}

static void block(int signo, int how) {
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, signo);
  sigprocmask(how, &set, NULL);
}

/* Arm a one-shot timer for `signo` and let it expire. */
static int expire(int signo, int thread) {
  struct sigevent event;
  memset(&event, 0, sizeof event);
  event.sigev_signo = signo;
  if (thread) {
    event.sigev_notify = SIGEV_THREAD_ID;
    event._sigev_un._tid = (int)syscall(SYS_gettid);
  } else {
    event.sigev_notify = SIGEV_SIGNAL;
  }
  int id;
  if (syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &id) != 0) return 75;
  struct itimerspec spec;
  memset(&spec, 0, sizeof spec);
  spec.it_value.tv_nsec = 1000;
  if (syscall(SYS_timer_settime, id, 0, &spec, NULL) != 0) return 76;
  struct timespec wait = {0, 3000000};
  nanosleep(&wait, NULL);
  return 0;
}

static const char *exec_mode;
static char *exec_self;

static void tgkill_self(int signo) {
  syscall(SYS_tgkill, getpid(), (int)syscall(SYS_gettid), signo);
}

/* A worker that arms a SIGEV_SIGNAL timer for SIGUSR1 and exits before it
   expires. */
static volatile pid_t armer_tid;
static void *arming_worker(void *arg) {
  (void)arg;
  armer_tid = (pid_t)syscall(SYS_gettid);
  struct sigevent event;
  memset(&event, 0, sizeof event);
  event.sigev_signo = SIGUSR1;
  event.sigev_notify = SIGEV_SIGNAL;
  int id;
  if (syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &id) != 0) _exit(75);
  struct itimerspec spec;
  memset(&spec, 0, sizeof spec);
  spec.it_value.tv_nsec = 20000000;
  if (syscall(SYS_timer_settime, id, 0, &spec, NULL) != 0) _exit(76);
  return NULL;
}

/* A worker that leaves a copy of SIGUSR1 pending on itself and execs. */
static void *exec_worker(void *arg) {
  (void)arg;
  if (!strcmp(exec_mode, "worker_timer")) {
    if (expire(SIGUSR1, 1)) _exit(80);
  } else {
    tgkill_self(SIGUSR1);
  }
  execl("/proc/self/exe", exec_self, exec_mode, "after", (char *)NULL);
  _exit(78);
}

int main(int argc, char **argv) {
  const char *mode = argc > 1 ? argv[1] : "thread";
  int signo = !strcmp(mode, "rt_process")   ? SIGRTMIN
              : !strcmp(mode, "cont_flush") || !strcmp(mode, "cont_flush_process") ? SIGTSTP
              : !strcmp(mode, "stw_other")  ? SIGUSR2
                                            : SIGUSR1;
  if (argc > 2) {
    /* The new image: the mask survives exec, so the signal is still blocked. */
    sigset_t pending;
    sigpending(&pending);
    int was_pending = sigismember(&pending, signo);
    signal(signo, on_signal);
    block(signo, SIG_UNBLOCK);
    printf("%s pending_after_exec=%d hits=%d\n", mode, was_pending, (int)hits);
    return 0;
  }
  signal(signo, on_signal);
  block(signo, SIG_BLOCK);
  int rc = 0;
  if (!strcmp(mode, "kill")) {
    kill(getpid(), signo);
  } else if (!strcmp(mode, "both")) {
    kill(getpid(), signo);
    rc = expire(signo, 0);
  } else if (!strcmp(mode, "taken_then_kill")) {
    rc = expire(signo, 0);
    block(signo, SIG_UNBLOCK);
    block(signo, SIG_BLOCK);
    printf("taken_then_kill before_exec hits=%d\n", (int)hits);
    fflush(stdout);
    kill(getpid(), signo);
  } else if (!strcmp(mode, "worker_timer") || !strcmp(mode, "leader_exec")) {
    if (!strcmp(mode, "leader_exec") && (rc = expire(signo, 1))) return rc;
    exec_mode = mode;
    exec_self = argv[0];
    pthread_t worker;
    pthread_create(&worker, NULL, exec_worker, NULL);
    pthread_join(worker, NULL);
    return 79;
  } else if (!strcmp(mode, "stw_taken")) {
    rc = expire(signo, 1);
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, signo);
    struct timespec zero = {0, 0};
    printf("stw_taken took=%d\n", sigtimedwait(&set, NULL, &zero) == signo);
    fflush(stdout);
    tgkill_self(signo);
  } else if (!strcmp(mode, "stw_other")) {
    block(SIGUSR1, SIG_BLOCK);
    rc = expire(signo, 1);
    tgkill_self(SIGUSR1);
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    sigaddset(&set, signo);
    struct timespec zero = {0, 0};
    if (sigtimedwait(&set, NULL, &zero) != SIGUSR1) return 81;
  } else if (!strcmp(mode, "sfd_thread")) {
    rc = expire(signo, 1);
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, signo);
    int fd = signalfd(-1, &set, SFD_CLOEXEC);
    struct signalfd_siginfo record;
    if (fd < 0 || read(fd, &record, sizeof record) != (ssize_t)sizeof record) return 77;
    tgkill_self(signo);
  } else if (!strcmp(mode, "cont_flush")) {
    rc = expire(signo, 1);
    kill(getpid(), SIGCONT);
    tgkill_self(signo);
  } else if (!strcmp(mode, "shared_sfd") || !strcmp(mode, "shared_sfd_no_timer")) {
    block(SIGUSR2, SIG_BLOCK);
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR2);
    int fd = signalfd(-1, &set, SFD_NONBLOCK);
    if (fd < 0) return 77;
    if (!strcmp(mode, "shared_sfd") && (rc = expire(signo, 0))) return rc;
    if (!strcmp(mode, "shared_sfd_no_timer")) kill(getpid(), signo);
    pid_t child = fork();
    if (child == 0) {
      sigemptyset(&set);
      sigaddset(&set, signo);
      _exit(signalfd(fd, &set, 0) == fd ? 0 : 1);
    }
    int status;
    if (waitpid(child, &status, 0) != child || status != 0) return 82;
    struct signalfd_siginfo record;
    if (read(fd, &record, sizeof record) != (ssize_t)sizeof record) return 83;
    close(fd);
    kill(getpid(), signo);
  } else if (!strcmp(mode, "dead_armer")) {
    pthread_t worker;
    if (pthread_create(&worker, NULL, arming_worker, NULL) != 0) return 84;
    if (pthread_join(worker, NULL) != 0) return 85;
    /* The armer is gone (tgkill finds no such thread) while its timer has
       not yet expired; then the timer's copy is pending at exec. */
    int retired = 0;
    for (int i = 0; i < 100000 && !retired; i++) {
      retired = syscall(SYS_tgkill, getpid(), (int)armer_tid, 0) != 0 && errno == ESRCH;
      if (!retired) sched_yield();
    }
    sigset_t pending;
    sigpending(&pending);
    int pending_at_retirement = sigismember(&pending, signo);
    struct timespec wait = {0, 50000000};
    nanosleep(&wait, NULL);
    sigpending(&pending);
    int pending_before_exec = sigismember(&pending, signo);
    printf("dead_armer retired=%d pending_at_retirement=%d pending_before_exec=%d\n",
           retired, pending_at_retirement, pending_before_exec);
    fflush(stdout);
    if (!retired || pending_at_retirement || !pending_before_exec) return 86;
  } else if (!strcmp(mode, "cont_flush_process")) {
    rc = expire(signo, 0);
    kill(getpid(), SIGCONT);
    kill(getpid(), signo);
  } else if (!strcmp(mode, "sfd_process")) {
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, signo);
    if (signalfd(-1, &set, SFD_CLOEXEC) < 0) return 77;
    rc = expire(signo, 0);
  } else {
    rc = expire(signo, !strcmp(mode, "thread"));
  }
  if (rc) return rc;
  execl("/proc/self/exe", argv[0], mode, "after", (char *)NULL);
  return 78;
}
