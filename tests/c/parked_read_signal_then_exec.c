/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A parked pipe read that an ignored alarm reaches and a blocked signal then
 * wakes, followed by execve.
 *
 * The main thread blocks SIGUSR2, ignores SIGALRM, arms a one-shot 10 ms
 * ITIMER_REAL and reads one byte from an empty pipe. A worker, which blocks
 * SIGALRM so that only the reader can receive the alarm, yields until the
 * alarm is due, sends SIGUSR2 to the reader once, sleeps 20 ms and writes the
 * byte. On Linux the ignored alarm is discarded, the blocked SIGUSR2 stays
 * pending, and the read returns the byte. The reader joins the worker and
 * execs this program by its argv[0] as `after`, which reports that SIGUSR2 is
 * still pending and blocked (both survive exec).
 *
 * Under Hermit on KVM the scheduler parks the read. The alarm is observed
 * and, being ignored, resumes the same wait; the blocked SIGUSR2 then
 * replaces the read's request with a signal wake before its next turn; the
 * read returns ERESTARTSYS and restarts, and the write completes it. The
 * replaced request's parked ownership outlived it, and once Detcore let
 * reverie-kvm skip the empty signal boundary after the restart, nothing
 * removed it, and the exec was refused (KVM parked signal protocol,
 * ProtocolFailure::Identity).
 *
 * Linux prints:
 *   read: n=1 byte=x
 *   after: usr2 pending=1 blocked=1
 *
 * Since SIGSHARE S2 the drain decides KVM's wake from the reader's signal
 * state, as on ptrace, so the blocked SIGUSR2 no longer wakes the read. With
 * argv[1] `caught`, the reader instead catches SIGUSR2 with SA_RESTART, so
 * the signal wakes the read after the alarm's observation, replaces its
 * request, and the read restarts after the handler, the path above. Linux
 * prints:
 *   read: n=1 byte=x handled=1
 *   after: usr2 pending=0 blocked=0
 */

#define _GNU_SOURCE

#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

static int pipe_fds[2];
static pthread_t reader;
static volatile long alarm_due_ns;
static volatile sig_atomic_t handled;

static void on_usr2(int signal) {
  (void)signal;
  handled++;
}

/* A real system call, so that each check is a point where Hermit schedules. */
static long monotonic_ns(void) {
  struct timespec t;
  syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &t);
  return t.tv_sec * 1000000000L + t.tv_nsec;
}

static void* worker(void* arg) {
  (void)arg;
  sigset_t alarm_set;
  sigemptyset(&alarm_set);
  sigaddset(&alarm_set, SIGALRM);
  pthread_sigmask(SIG_BLOCK, &alarm_set, NULL);
  while (alarm_due_ns == 0 || monotonic_ns() < alarm_due_ns) {
    sched_yield();
  }
  pthread_kill(reader, SIGUSR2);
  struct timespec twenty_ms = {0, 20000000};
  nanosleep(&twenty_ms, NULL);
  if (write(pipe_fds[1], "x", 1) != 1) {
    perror("write");
  }
  return NULL;
}

static int after(int caught) {
  sigset_t pending, blocked;
  sigpending(&pending);
  sigprocmask(SIG_BLOCK, NULL, &blocked);
  int is_pending = sigismember(&pending, SIGUSR2) == 1;
  int is_blocked = sigismember(&blocked, SIGUSR2) == 1;
  printf("after: usr2 pending=%d blocked=%d\n", is_pending, is_blocked);
  if (caught) {
    return !is_pending && !is_blocked ? 0 : 1;
  }
  return is_pending && is_blocked ? 0 : 1;
}

int main(int argc, char** argv) {
  if (argc == 2 && strcmp(argv[1], "after") == 0) {
    return after(0);
  }
  if (argc == 2 && strcmp(argv[1], "after-caught") == 0) {
    return after(1);
  }
  int caught = argc == 2 && strcmp(argv[1], "caught") == 0;
  if (caught) {
    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = on_usr2;
    sa.sa_flags = SA_RESTART;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGUSR2, &sa, NULL);
  } else {
    sigset_t usr2;
    sigemptyset(&usr2);
    sigaddset(&usr2, SIGUSR2);
    sigprocmask(SIG_BLOCK, &usr2, NULL);
  }
  signal(SIGALRM, SIG_IGN);
  if (pipe(pipe_fds) != 0) {
    perror("pipe");
    return 1;
  }
  reader = pthread_self();
  pthread_t thread;
  if (pthread_create(&thread, NULL, worker, NULL) != 0) {
    return 1;
  }
  struct itimerval ten_ms;
  memset(&ten_ms, 0, sizeof(ten_ms));
  ten_ms.it_value.tv_usec = 10000;
  long armed = monotonic_ns();
  setitimer(ITIMER_REAL, &ten_ms, NULL);
  alarm_due_ns = armed + 10000000L;
  char byte = 0;
  ssize_t n = read(pipe_fds[0], &byte, 1);
  pthread_join(thread, NULL);
  if (caught) {
    printf("read: n=%zd byte=%c handled=%d\n", n, n == 1 ? byte : '-', (int)handled);
  } else {
    printf("read: n=%zd byte=%c\n", n, n == 1 ? byte : '-');
  }
  fflush(stdout);
  if (n != 1) {
    return 1;
  }
  char* next[] = {argv[0], caught ? "after-caught" : "after", NULL};
  execv(argv[0], next);
  perror("execv");
  return 1;
}
