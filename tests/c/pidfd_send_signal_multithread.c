/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * pidfd_send_signal to the caller's own process, first with one thread and
 * then with a second thread waiting in sigsuspend()
 * (https://github.com/rrnewton/hermit/issues/4047). A pidfd names a whole
 * process, so this is a process-directed signal: Linux's complete_signal picks
 * the receiving thread, which Detcore does not model, so with two threads it
 * refuses the call by name as it refuses kill. A fail-closed run stops with
 * the policy-refusal status at the two-thread send, and
 * --allow-unsupported-syscalls returns ENOSYS. Everything else is the
 * kernel's answer: signal 0, a zombie target (still 0), and SIGKILL to a
 * two-threaded child, which kills it whichever thread it reaches. A one-thread
 * child blocked in pause() must be woken to run its handler, as kill wakes it
 * (https://github.com/rrnewton/hermit/issues/4099).
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

static void exit_42(int sig) {
  (void)sig;
  _exit(42);
}

static volatile int stop;

/* The second thread waits with SIGUSR1 blocked except inside sigsuspend, so
 * the final pthread_kill cannot slip in between its check and its wait. */
static void* waiter(void* unused) {
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
  int pidfd = syscall(SYS_pidfd_open, getpid(), 0);
  if (pidfd < 0) {
    perror("pidfd_open");
    return 2;
  }

  report("one thread, pidfd_send_signal", syscall(SYS_pidfd_send_signal, pidfd, SIGUSR1, NULL, 0));
  printf("one thread, handled: %d\n", (int)handled);

  /* A zombie (exited, not reaped) is still signalled successfully. */
  pid_t zombie = fork();
  if (zombie == 0) {
    _exit(0);
  }
  siginfo_t info;
  waitid(P_PID, zombie, &info, WEXITED | WNOWAIT);
  int zombie_fd = syscall(SYS_pidfd_open, zombie, 0);
  report("zombie, pidfd_send_signal", syscall(SYS_pidfd_send_signal, zombie_fd, SIGUSR1, NULL, 0));
  waitpid(zombie, NULL, 0);

  /* A one-thread child blocked in pause() takes the signal and runs its
   * handler, which exits 42. */
  int blocked_ready[2];
  pipe(blocked_ready);
  pid_t blocked = fork();
  if (blocked == 0) {
    signal(SIGUSR1, exit_42);
    write(blocked_ready[1], "r", 1);
    for (;;) {
      pause();
    }
  }
  char blocked_byte;
  read(blocked_ready[0], &blocked_byte, 1);
  int blocked_fd = syscall(SYS_pidfd_open, blocked, 0);
  report("blocked child, pidfd_send_signal",
         syscall(SYS_pidfd_send_signal, blocked_fd, SIGUSR1, NULL, 0));
  int blocked_status = 0;
  waitpid(blocked, &blocked_status, 0);
  printf("blocked child: %s %d\n", WIFSIGNALED(blocked_status) ? "killed by signal" : "exited",
         WIFSIGNALED(blocked_status) ? WTERMSIG(blocked_status) : WEXITSTATUS(blocked_status));

  /* SIGKILL kills a two-threaded process whichever thread it reaches. */
  int ready[2];
  pipe(ready);
  pid_t child = fork();
  if (child == 0) {
    pthread_t sibling;
    pthread_create(&sibling, NULL, waiter, NULL);
    write(ready[1], "r", 1);
    for (;;) {
      pause();
    }
  }
  char byte;
  read(ready[0], &byte, 1);
  int child_fd = syscall(SYS_pidfd_open, child, 0);
  report("two-threaded child, pidfd_send_signal SIGKILL",
         syscall(SYS_pidfd_send_signal, child_fd, SIGKILL, NULL, 0));
  int status = 0;
  waitpid(child, &status, 0);
  printf("two-threaded child: %s %d\n", WIFSIGNALED(status) ? "killed by signal" : "exited",
         WIFSIGNALED(status) ? WTERMSIG(status) : WEXITSTATUS(status));

  pthread_t thread;
  pthread_create(&thread, NULL, waiter, NULL);
  report("two threads, pidfd_send_signal 0", syscall(SYS_pidfd_send_signal, pidfd, 0, NULL, 0));
  report("two threads, pidfd_send_signal", syscall(SYS_pidfd_send_signal, pidfd, SIGUSR1, NULL, 0));

  stop = 1;
  pthread_kill(thread, SIGUSR1);
  pthread_join(thread, NULL);
  return 0;
}
