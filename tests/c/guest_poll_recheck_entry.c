/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A caught SIGUSR1 (handler h1) pending when a worker's pipe call begins, for
 * R2b-1 of https://github.com/rrnewton/hermit/issues/4273 (built on the Claude
 * design re-check's hbpoll probe). Under Hermit a hard happens-before edge
 * holds the worker at its call's entry until main's send is done; natively the
 * worker simply blocks, then the signal arrives. Usage:
 * guest_poll_recheck_entry MODE
 *
 *   read           a read of an empty pipe: read -1 EINTR, h1 1.
 *   write-partial  a 70000-byte write with room for 4096: write 4096, h1 1.
 *   writev-partial the same with writev: writev 4096, h1 1.
 *   write-partial-h2, writev-partial-h2
 *                  as write-partial and writev-partial, and then main makes
 *                  SIGUSR1 SIG_IGN and installs h2: h2 never runs.
 *   write-full-h2, writev-full-h2
 *                  the same with a full pipe (no progress): Linux restarts the
 *                  call invisibly, so it completes after main drains, and h2
 *                  never runs.
 *   MODE-two       read, write-partial, writev-partial or writev-full, with a
 *                  caught SIGUSR2 (h2) sent right after SIGUSR1: both are
 *                  pending at the call's entry, and both handlers run (the
 *                  Claude implementation review of R2b-1, F2).
 *   atomic-full-two, atomic-room-two
 *                  the same with a 100-byte writev (2 x 50 bytes, an atomic
 *                  pipe write) into a full pipe, or one with room: EINTR, or
 *                  writev 100, and both handlers run (the Claude re-check, N2).
 *   atomic33-full-two
 *                  the same with 33 iovecs of 3 bytes (99 bytes), more than
 *                  Hermit's stack copy of an iovec array holds (the Codex
 *                  re-check of R2b-1).
 *   atomic-full-two-swap, atomic-room-two-swap, atomic33-full-two-swap
 *                  as atomic-full-two, atomic-room-two and atomic33-full-two,
 *                  and main then
 *                  points the worker's iovec array at another buffer of the
 *                  same lengths, racing the worker's call (run under chaos
 *                  seeds): the same lines (the Claude re-check of R2b-1, R1).
 *   writev-partial-quiet-swap, atomic-full-quiet-swap
 *                  a 70000-byte writev with room for 4096, or a 100-byte one
 *                  into a full pipe, and no signal. Just before its call the
 *                  worker starts a thread that points the iovec array at a
 *                  buffer of other bytes once the call has begun, and main
 *                  drains only after that: the write completes, and every
 *                  byte main reads after the filler comes from the first
 *                  array, which Linux imported when the call began
 *                  (writev_determinism's atomic full-pipe case, which
 *                  reverted the first landing of R2b-1; the Claude re-check,
 *                  N5). The partial write has begun once the pipe is full,
 *                  as only its first 4096 bytes can fill it. The atomic one
 *                  writes nothing while it waits, so its rewriter waits 50 ms
 *                  of virtual time after the worker's last step before the
 *                  call. The worker needs a few scheduler turns to reach its
 *                  call, while main only wakes every millisecond to check, so
 *                  it gets them long before (failing would take over 100
 *                  turns at the default 0.5 ms each), whatever the schedule
 *                  or timeslice. A 1 ms wait was shorter than the worker's
 *                  last steps at a 1 us timeslice
 *                  (https://github.com/rrnewton/hermit/issues/4804).
 * In the -two modes the worker stays alive after its call until main is
 * done, so a handler that does not run was lost, not left undelivered.
 *   writev-full-ign, writev-partial-ign
 *                  SIGUSR1 is SIG_IGN when main sends it (Linux discards the
 *                  copy), then main installs h1 before the call begins: the
 *                  writev completes, 70000 (the Claude re-check, N1).
 *
 * Main calls getppid once, right after its send and nowhere else, so a
 * happens-before edge can anchor on it: the send has completed before it
 * begins. SIG_IGN is main's third rt_sigaction (after h1 and the one libc's
 * pthread_create makes).
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
#include <sys/ioctl.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <time.h>
#include <unistd.h>

static const char *mode;
static int fds[2];
static atomic_int ready, h1, h2, done, release;
static char other[100];
static struct iovec atomic_iov[2];
static struct iovec atomic33_iov[33];
static struct iovec large_iov[2];
static char large_other[70000];
static long drained_x, drained_y;
static atomic_int calling, swapped;
static pid_t worker_tid;
static long result;
static int error;

static int starts(const char *prefix) {
  return !strncmp(mode, prefix, strlen(prefix));
}

static void on_h1(int signal) {
  (void)signal;
  atomic_fetch_add(&h1, 1);
}

static void on_h2(int signal) {
  (void)signal;
  atomic_fetch_add(&h2, 1);
}

static void set_action(int signal, void (*handler)(int)) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = handler;
  sigaction(signal, &action, NULL);
}

static void sleep_ms(long ms) {
  struct timespec delay = {ms / 1000, (ms % 1000) * 1000000};
  nanosleep(&delay, NULL);
}

/* Point the worker's iovec array at the buffer of other bytes. */
static void swap_arrays(void) {
  if (starts("writev")) {
    large_iov[0].iov_base = large_other;
    large_iov[1].iov_base = large_other + 35000;
  } else if (starts("atomic33")) {
    atomic33_iov[0].iov_base = other;
  } else {
    atomic_iov[0].iov_base = other;
    atomic_iov[1].iov_base = other + 50;
  }
}

/* The -quiet-swap modes' rewriter, started by the worker just before its
 * call: it points the array elsewhere only once the call has begun (see the
 * modes above), then lets main drain. */
static void *rewrite_later(void *unused) {
  (void)unused;
  while (!atomic_load(&calling)) {
    sched_yield();
  }
  if (starts("writev")) {
    int capacity = fcntl(fds[1], F_GETPIPE_SZ);
    int queued = 0;
    while (ioctl(fds[0], FIONREAD, &queued) == 0 && queued < capacity) {
      sleep_ms(1);
    }
  } else {
    sleep_ms(50);
  }
  swap_arrays();
  atomic_store(&swapped, 1);
  return NULL;
}

static void start_rewriter(void) {
  pthread_t rewriter;
  if (strstr(mode, "-quiet") && strstr(mode, "-swap") &&
      pthread_create(&rewriter, NULL, rewrite_later, NULL) == 0) {
    pthread_detach(rewriter);
  }
}

static void *worker(void *unused) {
  (void)unused;
  static char bytes[70000];
  memset(bytes, 'x', sizeof(bytes));
  worker_tid = (pid_t)syscall(SYS_gettid);
  atomic_store(&ready, 1);
  if (starts("read")) {
    char byte;
    result = read(fds[0], &byte, 1);
  } else if (starts("atomic33")) {
    for (int i = 0; i < 33; i++) {
      atomic33_iov[i].iov_base = bytes + 3 * i;
      atomic33_iov[i].iov_len = 3;
    }
    result = writev(fds[1], atomic33_iov, 33);
  } else if (starts("atomic")) {
    atomic_iov[0] = (struct iovec){bytes, 50};
    atomic_iov[1] = (struct iovec){bytes + 50, 50};
    start_rewriter();
    atomic_store(&calling, 1);
    result = writev(fds[1], atomic_iov, 2);
  } else if (starts("writev")) {
    large_iov[0] = (struct iovec){bytes, 35000};
    large_iov[1] = (struct iovec){bytes + 35000, 35000};
    start_rewriter();
    atomic_store(&calling, 1);
    result = writev(fds[1], large_iov, 2);
  } else {
    result = write(fds[1], bytes, sizeof(bytes));
  }
  error = result < 0 ? errno : 0;
  atomic_store(&done, 1);
  while (strstr(mode, "-two") && !atomic_load(&release)) {
    sleep_ms(1);
  }
  return NULL;
}

static void fill_pipe(long room) {
  int capacity = fcntl(fds[1], F_GETPIPE_SZ);
  static char filler[1 << 20];
  int flags = fcntl(fds[1], F_GETFL);
  fcntl(fds[1], F_SETFL, flags | O_NONBLOCK);
  long left = capacity - room;
  while (left > 0) {
    long wrote = write(fds[1], filler, left);
    if (wrote <= 0) {
      break;
    }
    left -= wrote;
  }
  fcntl(fds[1], F_SETFL, flags);
}

static long drain_pipe(void) {
  static char sink[1 << 16];
  long total = 0;
  int flags = fcntl(fds[0], F_GETFL);
  fcntl(fds[0], F_SETFL, flags | O_NONBLOCK);
  for (;;) {
    long got = read(fds[0], sink, sizeof(sink));
    if (got <= 0) {
      break;
    }
    for (long i = 0; i < got; i++) {
      drained_x += sink[i] == 'x';
      drained_y += sink[i] == 'y';
    }
    total += got;
  }
  fcntl(fds[0], F_SETFL, flags);
  return total;
}

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s MODE\n", argv[0]);
    return 2;
  }
  mode = argv[1];
  setvbuf(stdout, NULL, _IONBF, 0);
  memset(large_other, 'y', sizeof(large_other));
  memset(other, 'y', sizeof(other));
  if (pipe(fds)) {
    perror("pipe");
    return 2;
  }
  set_action(SIGUSR1, strstr(mode, "-ign") ? SIG_IGN : on_h1);
  if (strstr(mode, "-two")) {
    set_action(SIGUSR2, on_h2);
  }
  if (strstr(mode, "-full")) {
    fill_pipe(0);
  } else if (!starts("read")) {
    fill_pipe(4096);
  }
  pthread_t thread;
  pthread_create(&thread, NULL, worker, NULL);
  while (!atomic_load(&ready)) {
    sched_yield();
  }
  if (!strstr(mode, "-quiet")) {
    syscall(SYS_tgkill, getpid(), worker_tid, SIGUSR1);
  }
  if (strstr(mode, "-two")) {
    syscall(SYS_tgkill, getpid(), worker_tid, SIGUSR2);
  }
  if (strstr(mode, "-ign")) {
    set_action(SIGUSR1, on_h1);
  }
  syscall(SYS_getppid);
  if (strstr(mode, "-swap") && !strstr(mode, "-quiet")) {
    swap_arrays();
  }
  if (strstr(mode, "-h2")) {
    set_action(SIGUSR1, SIG_IGN);
    set_action(SIGUSR1, on_h2);
  }
  sleep_ms(50);
  while (strstr(mode, "-quiet") && !atomic_load(&swapped)) {
    sleep_ms(1);
  }
  if (starts("read")) {
    if (write(fds[1], "x", 1) != 1) {
      return 2;
    }
  } else {
    while (!atomic_load(&done)) {
      if (drain_pipe() == 0) {
        sleep_ms(10);
      }
    }
    drain_pipe(); /* what the call wrote after the last drain */
  }
  atomic_store(&release, 1);
  pthread_join(thread, NULL);
  printf("%s: %s %ld %s, h1 %d, h2 %d\n", mode,
         starts("read") ? "read"
                        : (starts("writev") || starts("atomic") ? "writev" : "write"),
         result,
         error == EINTR ? "EINTR" : (error ? "other" : "0"), atomic_load(&h1),
         atomic_load(&h2));
  if (strstr(mode, "-quiet")) {
    printf("%s: bytes from %s\n", mode,
           drained_x && drained_y ? "both arrays"
                                  : (drained_x ? "the first array"
                                               : (drained_y ? "the second array" : "no array")));
  }
  return 0;
}
