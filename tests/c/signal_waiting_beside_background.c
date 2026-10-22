/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A scheduler SIGCHLD waits in main's shared queue while a call that cannot
// take it runs outside the schedule, and main's sigsuspend must still be armed
// for it and rejoin at once (Claude and Codex re-checks of
// https://github.com/rrnewton/hermit/pull/4314). Counting any background call
// beside a signalfd that was ever made kept it from being armed, and a
// SIGCHLD deferred behind runnable work made Hermit panic.
//
// Usage: signal_waiting_beside_background MODE
//   bg            a sibling thread is in vfork; its child yields 3000 times
//   bg-closed     as bg, after a signalfd for SIGCHLD was made and closed
//   bg-otherproc  another process holds a signalfd for SIGCHLD and is in
//                 vfork; its child yields 3000 times
//   select-closed a signalfd for SIGCHLD was made and closed; a sibling
//                 thread sleeps in select(65) with no sets until main sends
//                 it SIGUSR2
// Then main blocks SIGCHLD, forks a child that exits, reaps it and yields 100
// times, so the scheduler's SIGCHLD waits in the shared queue; a thread W
// yields until main is done, at most 20000 times, while main's sigsuspend
// takes the SIGCHLD.
#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/select.h>
#include <sys/signalfd.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile sig_atomic_t got_chld;
static atomic_int* done;
static atomic_int started;

static void on_chld(int signo) {
  (void)signo;
  got_chld = 1;
}

static void on_usr2(int signo) {
  (void)signo;
}

static void yield_in_vfork(void) {
  pid_t child = vfork();
  if (child == 0) {
    for (int i = 0; i < 3000; i++) {
      sched_yield();
    }
    _exit(0);
  }
  waitpid(child, NULL, 0);
}

static void* vfork_in_background(void* unused) {
  (void)unused;
  atomic_store(&started, 1);
  yield_in_vfork();
  return NULL;
}

static void* select_until_signaled(void* unused) {
  (void)unused;
  atomic_store(&started, 1);
  syscall(SYS_select, 65, NULL, NULL, NULL, NULL);
  return NULL;
}

static void* yield_until_done(void* saw_done) {
  for (int i = 0; i < 20000 && !atomic_load(done); i++) {
    sched_yield();
  }
  *(int*)saw_done = atomic_load(done);
  return NULL;
}

static void closed_signalfd(void) {
  sigset_t s;
  sigemptyset(&s);
  sigaddset(&s, SIGCHLD);
  int fd = signalfd(-1, &s, SFD_CLOEXEC);
  if (fd < 0) {
    perror("signalfd");
    _exit(1);
  }
  close(fd);
}

int main(int argc, char** argv) {
  const char* mode = argc > 1 ? argv[1] : "bg";
  setvbuf(stdout, NULL, _IONBF, 0);
  done = mmap(NULL, sizeof(*done), PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  if (done == MAP_FAILED) {
    perror("mmap");
    return 1;
  }
  signal(SIGCHLD, on_chld);
  signal(SIGUSR2, on_usr2);
  sigset_t blocked;
  sigemptyset(&blocked);
  sigaddset(&blocked, SIGCHLD);
  sigaddset(&blocked, SIGUSR2);
  sigprocmask(SIG_BLOCK, &blocked, NULL);
  if (!strcmp(mode, "bg-closed") || !strcmp(mode, "select-closed")) {
    closed_signalfd();
  }
  pthread_t background;
  int has_background = 0;
  if (!strcmp(mode, "bg") || !strcmp(mode, "bg-closed")) {
    has_background = !pthread_create(&background, NULL, vfork_in_background, NULL);
  } else if (!strcmp(mode, "select-closed")) {
    sigset_t usr2;
    sigemptyset(&usr2);
    sigaddset(&usr2, SIGUSR2);
    pthread_sigmask(SIG_UNBLOCK, &usr2, NULL);
    has_background = !pthread_create(&background, NULL, select_until_signaled, NULL);
    pthread_sigmask(SIG_BLOCK, &usr2, NULL);
  } else if (!strcmp(mode, "bg-otherproc")) {
    pid_t other = fork();
    if (other == 0) {
      sigset_t s;
      sigemptyset(&s);
      sigaddset(&s, SIGCHLD);
      if (signalfd(-1, &s, 0) < 0) {
        _exit(1);
      }
      yield_in_vfork();
      _exit(0);
    }
    atomic_store(&started, 1);
  } else {
    fprintf(stderr, "unknown mode %s\n", mode);
    return 64;
  }
  if (strcmp(mode, "bg-otherproc") && !has_background) {
    perror("pthread_create");
    return 1;
  }
  while (!atomic_load(&started)) {
    sched_yield();
  }
  for (int i = 0; i < 50; i++) {
    sched_yield();
  }
  int saw_done = 0;
  pthread_t watcher;
  if (pthread_create(&watcher, NULL, yield_until_done, &saw_done)) {
    perror("pthread_create");
    return 1;
  }
  pid_t child = fork();
  if (child == 0) {
    _exit(0);
  }
  waitpid(child, NULL, 0);
  for (int i = 0; i < 100; i++) {
    sched_yield();
  }
  sigset_t none;
  sigemptyset(&none);
  sigsuspend(&none);
  int woke = got_chld;
  atomic_store(done, 1);
  pthread_join(watcher, NULL);
  if (!strcmp(mode, "select-closed")) {
    pthread_kill(background, SIGUSR2);
  }
  if (has_background) {
    pthread_join(background, NULL);
  }
  printf("%s: woke chld %d; sibling saw done %d\n", mode, woke, saw_done);
  return 0;
}
