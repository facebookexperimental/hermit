/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Record/replay guest: rt_sigtimedwait and the SIGCHLD of a child's exit.
 *
 * The parent waits for SIGUSR1 with a one-second rt_sigtimedwait while its
 * child exits after 100 ms. Linux decides from SIGCHLD's disposition:
 *
 *   caught   SIGCHLD has a handler and is unblocked: it ends the wait with
 *            EINTR and the handler runs.
 *   blocked  SIGCHLD has a handler but is blocked: the wait times out with
 *            EAGAIN, and the handler runs once SIGCHLD is unblocked.
 *   default  SIGCHLD keeps its default action, which ignores it: the wait
 *            times out with EAGAIN.
 *
 * The guest checks its own result against Linux's and exits nonzero on a
 * mismatch. Record and replay used to return EAGAIN for `caught` once their
 * waits held SIGCHLD as `hermit run` does (Codex review of
 * https://github.com/rrnewton/hermit/pull/3989).
 */

#define _GNU_SOURCE

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t sigchlds;

static void count_sigchld(int signal) {
  (void)signal;
  sigchlds++;
}

int main(int argc, char** argv) {
  if (argc != 2 || (strcmp(argv[1], "caught") != 0 &&
                    strcmp(argv[1], "blocked") != 0 &&
                    strcmp(argv[1], "default") != 0)) {
    fprintf(stderr, "usage: %s caught|blocked|default\n", argv[0]);
    return 2;
  }
  int caught = strcmp(argv[1], "default") != 0;
  int blocked = strcmp(argv[1], "blocked") == 0;
  if (caught) {
    struct sigaction action;
    memset(&action, 0, sizeof action);
    action.sa_handler = count_sigchld;
    if (sigaction(SIGCHLD, &action, NULL) != 0) {
      perror("sigaction");
      return 1;
    }
  }
  sigset_t waited;
  sigemptyset(&waited);
  sigaddset(&waited, SIGUSR1);
  sigset_t blocking = waited;
  if (blocked) {
    sigaddset(&blocking, SIGCHLD);
  }
  if (sigprocmask(SIG_BLOCK, &blocking, NULL) != 0) {
    perror("sigprocmask");
    return 1;
  }
  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return 1;
  }
  if (child == 0) {
    usleep(100000);
    _exit(0);
  }
  struct timespec timeout = {1, 0};
  errno = 0;
  int result = sigtimedwait(&waited, NULL, &timeout);
  int result_errno = errno;
  int status = 0;
  if (waitpid(child, &status, 0) != child) {
    perror("waitpid");
    return 1;
  }
  if (blocked) {
    sigset_t sigchld_set;
    sigemptyset(&sigchld_set);
    sigaddset(&sigchld_set, SIGCHLD);
    sigprocmask(SIG_UNBLOCK, &sigchld_set, NULL);
  }
  printf(
      "sigtimedwait-child-exit mode=%s result=%d errno=%d sigchlds=%d\n",
      argv[1],
      result,
      result < 0 ? result_errno : 0,
      (int)sigchlds);
  int expected_errno = strcmp(argv[1], "caught") == 0 ? EINTR : EAGAIN;
  int expected_sigchlds = caught ? 1 : 0;
  if (result != -1 || result_errno != expected_errno ||
      sigchlds != expected_sigchlds || status != 0) {
    fprintf(stderr, "sigtimedwait-child-exit %s mismatch\n", argv[1]);
    return 1;
  }
  return 0;
}
