/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// SIGTRAP's action and mask bit stay as the guest set them across
// pthread_create and a joined thread, and the thread's procfs status reports
// them, as natively (https://github.com/rrnewton/hermit/issues/4409,
// https://github.com/rrnewton/hermit/issues/4485). Under ptrace, a
// single-step of Reverie's ends with a forced SIGTRAP, which resets a blocked
// or ignored SIGTRAP to SIG_DFL and unblocks it; glibc's pthread_create blocks
// every signal around the clone, and Detcore injects those calls.
//
// Usage: signal_state_kept MODE STATE
//   MODE  single: one thread throughout; threads: a joined second thread.
//   STATE ign, ign-blocked, handler, handler-blocked, dfl-blocked: SIGTRAP's
//         action and mask bit before.
//
// Prints SIGTRAP's state as each thread sees it (the action's handler kind,
// flags and sa_mask, the rt_sigprocmask bit, procfs SigBlk, SigIgn and
// SigCgt, and the whole SigBlk), before and after the thread and in it (or
// twice, in single mode), then "done". The caller compares the output with
// the native run's.

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
  if (sigaction(SIGTRAP, NULL, &action) != 0) {
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
  unsigned long long bit = 1ULL << (SIGTRAP - 1);
  unsigned long long blocked = status_field("SigBlk:");
  printf(
      "%s: action=%s flags=%#x usr2-in-sa_mask=%d blocked=%d "
      "SigBlk=%d SigIgn=%d SigCgt=%d SigBlk-all=%#llx\n",
      where,
      kind,
      (unsigned)action.sa_flags,
      sigismember(&action.sa_mask, SIGUSR2),
      sigismember(&mask, SIGTRAP),
      (blocked & bit) != 0,
      (status_field("SigIgn:") & bit) != 0,
      (status_field("SigCgt:") & bit) != 0,
      blocked);
  fflush(stdout);
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
  } else if (strcmp(state, "dfl-blocked") == 0) {
    action.sa_handler = SIG_DFL;
  } else {
    fprintf(stderr, "unknown state %s\n", state);
    exit(2);
  }
  if (sigaction(SIGTRAP, &action, NULL) != 0) {
    perror("sigaction");
    exit(2);
  }
  sigset_t mask;
  sigemptyset(&mask);
  sigaddset(&mask, SIGTRAP);
  if (pthread_sigmask(block ? SIG_BLOCK : SIG_UNBLOCK, &mask, NULL) != 0) {
    fprintf(stderr, "pthread_sigmask failed\n");
    exit(2);
  }
}

static void* thread_main(void* arg) {
  (void)arg;
  report("thread");
  return NULL;
}

int main(int argc, char** argv) {
  if (argc != 3 || (strcmp(argv[1], "single") != 0 && strcmp(argv[1], "threads") != 0)) {
    fprintf(stderr, "usage: %s single|threads STATE\n", argv[0]);
    return 2;
  }
  int threads = strcmp(argv[1], "threads") == 0;
  // Start from an empty mask, whatever the caller blocked, so that the
  // native and the Hermit run start alike.
  sigset_t empty;
  sigemptyset(&empty);
  if (pthread_sigmask(SIG_SETMASK, &empty, NULL) != 0) {
    fprintf(stderr, "pthread_sigmask failed\n");
    return 2;
  }
  set_state(argv[2]);
  report("before");
  if (threads) {
    pthread_t thread;
    if (pthread_create(&thread, NULL, thread_main, NULL) != 0) {
      fprintf(stderr, "pthread_create failed\n");
      return 2;
    }
    if (pthread_join(thread, NULL) != 0) {
      fprintf(stderr, "pthread_join failed\n");
      return 2;
    }
  }
  report("after");
  printf("done\n");
  return 0;
}
