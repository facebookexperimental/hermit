/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A signalfd in a process with more than one thread. Linux evaluates a
// signalfd for the thread that polls or reads it: its own pending signals and
// its process's (signalfd_poll and signalfd_dequeue, fs/signalfd.c), the
// thread's own first. The process creates the signalfd while a second thread
// exists (as QEMU does after starting its RCU thread) and starts a third while
// it is open. A signal sent to one thread with tgkill is readable by that
// thread only; one queued for the process (a timer's SIGALRM, a child's
// SIGCHLD) by every thread, and read once. A read takes the reading thread's
// own signal before the process's, whatever their numbers. A ppoll ends for a
// signal sent to the waiting thread, and not for one sent to another thread.

#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/select.h>
#include <sys/signalfd.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int sfd = -1;

// A worker thread runs one job at a time, handed over under `lock`.
struct worker {
  pthread_t thread;
  pid_t tid;
  void (*job)(void *);
  void *argument;
  int done;
  int stop;
};

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t changed = PTHREAD_COND_INITIALIZER;

static void *work(void *argument) {
  struct worker *self = argument;
  pthread_mutex_lock(&lock);
  self->tid = (pid_t)syscall(SYS_gettid);
  pthread_cond_broadcast(&changed);
  for (;;) {
    while (!self->job && !self->stop) pthread_cond_wait(&changed, &lock);
    if (self->stop) break;
    void (*job)(void *) = self->job;
    pthread_mutex_unlock(&lock);
    job(self->argument);
    pthread_mutex_lock(&lock);
    self->job = NULL;
    self->done = 1;
    pthread_cond_broadcast(&changed);
  }
  pthread_mutex_unlock(&lock);
  return NULL;
}

static int start(struct worker *w) {
  memset(w, 0, sizeof *w);
  if (pthread_create(&w->thread, NULL, work, w) != 0) return 0;
  pthread_mutex_lock(&lock);
  while (!w->tid) pthread_cond_wait(&changed, &lock);
  pthread_mutex_unlock(&lock);
  return 1;
}

// Hands `job` to `w` without waiting for it.
static void hand(struct worker *w, void (*job)(void *), void *argument) {
  pthread_mutex_lock(&lock);
  w->argument = argument;
  w->done = 0;
  w->job = job;
  pthread_cond_broadcast(&changed);
  pthread_mutex_unlock(&lock);
}

static void finish(struct worker *w) {
  pthread_mutex_lock(&lock);
  while (!w->done) pthread_cond_wait(&changed, &lock);
  pthread_mutex_unlock(&lock);
}

static void run(struct worker *w, void (*job)(void *), void *argument) {
  hand(w, job, argument);
  finish(w);
}

static void stop(struct worker *w) {
  pthread_mutex_lock(&lock);
  w->stop = 1;
  pthread_cond_broadcast(&changed);
  pthread_mutex_unlock(&lock);
  pthread_join(w->thread, NULL);
}

static void report(const char *what, int ok, int *all) {
  printf("%s: %s\n", what, ok ? "yes" : "no");
  *all = *all && ok;
}

// What a job saw.
struct seen {
  int result;
  int revents;
  int signals[2];
};

static void poll_now(void *argument) {
  struct seen *seen = argument;
  struct pollfd entry = {sfd, POLLIN, 0};
  seen->result = poll(&entry, 1, 0);
  seen->revents = entry.revents;
}

static void select_now(void *argument) {
  struct seen *seen = argument;
  fd_set readable;
  FD_ZERO(&readable);
  FD_SET(sfd, &readable);
  struct timeval none = {0, 0};
  seen->result = select(sfd + 1, &readable, NULL, NULL, &none);
  seen->revents = FD_ISSET(sfd, &readable);
}

static void read_now(void *argument) {
  struct seen *seen = argument;
  struct signalfd_siginfo records[2];
  memset(records, 0, sizeof records);
  ssize_t got = read(sfd, records, sizeof records);
  seen->result = got < 0 ? -errno : (int)got;
  seen->signals[0] = got >= (ssize_t)sizeof records[0] ? (int)records[0].ssi_signo : 0;
  seen->signals[1] = got >= (ssize_t)sizeof records ? (int)records[1].ssi_signo : 0;
}

static void wait_for_signal(void *argument) {
  struct seen *seen = argument;
  struct pollfd entry = {sfd, POLLIN, 0};
  struct timespec patience = {5, 0};
  seen->result = ppoll(&entry, 1, &patience, NULL);
  seen->revents = entry.revents;
}

static void wait_briefly(void *argument) {
  struct seen *seen = argument;
  struct pollfd entry = {sfd, POLLIN, 0};
  struct timespec brief = {0, 50000000};
  seen->result = ppoll(&entry, 1, &brief, NULL);
  seen->revents = entry.revents;
}

static int polled(struct worker *w) {
  struct seen seen = {0};
  if (w) {
    run(w, poll_now, &seen);
  } else {
    poll_now(&seen);
  }
  return seen.result == 1 && seen.revents == POLLIN ? 1 : seen.result == 0 ? 0 : -1;
}

static void nap(long milliseconds) {
  struct timespec span = {milliseconds / 1000, (milliseconds % 1000) * 1000000};
  nanosleep(&span, NULL);
}

// Waits, a bounded number of times, for `signal` to be pending for the
// calling thread or its process.
static int pending(int signal) {
  for (int tries = 0; tries < 500; tries++) {
    sigset_t set;
    if (sigpending(&set) == 0 && sigismember(&set, signal)) return 1;
    nap(2);
  }
  return 0;
}

int main(void) {
  int all = 1;
  // Every signal used stays blocked in every thread, so none is delivered,
  // and the signalfd reads them all.
  int signals[] = {SIGUSR1, SIGUSR2, SIGTERM, SIGALRM, SIGCHLD};
  sigset_t set;
  sigemptyset(&set);
  for (size_t i = 0; i < sizeof signals / sizeof signals[0]; i++) sigaddset(&set, signals[i]);
  if (pthread_sigmask(SIG_BLOCK, &set, NULL) != 0) return 2;

  // A child that exits when told, for a SIGCHLD later, made before any
  // thread or signalfd.
  int go[2];
  if (pipe(go) != 0) return 2;
  pid_t child = fork();
  if (child < 0) return 2;
  if (child == 0) {
    char byte;
    close(go[1]);
    _exit(read(go[0], &byte, 1) == 1 ? 7 : 8);
  }
  close(go[0]);

  struct worker first, second;
  if (!start(&first)) return 3;
  sfd = signalfd(-1, &set, SFD_NONBLOCK | SFD_CLOEXEC);
  report("signalfd in a process with a second thread returns a descriptor", sfd >= 0, &all);
  if (sfd < 0) {
    stop(&first);
    return 9;
  }
  report("a thread starts while the signalfd is open", start(&second), &all);

  // A signal sent to the first worker alone.
  pid_t pid = getpid();
  report("tgkill to the first worker succeeds",
         syscall(SYS_tgkill, pid, first.tid, SIGUSR1) == 0, &all);
  report("the main thread's poll does not see the first worker's signal", polled(NULL) == 0, &all);
  report("the second worker's poll does not see it", polled(&second) == 0, &all);
  struct seen seen = {0};
  run(&second, select_now, &seen);
  report("nor does the second worker's select", seen.result == 0 && !seen.revents, &all);
  report("the first worker's poll sees it", polled(&first) == 1, &all);
  run(&first, select_now, &seen);
  report("and its select", seen.result == 1 && seen.revents, &all);
  memset(&seen, 0, sizeof seen);
  read_now(&seen);
  report("the main thread's read finds nothing", seen.result == -EAGAIN, &all);
  run(&first, read_now, &seen);
  report("the first worker reads it",
         seen.result == (int)sizeof(struct signalfd_siginfo) && seen.signals[0] == SIGUSR1, &all);
  report("then its poll sees nothing", polled(&first) == 0, &all);

  // A timer's SIGALRM, queued for the process.
  struct itimerval once = {{0, 0}, {0, 20000}};
  report("a timer queues SIGALRM for the process",
         setitimer(ITIMER_REAL, &once, NULL) == 0 && pending(SIGALRM), &all);
  report("every thread's poll sees it",
         polled(NULL) == 1 && polled(&first) == 1 && polled(&second) == 1, &all);
  run(&second, read_now, &seen);
  report("the second worker reads it",
         seen.result == (int)sizeof(struct signalfd_siginfo) && seen.signals[0] == SIGALRM, &all);
  report("then no thread's poll sees anything",
         polled(NULL) == 0 && polled(&first) == 0 && polled(&second) == 0, &all);

  // The second worker's own SIGTERM before the process's SIGALRM, though
  // SIGALRM has the lower number.
  report("tgkill to the second worker succeeds",
         syscall(SYS_tgkill, pid, second.tid, SIGTERM) == 0, &all);
  report("a second timer queues SIGALRM for the process",
         setitimer(ITIMER_REAL, &once, NULL) == 0 && pending(SIGALRM), &all);
  run(&second, read_now, &seen);
  report("one read takes the thread's own signal first",
         seen.result == 2 * (int)sizeof(struct signalfd_siginfo) && seen.signals[0] == SIGTERM &&
             seen.signals[1] == SIGALRM,
         &all);

  // A child's SIGCHLD, queued for the process, read by the main thread.
  report("the child's exit queues SIGCHLD for the process",
         write(go[1], "x", 1) == 1 && pending(SIGCHLD), &all);
  read_now(&seen);
  report("the main thread reads it",
         seen.result == (int)sizeof(struct signalfd_siginfo) && seen.signals[0] == SIGCHLD, &all);
  int status = 0;
  report("the child exited with 7",
         waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 7,
         &all);

  // A ppoll ends for a signal sent to the waiting thread.
  hand(&first, wait_for_signal, &seen);
  nap(10);
  report("tgkill to the waiting worker succeeds",
         syscall(SYS_tgkill, pid, first.tid, SIGUSR2) == 0, &all);
  finish(&first);
  report("its ppoll ends with the signalfd readable", seen.result == 1 && seen.revents == POLLIN,
         &all);
  // And not for one sent to another thread: the first worker's SIGUSR2 is
  // still queued.
  struct seen other = {0};
  run(&second, wait_briefly, &other);
  report("another worker's ppoll times out", other.result == 0 && other.revents == 0, &all);
  run(&first, read_now, &seen);
  report("the waiting worker reads its signal",
         seen.result == (int)sizeof(struct signalfd_siginfo) && seen.signals[0] == SIGUSR2, &all);

  stop(&first);
  stop(&second);
  return all ? 0 : 9;
}
