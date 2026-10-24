/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* writev with room for 4096 of 70000 bytes and a SIGUSR1 from a timer that
 * names the writer, or from main, for R2b-1 of
 * https://github.com/rrnewton/hermit/issues/4273 (from the Claude design
 * re-check of rev9, A12). Usage: guest_poll_recheck_timer MODE
 *  writev-timer          writer leaves SIGUSR1 unblocked (h1 caught), arms a
 *                        SIGEV_THREAD_ID timer naming itself (100 ms), then writev
 *                        70000 with room for 4096. Linux: writev 4096, h1 1.
 *                        Control: shows the scheduler-sent copy reaches writev.
 *  writev-blocked-timer  same, but the writer blocks SIGUSR1 in its own mask.
 *                        Linux: stays pending, writev 70000, h1 0.
 *  writev-blocked-tgkill writer blocks SIGUSR1; main tgkills it at 50 ms.
 *                        Linux: writev 70000, h1 0.
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
#include <sys/uio.h>
#include <time.h>
#include <unistd.h>

#ifndef sigev_notify_thread_id
#define sigev_notify_thread_id _sigev_un._tid
#endif

static const char *mode;
static int fds[2];
static atomic_int ready, h1, done;
static pid_t worker_tid;
static long result;
static int error;
static int is(const char *n) { return !strcmp(mode, n); }
static void on_h1(int s) { (void)s; atomic_fetch_add(&h1, 1); }
static void sleep_ms(long ms) { struct timespec d = {ms / 1000, (ms % 1000) * 1000000}; nanosleep(&d, NULL); }
static void *worker(void *u) {
  (void)u;
  worker_tid = (pid_t)syscall(SYS_gettid);
  static char bytes[70000];
  memset(bytes, 'x', sizeof(bytes));
  {
    sigset_t m; sigemptyset(&m); sigaddset(&m, SIGUSR1);
    pthread_sigmask(strstr(mode, "blocked") ? SIG_BLOCK : SIG_UNBLOCK, &m, NULL);
  }
  if (strstr(mode, "timer")) {
    struct sigevent ev; memset(&ev, 0, sizeof(ev));
    ev.sigev_notify = SIGEV_THREAD_ID;
    ev.sigev_signo = SIGUSR1;
    ev.sigev_notify_thread_id = worker_tid;
    timer_t t;
    if (timer_create(CLOCK_MONOTONIC, &ev, &t)) { perror("timer_create"); exit(3); }
    struct itimerspec its; memset(&its, 0, sizeof(its));
    its.it_value.tv_nsec = 100 * 1000000L;
    if (timer_settime(t, 0, &its, NULL)) { perror("timer_settime"); exit(3); }
  }
  atomic_store(&ready, 1);
  struct iovec iov[2] = {{bytes, 35000}, {bytes + 35000, 35000}};
  result = writev(fds[1], iov, 2);
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
  setvbuf(stdout, NULL, _IONBF, 0);
  if (pipe(fds)) return 2;
  struct sigaction a; memset(&a, 0, sizeof(a)); a.sa_handler = on_h1; sigaction(SIGUSR1, &a, NULL);
  /* main never takes SIGUSR1 */
  sigset_t m; sigemptyset(&m); sigaddset(&m, SIGUSR1);
  pthread_sigmask(SIG_BLOCK, &m, NULL);
  fill_pipe(4096);
  pthread_t th;
  pthread_create(&th, NULL, worker, NULL);
  while (!atomic_load(&ready)) sched_yield();
  /* the worker unblocks only if its mode leaves SIGUSR1 unblocked: it cleared
   * nothing, so for "writev-timer" it must unblock explicitly. Threads inherit
   * main's mask, so handle that in the worker via the mode check below. */
  sleep_ms(50);
  if (is("writev-blocked-tgkill")) syscall(SYS_tgkill, getpid(), worker_tid, SIGUSR1);
  sleep_ms(300);
  while (!atomic_load(&done)) { if (drain_pipe() == 0) sleep_ms(10); }
  pthread_join(th, NULL);
  printf("%s: writev %ld %s, h1 %d\n", mode, result,
         error == EINTR ? "EINTR" : (error ? "other" : "0"), atomic_load(&h1));
  return 0;
}
