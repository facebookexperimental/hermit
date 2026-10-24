/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// SIGSEGV's action and mask bit stay as the guest set them across rdtsc and
// cpuid, as natively, where neither instruction faults
// (https://github.com/rrnewton/hermit/issues/4485). Under ptrace, Hermit
// makes both fault so that it can emulate them, and Linux reports the fault
// with a forced SIGSEGV, which resets a blocked or ignored SIGSEGV to SIG_DFL
// and unblocks it before Reverie suppresses the signal.
//
// Usage: sigsegv_state_kept MODE STATE
//   MODE  single: one thread throughout; threads: a joined second thread
//         traps too.
//   STATE ign, ign-blocked, handler, handler-blocked, dfl, dfl-blocked:
//         SIGSEGV's action and mask bit before.
//
// Prints SIGSEGV's state (the action's handler kind, flags and sa_mask, the
// rt_sigprocmask bit, and procfs SigBlk, SigIgn and SigCgt) after each
// trapped instruction: an rdtsc, a cpuid, an rdtsc first thing in a SIGUSR1
// handler whose sa_mask blocks SIGSEGV, and an rdtsc right after that
// handler returns; in threads mode also in the thread and after the join.
// Then "done". The caller compares the output with the native run's.

#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void handler(int signo) {
  (void)signo;
}

static unsigned long long status_field(const char* name) {
  FILE* file = fopen("/proc/thread-self/status", "r");
  if (!file) {
    perror("fopen /proc/thread-self/status");
    exit(2);
  }
  char line[256];
  size_t length = strlen(name);
  unsigned long long value = ~0ULL;
  while (fgets(line, sizeof line, file)) {
    if (strncmp(line, name, length) == 0) {
      value = strtoull(line + length, NULL, 16);
      break;
    }
  }
  fclose(file);
  if (value == ~0ULL) {
    fprintf(stderr, "no %s in /proc/thread-self/status\n", name);
    exit(2);
  }
  return value;
}

static void report(const char* where) {
  struct sigaction action;
  if (sigaction(SIGSEGV, NULL, &action) != 0) {
    perror("sigaction");
    exit(2);
  }
  const char* kind = action.sa_handler == SIG_DFL ? "dfl"
      : action.sa_handler == SIG_IGN              ? "ign"
      : action.sa_handler == handler              ? "handler"
                                                  : "other";
  sigset_t mask;
  if (pthread_sigmask(SIG_BLOCK, NULL, &mask) != 0) {
    fprintf(stderr, "pthread_sigmask failed\n");
    exit(2);
  }
  unsigned long long bit = 1ULL << (SIGSEGV - 1);
  printf(
      "%s: action=%s flags=%#x usr2-in-sa_mask=%d blocked=%d "
      "SigBlk=%d SigIgn=%d SigCgt=%d\n",
      where,
      kind,
      (unsigned)action.sa_flags,
      sigismember(&action.sa_mask, SIGUSR2),
      sigismember(&mask, SIGSEGV),
      (status_field("SigBlk:") & bit) != 0,
      (status_field("SigIgn:") & bit) != 0,
      (status_field("SigCgt:") & bit) != 0);
  fflush(stdout);
}

static void rdtsc(void) {
  unsigned lo, hi;
  __asm__ volatile("rdtsc" : "=a"(lo), "=d"(hi));
  (void)lo;
  (void)hi;
}

static void cpuid(void) {
  unsigned a = 0, b, c = 0, d;
  __asm__ volatile("cpuid" : "+a"(a), "=b"(b), "+c"(c), "=d"(d));
  (void)b;
  (void)d;
}

static void set_state(const char* state) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  sigemptyset(&action.sa_mask);
  sigaddset(&action.sa_mask, SIGUSR2);
  action.sa_flags = SA_RESTART;
  int block = 1;
  if (strcmp(state, "ign") == 0) {
    action.sa_handler = SIG_IGN;
    block = 0;
  } else if (strcmp(state, "ign-blocked") == 0) {
    action.sa_handler = SIG_IGN;
  } else if (strcmp(state, "handler") == 0) {
    action.sa_handler = handler;
    block = 0;
  } else if (strcmp(state, "handler-blocked") == 0) {
    action.sa_handler = handler;
  } else if (strcmp(state, "dfl") == 0) {
    action.sa_handler = SIG_DFL;
    block = 0;
  } else if (strcmp(state, "dfl-blocked") == 0) {
    action.sa_handler = SIG_DFL;
  } else {
    fprintf(stderr, "unknown state %s\n", state);
    exit(2);
  }
  if (sigaction(SIGSEGV, &action, NULL) != 0) {
    perror("sigaction");
    exit(2);
  }
  sigset_t mask;
  sigemptyset(&mask);
  sigaddset(&mask, SIGSEGV);
  if (pthread_sigmask(block ? SIG_BLOCK : SIG_UNBLOCK, &mask, NULL) != 0) {
    fprintf(stderr, "pthread_sigmask failed\n");
    exit(2);
  }
}

// The first instruction is the trapped rdtsc, with SIGSEGV blocked by the
// handler's sa_mask whatever the mask before.
static void usr1_handler(int signo) {
  (void)signo;
  rdtsc();
  report("in-handler");
}

static void* thread_main(void* arg) {
  (void)arg;
  rdtsc();
  report("thread");
  return NULL;
}

int main(int argc, char** argv) {
  if (argc != 3 || (strcmp(argv[1], "single") != 0 && strcmp(argv[1], "threads") != 0)) {
    fprintf(stderr, "usage: %s single|threads STATE\n", argv[0]);
    return 2;
  }
  // Start from an empty mask, whatever the caller blocked, so that the
  // native and the Hermit run start alike.
  sigset_t empty;
  sigemptyset(&empty);
  if (pthread_sigmask(SIG_SETMASK, &empty, NULL) != 0) {
    fprintf(stderr, "pthread_sigmask failed\n");
    return 2;
  }
  struct sigaction usr1;
  memset(&usr1, 0, sizeof usr1);
  usr1.sa_handler = usr1_handler;
  sigemptyset(&usr1.sa_mask);
  sigaddset(&usr1.sa_mask, SIGSEGV);
  if (sigaction(SIGUSR1, &usr1, NULL) != 0) {
    perror("sigaction SIGUSR1");
    return 2;
  }
  set_state(argv[2]);
  report("before");
  rdtsc();
  report("rdtsc");
  cpuid();
  report("cpuid");
  raise(SIGUSR1);
  rdtsc();
  report("after-handler");
  if (strcmp(argv[1], "threads") == 0) {
    pthread_t thread;
    if (pthread_create(&thread, NULL, thread_main, NULL) != 0) {
      fprintf(stderr, "pthread_create failed\n");
      return 2;
    }
    if (pthread_join(thread, NULL) != 0) {
      fprintf(stderr, "pthread_join failed\n");
      return 2;
    }
    rdtsc();
    report("after-join");
  }
  printf("done\n");
  return 0;
}
