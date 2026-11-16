/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* A forked child corrupts its own signal frame so rt_sigreturn refuses
 * it. Linux kills only that task with SIGSEGV; the parent must observe
 * the signal death and keep running. (On the KVM backend this once
 * aborted the whole run when the backend forced the child's exit
 * before the scheduler had an exit reservation for it:
 * https://github.com/facebookexperimental/hermit/issues/122.)
 * Prints "child status signal=11 exit=0" and "parent survived".
 */

#define _GNU_SOURCE
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <ucontext.h>
#include <unistd.h>

static void handler(int sig, siginfo_t* info, void* uctx) {
  (void)sig;
  (void)info;
  ucontext_t* uc = uctx;
  /* Point the frame's FP state at unmapped memory: rt_sigreturn must
   * refuse this frame on every machine. */
  uc->uc_mcontext.fpregs = (void*)0x414141000ULL;
}

int main(void) {
  struct sigaction act;
  memset(&act, 0, sizeof(act));
  act.sa_sigaction = handler;
  act.sa_flags = SA_SIGINFO;
  sigemptyset(&act.sa_mask);
  if (sigaction(SIGUSR1, &act, NULL) != 0) {
    printf("sigaction failed\n");
    return 2;
  }
  pid_t pid = fork();
  if (pid == 0) {
    /* No SIGSEGV handler (a sanitizer runtime installs one): the
     * refused frame must end this task by the signal itself. */
    signal(SIGSEGV, SIG_DFL);
    kill(getpid(), SIGUSR1);
    _exit(42);
  }
  if (pid < 0) {
    printf("fork failed\n");
    return 3;
  }
  int status = 0;
  waitpid(pid, &status, 0);
  printf(
      "child status signal=%d exit=%d\n",
      WIFSIGNALED(status) ? WTERMSIG(status) : 0,
      WIFEXITED(status) ? WEXITSTATUS(status) : 0);
  printf("parent survived\n");
  return 0;
}
