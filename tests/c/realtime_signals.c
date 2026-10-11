/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Realtime signals, 32 to 64, reach the guest under Hermit
 * (https://github.com/rrnewton/reverie/issues/207,
 * https://github.com/rrnewton/hermit/issues/3941):
 *
 * 1. Three sigqueue calls to this process queue SIGRTMIN+3 (value 30), then
 *    SIGRTMIN+2 twice (values 20 and 21), all blocked. Unblocking both
 *    delivers the lower signal first: Linux sets up the frame for SIGRTMIN+2
 *    value 20, whose handler mask blocks SIGRTMIN+2, then nests the frame for
 *    SIGRTMIN+3 above it. So the handlers run SIGRTMIN+3 (30), SIGRTMIN+2 (20),
 *    and, once that handler's return unblocks it, SIGRTMIN+2 (21), each with
 *    its value, SI_QUEUE and this pid.
 * 2. A thread asleep in sigsuspend, with SIGRTMIN+1 blocked outside it, is
 *    woken by a tgkill of SIGRTMIN+1 from the main thread.
 * 3. glibc's pthread_cancel of a thread asleep in pause sends it signal 32,
 *    which glibc reserves for cancellation, and the thread ends canceled.
 *
 * Part 1 runs before the thread exists: a process-directed signal is sent
 * only while the process has one thread.
 */

#define _GNU_SOURCE
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#define MAX_DELIVERIES 8

static volatile sig_atomic_t deliveries;
static int delivered_signal[MAX_DELIVERIES];
static int delivered_value[MAX_DELIVERIES];
static int delivered_code[MAX_DELIVERIES];
static pid_t delivered_pid[MAX_DELIVERIES];

static void record_delivery(int signal, siginfo_t* info, void* context) {
  (void)context;
  int slot = deliveries;
  if (slot < MAX_DELIVERIES) {
    delivered_signal[slot] = signal;
    delivered_value[slot] = info->si_value.sival_int;
    delivered_code[slot] = info->si_code;
    delivered_pid[slot] = info->si_pid;
  }
  deliveries = slot + 1;
}

static volatile sig_atomic_t woken;
static int ready[2];
static pid_t sleeper_tid;

static void note_wake(int signal) {
  (void)signal;
  woken++;
}

static void* sleeper(void* unused) {
  (void)unused;
  sleeper_tid = (pid_t)syscall(SYS_gettid);
  sigset_t none;
  sigemptyset(&none);
  if (write(ready[1], "r", 1) != 1) {
    return NULL;
  }
  /* The signal is blocked except inside sigsuspend. */
  while (!woken) {
    sigsuspend(&none);
  }
  return NULL;
}

static void block(const sigset_t* set, int how) {
  if (pthread_sigmask(how, set, NULL) != 0) {
    perror("pthread_sigmask");
    _exit(2);
  }
}

static int queued_in_order(void) {
  int low = SIGRTMIN + 2;
  int high = SIGRTMIN + 3;
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_sigaction = record_delivery;
  action.sa_flags = SA_SIGINFO;
  sigemptyset(&action.sa_mask);
  if (sigaction(low, &action, NULL) != 0 ||
      sigaction(high, &action, NULL) != 0) {
    perror("sigaction");
    return 1;
  }
  sigset_t both;
  sigemptyset(&both);
  sigaddset(&both, low);
  sigaddset(&both, high);
  block(&both, SIG_BLOCK);
  const struct {
    int signal;
    int value;
  } sends[] = {{high, 30}, {low, 20}, {low, 21}};
  for (unsigned i = 0; i < sizeof(sends) / sizeof(sends[0]); i++) {
    union sigval value = {.sival_int = sends[i].value};
    if (sigqueue(getpid(), sends[i].signal, value) != 0) {
      perror("sigqueue");
      return 1;
    }
  }
  printf("queued, delivered while blocked: %d\n", (int)deliveries);
  block(&both, SIG_UNBLOCK);
  int failures = 0;
  for (int i = 0; i < deliveries && i < MAX_DELIVERIES; i++) {
    int code_ok = delivered_code[i] == SI_QUEUE;
    int pid_ok = delivered_pid[i] == getpid();
    printf(
        "delivered SIGRTMIN+%d value %d%s%s\n",
        delivered_signal[i] - SIGRTMIN,
        delivered_value[i],
        code_ok ? " SI_QUEUE" : " wrong si_code",
        pid_ok ? " from this pid" : " from another pid");
    failures += !code_ok + !pid_ok;
  }
  return failures;
}

static int sigsuspend_wake(void) {
  int wake = SIGRTMIN + 1;
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = note_wake;
  sigemptyset(&action.sa_mask);
  if (sigaction(wake, &action, NULL) != 0) {
    perror("sigaction");
    return 1;
  }
  sigset_t blocked;
  sigemptyset(&blocked);
  sigaddset(&blocked, wake);
  block(&blocked, SIG_BLOCK);
  if (pipe(ready) != 0) {
    perror("pipe");
    return 1;
  }
  pthread_t thread;
  if (pthread_create(&thread, NULL, sleeper, NULL) != 0) {
    perror("pthread_create");
    return 1;
  }
  char byte;
  if (read(ready[0], &byte, 1) != 1) {
    perror("read");
    return 1;
  }
  long sent = syscall(SYS_tgkill, getpid(), sleeper_tid, wake);
  printf("tgkill SIGRTMIN+1 to the sleeper: %ld\n", sent);
  pthread_join(thread, NULL);
  printf("sleeper woken: %d\n", (int)woken);
  return sent != 0 || woken != 1;
}

static void* pauser(void* unused) {
  (void)unused;
  if (write(ready[1], "p", 1) != 1) {
    return NULL;
  }
  for (;;) {
    pause();
  }
  return NULL;
}

static int cancel_pauser(void) {
  pthread_t thread;
  if (pthread_create(&thread, NULL, pauser, NULL) != 0) {
    perror("pthread_create");
    return 1;
  }
  char byte;
  if (read(ready[0], &byte, 1) != 1) {
    perror("read");
    return 1;
  }
  int sent = pthread_cancel(thread);
  void* result = NULL;
  pthread_join(thread, &result);
  int canceled = result == PTHREAD_CANCELED;
  printf("pthread_cancel: %d, canceled: %d\n", sent, canceled);
  return sent != 0 || !canceled;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  int failures = queued_in_order();
  failures += sigsuspend_wake();
  failures += cancel_pauser();
  printf("failures=%d\n", failures);
  return failures != 0;
}
