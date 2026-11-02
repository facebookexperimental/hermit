/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Threads of one process take signals at the same time
// (https://github.com/rrnewton/hermit/issues/4899).
//
// Linux delivers each thread's signals on that thread's own return to user
// mode. On KVM, Detcore once held one signal-delivery permit per process: a
// thread that reached a signal boundary while a sibling waited for its turn
// to deliver was refused, and the backend failed the run ("KVM signal hook
// exhausted one return boundary without selecting delivery").
//
//   raise            the main thread and a worker each raise() a caught signal
//   tgkill           each sends itself a caught signal with tgkill
//   spin             a worker signals the main thread, which spins in getpid
//                    until its handler runs, then signals itself
//   sigsuspend       the same, with the main thread in a sigsuspend loop
//   alarm-<wait>     the main thread arms a 2 ms ITIMER_REAL and waits for its
//                    SIGALRM in <wait>: spin, nanosleep, clock_nanosleep,
//                    pause, sigsuspend, read or waitid. The worker starts
//                    with SIGALRM blocked, so only the main thread takes it,
//                    and keeps signaling itself until the main thread has, so
//                    its boundaries come while the main thread takes the
//                    alarm.
//   thread-exit      a worker keeps signaling itself while another worker
//                    exits, until the main thread has joined that one
//   exit-group       a worker keeps signaling itself while the main thread
//                    calls exit_group(3)
//   fatal            a worker keeps signaling itself while the main thread
//                    raises SIGTERM, whose default action ends the process
//   fatal-alarm      the same, with the main thread in pause() until a 2 ms
//                    ITIMER_REAL whose SIGALRM keeps its default action
//   alarm-exit-group [count]
//                    the main thread arms a 2 ms ITIMER_REAL, caught, and
//                    sleeps in nanosleep(100 s); the worker signals itself
//                    `count` times (default 1100) and then calls
//                    exit_group(3). On KVM that count lands the group exit
//                    while the alarm interrupts the main thread's sleep, so
//                    the exit comes while the main thread holds its observed
//                    wait's permit.
//
// In thread-exit, exit-group, fatal and fatal-alarm the worker's only
// scheduler requests are its signal deliveries, so whenever the exit is
// granted the worker is waiting with a delivery permit of its own.
//
// exit-group, fatal, fatal-alarm and alarm-exit-group run in a child process,
// which the mode ends, and the parent prints how it ended. Linux prints
// "<mode>: ok" for the first twelve modes, "exit-group: exited 3", "fatal:
// killed by signal 15", "fatal-alarm: killed by signal 14" and
// "alarm-exit-group: exited 3", and exits 0. A wait the alarm should have
// interrupted but did not prints its result instead. Other exit statuses: 2
// a setup call failed, 64 an unknown mode or a bad count.

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t usr1_seen;
static volatile sig_atomic_t usr2_seen;
static volatile sig_atomic_t alrm_seen;
static volatile sig_atomic_t stop;
static pthread_t main_thread;
static const char* mode;
static long count = 1100;

static void on_usr1(int sig) {
  (void)sig;
  usr1_seen = 1;
}

static void on_usr2(int sig) {
  (void)sig;
  usr2_seen = 1;
}

static void on_alrm(int sig) {
  (void)sig;
  alrm_seen = 1;
}

static void install(int sig, void (*handler)(int)) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = handler; // no SA_RESTART: an interrupted wait fails
  if (sigaction(sig, &action, NULL) != 0) {
    perror("sigaction");
    exit(2);
  }
}

static void signal_self(int sig) {
  syscall(SYS_tgkill, getpid(), (pid_t)syscall(SYS_gettid), sig);
}

static int is(const char* name) {
  return strcmp(mode, name) == 0;
}

static void* worker(void* arg) {
  (void)arg;
  if (is("raise")) {
    raise(SIGUSR2);
  } else if (is("tgkill")) {
    signal_self(SIGUSR2);
  } else if (is("spin") || is("sigsuspend")) {
    pthread_kill(main_thread, SIGUSR1);
    signal_self(SIGUSR2);
  } else if (is("alarm-exit-group")) {
    for (long i = 0; i < count; i++) {
      signal_self(SIGUSR2);
    }
    syscall(SYS_exit_group, 3);
  } else if (strncmp(mode, "alarm-", 6) == 0) {
    // SIGALRM stays blocked here, as the main thread created this thread.
    do {
      signal_self(SIGUSR2);
    } while (!alrm_seen);
  } else {
    // thread-exit, exit-group, fatal and fatal-alarm.
    do {
      signal_self(SIGUSR2);
    } while (!stop);
  }
  return NULL;
}

static void* leaver(void* arg) {
  (void)arg;
  return NULL;
}

static long alarm_wait(const char* wait, int* err) {
  struct timespec long_sleep = {100, 0};
  long ret = 0;
  *err = 0;
  if (strcmp(wait, "spin") == 0) {
    while (!alrm_seen) {
      syscall(SYS_getpid);
    }
    return -1;
  } else if (strcmp(wait, "nanosleep") == 0) {
    ret = syscall(SYS_nanosleep, &long_sleep, NULL);
  } else if (strcmp(wait, "clock_nanosleep") == 0) {
    ret = syscall(SYS_clock_nanosleep, CLOCK_MONOTONIC, 0, &long_sleep, NULL);
  } else if (strcmp(wait, "pause") == 0) {
    ret = syscall(SYS_pause);
  } else if (strcmp(wait, "sigsuspend") == 0) {
    sigset_t unblocked;
    pthread_sigmask(SIG_BLOCK, NULL, &unblocked);
    sigdelset(&unblocked, SIGALRM);
    while (!alrm_seen) {
      ret = sigsuspend(&unblocked);
    }
    *err = errno;
    return ret;
  } else if (strcmp(wait, "read") == 0) {
    int pipefd[2];
    char byte;
    if (pipe(pipefd) != 0) {
      perror("pipe");
      exit(2);
    }
    ret = read(pipefd[0], &byte, 1);
  } else if (strcmp(wait, "waitid") == 0) {
    pid_t child = fork();
    if (child < 0) {
      perror("fork");
      exit(2);
    }
    if (child == 0) {
      struct timespec one = {1, 0};
      syscall(SYS_nanosleep, &one, NULL);
      _exit(0);
    }
    siginfo_t info;
    memset(&info, 0, sizeof(info));
    ret = waitid(P_PID, (id_t)child, &info, WEXITED);
    *err = errno;
    if (ret != 0) {
      while (waitpid(child, NULL, 0) < 0 && errno == EINTR) {
      }
    }
    return ret;
  } else {
    fprintf(stderr, "unknown wait %s\n", wait);
    exit(64);
  }
  *err = errno;
  return ret;
}

// Runs `mode` in the calling process, which it ends in the last three modes.
static int run(void) {
  int alarm_mode = strncmp(mode, "alarm-", 6) == 0;
  int fatal_alarm = is("fatal-alarm");
  main_thread = pthread_self();
  install(SIGUSR1, on_usr1);
  install(SIGUSR2, on_usr2);
  if (!fatal_alarm) {
    install(SIGALRM, on_alrm);
  }
  if (is("sigsuspend")) {
    sigset_t usr1;
    sigemptyset(&usr1);
    sigaddset(&usr1, SIGUSR1);
    pthread_sigmask(SIG_BLOCK, &usr1, NULL);
  }
  sigset_t alrm;
  sigemptyset(&alrm);
  sigaddset(&alrm, SIGALRM);
  if (alarm_mode || fatal_alarm) {
    // The worker inherits SIGALRM blocked, so only this thread can take it.
    pthread_sigmask(SIG_BLOCK, &alrm, NULL);
  }
  pthread_t thread;
  if (pthread_create(&thread, NULL, worker, NULL) != 0) {
    fprintf(stderr, "pthread_create failed\n");
    return 2;
  }
  if ((alarm_mode && !is("alarm-sigsuspend")) || fatal_alarm) {
    pthread_sigmask(SIG_UNBLOCK, &alrm, NULL);
  }

  if (is("alarm-exit-group")) {
    struct itimerval two_ms = {{0, 0}, {0, 2000}};
    struct timespec long_sleep = {100, 0};
    if (setitimer(ITIMER_REAL, &two_ms, NULL) != 0) {
      perror("setitimer");
      return 2;
    }
    syscall(SYS_nanosleep, &long_sleep, NULL);
    // The worker's exit_group ends the process, here or before.
    for (;;) {
      syscall(SYS_sched_yield);
    }
  } else if (is("raise")) {
    raise(SIGUSR1);
  } else if (is("tgkill")) {
    signal_self(SIGUSR1);
  } else if (is("spin")) {
    while (!usr1_seen) {
      syscall(SYS_getpid);
    }
  } else if (is("sigsuspend")) {
    sigset_t unblocked;
    pthread_sigmask(SIG_BLOCK, NULL, &unblocked);
    sigdelset(&unblocked, SIGUSR1);
    while (!usr1_seen) {
      sigsuspend(&unblocked);
    }
  } else if (alarm_mode) {
    struct itimerval two_ms = {{0, 0}, {0, 2000}};
    if (setitimer(ITIMER_REAL, &two_ms, NULL) != 0) {
      perror("setitimer");
      return 2;
    }
    int err;
    long ret = alarm_wait(mode + 6, &err);
    while (!alrm_seen) {
      syscall(SYS_getpid);
    }
    if (strcmp(mode + 6, "spin") != 0 && !(ret == -1 && err == EINTR)) {
      printf("%s: the wait returned %ld errno %d\n", mode, ret, err);
      return 0;
    }
  } else if (is("thread-exit")) {
    pthread_t leaving;
    if (pthread_create(&leaving, NULL, leaver, NULL) != 0) {
      fprintf(stderr, "pthread_create failed\n");
      return 2;
    }
    pthread_join(leaving, NULL);
    stop = 1;
  } else if (is("exit-group")) {
    syscall(SYS_exit_group, 3);
  } else if (is("fatal")) {
    raise(SIGTERM);
  } else {
    struct itimerval two_ms = {{0, 0}, {0, 2000}};
    if (setitimer(ITIMER_REAL, &two_ms, NULL) != 0) {
      perror("setitimer");
      return 2;
    }
    for (;;) {
      pause();
    }
  }
  pthread_join(thread, NULL);
  if (!usr2_seen || ((is("raise") || is("tgkill")) && !usr1_seen)) {
    printf("%s: missed a signal\n", mode);
    return 0;
  }
  printf("%s: ok\n", mode);
  return 0;
}

int main(int argc, char** argv) {
  if (argc != 2 && !(argc == 3 && strcmp(argv[1], "alarm-exit-group") == 0)) {
    fprintf(stderr, "usage: %s <mode> | alarm-exit-group [count]\n", argv[0]);
    return 64;
  }
  mode = argv[1];
  if (argc == 3) {
    char* end;
    count = strtol(argv[2], &end, 10);
    if (*argv[2] == '\0' || *end != '\0' || count < 0) {
      fprintf(stderr, "bad count %s\n", argv[2]);
      return 64;
    }
  }
  int ends_process = is("exit-group") || is("fatal") || is("fatal-alarm") ||
      is("alarm-exit-group");
  if (!(is("raise") || is("tgkill") || is("spin") || is("sigsuspend") ||
        is("thread-exit") || ends_process || strncmp(mode, "alarm-", 6) == 0)) {
    fprintf(stderr, "unknown mode %s\n", mode);
    return 64;
  }
  if (!ends_process) {
    return run();
  }
  // A child process ends itself; this one reports how, and exits 0.
  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return 2;
  }
  if (child == 0) {
    _exit(run());
  }
  int status;
  while (waitpid(child, &status, 0) < 0) {
    if (errno != EINTR) {
      perror("waitpid");
      return 2;
    }
  }
  if (WIFEXITED(status)) {
    printf("%s: exited %d\n", mode, WEXITSTATUS(status));
  } else if (WIFSIGNALED(status)) {
    printf("%s: killed by signal %d\n", mode, WTERMSIG(status));
  } else {
    printf("%s: status %#x\n", mode, status);
  }
  return 0;
}
