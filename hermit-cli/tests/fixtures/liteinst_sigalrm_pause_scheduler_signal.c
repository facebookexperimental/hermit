/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A pause() that a signal the scheduler sends must not end, as on Linux,
 * where only a caught, unblocked signal ends it with EINTR
 * (https://github.com/rrnewton/hermit/issues/5338). Each mode forks its
 * child before it installs a SIGALRM handler, which the in-guest backends
 * admit only in a process that makes no further fork.
 *
 *   default   A child exits at 1 s while the parent sits in pause() with a
 *             SIGALRM handler and alarm(3). SIGCHLD has its default (ignore)
 *             action, so Linux discards it, and pause() returns EINTR for
 *             the SIGALRM at 3 s, the handler run first.
 *   ignored   As default, with SIGCHLD set to SIG_IGN: the child is reaped
 *             by the kernel, and waitpid() reports ECHILD.
 *   blocked   As default, with SIGCHLD blocked: pause() still ends at 3 s
 *             for the SIGALRM, and the SIGCHLD stays pending, which
 *             rt_sigtimedwait() then takes, naming the child.
 *   timer-urg, timer-ignored, timer-blocked
 *             No handler: a child sits in pause() with alarm(3) and a POSIX
 *             timer whose signal fires at 1 s: SIGURG at its default
 *             (ignore) action, SIGUSR1 set to SIG_IGN, or SIGUSR1 blocked.
 *             None ends the pause, so the child dies of SIGALRM at 3 s.
 *
 * The lines name what happened, not the times: "at_alarm" says whether
 * pause() returned at or after the alarm (2.5 s or later) rather than at the
 * child's exit or the timer.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static struct timespec start;
static volatile sig_atomic_t handled;
static volatile long handled_at = -1;

static long elapsed_ms(void) {
  struct timespec now;
  clock_gettime(CLOCK_MONOTONIC, &now);
  return (now.tv_sec - start.tv_sec) * 1000 + (now.tv_nsec - start.tv_nsec) / 1000000;
}

static void on_alarm(int sig) {
  (void)sig;
  handled = 1;
  handled_at = elapsed_ms();
}

static void block(int sig) {
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, sig);
  sigprocmask(SIG_BLOCK, &set, NULL);
}

static int child_exit(const char* mode) {
  int blocked = strcmp(mode, "blocked") == 0;
  if (strcmp(mode, "ignored") == 0) {
    signal(SIGCHLD, SIG_IGN);
  }
  if (blocked) {
    block(SIGCHLD);
  }
  pid_t child = fork();
  if (child < 0) {
    printf("fork errno=%d\n", errno);
    return 2;
  }
  if (child == 0) {
    struct timespec delay = {1, 0};
    nanosleep(&delay, NULL);
    _exit(0);
  }
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = on_alarm;
  if (sigaction(SIGALRM, &action, NULL) != 0) {
    printf("sigaction errno=%d\n", errno);
    return 2;
  }
  alarm(3);
  errno = 0;
  int ret = pause();
  int error = errno;
  long at = elapsed_ms();
  printf("%s: pause=%d errno=%d at_alarm=%d handled_first=%d\n", mode, ret, error, at >= 2500,
         handled && handled_at <= at);
  if (blocked) {
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGCHLD);
    siginfo_t info;
    struct timespec now = {0, 0};
    errno = 0;
    int taken = sigtimedwait(&set, &info, &now);
    if (taken < 0) {
      printf("%s: sigtimedwait=-1 errno=%d\n", mode, errno);
    } else {
      printf("%s: sigtimedwait=%d from_child=%d\n", mode, taken, info.si_pid == child);
    }
  }
  errno = 0;
  pid_t reaped = waitpid(child, NULL, 0);
  printf("%s: waitpid=%s errno=%d\n", mode, reaped == child ? "child" : "-1", reaped == child ? 0 : errno);
  return 0;
}

static int timer_signal(const char* mode) {
  int sig = strcmp(mode, "timer-urg") == 0 ? SIGURG : SIGUSR1;
  pid_t child = fork();
  if (child < 0) {
    printf("fork errno=%d\n", errno);
    return 2;
  }
  if (child == 0) {
    if (strcmp(mode, "timer-ignored") == 0) {
      signal(SIGUSR1, SIG_IGN);
    } else if (strcmp(mode, "timer-blocked") == 0) {
      block(SIGUSR1);
    }
    struct sigevent event;
    memset(&event, 0, sizeof(event));
    event.sigev_notify = SIGEV_SIGNAL;
    event.sigev_signo = sig;
    timer_t timer;
    if (timer_create(CLOCK_MONOTONIC, &event, &timer) != 0) {
      printf("timer_create errno=%d\n", errno);
      _exit(2);
    }
    struct itimerspec expiry;
    memset(&expiry, 0, sizeof(expiry));
    expiry.it_value.tv_sec = 1;
    if (timer_settime(timer, 0, &expiry, NULL) != 0) {
      printf("timer_settime errno=%d\n", errno);
      _exit(2);
    }
    alarm(3);
    int ret = pause();
    printf("%s: pause=%d errno=%d at_alarm=%d\n", mode, ret, errno, elapsed_ms() >= 2500);
    _exit(1);
  }
  int status = 0;
  waitpid(child, &status, 0);
  printf("%s: child_signal=%d\n", mode, WIFSIGNALED(status) ? WTERMSIG(status) : 0);
  return 0;
}

int main(int argc, char** argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  clock_gettime(CLOCK_MONOTONIC, &start);
  const char* mode = argc > 1 ? argv[1] : "default";
  if (strncmp(mode, "timer-", 6) == 0) {
    return timer_signal(mode);
  }
  if (strcmp(mode, "default") == 0 || strcmp(mode, "ignored") == 0 ||
      strcmp(mode, "blocked") == 0) {
    return child_exit(mode);
  }
  fprintf(stderr, "unknown mode %s\n", mode);
  return 2;
}
