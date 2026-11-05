/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A single-threaded parent waits while its only child exits, under each
 * SIGCHLD disposition (SIGSHARE S4). Linux sends the child's exit signal to
 * the parent and wakes its wait queue (do_notify_parent, kernel/signal.c):
 * a caught SIGCHLD interrupts a blocking call, an ignored or default-ignored
 * one does not, and SA_NOCLDWAIT or SIG_IGN reap the child at once.
 *
 * Usage: child_exit_sigchld_wake MODE, where MODE is one of
 *   read-caught:     SIGCHLD caught (no SA_RESTART); the parent blocks in
 *                    read() on an empty self-pipe that the handler writes.
 *                    The read ends with EINTR, and the next read gets the
 *                    handler's byte.
 *   read-nocldstop:  the same with SA_NOCLDSTOP, which changes nothing for
 *                    an exit.
 *   read-nocldwait:  the same with SA_NOCLDWAIT: the signal is still sent,
 *                    and the child is reaped at once (waitpid: ECHILD).
 *   read-ignored:    SIGCHLD ignored; the parent blocks in read() on a pipe
 *                    whose only writer is the child. Nothing interrupts the
 *                    read; it returns 0 when the child's exit closes the pipe,
 *                    and the child was reaped (ECHILD).
 *   read-default:    the same with SIGCHLD at its default: no interruption,
 *                    and the child stays waitable.
 *   wait4-caught:    SIGCHLD caught; the parent blocks in wait4() for the
 *                    child, which returns it, and the handler runs once.
 *   wait4-default:   SIGCHLD at its default; wait4() returns the child, and
 *                    no signal is delivered.
 *   ppoll-caught:    SIGCHLD caught and blocked outside the call; the parent
 *                    blocks in ppoll() on the empty self-pipe with an empty
 *                    temporary mask: EINTR, the handler runs once.
 *   sigsuspend-caught: SIGCHLD caught and blocked; sigsuspend(empty) until the
 *                    handler saw the exit.
 *
 * Each mode prints one line, the same natively and under Hermit.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int self_pipe[2];
static volatile sig_atomic_t handler_runs;

static void on_chld(int sig) {
  (void)sig;
  int saved = errno;
  handler_runs++;
  char byte = 'c';
  if (write(self_pipe[1], &byte, 1) != 1) {
    _exit(90);
  }
  errno = saved;
}

static void sleep_ms(long ms) {
  struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
  while (nanosleep(&ts, &ts) != 0 && errno == EINTR) {
  }
}

static void install(int flags, void (*handler)(int)) {
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_handler = handler;
  sa.sa_flags = flags;
  sigemptyset(&sa.sa_mask);
  if (sigaction(SIGCHLD, &sa, NULL) != 0) {
    perror("sigaction");
    exit(2);
  }
}

static void block_sigchld(void) {
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGCHLD);
  if (sigprocmask(SIG_BLOCK, &set, NULL) != 0) {
    perror("sigprocmask");
    exit(2);
  }
}

/* A child that exits with status 7 after a short sleep, so the parent is
 * already blocked. With `keep_writer`, it keeps the pipe's write end open
 * until it exits; otherwise it closes both ends at once. */
static pid_t spawn(int keep_writer) {
  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    exit(2);
  }
  if (child == 0) {
    close(self_pipe[0]);
    if (!keep_writer) {
      close(self_pipe[1]);
    }
    sleep_ms(20);
    _exit(7);
  }
  return child;
}

/* Read once from the self-pipe, counting EINTR, until a read returns >= 0. */
static ssize_t read_counting_eintr(int* eintr) {
  char buf[8];
  for (;;) {
    ssize_t n = read(self_pipe[0], buf, sizeof buf);
    if (n < 0 && errno == EINTR) {
      (*eintr)++;
      continue;
    }
    return n;
  }
}

static int read_caught(const char* mode, int flags) {
  install(flags, on_chld);
  pid_t child = spawn(0);
  int eintr = 0;
  ssize_t n = read_counting_eintr(&eintr);
  int status = 0;
  pid_t waited = waitpid(child, &status, 0);
  if (flags & SA_NOCLDWAIT) {
    printf(
        "%s eintr=%d n=%zd handler=%d wait=%d echild=%d\n",
        mode,
        eintr,
        n,
        (int)handler_runs,
        (int)waited,
        waited < 0 && errno == ECHILD);
  } else {
    printf(
        "%s eintr=%d n=%zd handler=%d status=%d\n",
        mode,
        eintr,
        n,
        (int)handler_runs,
        waited == child && WIFEXITED(status) ? WEXITSTATUS(status) : -1);
  }
  return 0;
}

static int read_uncaught(const char* mode, void (*disposition)(int)) {
  install(0, disposition);
  pid_t child = spawn(1);
  close(self_pipe[1]);
  int eintr = 0;
  ssize_t n = read_counting_eintr(&eintr);
  int status = 0;
  pid_t waited = waitpid(child, &status, 0);
  if (disposition == SIG_IGN) {
    printf(
        "%s eintr=%d n=%zd wait=%d echild=%d\n",
        mode,
        eintr,
        n,
        (int)waited,
        waited < 0 && errno == ECHILD);
  } else {
    printf(
        "%s eintr=%d n=%zd status=%d\n",
        mode,
        eintr,
        n,
        waited == child && WIFEXITED(status) ? WEXITSTATUS(status) : -1);
  }
  return 0;
}

static int wait4_default(void) {
  install(0, SIG_DFL);
  pid_t child = spawn(0);
  int status = 0;
  pid_t waited = wait4(child, &status, 0, NULL);
  printf(
      "wait4-default pid_ok=%d status=%d\n",
      waited == child,
      WIFEXITED(status) ? WEXITSTATUS(status) : -1);
  return 0;
}

static int wait4_caught(void) {
  install(SA_RESTART, on_chld);
  pid_t child = spawn(0);
  int status = 0;
  pid_t waited;
  int eintr = 0;
  while ((waited = wait4(child, &status, 0, NULL)) < 0 && errno == EINTR) {
    eintr++;
  }
  printf(
      "wait4-caught pid_ok=%d eintr=%d status=%d handler=%d\n",
      waited == child,
      eintr,
      WIFEXITED(status) ? WEXITSTATUS(status) : -1,
      (int)handler_runs);
  return 0;
}

static int ppoll_caught(void) {
  install(0, on_chld);
  block_sigchld();
  pid_t child = spawn(0);
  sigset_t empty;
  sigemptyset(&empty);
  struct pollfd pfd = {.fd = self_pipe[0], .events = POLLIN};
  int rc = ppoll(&pfd, 1, NULL, &empty);
  int eintr = rc < 0 && errno == EINTR;
  int status = 0;
  pid_t waited = waitpid(child, &status, 0);
  printf(
      "ppoll-caught rc=%d eintr=%d handler=%d status=%d\n",
      rc,
      eintr,
      (int)handler_runs,
      waited == child && WIFEXITED(status) ? WEXITSTATUS(status) : -1);
  return 0;
}

static int sigsuspend_caught(void) {
  install(0, on_chld);
  block_sigchld();
  pid_t child = spawn(0);
  sigset_t empty;
  sigemptyset(&empty);
  int suspends = 0;
  while (!handler_runs) {
    sigsuspend(&empty);
    suspends++;
  }
  int status = 0;
  pid_t waited = waitpid(child, &status, 0);
  printf(
      "sigsuspend-caught suspends=%d handler=%d status=%d\n",
      suspends,
      (int)handler_runs,
      waited == child && WIFEXITED(status) ? WEXITSTATUS(status) : -1);
  return 0;
}

int main(int argc, char** argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s MODE\n", argv[0]);
    return 2;
  }
  setvbuf(stdout, NULL, _IONBF, 0);
  if (pipe(self_pipe) != 0) {
    perror("pipe");
    return 2;
  }
  const char* mode = argv[1];
  if (strcmp(mode, "read-caught") == 0) {
    return read_caught(mode, 0);
  } else if (strcmp(mode, "read-nocldstop") == 0) {
    return read_caught(mode, SA_NOCLDSTOP);
  } else if (strcmp(mode, "read-nocldwait") == 0) {
    return read_caught(mode, SA_NOCLDWAIT);
  } else if (strcmp(mode, "read-ignored") == 0) {
    return read_uncaught(mode, SIG_IGN);
  } else if (strcmp(mode, "read-default") == 0) {
    return read_uncaught(mode, SIG_DFL);
  } else if (strcmp(mode, "wait4-caught") == 0) {
    return wait4_caught();
  } else if (strcmp(mode, "wait4-default") == 0) {
    return wait4_default();
  } else if (strcmp(mode, "ppoll-caught") == 0) {
    return ppoll_caught();
  } else if (strcmp(mode, "sigsuspend-caught") == 0) {
    return sigsuspend_caught();
  }
  fprintf(stderr, "unknown mode %s\n", mode);
  return 2;
}
