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
 * (https://github.com/rrnewton/hermit/issues/4091). A child that is never a
 * zombie is ESRCH once it exits: its parent ignores SIGCHLD or sets
 * SA_NOCLDWAIT, so Linux reaps it at exit, or it is an orphan, which the
 * container's init reaps.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t handled;

static void report(const char* what, long ret);

/* Let the reaper Linux chose (the kernel itself, or init) finish natively. */
static void nap(void) {
  struct timespec ts = {0, 200 * 1000 * 1000};
  nanosleep(&ts, NULL);
}

/* Wait, without reaping, until the descriptor's writers have closed it. */
static void wait_eof(int fd) {
  char c;
  while (read(fd, &c, 1) > 0) {
  }
}

/* A child whose parent reaps it automatically: SIGCHLD set to SIG_IGN, or
 * SA_NOCLDWAIT. The disposition is restored afterwards. */
static void autoreaped_child(const char* what, int nocldwait) {
  struct sigaction action, saved;
  memset(&action, 0, sizeof(action));
  action.sa_handler = nocldwait ? SIG_DFL : SIG_IGN;
  action.sa_flags = nocldwait ? SA_NOCLDWAIT : 0;
  sigaction(SIGCHLD, &action, &saved);
  int done[2];
  pipe(done);
  pid_t child = fork();
  if (child == 0) {
    close(done[0]);
    _exit(0);
  }
  close(done[1]);
  wait_eof(done[0]);
  close(done[0]);
  nap();
  char label[64];
  snprintf(label, sizeof(label), "%s, kill", what);
  report(label, kill(child, SIGTERM));
  snprintf(label, sizeof(label), "%s, kill 0", what);
  report(label, kill(child, 0));
  sigaction(SIGCHLD, &saved, NULL);
}

/* A grandchild orphaned by its parent's exit, with no subreaper: the
 * container's init adopts and reaps it. */
static void orphaned_grandchild(void) {
  int pid_pipe[2], parent_gone[2];
  pipe(pid_pipe);
  pipe(parent_gone);
  pid_t child = fork();
  if (child == 0) {
    close(pid_pipe[0]);
    close(parent_gone[1]);
    pid_t grandchild = fork();
    if (grandchild == 0) {
      wait_eof(parent_gone[0]);
      close(pid_pipe[1]);
      _exit(0);
    }
    write(pid_pipe[1], &grandchild, sizeof(grandchild));
    close(pid_pipe[1]);
    _exit(0);
  }
  close(pid_pipe[1]);
  close(parent_gone[0]);
  pid_t grandchild;
  read(pid_pipe[0], &grandchild, sizeof(grandchild));
  waitpid(child, NULL, 0);
  close(parent_gone[1]);
  wait_eof(pid_pipe[0]);
  close(pid_pipe[0]);
  nap();
  report("orphan, kill", kill(grandchild, SIGTERM));
  report("orphan, kill 0", kill(grandchild, 0));
}

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
  report("zombie, kill 0", kill(zombie, 0));
  report("zombie, kill 99", kill(zombie, 99));
  waitpid(zombie, NULL, 0);
  report("reaped, kill", kill(zombie, SIGTERM));
  report("reaped, kill 0", kill(zombie, 0));

  autoreaped_child("SIG_IGN child", 0);
  autoreaped_child("SA_NOCLDWAIT child", 1);
  orphaned_grandchild();

  pthread_t thread;
  pthread_create(&thread, NULL, pauser, NULL);
  report("two threads, kill", kill(getpid(), SIGUSR1));
  report("two threads, rt_sigqueueinfo", queue_to_self());

  stop = 1;
  pthread_kill(thread, SIGUSR1);
  pthread_join(thread, NULL);
  return 0;
}
