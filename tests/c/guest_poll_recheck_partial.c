/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Pipe writes with room for 4096 of 70000 bytes, for R2b-1 of
 * https://github.com/rrnewton/hermit/issues/4273 (from the Claude design
 * re-checks of rev7 to rev9). Usage: guest_poll_recheck_partial MODE [TURNS]
 *  write-partial-ign    room 4096; SIGUSR1 SIG_IGN when sent; TURNS yields; then h1.
 *                       Linux discards at send: write 70000, h1 0.
 *  write-partial-rtign  same with SIGRTMIN (no 4293 record covers RT signals).
 *  write-partial-rtign-stay  SIGRTMIN stays SIG_IGN (no handler later).
 *  write-partial-chldblk every thread blocks SIGCHLD (handler h1 installed); the
 *                       worker forks a child that exits after 100 ms, then writes.
 *                       Linux: SIGCHLD stays pending, no wake: write 70000, h1 0.
 *  writev-partial-rtign, writev-partial-rtign-stay  the RT modes, with writev.
 *  writev-partial-chld  a guest's caught SIGCHLD (tgkill): writev 4096, h1 1.
 *  writev-partial-childexit  a child exits, so the scheduler sends the process
 *                       its caught SIGCHLD: Linux writev 4096, h1 1.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <sys/uio.h>
#include <time.h>
#include <unistd.h>

static const char *mode;
static int turns;
static int fds[2];
static atomic_int ready, h1, done;
static pid_t worker_tid;
static long result;
static int error;
static int is(const char *n) { return !strcmp(mode, n); }
static void on_h1(int s) { (void)s; atomic_fetch_add(&h1, 1); }
static void set_action(int s, void (*h)(int)) {
  struct sigaction a; memset(&a, 0, sizeof(a)); a.sa_handler = h; sigaction(s, &a, NULL);
}
static void sleep_ms(long ms) { struct timespec d = {ms / 1000, (ms % 1000) * 1000000}; nanosleep(&d, NULL); }
static void *worker(void *u) {
  (void)u;
  worker_tid = (pid_t)syscall(SYS_gettid);
  static char bytes[70000];
  memset(bytes, 'x', sizeof(bytes));
  if (is("write-partial-chldblk") || is("writev-partial-childexit")) {
    pid_t c = fork();
    if (c == 0) { sleep_ms(100); _exit(0); }
  }
  atomic_store(&ready, 1);
  if (is("writev-partial-rtign-stay") || is("writev-partial-rtign") || is("writev-partial-chld") || is("writev-partial-childexit")) {
    struct iovec iov[2] = {{bytes, 35000}, {bytes + 35000, 35000}};
    result = writev(fds[1], iov, 2);
  } else {
    result = write(fds[1], bytes, sizeof(bytes));
  }
  error = result < 0 ? errno : 0;
  atomic_store(&done, 1);
  return NULL;
}
static void fill_pipe(long room) {
  int cap = fcntl(fds[1], F_GETPIPE_SZ);
  static char f[1 << 20];
  int fl = fcntl(fds[1], F_GETFL);
  fcntl(fds[1], F_SETFL, fl | O_NONBLOCK);
  long left = cap - room;
  while (left > 0) { long w = write(fds[1], f, left); if (w <= 0) break; left -= w; }
  fcntl(fds[1], F_SETFL, fl);
}
static long drain_pipe(void) {
  static char s[1 << 16]; long t = 0;
  int fl = fcntl(fds[0], F_GETFL);
  fcntl(fds[0], F_SETFL, fl | O_NONBLOCK);
  for (;;) { long g = read(fds[0], s, sizeof(s)); if (g <= 0) break; t += g; }
  fcntl(fds[0], F_SETFL, fl);
  return t;
}
int main(int argc, char **argv) {
  if (argc < 2) return 2;
  mode = argv[1];
  turns = argc > 2 ? atoi(argv[2]) : 0;
  setvbuf(stdout, NULL, _IONBF, 0);
  if (pipe(fds)) return 2;
  int sig = (is("write-partial-rtign") || is("writev-partial-rtign") || is("write-partial-rtign-stay") || is("writev-partial-rtign-stay")) ? SIGRTMIN : SIGUSR1;
  if (is("writev-partial-chld")) {
    sig = SIGCHLD;
    set_action(SIGCHLD, on_h1);
  } else if (is("writev-partial-childexit")) {
    set_action(SIGCHLD, on_h1);
  } else if (is("write-partial-chldblk")) {
    set_action(SIGCHLD, on_h1);
    sigset_t m; sigemptyset(&m); sigaddset(&m, SIGCHLD);
    pthread_sigmask(SIG_BLOCK, &m, NULL);
  } else {
    set_action(sig, SIG_IGN);
  }
  fill_pipe(4096);
  pthread_t th;
  pthread_create(&th, NULL, worker, NULL);
  while (!atomic_load(&ready)) sched_yield();
  sleep_ms(50);
  if (is("write-partial-chldblk") || is("writev-partial-childexit")) {
    sleep_ms(300);
  } else {
    syscall(SYS_tgkill, getpid(), worker_tid, sig);
    for (int i = 0; i < turns; i++) sched_yield();
    if (!is("writev-partial-chld") && !strstr(mode, "rtign-stay")) set_action(sig, on_h1);
  }
  sleep_ms(50);
  while (!atomic_load(&done)) { if (drain_pipe() == 0) sleep_ms(10); }
  pthread_join(th, NULL);
  if (is("write-partial-chldblk") || is("writev-partial-childexit")) { while (waitpid(-1, NULL, WNOHANG) > 0) {} }
  printf("%s %d: %s %ld %s, h1 %d\n", mode, turns,
         strncmp(mode, "writev", 6) ? "write" : "writev", result,
         error == EINTR ? "EINTR" : (error ? "other" : "0"), atomic_load(&h1));
  return 0;
}
