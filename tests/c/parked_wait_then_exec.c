/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A parked wait, then execve.
 *
 * The thread sleeps while a one-shot ITIMER_REAL fires, then replaces its
 * image. On KVM, Detcore parks the wait and owns it as a parked request
 * until the wait completes. If any of that ownership outlived the wait, the
 * exec would be refused (`complete_signal_exec`, ProtocolFailure::Identity).
 * Detcore lets reverie-kvm skip the signal boundary at system-call returns
 * that can deliver nothing (`skip_empty_signal_boundaries`); this guest checks
 * that no cleanup was skipped with it.
 *
 *   caught   nanosleep is interrupted by a caught SIGALRM (EINTR);
 *   ignored  SIGALRM is ignored, so nanosleep sleeps its full time (0);
 *   pause    pause is interrupted by a caught SIGALRM (EINTR).
 *
 * Each mode then execs this program by its argv[0], so run it by its path,
 * as `after <mode>`, which sleeps once more, so a wait is parked in the new
 * image too, and prints a second line.
 * Linux prints, for example:
 *   caught: rc=-1 eintr=1 handler=1
 *   after caught: rc=0
 */

#define _GNU_SOURCE

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t handler_ran = 0;

static void on_alarm(int signo) {
  (void)signo;
  handler_ran = 1;
}

static int after(const char* mode) {
  struct timespec one_ms = {0, 1000000};
  int rc = nanosleep(&one_ms, NULL);
  printf("after %s: rc=%d\n", mode, rc);
  return rc == 0 ? 0 : 1;
}

int main(int argc, char** argv) {
  if (argc == 3 && strcmp(argv[1], "after") == 0) {
    return after(argv[2]);
  }
  if (argc != 2) {
    fprintf(stderr, "usage: %s caught|ignored|pause\n", argv[0]);
    return 2;
  }
  const char* mode = argv[1];
  int caught = strcmp(mode, "caught") == 0;
  int ignored = strcmp(mode, "ignored") == 0;
  int paused = strcmp(mode, "pause") == 0;
  if (!caught && !ignored && !paused) {
    fprintf(stderr, "unknown mode %s\n", mode);
    return 2;
  }

  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = ignored ? SIG_IGN : on_alarm;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGALRM, &action, NULL) != 0) {
    perror("sigaction");
    return 1;
  }
  struct itimerval ten_ms;
  memset(&ten_ms, 0, sizeof(ten_ms));
  ten_ms.it_value.tv_usec = 10000;
  if (setitimer(ITIMER_REAL, &ten_ms, NULL) != 0) {
    perror("setitimer");
    return 1;
  }

  errno = 0;
  int rc;
  if (paused) {
    rc = pause();
  } else {
    struct timespec sleep_for = {0, ignored ? 30000000 : 0};
    if (caught) {
      sleep_for.tv_sec = 1;
    }
    rc = nanosleep(&sleep_for, NULL);
  }
  int eintr = errno == EINTR;
  printf(
      "%s: rc=%d eintr=%d handler=%d\n", mode, rc, eintr, (int)handler_ran);
  fflush(stdout);
  int as_linux =
      ignored ? rc == 0 && !handler_ran : rc == -1 && eintr && handler_ran;
  if (!as_linux) {
    return 1;
  }

  char* next[] = {argv[0], "after", (char*)mode, NULL};
  execv(argv[0], next);
  perror("execv");
  return 1;
}
