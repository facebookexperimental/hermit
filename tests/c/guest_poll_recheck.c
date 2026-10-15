/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A restartable internal IO poll (a pipe read, a scalar or vectored pipe
 * write) re-checks, at its own turn, which signals still interrupt it
 * (https://github.com/rrnewton/hermit/issues/4273, R2b-1). A worker blocks in
 * the call; main signals it, possibly flushes or re-sends, then completes the
 * call. Usage: guest_poll_recheck MODE [TURNS]
 *
 *   caught          A caught SIGUSR1 (h1) ends the worker's pipe read: EINTR.
 *   caught-restart  The same with SA_RESTART: the read restarts and reads 1.
 *   dflthandler     SIGWINCH is SIG_DFL (default-ignored) when main sends it,
 *                   then main installs h1 after TURNS sched_yield calls: Linux
 *                   discarded the copy, so the read reads 1.
 *   ignhandler      The same with SIGUSR1 set to SIG_IGN at the send.
 *   flushresend     A kept SIGUSR1 (h1), TURNS sched_yield calls, SIG_IGN
 *                   (flushes it), a discarded SIGUSR1, then h2: h2 never runs.
 *   write-empty     The worker writes 70000 bytes into a full pipe (no
 *                   progress); a caught SIGUSR1 ends it: EINTR.
 *   write-partial   The pipe has room for 4096 bytes; the caught SIGUSR1 comes
 *                   after they are written: the write returns 4096.
 *   write-flush     A kept SIGUSR1, a turn, SIG_IGN, then main drains the pipe:
 *                   the write completes in full.
 *   writev-flush    The same with writev.
 *   write-partial-flush, writev-partial-flush
 *                   As write-flush and writev-flush with room for 4096 bytes,
 *                   so the flush comes after progress: the wake stands, as
 *                   Linux's TIF_SIGPENDING does, and the write returns 4096.
 *
 *   dataready       main writes the byte, then sends the reader a caught
 *                   SIGUSR1 (h1): Linux checks for data first, so read 1, h1 1
 *                   (the handler on the call's return), or EINTR if the signal
 *                   came first.
 *   committed-replace
 *                   A child exits while main blocks SIGCHLD, so the scheduler
 *                   sends the caught SIGCHLD (h1) to the blocked reader; main
 *                   then makes SIGCHLD SIG_IGN (flushing it), sends the reader a
 *                   SIGCHLD Linux discards, and installs h1 again. Linux: the
 *                   first copy wakes the reader (EINTR, h1 1) unless the flush
 *                   comes first (read 1, h1 0); the discarded copy never ends it.
 *   committed-replace-shared
 *                   The same with the discarded SIGCHLD sent to the process's
 *                   shared queue by a SIGEV_SIGNAL timer (a guest's
 *                   process-directed kill is refused under --strict:
 *                   https://github.com/rrnewton/hermit/issues/4034).
 *
 * A MODE prefixed "rt-" uses SIGRTMIN where it says SIGUSR1: a real-time
 * signal, outside Hermit's record of copies Linux discards at send
 * (https://github.com/rrnewton/hermit/issues/4319).
 *
 * Each line is what the worker saw, the handler counts, and how many bytes
 * main read back.
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
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static const char *mode;
static int real_time;
static int turns;
static int fds[2];
static atomic_int ready;
static atomic_int h1;
static atomic_int h2;
static atomic_int done;
static pid_t worker_tid;
static long result;
static int error;

static int is(const char *name) { return !strcmp(mode, name); }

static void on_h1(int signal) {
  (void)signal;
  atomic_fetch_add(&h1, 1);
}

static void on_h2(int signal) {
  (void)signal;
  atomic_fetch_add(&h2, 1);
}

static void set_action(int signal, void (*handler)(int), int flags) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = handler;
  action.sa_flags = flags;
  sigaction(signal, &action, NULL);
}

static void sleep_ms(long ms) {
  struct timespec delay = {ms / 1000, (ms % 1000) * 1000000};
  nanosleep(&delay, NULL);
}

static const char *errno_name(int value) {
  switch (value) {
  case 0:
    return "0";
  case EINTR:
    return "EINTR";
  case EAGAIN:
    return "EAGAIN";
  default:
    return "other";
  }
}

static int writing(void) { return !strncmp(mode, "write", 5); }

static void *worker(void *unused) {
  (void)unused;
  worker_tid = (pid_t)syscall(SYS_gettid);
  if (!strncmp(mode, "committed-replace", 17)) {
    sigset_t chld;
    sigemptyset(&chld);
    sigaddset(&chld, SIGCHLD);
    pthread_sigmask(SIG_UNBLOCK, &chld, NULL);
  }
  static char bytes[70000];
  memset(bytes, 'x', sizeof(bytes));
  atomic_store(&ready, 1);
  if (is("writev-flush") || is("writev-partial-flush")) {
    struct iovec iov[2] = {{bytes, 35000}, {bytes + 35000, 35000}};
    result = writev(fds[1], iov, 2);
  } else if (writing()) {
    result = write(fds[1], bytes, sizeof(bytes));
  } else {
    char byte;
    result = read(fds[0], &byte, 1);
  }
  error = result < 0 ? errno : 0;
  atomic_store(&done, 1);
  return NULL;
}

/* Fill the pipe so that only `room` bytes fit. */
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
    total += got;
  }
  fcntl(fds[0], F_SETFL, flags);
  return total;
}

static void yield_turns(void) {
  for (int i = 0; i < turns; i++) {
    sched_yield();
  }
}

int main(int argc, char **argv) {
  if (argc < 2) {
    fprintf(stderr, "usage: %s MODE [TURNS]\n", argv[0]);
    return 2;
  }
  mode = argv[1];
  if (!strncmp(mode, "rt-", 3)) {
    real_time = 1;
    mode += 3;
  }
  turns = argc > 2 ? atoi(argv[2]) : 0;
  int usr1 = real_time ? SIGRTMIN : SIGUSR1;
  setvbuf(stdout, NULL, _IONBF, 0);
  if (pipe(fds)) {
    perror("pipe");
    return 2;
  }
  int signal = is("dflthandler") ? SIGWINCH : usr1;
  if (!strncmp(mode, "committed-replace", 17)) {
    signal = SIGCHLD;
    set_action(SIGCHLD, on_h1, 0);
    sigset_t chld;
    sigemptyset(&chld);
    sigaddset(&chld, SIGCHLD);
    pthread_sigmask(SIG_BLOCK, &chld, NULL);
  } else if (is("ignhandler")) {
    set_action(usr1, SIG_IGN, 0);
  } else if (!is("dflthandler")) {
    set_action(usr1, on_h1, is("caught-restart") ? SA_RESTART : 0);
  }
  if (is("write-empty") || is("write-flush") || is("writev-flush")) {
    fill_pipe(0);
  } else if (is("write-partial") || is("write-partial-flush") ||
             is("writev-partial-flush")) {
    fill_pipe(4096);
  }
  pthread_t thread;
  pthread_create(&thread, NULL, worker, NULL);
  while (!atomic_load(&ready)) {
    sched_yield();
  }
  sleep_ms(50); /* let the worker reach its call */
  if (!strncmp(mode, "committed-replace", 17)) {
    pid_t child = fork();
    if (child == 0) {
      _exit(0);
    }
    sleep_ms(20 * turns);
    set_action(SIGCHLD, SIG_IGN, 0);
    if (is("committed-replace-shared")) {
      timer_t timer;
      struct sigevent event;
      memset(&event, 0, sizeof(event));
      event.sigev_notify = SIGEV_SIGNAL;
      event.sigev_signo = SIGCHLD;
      struct itimerspec expiry = {{0, 0}, {0, 1000000}};
      if (timer_create(CLOCK_MONOTONIC, &event, &timer) ||
          timer_settime(timer, 0, &expiry, NULL)) {
        return 2;
      }
      sleep_ms(5);
      timer_delete(timer);
    } else {
      syscall(SYS_tgkill, getpid(), worker_tid, SIGCHLD);
    }
    set_action(SIGCHLD, on_h1, 0);
    waitpid(child, NULL, WNOHANG);
  } else if (is("dataready")) {
    if (write(fds[1], "x", 1) != 1) {
      return 2;
    }
    syscall(SYS_tgkill, getpid(), worker_tid, signal);
  } else {
    syscall(SYS_tgkill, getpid(), worker_tid, signal);
  }
  if (is("dflthandler") || is("ignhandler")) {
    yield_turns();
    set_action(signal, on_h1, 0);
  } else if (is("flushresend") || strstr(mode, "flush")) {
    yield_turns();
    set_action(usr1, SIG_IGN, 0);
    if (is("flushresend")) {
      syscall(SYS_tgkill, getpid(), worker_tid, usr1);
      set_action(usr1, on_h2, 0);
    }
  }
  sleep_ms(50);
  if (writing()) {
    while (!atomic_load(&done)) {
      if (drain_pipe() == 0) {
        sleep_ms(10);
      }
    }
  } else if (!is("dataready") && write(fds[1], "x", 1) != 1) {
    return 2;
  }
  pthread_join(thread, NULL);
  printf("%s%s %d: %s %ld %s, h1 %d, h2 %d\n", real_time ? "rt-" : "", mode, turns,
         writing() ? (!strncmp(mode, "writev", 6) ? "writev" : "write") : "read", result,
         errno_name(error), atomic_load(&h1), atomic_load(&h2));
  return 0;
}
