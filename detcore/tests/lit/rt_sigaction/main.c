// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

// RUN: %me | FileCheck %s
// CHECK: SIGHUP is masked
// CHECK-NEXT: SIGALRM is masked
// CHECK-NEXT: SIGSTKFLT is masked
// CHECK-EMPTY:

#include <assert.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/time.h>
#include <unistd.h>

static _Atomic int exit_flag;

static void checksig(const char* desc, const sigset_t* set, int sig) {
  /* use unlocked_stdio here since this function is called in a sighandler */
  fputs_unlocked(desc, stdout);
  if (sigismember(set, sig)) {
    fputs_unlocked(" is ", stdout);
  } else {
    fputs_unlocked(" is not ", stdout);
  }
  fputs_unlocked("masked\n", stdout);
}

static void sigalrm_handler(int sig, siginfo_t* siginfo, void* ctx) {
  sigset_t set;

  sigemptyset(&set);
  assert(sigprocmask(SIG_BLOCK, NULL, &set) == 0);

  checksig("SIGHUP", &set, SIGHUP);
  checksig("SIGALRM", &set, SIGALRM);
  checksig("SIGSTKFLT", &set, SIGSTKFLT);
  atomic_store(&exit_flag, 1);
}

static void mask_signal_via_sigaction(void) {
  struct sigaction sa;
  sigset_t set;

  memset(&sa, 0, sizeof(sa));
  sigemptyset(&set);
  sigaddset(&set, SIGHUP);
  sigaddset(&set, SIGSTKFLT);

  sa.sa_flags = SA_RESTART | SA_RESETHAND | SA_SIGINFO;
  sa.sa_mask = set;
  sa.sa_sigaction = sigalrm_handler;

  assert(sigaction(SIGALRM, &sa, NULL) == 0);

  struct itimerval it = {
      .it_interval =
          {
              .tv_sec = 0,
              .tv_usec = 0,
          },
      .it_value =
          {
              .tv_sec = 0,
              .tv_usec = 100000,
          },
  };

  assert(setitimer(ITIMER_REAL, &it, NULL) == 0);
}

int main(int argc, char* argv[]) {
  mask_signal_via_sigaction();
  while (atomic_load(&exit_flag) == 0)
    ;
  return 0;
}
