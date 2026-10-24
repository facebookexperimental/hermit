/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

extern char** environ;
static volatile sig_atomic_t alarms;

static void require(int condition, const char* message) {
  if (!condition) {
    fprintf(stderr, "%s (errno=%d)\n", message, errno);
    exit(1);
  }
}

static uint64_t clock_ns(void) {
  struct timespec now;
  require(
      syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &now) == 0,
      "read monotonic clock");
  return (uint64_t)now.tv_sec * UINT64_C(1000000000) + (uint64_t)now.tv_nsec;
}

static uint64_t advance_clock(uint64_t previous) {
  for (int i = 0; i < 8; ++i) {
    uint64_t next = clock_ns();
    require(
        next > previous, "monotonic time must advance, including across exec");
    previous = next;
  }
  return previous;
}

static void caught_alarm(int signal) {
  if (signal != SIGALRM)
    _exit(2);
  ++alarms;
}

static void after_exec(char** argv) {
  int timers[2] = {atoi(argv[2]), atoi(argv[3])};
  uint64_t before = strtoull(argv[4], NULL, 10);
  uint64_t after = advance_clock(before);
  struct sigaction action = {.sa_handler = caught_alarm};
  sigemptyset(&action.sa_mask);
  require(sigaction(SIGALRM, &action, NULL) == 0, "install SIGALRM handler");

  struct itimerval itimer;
  require(getitimer(ITIMER_REAL, &itimer) == 0, "read surviving ITIMER_REAL");
  require(
      itimer.it_value.tv_sec || itimer.it_value.tv_usec,
      "ITIMER_REAL must remain armed across exec");

  /* SIGUSR2 stays at its default disposition. A surviving POSIX timer kills
   * this image at one second, before the surviving itimer fires at two. */
  struct timespec remaining = {.tv_sec = 3};
  while (nanosleep(&remaining, &remaining) != 0) {
    require(
        errno == EINTR && alarms == 1, "sleep interrupted by surviving itimer");
  }
  require(alarms == 1, "ITIMER_REAL must deliver exactly once after exec");
  require(
      advance_clock(after) > after + UINT64_C(3000000000),
      "survive beyond the deleted POSIX timer deadline");
  require(getitimer(ITIMER_REAL, &itimer) == 0, "read expired ITIMER_REAL");
  require(
      !itimer.it_value.tv_sec && !itimer.it_value.tv_usec &&
          !itimer.it_interval.tv_sec && !itimer.it_interval.tv_usec,
      "surviving one-shot itimer must be disarmed after delivery");

  for (int i = 0; i < 2; ++i) {
    struct itimerspec value;
    errno = 0;
    require(
        syscall(SYS_timer_gettime, timers[i], &value) == -1 && errno == EINVAL,
        "both armed and disarmed POSIX timers must be deleted by exec");
  }
  puts(
      "PASS exec deleted POSIX timers; failed exec retained timers; ITIMER_REAL survived; clock advanced");
}

static void failed_exec_expiry(char** argv) {
  struct sigaction action = {.sa_handler = SIG_DFL};
  sigemptyset(&action.sa_mask);
  require(
      sigaction(SIGUSR2, &action, NULL) == 0, "use fatal POSIX timer signal");
  sigset_t empty;
  sigemptyset(&empty);
  require(
      sigprocmask(SIG_SETMASK, &empty, NULL) == 0,
      "unblock POSIX timer signal");
  int timer;
  struct sigevent event = {
      .sigev_notify = SIGEV_SIGNAL, .sigev_signo = SIGUSR2};
  require(
      syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &timer) == 0,
      "create failed-exec expiry timer");
  const struct itimerspec value = {.it_value = {.tv_sec = 1}};
  require(
      syscall(SYS_timer_settime, timer, 0, &value, NULL) == 0,
      "arm failed-exec expiry timer");
  errno = 0;
  execve("/__hermit_exec_posix_timer_missing__", argv, environ);
  require(
      errno == ENOENT, "failed exec must return ENOENT before timer expiry");

  /* The host checks this fresh artifact before accepting SIGUSR2. It proves
   * that exec returned, even when the fatal signal prevents output capture. */
  const char marker[] = "failed exec returned ENOENT\n";
  int fd = open(argv[2], O_WRONLY | O_CREAT | O_EXCL, 0600);
  require(fd >= 0, "create failed-exec completion marker");
  require(
      write(fd, marker, sizeof(marker) - 1) == (ssize_t)(sizeof(marker) - 1),
      "write failed-exec completion marker");
  require(fsync(fd) == 0, "flush failed-exec completion marker");
  require(close(fd) == 0, "close failed-exec completion marker");

  struct timespec remaining = {.tv_sec = 2};
  while (nanosleep(&remaining, &remaining) != 0) {
    require(errno == EINTR, "wait for retained POSIX timer expiry");
  }
  require(0, "failed exec cancelled the POSIX timer deadline");
}

int main(int argc, char** argv) {
  if (argc == 3 && strcmp(argv[1], "failed-exec-expiry") == 0) {
    failed_exec_expiry(argv);
    return 1;
  }
  if (argc == 5 && strcmp(argv[1], "after") == 0) {
    after_exec(argv);
    return 0;
  }
  require(argc == 1, "unexpected guest arguments");
  advance_clock(clock_ns());

  sigset_t empty;
  sigemptyset(&empty);
  require(sigprocmask(SIG_SETMASK, &empty, NULL) == 0, "unblock timer signals");
  struct sigaction default_signal = {.sa_handler = SIG_DFL};
  sigemptyset(&default_signal.sa_mask);
  require(
      sigaction(SIGUSR2, &default_signal, NULL) == 0,
      "use fatal default disposition for old POSIX timer");
  /* Use kernel timer IDs so the new image can query the exact old objects
   * without depending on libc's opaque timer_t representation. */
  int timers[2];
  struct sigevent event = {
      .sigev_notify = SIGEV_SIGNAL, .sigev_signo = SIGUSR2};
  require(
      syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &timers[0]) == 0,
      "create signal-delivering POSIX timer");
  event.sigev_notify = SIGEV_NONE;
  require(
      syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &timers[1]) == 0,
      "create disarmed POSIX timer");
  const struct itimerspec value = {
      .it_value = {.tv_sec = 1}, .it_interval = {.tv_sec = 1}};
  require(
      syscall(SYS_timer_settime, timers[0], 0, &value, NULL) == 0,
      "arm periodic POSIX timer");

  errno = 0;
  execve("/__hermit_exec_posix_timer_missing__", argv, environ);
  require(errno == ENOENT, "failed exec must return ENOENT");
  for (int i = 0; i < 2; ++i) {
    struct itimerspec retained;
    require(
        syscall(SYS_timer_gettime, timers[i], &retained) == 0,
        "failed exec must retain POSIX timer IDs");
    if (i == 0) {
      require(
          retained.it_value.tv_sec || retained.it_value.tv_nsec,
          "failed exec must leave POSIX timer armed");
      require(
          retained.it_interval.tv_sec == 1 && !retained.it_interval.tv_nsec,
          "failed exec must preserve POSIX timer interval");
    }
  }
  const struct itimerval itimer = {.it_value = {.tv_sec = 2}};
  require(
      setitimer(ITIMER_REAL, &itimer, NULL) == 0,
      "arm ITIMER_REAL before exec");
  char first[24], second[24], timestamp[32];
  snprintf(first, sizeof(first), "%d", timers[0]);
  snprintf(second, sizeof(second), "%d", timers[1]);
  snprintf(timestamp, sizeof(timestamp), "%" PRIu64, advance_clock(clock_ns()));
  char* next[] = {argv[0], "after", first, second, timestamp, NULL};
  execve(argv[0], next, environ);
  require(0, "exec guest image");
  return 1;
}
