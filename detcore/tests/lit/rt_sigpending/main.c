/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// rt_sigpending must not report a copy Linux discarded when it was sent
// (https://github.com/rrnewton/hermit/issues/4293's record, applied to
// rt_sigpending). SIGURG is ignored by default, so Linux discards a
// process-directed copy at generation when some thread leaves it
// unblocked (`sig_ignored`, kernel/signal.c): it is never queued, and
// sigpending reads 0. Under ptrace the host kernel queues the copy for
// the tracer anyway, so without subtracting the discarded-at-send
// record the guest reads 1.
//
//   discarded       worker blocks SIGURG, main does not; the worker
//                   sends kill(getpid(), SIGURG) and reads its own
//                   pending set: 0 on Linux (the RED case). The reader
//                   must be the blocked worker: an unblocked thread
//                   that resumes first dequeues the host copy and
//                   discards it, hiding the bug.
//   shared-blocked  every thread blocks SIGURG; the same send is queued
//                   in the shared queue (a blocked signal is not
//                   discarded): 1.
//   private-blocked the worker blocks SIGURG and main sends it
//                   pthread_kill(worker, SIGURG): queued privately for
//                   the worker, whose pending set reads 1.
//   caught-blocked  a handler is installed and main blocks SIGURG; the
//                   send is queued (blocked, and caught signals are
//                   never discarded at send): pending 1, and unblocking
//                   runs the handler.
//   pending-ign     as discarded, with SIGUSR1 under explicit SIG_IGN:
//                   pending 0 (not default-ignore-specific).
//   pending-late-handler  as discarded, handler installed after the
//                   send, before the read: still 0 (send-time record).
//   pending-coalesce  worker sends itself a kept private SIGURG and a
//                   discarded shared one: pending 1 (per-queue kept copy).
//   pending-short   discarded shape read with raw rt_sigpending size 4:
//                   prefix cleared, byte past the prefix untouched.
//   pending-zero    raw rt_sigpending size 0: buffer untouched.
//
// Use a bogus 'RUN' directive so this test isn't disabled:
// RUN: true

/*
RUN: %me discarded | FileCheck --check-prefix=NATIVE-DISCARDED %s
NATIVE-DISCARDED: discarded: SIGURG pending 0
RUN: %me shared-blocked | FileCheck --check-prefix=NATIVE-SHARED %s
NATIVE-SHARED: shared-blocked: SIGURG pending 1
RUN: %me private-blocked | FileCheck --check-prefix=NATIVE-PRIVATE %s
NATIVE-PRIVATE: private-blocked: SIGURG pending 1
RUN: %me caught-blocked | FileCheck --check-prefix=NATIVE-CAUGHT %s
NATIVE-CAUGHT: caught-blocked: SIGURG pending 1 handled 1
RUN: %me pending-ign | FileCheck --check-prefix=NATIVE-IGN %s
NATIVE-IGN: pending-ign: SIGUSR1 pending 0
RUN: %me pending-late-handler | FileCheck --check-prefix=NATIVE-LATE %s
NATIVE-LATE: pending-late-handler: SIGURG pending 0
RUN: %me pending-coalesce | FileCheck --check-prefix=NATIVE-COALESCE %s
NATIVE-COALESCE: pending-coalesce: SIGURG pending 1
RUN: %me pending-short | FileCheck --check-prefix=NATIVE-SHORT %s
NATIVE-SHORT: pending-short: ret 0 pendingbyte 0 suffix 170
RUN: %me pending-zero | FileCheck --check-prefix=NATIVE-ZERO %s
NATIVE-ZERO: pending-zero: ret 0 first 170
*/

#include <assert.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static int ready_pipe[2];
static int go_pipe[2];
static int result_pipe[2];

static void block_sigurg(void) {
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGURG);
  assert(pthread_sigmask(SIG_BLOCK, &set, NULL) == 0);
}

static int pending_sigurg(void) {
  sigset_t set;
  assert(sigpending(&set) == 0);
  return sigismember(&set, SIGURG) == 1;
}

// Blocks SIGURG, announces itself, then waits until told to stop.
static void* blocking_worker(void* unused) {
  (void)unused;
  block_sigurg();
  assert(write(ready_pipe[1], "R", 1) == 1);
  char byte;
  assert(read(go_pipe[0], &byte, 1) == 1);
  return NULL;
}

// Blocks SIGURG, announces itself, then reports its own pending set.
static void* pending_worker(void* unused) {
  (void)unused;
  block_sigurg();
  assert(write(ready_pipe[1], "R", 1) == 1);
  char byte;
  assert(read(go_pipe[0], &byte, 1) == 1);
  char result = pending_sigurg() ? '1' : '0';
  assert(write(result_pipe[1], &result, 1) == 1);
  return NULL;
}

static int worker_signal = SIGURG;
static int worker_late_handler;
static int worker_coalesce;
static long worker_raw_size = -1;

static void block_sig(int sig) {
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, sig);
  assert(pthread_sigmask(SIG_BLOCK, &set, NULL) == 0);
}

static int pending_sig(int sig) {
  sigset_t set;
  assert(sigpending(&set) == 0);
  return sigismember(&set, sig) == 1;
}

static void noop_handler(int sig) {
  (void)sig;
}

// Blocks the worker signal, announces itself, and once released sends
// the process-directed signal itself and reports its own pending set.
// Main leaves it unblocked, so Linux discards the copy at generation.
static void* sending_worker(void* unused) {
  (void)unused;
  block_sig(worker_signal);
  assert(write(ready_pipe[1], "R", 1) == 1);
  char byte;
  assert(read(go_pipe[0], &byte, 1) == 1);
  if (worker_coalesce) {
    assert(pthread_kill(pthread_self(), worker_signal) == 0);
  }
  assert(kill(getpid(), worker_signal) == 0);
  if (worker_late_handler) {
    struct sigaction action;
    memset(&action, 0, sizeof action);
    action.sa_handler = noop_handler;
    assert(sigaction(worker_signal, &action, NULL) == 0);
  }
  if (worker_raw_size >= 0) {
    unsigned char buffer[8];
    memset(buffer, 0xaa, sizeof buffer);
    long ret = syscall(SYS_rt_sigpending, buffer, worker_raw_size);
    unsigned char out[3] = {(unsigned char)ret, buffer[2], buffer[4]};
    if (worker_raw_size == 0) {
      out[1] = buffer[0];
    }
    assert(write(result_pipe[1], out, 3) == 3);
  } else {
    char result = pending_sig(worker_signal) ? '1' : '0';
    assert(write(result_pipe[1], &result, 1) == 1);
  }
  return NULL;
}

static pthread_t spawn(void* (*start)(void*)) {
  assert(pipe(ready_pipe) == 0);
  assert(pipe(go_pipe) == 0);
  pthread_t thread;
  assert(pthread_create(&thread, NULL, start, NULL) == 0);
  char byte;
  assert(read(ready_pipe[0], &byte, 1) == 1);
  return thread;
}

static void dismissed(pthread_t thread) {
  assert(write(go_pipe[1], "D", 1) == 1);
  assert(pthread_join(thread, NULL) == 0);
}

static void discarded(void) {
  assert(pipe(result_pipe) == 0);
  pthread_t worker = spawn(sending_worker);
  // Main keeps SIGURG unblocked (it never blocked it; the call makes
  // that state explicit before the send).
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGURG);
  assert(pthread_sigmask(SIG_UNBLOCK, &set, NULL) == 0);
  assert(write(go_pipe[1], "G", 1) == 1);
  char result;
  assert(read(result_pipe[0], &result, 1) == 1);
  printf("discarded: SIGURG pending %c\n", result);
  assert(pthread_join(worker, NULL) == 0);
}

static char run_sending_worker(void) {
  assert(pipe(result_pipe) == 0);
  pthread_t worker = spawn(sending_worker);
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, worker_signal);
  assert(pthread_sigmask(SIG_UNBLOCK, &set, NULL) == 0);
  assert(write(go_pipe[1], "G", 1) == 1);
  char result;
  assert(read(result_pipe[0], &result, 1) == 1);
  assert(pthread_join(worker, NULL) == 0);
  return result;
}

static void pending_ign(void) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = SIG_IGN;
  assert(sigaction(SIGUSR1, &action, NULL) == 0);
  worker_signal = SIGUSR1;
  printf("pending-ign: SIGUSR1 pending %c\n", run_sending_worker());
}

static void pending_late_handler(void) {
  worker_late_handler = 1;
  printf("pending-late-handler: SIGURG pending %c\n", run_sending_worker());
}

static void pending_coalesce(void) {
  worker_coalesce = 1;
  printf("pending-coalesce: SIGURG pending %c\n", run_sending_worker());
}

static void pending_raw(long size) {
  worker_raw_size = size;
  assert(pipe(result_pipe) == 0);
  pthread_t worker = spawn(sending_worker);
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGURG);
  assert(pthread_sigmask(SIG_UNBLOCK, &set, NULL) == 0);
  assert(write(go_pipe[1], "G", 1) == 1);
  unsigned char out[3];
  assert(read(result_pipe[0], out, 3) == 3);
  assert(pthread_join(worker, NULL) == 0);
  if (size == 0) {
    printf("pending-zero: ret %d first %d\n", out[0], out[1]);
  } else {
    printf("pending-short: ret %d pendingbyte %d suffix %d\n", out[0], out[1],
           out[2]);
  }
}

static void shared_blocked(void) {
  pthread_t worker = spawn(blocking_worker);
  block_sigurg();
  assert(kill(getpid(), SIGURG) == 0);
  printf("shared-blocked: SIGURG pending %d\n", pending_sigurg());
  dismissed(worker);
}

static void private_blocked(void) {
  assert(pipe(result_pipe) == 0);
  pthread_t worker = spawn(pending_worker);
  assert(pthread_kill(worker, SIGURG) == 0);
  assert(write(go_pipe[1], "G", 1) == 1);
  char result;
  assert(read(result_pipe[0], &result, 1) == 1);
  printf("private-blocked: SIGURG pending %c\n", result);
  assert(pthread_join(worker, NULL) == 0);
}

static volatile sig_atomic_t handled;

static void handler(int sig) {
  (void)sig;
  handled = 1;
}

static void caught_blocked(void) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = handler;
  assert(sigaction(SIGURG, &action, NULL) == 0);
  block_sigurg();
  assert(kill(getpid(), SIGURG) == 0);
  int pending = pending_sigurg();
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGURG);
  assert(pthread_sigmask(SIG_UNBLOCK, &set, NULL) == 0);
  printf("caught-blocked: SIGURG pending %d handled %d\n", pending, handled ? 1 : 0);
}

int main(int argc, char* argv[]) {
  if (argc != 2) {
    fprintf(stderr, "%s [discarded | shared-blocked | private-blocked | caught-blocked | pending-ign | pending-late-handler | pending-coalesce | pending-short | pending-zero]\n", argv[0]);
    return 1;
  }
  if (strcmp(argv[1], "discarded") == 0) {
    discarded();
  } else if (strcmp(argv[1], "shared-blocked") == 0) {
    shared_blocked();
  } else if (strcmp(argv[1], "private-blocked") == 0) {
    private_blocked();
  } else if (strcmp(argv[1], "caught-blocked") == 0) {
    caught_blocked();
  } else if (strcmp(argv[1], "pending-ign") == 0) {
    pending_ign();
  } else if (strcmp(argv[1], "pending-late-handler") == 0) {
    pending_late_handler();
  } else if (strcmp(argv[1], "pending-coalesce") == 0) {
    pending_coalesce();
  } else if (strcmp(argv[1], "pending-short") == 0) {
    pending_raw(4);
  } else if (strcmp(argv[1], "pending-zero") == 0) {
    pending_raw(0);
  } else {
    fprintf(stderr, "%s [discarded | shared-blocked | private-blocked | caught-blocked | pending-ign | pending-late-handler | pending-coalesce | pending-short | pending-zero]\n", argv[0]);
    return 1;
  }
  return 0;
}
