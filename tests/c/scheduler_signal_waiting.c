/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A signal the scheduler sends while every thread blocks it waits in the
// process's shared queue. Detcore arms a later call that the signal will end
// at once, so the call rejoins at a fixed point
// (https://github.com/rrnewton/hermit/issues/4234). Most modes remove the
// signal before that call in one of the ways Linux does, so no call may be
// armed for it; arming one held the release barrier for a continuation that
// never came and refused the run (Claude and Codex reviews of
// https://github.com/rrnewton/hermit/pull/4314).
//
// Usage: scheduler_signal_waiting MODE [chld|alrm]
//
// The signal S is SIGCHLD from a child's exit (chld, the default) or SIGALRM
// from a one-shot ITIMER_REAL (alrm). S and SIGUSR1 are blocked and S is
// caught; S is produced, and main yields (chld) or sleeps (alrm) so the
// scheduler sends it while every thread blocks it. Then:
//   control   the copy stays pending: main's sigsuspend takes it at once.
//   dfl, ign  S's disposition is reset to default, or ignored. Linux flushes
//             the copy when the action ignores it (SIGCHLD's default does,
//             SIGALRM's does not).
//   sigwait   sigwaitinfo takes it.
//   signalfd  a signalfd read takes it.
//   handler   main unblocks S (its handler takes it) and blocks it again.
//   race      a thread U sigsuspends while main takes the copy with its
//             handler; main then wakes U with SIGUSR1.
//   private   main queues a second S to itself with tgkill and unblocks S:
//             Linux takes the thread's own copy first, and the handler keeps
//             S blocked on return, so the shared copy stays pending. A thread
//             W then yields until main is done, at most 2000 times, while
//             main's sigsuspend takes the shared copy.
//   other     main queues SIGUSR1 to itself, whose handler blocks S. Its
//             first sigsuspend, which S would end, is ended by SIGUSR1 (the
//             thread's own queue is dequeued first), so S stays pending and
//             ends the second at once, while a thread W yields until main is
//             done, at most 2000 times.
//   sfd-idle, sfd-closed
//             a signalfd for S is made before S is produced and never read,
//             or closed at once. S stays pending and ends main's sigsuspend
//             at once while W yields until main is done, at most 2000 times.
// After the removal (all but race, private, other and sfd-*) the handler is installed
// again, a thread T yields 200 times and sends main SIGUSR1, and main waits in
// sigsuspend with nothing blocked.
#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/signalfd.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <ucontext.h>
#include <unistd.h>

static int sig = SIGCHLD;
static int keep_blocked_once;
static volatile sig_atomic_t entries;
static volatile sig_atomic_t first_code;
static volatile sig_atomic_t got_usr1;
static atomic_int go;
static atomic_int done;
static pthread_t main_thread;

static void on_sig(int signo, siginfo_t* info, void* context) {
  (void)signo;
  if (++entries == 1) {
    first_code = info->si_code;
    if (keep_blocked_once) {
      sigaddset(&((ucontext_t*)context)->uc_sigmask, sig);
    }
  }
}

static void on_usr1(int signo) {
  (void)signo;
  got_usr1 = 1;
}

static void install_handler(void) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_sigaction = on_sig;
  action.sa_flags = SA_SIGINFO;
  sigemptyset(&action.sa_mask);
  sigaction(sig, &action, NULL);
}

static void* send_usr1_later(void* unused) {
  (void)unused;
  for (int i = 0; i < 200; i++) {
    sched_yield();
  }
  pthread_kill(main_thread, SIGUSR1);
  return NULL;
}

static void* suspend_when_told(void* unused) {
  (void)unused;
  while (!atomic_load(&go)) {
    sched_yield();
  }
  sigset_t none;
  sigemptyset(&none);
  sigsuspend(&none);
  return NULL;
}

static void* yield_until_done(void* saw_done) {
  for (int i = 0; i < 2000 && !atomic_load(&done); i++) {
    sched_yield();
  }
  *(int*)saw_done = atomic_load(&done);
  return NULL;
}

static int sig_pending(void) {
  sigset_t pending;
  sigpending(&pending);
  return sigismember(&pending, sig);
}

// Whether S is pending in this thread's own queue and in the shared one.
static void queues(int* own, int* shared) {
  unsigned long long bit = 1ULL << (sig - 1);
  unsigned long long value;
  char line[256];
  FILE* status = fopen("/proc/thread-self/status", "r");
  if (status == NULL) {
    perror("fopen");
    exit(1);
  }
  while (fgets(line, sizeof(line), status) != NULL) {
    if (sscanf(line, "SigPnd: %llx", &value) == 1) {
      *own = (value & bit) != 0;
    } else if (sscanf(line, "ShdPnd: %llx", &value) == 1) {
      *shared = (value & bit) != 0;
    }
  }
  fclose(status);
}

static void unblock_and_block(void) {
  sigset_t s;
  sigemptyset(&s);
  sigaddset(&s, sig);
  sigprocmask(SIG_UNBLOCK, &s, NULL);
  sigprocmask(SIG_BLOCK, &s, NULL);
}

static void produce(void) {
  if (sig == SIGCHLD) {
    pid_t child = fork();
    if (child == 0) {
      _exit(0);
    }
    waitpid(child, NULL, 0);
    for (int i = 0; i < 100; i++) {
      sched_yield();
    }
  } else {
    struct itimerval timer = {.it_value = {.tv_usec = 20000}};
    setitimer(ITIMER_REAL, &timer, NULL);
    struct timespec delay = {.tv_nsec = 120000000};
    nanosleep(&delay, NULL);
  }
}

int main(int argc, char** argv) {
  const char* mode = argc > 1 ? argv[1] : "control";
  const char* source = argc > 2 ? argv[2] : "chld";
  sig = strcmp(source, "alrm") ? SIGCHLD : SIGALRM;
  setvbuf(stdout, NULL, _IONBF, 0);
  main_thread = pthread_self();
  install_handler();
  signal(SIGUSR1, on_usr1);
  sigset_t blocked;
  sigemptyset(&blocked);
  sigaddset(&blocked, sig);
  sigaddset(&blocked, SIGUSR1);
  sigprocmask(SIG_BLOCK, &blocked, NULL);
  int sfd = -1;
  int sfd_mode = !strncmp(mode, "sfd-", 4);
  if (!strcmp(mode, "signalfd") || sfd_mode) {
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, sig);
    sfd = signalfd(-1, &s, 0);
    if (sfd < 0) {
      perror("signalfd");
      return 1;
    }
    if (!strcmp(mode, "sfd-closed")) {
      close(sfd);
    }
  }
  pthread_t helper;
  int race = !strcmp(mode, "race");
  if (race && pthread_create(&helper, NULL, suspend_when_told, NULL)) {
    perror("pthread_create");
    return 1;
  }
  produce();
  int before = sig_pending();
  if (race) {
    atomic_store(&go, 1);
    sched_yield();
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, sig);
    sigprocmask(SIG_UNBLOCK, &s, NULL);
    pthread_kill(helper, SIGUSR1);
    pthread_join(helper, NULL);
    printf("race %s: pending before %d; taken %d\n", source, before, (int)entries);
    return 0;
  }
  if (!strcmp(mode, "private")) {
    int own_before = 0, shared_before = 0, own_after = 0, shared_after = 0;
    syscall(SYS_tgkill, getpid(), syscall(SYS_gettid), sig);
    queues(&own_before, &shared_before);
    keep_blocked_once = 1;
    unblock_and_block();
    queues(&own_after, &shared_after);
    int taken_first = entries;
    int saw_done = 0;
    if (pthread_create(&helper, NULL, yield_until_done, &saw_done)) {
      perror("pthread_create");
      return 1;
    }
    sigset_t none;
    sigemptyset(&none);
    sigsuspend(&none);
    atomic_store(&done, 1);
    pthread_join(helper, NULL);
    printf("private %s: queued own %d shared %d; first take tkill %d, then own %d "
           "shared %d; taken %d then %d; sibling saw done %d\n",
           source, own_before, shared_before, first_code == SI_TKILL, own_after,
           shared_after, taken_first, (int)entries, saw_done);
    return 0;
  }
  if (sfd_mode) {
    int saw_done = 0;
    if (pthread_create(&helper, NULL, yield_until_done, &saw_done)) {
      perror("pthread_create");
      return 1;
    }
    sigset_t none;
    sigemptyset(&none);
    sigsuspend(&none);
    atomic_store(&done, 1);
    pthread_join(helper, NULL);
    printf("%s %s: pending before %d; woke sig %d; sibling saw done %d\n", mode, source,
           before, (int)entries, saw_done);
    return 0;
  }
  if (!strcmp(mode, "other")) {
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_handler = on_usr1;
    sigemptyset(&action.sa_mask);
    sigaddset(&action.sa_mask, sig);
    sigaction(SIGUSR1, &action, NULL);
    pthread_kill(main_thread, SIGUSR1);
    int saw_done = 0;
    if (pthread_create(&helper, NULL, yield_until_done, &saw_done)) {
      perror("pthread_create");
      return 1;
    }
    sigset_t none;
    sigemptyset(&none);
    sigsuspend(&none);
    int first_usr1 = got_usr1, first_sig = entries, between = sig_pending();
    sigsuspend(&none);
    atomic_store(&done, 1);
    pthread_join(helper, NULL);
    printf("other %s: pending before %d; first woke usr1 %d sig %d, then pending %d; "
           "second woke sig %d; sibling saw done %d\n",
           source, before, first_usr1, first_sig, between, (int)entries, saw_done);
    return 0;
  }
  if (!strcmp(mode, "dfl")) {
    signal(sig, SIG_DFL);
  } else if (!strcmp(mode, "ign")) {
    signal(sig, SIG_IGN);
  } else if (!strcmp(mode, "sigwait")) {
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, sig);
    sigwaitinfo(&s, NULL);
  } else if (!strcmp(mode, "signalfd")) {
    struct signalfd_siginfo info;
    if (read(sfd, &info, sizeof(info)) != (ssize_t)sizeof(info)) {
      perror("read");
    }
  } else if (!strcmp(mode, "handler")) {
    unblock_and_block();
  }
  int after = sig_pending();
  install_handler();
  entries = 0;
  pthread_t sender;
  if (pthread_create(&sender, NULL, send_usr1_later, NULL)) {
    perror("pthread_create");
    return 1;
  }
  sigset_t none;
  sigemptyset(&none);
  sigsuspend(&none);
  pthread_join(sender, NULL);
  printf("%s %s: pending before %d after %d; woke sig %d usr1 %d\n", mode, source,
         before, after, (int)entries, (int)got_usr1);
  return 0;
}
