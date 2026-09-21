/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

/* Three binary arguments select eight separately executed modes:
 * SA_RESTART, absolute clock_nanosleep, and worker-arm/exit. The selected CLI
 * regression compiles this fixture once and verifies all eight modes twice. */
static volatile sig_atomic_t deliveries;
static volatile sig_atomic_t observed_code;
static volatile sig_atomic_t observed_tid;
static void caught(int number, siginfo_t *info, void *context) {
  (void)context;
  if (number != SIGALRM) _exit(70);
  observed_code = info->si_code;
  observed_tid = (sig_atomic_t)syscall(SYS_gettid);
  ++deliveries;
}
static int arm(void) {
  const struct itimerval timer = {.it_value = {.tv_sec = 1, .tv_usec = 0}};
  return setitimer(ITIMER_REAL, &timer, NULL);
}
static void *arm_and_exit(void *unused) {
  (void)unused;
  return (void *)(long)(arm() == 0 ? 0 : 1);
}
int main(int argc, char **argv) {
  if (argc != 4) return 71;
  int restart = atoi(argv[1]), absolute = atoi(argv[2]), worker = atoi(argv[3]);
  if ((restart & ~1) || (absolute & ~1) || (worker & ~1)) return 72;
  struct sigaction action = {0};
  action.sa_sigaction = caught;
  action.sa_flags = SA_SIGINFO | (restart ? SA_RESTART : 0);
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGALRM, &action, NULL)) return 73;
  pid_t receiver = (pid_t)syscall(SYS_gettid);
  if (worker) {
    pthread_t thread;
    void *result = NULL;
    if (pthread_create(&thread, NULL, arm_and_exit, NULL) ||
        pthread_join(thread, &result) || result != NULL) return 74;
  } else if (arm()) return 75;
  struct timespec request = {.tv_sec = 2, .tv_nsec = 0};
  if (absolute) {
    struct timespec now;
    if (clock_gettime(CLOCK_REALTIME, &now)) return 76;
    request.tv_sec += now.tv_sec;
    request.tv_nsec = now.tv_nsec;
  }
  struct timespec remaining = {.tv_sec = 1234567, .tv_nsec = 7654321};
  int result = absolute ? clock_nanosleep(CLOCK_REALTIME, TIMER_ABSTIME, &request, &remaining)
                        : (nanosleep(&request, &remaining) == -1 ? errno : 0);
  if (result != EINTR || deliveries != 1 || observed_code != SI_KERNEL || observed_tid != receiver) return 87;
  if (absolute) {
    if (remaining.tv_sec != 1234567 || remaining.tv_nsec != 7654321) return 78;
  } else if (remaining.tv_sec < 0 || remaining.tv_sec >= 2 || remaining.tv_nsec < 0 ||
             remaining.tv_nsec >= 1000000000 || (remaining.tv_sec == 0 && remaining.tv_nsec == 0)) return 79;
  struct itimerval old;
  if (getitimer(ITIMER_REAL, &old) || old.it_value.tv_sec || old.it_value.tv_usec ||
      old.it_interval.tv_sec || old.it_interval.tv_usec) return 80;
  const struct timespec zero = {0};
  if (nanosleep(&zero, NULL)) return 81;
  printf("PASS caught=1 code=SI_KERNEL receiver=leader restart=%d absolute=%d worker_arm_exit=%d\n",
         restart, absolute, worker);
  return 0;
}
