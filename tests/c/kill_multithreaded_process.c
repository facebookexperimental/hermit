/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A process-directed signal (kill, rt_sigqueueinfo) to the caller's own
 * process, first with one thread and then with a second thread waiting in
 * sigsuspend() (https://github.com/rrnewton/hermit/issues/4034). Linux's
 * complete_signal picks the receiving thread from the threads that want the
 * signal; Detcore does not model that choice yet, so with two threads it
 * refuses the call by name: a fail-closed run stops with the policy-refusal
 * status after the first block, and --allow-unsupported-syscalls returns
 * ENOSYS. A zombie child (exited, not reaped) is still signalled
 * successfully, and once reaped it is ESRCH
 * (https://github.com/rrnewton/hermit/issues/4091).
 */

#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile sig_atomic_t handled;

static void on_usr1(int sig) {
  (void)sig;
  handled++;
}

static void report(const char* what, long ret) {
  if (ret == 0) {
    printf("%s: 0\n", what);
  } else {
    printf("%s: %ld %s\n", what, ret, strerrorname_np(errno));
  }
}

static long queue_to_self(void) {
  siginfo_t info;
  memset(&info, 0, sizeof(info));
  info.si_signo = SIGUSR1;
  info.si_code = SI_QUEUE;
  info.si_pid = getpid();
  info.si_uid = getuid();
  return syscall(SYS_rt_sigqueueinfo, getpid(), SIGUSR1, &info);
}

static volatile int stop;

/* The second thread waits with SIGUSR1 blocked except inside sigsuspend, so
 * the final pthread_kill cannot slip in between its check and its wait. */
static void* pauser(void* unused) {
  (void)unused;
  sigset_t usr1, none;
  sigemptyset(&usr1);
  sigaddset(&usr1, SIGUSR1);
  sigemptyset(&none);
  pthread_sigmask(SIG_BLOCK, &usr1, NULL);
  while (!stop) {
    sigsuspend(&none);
  }
  return NULL;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = on_usr1;
  sigaction(SIGUSR1, &action, NULL);

  report("one thread, kill", kill(getpid(), SIGUSR1));
  report("one thread, rt_sigqueueinfo", queue_to_self());
  printf("one thread, handled: %d\n", (int)handled);

  pid_t zombie = fork();
  if (zombie == 0) {
    _exit(0);
  }
  siginfo_t info;
  waitid(P_PID, zombie, &info, WEXITED | WNOWAIT);
  report("zombie, kill", kill(zombie, SIGTERM));
  waitpid(zombie, NULL, 0);
  report("reaped, kill", kill(zombie, SIGTERM));

  pthread_t thread;
  pthread_create(&thread, NULL, pauser, NULL);
  report("two threads, kill", kill(getpid(), SIGUSR1));
  report("two threads, rt_sigqueueinfo", queue_to_self());

  stop = 1;
  pthread_kill(thread, SIGUSR1);
  pthread_join(thread, NULL);
  return 0;
}
