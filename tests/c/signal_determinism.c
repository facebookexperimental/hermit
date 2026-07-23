/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <unistd.h>

#define ALT_STACK_SIZE (64 * 1024)

static volatile sig_atomic_t alarm_deliveries;
static volatile sig_atomic_t alarm_phase;
static volatile sig_atomic_t alarm_observed_phase;

static volatile sig_atomic_t reentrant_depth;
static volatile sig_atomic_t reentrant_deliveries;
static volatile sig_atomic_t reentrant_max_depth;

static unsigned char alternate_stack[ALT_STACK_SIZE];
static volatile sig_atomic_t altstack_deliveries;
static volatile sig_atomic_t altstack_address_ok = 1;

static int blocking_read_pipe[2];
static volatile sig_atomic_t blocking_read_deliveries;
static volatile sig_atomic_t blocking_read_handler_failed;

static int nonrestartable_write_fd = -1;
static int nonrestartable_signal;
static volatile sig_atomic_t nonrestartable_deliveries;
static volatile sig_atomic_t nonrestartable_handler_failed;

static void write_message(const char* message, size_t length) {
  (void)write(STDOUT_FILENO, message, length);
}

static int signal_is_blocked(int signal_number) {
  sigset_t current;
  if (pthread_sigmask(SIG_SETMASK, NULL, &current) != 0) {
    return -1;
  }
  return sigismember(&current, signal_number);
}

static void alarm_handler(int signal_number) {
  (void)signal_number;
  alarm_observed_phase = alarm_phase;
  ++alarm_deliveries;
  static const char message[] = "alarm delivered\n";
  write_message(message, sizeof(message) - 1);
}

static int test_itimer_delivery(void) {
  sigset_t blocked;
  sigset_t previous;
  sigemptyset(&blocked);
  sigaddset(&blocked, SIGALRM);
  if (sigprocmask(SIG_BLOCK, &blocked, &previous) != 0) {
    perror("sigprocmask");
    return 1;
  }

  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = alarm_handler;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGALRM, &action, NULL) != 0) {
    perror("sigaction");
    return 1;
  }

  alarm_phase = 1;
  const struct itimerval timer = {
      .it_value = {.tv_sec = 0, .tv_usec = 1},
  };
  if (setitimer(ITIMER_REAL, &timer, NULL) != 0) {
    perror("setitimer");
    return 1;
  }

  sigset_t pending;
  int was_pending = 0;
  for (int attempt = 0; attempt < 100000 && !was_pending; ++attempt) {
    if (sigpending(&pending) != 0) {
      perror("sigpending");
      return 1;
    }
    was_pending = sigismember(&pending, SIGALRM);
    if (!was_pending && sched_yield() != 0) {
      perror("sched_yield");
      return 1;
    }
  }
  if (was_pending != 1) {
    fputs("SIGALRM was not pending at the delivery point\n", stderr);
    return 1;
  }

  alarm_phase = 2;
  sigset_t wait_mask = previous;
  sigdelset(&wait_mask, SIGALRM);
  while (alarm_deliveries == 0) {
    errno = 0;
    if (sigsuspend(&wait_mask) != -1 || errno != EINTR) {
      perror("sigsuspend");
      return 1;
    }
  }
  if (sigprocmask(SIG_SETMASK, &previous, NULL) != 0) {
    perror("sigprocmask restore");
    return 1;
  }
  if (alarm_deliveries != 1 || alarm_observed_phase != 2) {
    fprintf(
        stderr,
        "unexpected alarm delivery: count=%d phase=%d\n",
        (int)alarm_deliveries,
        (int)alarm_observed_phase);
    return 1;
  }

  printf(
      "alarm pending=%d phase=%d deliveries=%d\n",
      was_pending,
      (int)alarm_observed_phase,
      (int)alarm_deliveries);
  return 0;
}

static void blocking_read_handler(int signal_number) {
  (void)signal_number;
  sigset_t mask;
  sigemptyset(&mask);
  sigaddset(&mask, SIGUSR1);
  if (sigprocmask(SIG_BLOCK, &mask, NULL) != 0 ||
      write(blocking_read_pipe[1], "xx", 2) != 2) {
    blocking_read_handler_failed = 1;
    return;
  }
  ++blocking_read_deliveries;
}

static int test_blocking_read_interrupted_by_signal(int restart) {
  if (pipe(blocking_read_pipe) != 0) {
    perror("pipe");
    return 1;
  }

  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = blocking_read_handler;
  action.sa_flags = restart ? SA_RESTART : 0;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGALRM, &action, NULL) != 0) {
    perror("sigaction");
    return 1;
  }
  if (alarm(1) != 0) {
    fputs("unexpected pending alarm\n", stderr);
    return 1;
  }

  char bytes[2];
  if (!restart) {
    errno = 0;
    if (read(blocking_read_pipe[0], &bytes[0], 1) != -1 || errno != EINTR) {
      fputs("blocking read was not interrupted with EINTR\n", stderr);
      return 1;
    }
  }

  if (read(blocking_read_pipe[0], &bytes[0], 1) != 1 ||
      read(blocking_read_pipe[0], &bytes[1], 1) != 1) {
    perror("read");
    return 1;
  }
  if (blocking_read_handler_failed || blocking_read_deliveries != 1 ||
      bytes[0] != 'x' || bytes[1] != 'x') {
    fprintf(
        stderr,
        "blocking read signal failed: handler_failed=%d deliveries=%d bytes=%c%c\n",
        (int)blocking_read_handler_failed,
        (int)blocking_read_deliveries,
        bytes[0],
        bytes[1]);
    return 1;
  }

  printf(
      "blocking read %s deliveries=%d bytes=%c%c\n",
      restart ? "restarted" : "interrupted",
      (int)blocking_read_deliveries,
      bytes[0],
      bytes[1]);
  return 0;
}

static void nonrestartable_wait_handler(int signal_number) {
  (void)signal_number;
  ++nonrestartable_deliveries;
  if (nonrestartable_write_fd >= 0 &&
      write(nonrestartable_write_fd, "x", 1) != 1) {
    nonrestartable_handler_failed = 1;
  }
  if (nonrestartable_signal != 0 &&
      kill(getpid(), nonrestartable_signal) != 0) {
    nonrestartable_handler_failed = 1;
  }
}

static int arm_nonrestartable_wait(int write_fd, int queued_signal) {
  nonrestartable_write_fd = write_fd;
  nonrestartable_signal = queued_signal;
  nonrestartable_deliveries = 0;
  nonrestartable_handler_failed = 0;

  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = nonrestartable_wait_handler;
  action.sa_flags = SA_RESTART;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGALRM, &action, NULL) != 0) {
    perror("sigaction");
    return -1;
  }
  if (alarm(1) != 0) {
    fputs("unexpected pending alarm\n", stderr);
    return -1;
  }
  return 0;
}

static int check_nonrestartable_result(const char* syscall_name, int result,
                                       int saved_errno) {
  if (result != -1 || saved_errno != EINTR) {
    fprintf(
        stderr,
        "%s was restarted despite SA_RESTART: result=%d errno=%d\n",
        syscall_name,
        result,
        saved_errno);
    return -1;
  }
  if (nonrestartable_handler_failed || nonrestartable_deliveries != 1) {
    fprintf(
        stderr,
        "%s signal handler failed: handler_failed=%d deliveries=%d\n",
        syscall_name,
        (int)nonrestartable_handler_failed,
        (int)nonrestartable_deliveries);
    return -1;
  }
  return 0;
}

static int test_poll_interrupted_despite_sa_restart(void) {
  int descriptors[2];
  if (pipe(descriptors) != 0) {
    perror("pipe");
    return 1;
  }
  if (arm_nonrestartable_wait(descriptors[1], 0) != 0) {
    return 1;
  }

  struct pollfd descriptor = {
      .fd = descriptors[0],
      .events = POLLIN,
  };
  errno = 0;
  const int result = poll(&descriptor, 1, -1);
  const int saved_errno = errno;
  if (check_nonrestartable_result("poll", result, saved_errno) != 0) {
    return 1;
  }

  char byte = 0;
  if (read(descriptors[0], &byte, 1) != 1 || byte != 'x') {
    fputs("poll handler did not make its descriptor readable\n", stderr);
    return 1;
  }
  close(descriptors[0]);
  close(descriptors[1]);
  printf(
      "poll interrupted deliveries=%d\n",
      (int)nonrestartable_deliveries);
  return 0;
}

static int test_epoll_wait_interrupted_despite_sa_restart(void) {
  int descriptors[2];
  if (pipe(descriptors) != 0) {
    perror("pipe");
    return 1;
  }
  const int epoll_fd = epoll_create1(EPOLL_CLOEXEC);
  if (epoll_fd < 0) {
    perror("epoll_create1");
    return 1;
  }
  struct epoll_event registration = {
      .events = EPOLLIN,
      .data.fd = descriptors[0],
  };
  if (epoll_ctl(epoll_fd, EPOLL_CTL_ADD, descriptors[0], &registration) != 0) {
    perror("epoll_ctl");
    return 1;
  }
  if (arm_nonrestartable_wait(descriptors[1], 0) != 0) {
    return 1;
  }

  struct epoll_event event;
  errno = 0;
  const int result = epoll_wait(epoll_fd, &event, 1, -1);
  const int saved_errno = errno;
  if (check_nonrestartable_result("epoll_wait", result, saved_errno) != 0) {
    return 1;
  }

  char byte = 0;
  if (read(descriptors[0], &byte, 1) != 1 || byte != 'x') {
    fputs("epoll_wait handler did not make its descriptor readable\n", stderr);
    return 1;
  }
  close(epoll_fd);
  close(descriptors[0]);
  close(descriptors[1]);
  printf(
      "epoll_wait interrupted deliveries=%d\n",
      (int)nonrestartable_deliveries);
  return 0;
}

static int test_sigtimedwait_interrupted_despite_sa_restart(void) {
  sigset_t wait_set;
  sigset_t previous;
  sigemptyset(&wait_set);
  sigaddset(&wait_set, SIGUSR2);
  if (sigprocmask(SIG_BLOCK, &wait_set, &previous) != 0) {
    perror("sigprocmask");
    return 1;
  }
  if (arm_nonrestartable_wait(-1, SIGUSR2) != 0) {
    return 1;
  }

  const struct timespec timeout = {
      .tv_sec = 5,
      .tv_nsec = 0,
  };
  errno = 0;
  const int result = sigtimedwait(&wait_set, NULL, &timeout);
  const int saved_errno = errno;
  const int interruption_failed =
      check_nonrestartable_result("rt_sigtimedwait", result, saved_errno);

  sigset_t pending;
  if (sigpending(&pending) != 0) {
    perror("sigpending");
    return 1;
  }
  const int signal_was_pending = sigismember(&pending, SIGUSR2);
  if (signal_was_pending == 1) {
    const struct timespec no_wait = {
        .tv_sec = 0,
        .tv_nsec = 0,
    };
    if (sigtimedwait(&wait_set, NULL, &no_wait) != SIGUSR2) {
      fputs("rt_sigtimedwait did not consume pending SIGUSR2\n", stderr);
      return 1;
    }
  }
  if (sigprocmask(SIG_SETMASK, &previous, NULL) != 0) {
    perror("sigprocmask restore");
    return 1;
  }
  if (interruption_failed != 0) {
    return 1;
  }
  if (signal_was_pending != 1) {
    fputs("SIGUSR2 was not pending after rt_sigtimedwait interruption\n", stderr);
    return 1;
  }
  printf(
      "rt_sigtimedwait interrupted deliveries=%d pending=SIGUSR2\n",
      (int)nonrestartable_deliveries);
  return 0;
}

static void* check_clone_mask(void* argument) {
  (void)argument;
  const int blocked = signal_is_blocked(SIGUSR1);
  if (blocked == 1) {
    static const char message[] = "clone mask=blocked\n";
    write_message(message, sizeof(message) - 1);
    return NULL;
  }
  return (void*)(uintptr_t)1;
}

static int test_masks_across_fork_and_clone(void) {
  sigset_t blocked;
  sigset_t previous;
  sigemptyset(&blocked);
  sigaddset(&blocked, SIGUSR1);
  if (sigprocmask(SIG_BLOCK, &blocked, &previous) != 0) {
    perror("sigprocmask");
    return 1;
  }
  if (signal_is_blocked(SIGUSR1) != 1) {
    fputs("parent did not block SIGUSR1\n", stderr);
    return 1;
  }
  static const char parent_message[] = "parent mask=blocked\n";
  write_message(parent_message, sizeof(parent_message) - 1);

  const pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return 1;
  }
  if (child == 0) {
    if (signal_is_blocked(SIGUSR1) == 1) {
      static const char message[] = "fork mask=blocked\n";
      write_message(message, sizeof(message) - 1);
      _exit(0);
    }
    _exit(1);
  }

  int status = 0;
  if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    fputs("fork child did not inherit the signal mask\n", stderr);
    return 1;
  }

  pthread_t thread;
  if (pthread_create(&thread, NULL, check_clone_mask, NULL) != 0) {
    fputs("pthread_create failed\n", stderr);
    return 1;
  }
  void* result = NULL;
  if (pthread_join(thread, &result) != 0 || result != NULL) {
    fputs("clone thread did not inherit the signal mask\n", stderr);
    return 1;
  }

  if (sigprocmask(SIG_SETMASK, &previous, NULL) != 0) {
    perror("sigprocmask restore");
    return 1;
  }
  return 0;
}

static void reentrant_handler(int signal_number) {
  (void)signal_number;
  ++reentrant_depth;
  ++reentrant_deliveries;
  if (reentrant_depth > reentrant_max_depth) {
    reentrant_max_depth = reentrant_depth;
  }

  if (reentrant_depth == 1) {
    static const char message[] = "handler depth=1\n";
    write_message(message, sizeof(message) - 1);
  } else if (reentrant_depth == 2) {
    static const char message[] = "handler depth=2\n";
    write_message(message, sizeof(message) - 1);
  }

  if (reentrant_deliveries == 1) {
    (void)kill(getpid(), SIGUSR1);
  }
  --reentrant_depth;
}

static int test_handler_reentrance(void) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = reentrant_handler;
  action.sa_flags = SA_NODEFER;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGUSR1, &action, NULL) != 0) {
    perror("sigaction");
    return 1;
  }
  if (raise(SIGUSR1) != 0) {
    perror("raise");
    return 1;
  }
  if (reentrant_deliveries != 2 || reentrant_max_depth != 2 ||
      reentrant_depth != 0) {
    fprintf(
        stderr,
        "signal handler did not reenter: deliveries=%d max_depth=%d depth=%d\n",
        (int)reentrant_deliveries,
        (int)reentrant_max_depth,
        (int)reentrant_depth);
    return 1;
  }
  printf(
      "reentrant deliveries=%d max_depth=%d\n",
      (int)reentrant_deliveries,
      (int)reentrant_max_depth);
  return 0;
}

static void altstack_handler(int signal_number) {
  (void)signal_number;
  unsigned char marker = 0;
  const uintptr_t address = (uintptr_t)&marker;
  const uintptr_t start = (uintptr_t)alternate_stack;
  const uintptr_t end = start + sizeof(alternate_stack);
  if (address < start || address >= end) {
    altstack_address_ok = 0;
  }
  ++altstack_deliveries;
  static const char message[] = "altstack handler\n";
  write_message(message, sizeof(message) - 1);
}

static int test_altstack_preservation(void) {
  const stack_t alternate = {
      .ss_sp = alternate_stack,
      .ss_size = sizeof(alternate_stack),
      .ss_flags = 0,
  };
  stack_t previous;
  if (sigaltstack(&alternate, &previous) != 0) {
    perror("sigaltstack");
    return 1;
  }

  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = altstack_handler;
  action.sa_flags = SA_ONSTACK;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGUSR2, &action, NULL) != 0) {
    perror("sigaction");
    return 1;
  }

  if (raise(SIGUSR2) != 0) {
    perror("raise");
    return 1;
  }
  stack_t current;
  if (sigaltstack(NULL, &current) != 0) {
    perror("sigaltstack query");
    return 1;
  }
  const int preserved =
      (current.ss_flags & SS_DISABLE) == 0 &&
      current.ss_sp == alternate.ss_sp &&
      current.ss_size == alternate.ss_size;

  if (raise(SIGUSR2) != 0) {
    perror("raise");
    return 1;
  }
  if (sigaltstack(&previous, NULL) != 0) {
    perror("sigaltstack restore");
    return 1;
  }
  if (!preserved || !altstack_address_ok || altstack_deliveries != 2) {
    fprintf(
        stderr,
        "alternate signal stack was not preserved: deliveries=%d address_ok=%d preserved=%d\n",
        (int)altstack_deliveries,
        (int)altstack_address_ok,
        preserved);
    return 1;
  }
  printf(
      "altstack deliveries=%d preserved=%d\n",
      (int)altstack_deliveries,
      preserved);
  return 0;
}

static int check_pending_after_exec(void) {
  const int blocked = signal_is_blocked(SIGUSR1);
  sigset_t pending;
  if (sigpending(&pending) != 0) {
    perror("sigpending");
    return 1;
  }
  const int pending_before_wait = sigismember(&pending, SIGUSR1);
  if (blocked != 1 || pending_before_wait != 1) {
    fprintf(
        stderr,
        "exec did not preserve signal state: blocked=%d pending=%d\n",
        blocked,
        pending_before_wait);
    return 1;
  }

  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGUSR1);
  int received = 0;
  if (sigwait(&set, &received) != 0 || received != SIGUSR1) {
    fputs("sigwait did not consume the pending SIGUSR1\n", stderr);
    return 1;
  }
  puts("exec mask=blocked pending=preserved consumed=SIGUSR1");
  return 0;
}

static int test_pending_across_exec(const char* executable) {
  sigset_t blocked;
  sigemptyset(&blocked);
  sigaddset(&blocked, SIGUSR1);
  if (sigprocmask(SIG_BLOCK, &blocked, NULL) != 0) {
    perror("sigprocmask");
    return 1;
  }
  if (raise(SIGUSR1) != 0) {
    perror("raise");
    return 1;
  }
  sigset_t pending;
  if (sigpending(&pending) != 0 || sigismember(&pending, SIGUSR1) != 1) {
    fputs("SIGUSR1 was not pending before exec\n", stderr);
    return 1;
  }

  char* const arguments[] = {
      (char*)executable,
      (char*)"pending-exec-check",
      NULL,
  };
  execv("/proc/self/exe", arguments);
  perror("execv");
  return 1;
}

int main(int argc, char** argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s SCENARIO\n", argv[0]);
    return 2;
  }
  if (strcmp(argv[1], "itimer-delivery") == 0) {
    return test_itimer_delivery();
  }
  if (strcmp(argv[1], "masks-fork-clone") == 0) {
    return test_masks_across_fork_and_clone();
  }
  if (strcmp(argv[1], "blocking-read-interrupted") == 0) {
    return test_blocking_read_interrupted_by_signal(0);
  }
  if (strcmp(argv[1], "blocking-read-restarted") == 0) {
    return test_blocking_read_interrupted_by_signal(1);
  }
  if (strcmp(argv[1], "poll-sa-restart") == 0) {
    return test_poll_interrupted_despite_sa_restart();
  }
  if (strcmp(argv[1], "epoll-wait-sa-restart") == 0) {
    return test_epoll_wait_interrupted_despite_sa_restart();
  }
  if (strcmp(argv[1], "sigtimedwait-sa-restart") == 0) {
    return test_sigtimedwait_interrupted_despite_sa_restart();
  }
  if (strcmp(argv[1], "handler-reentrance") == 0) {
    return test_handler_reentrance();
  }
  if (strcmp(argv[1], "altstack-preservation") == 0) {
    return test_altstack_preservation();
  }
  if (strcmp(argv[1], "pending-exec") == 0) {
    return test_pending_across_exec(argv[0]);
  }
  if (strcmp(argv[1], "pending-exec-check") == 0) {
    return check_pending_after_exec();
  }
  fprintf(stderr, "unknown scenario: %s\n", argv[1]);
  return 2;
}
