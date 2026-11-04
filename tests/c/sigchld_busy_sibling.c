/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A parent's SIGCHLD for a child that ends, while a sibling thread stays
 * runnable until the parent has reaped the child.
 *
 * Usage: sigchld_busy_sibling <kill|read|epoll|exit|sigsuspend|kill-unblocked|exit-unblocked>
 *
 * kill:       the child waits in pause(); the parent SIGKILLs it and reaps
 *             it.
 * read:       the child waits in pause(); the parent blocks in read() on a
 *             pipe while the sibling SIGKILLs the child, sleeps (letting
 *             the scheduler's internal read probes commit turns that never
 *             resume the parent), then writes the byte the read waits for;
 *             the parent reaps the child. The fenced SIGCHLD copy must
 *             survive those probe grants (PR 4627 review F1).
 * epoll:      like kill, but the sibling's never-blocking loop is a
 *             successful epoll_wait on a permanently writable pipe
 *             instead of sched_yield: each completed polling call is
 *             guest progress the bounded delivery counts, so the parent
 *             is still re-admitted (PR 4627 review F5).
 * exit:       the child exits at once; the parent reaps it.
 * sigsuspend: the child exits at once; the parent, which blocks SIGCHLD
 *             from before the fork, waits in sigsuspend() until its SIGCHLD
 *             handler has run, then reaps it. (A pause() loop would race:
 *             a SIGCHLD handled between its check and the call leaves it
 *             waiting forever, on Linux too.)
 *
 * The sibling blocks SIGCHLD and calls sched_yield() until the parent sets
 * `done`, so it never blocks. In the two -unblocked modes, kill and exit
 * otherwise, the sibling leaves SIGCHLD unblocked, so the parent's copy may
 * be taken by either thread. On Linux the parent runs beside it and the
 * program prints one line and exits 0.
 */

#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

static atomic_int done;
static atomic_int sibling_started;
static atomic_int handled;
static atomic_int parent_in_read;
static atomic_long yields;
static int sibling_unblocked;
static int read_mode;
static int epoll_mode;
static pid_t read_child;
static int pipefd[2];

static void on_sigchld(int sig) {
  (void)sig;
  atomic_store(&handled, 1);
}

static void* sibling(void* arg) {
  (void)arg;
  if (!sibling_unblocked) {
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGCHLD);
    pthread_sigmask(SIG_BLOCK, &set, NULL);
  }
  atomic_store(&sibling_started, 1);
  int epfd = -1;
  struct epoll_event ev;
  if (epoll_mode) {
    int ready_pipe[2];
    if (pipe(ready_pipe) != 0) {
      _exit(5);
    }
    epfd = epoll_create1(0);
    ev.events = EPOLLOUT;
    ev.data.fd = ready_pipe[1];
    if (epfd < 0 || epoll_ctl(epfd, EPOLL_CTL_ADD, ready_pipe[1], &ev) != 0) {
      _exit(6);
    }
  }
  if (read_mode) {
    while (!atomic_load(&parent_in_read)) {
      sched_yield();
    }
    for (int i = 0; i < 10; i++) {
      sched_yield();
    }
    if (kill(read_child, SIGKILL) != 0) {
      _exit(3);
    }
    for (int i = 0; i < 10; i++) {
      usleep(1000);
    }
    if (write(pipefd[1], "x", 1) != 1) {
      _exit(4);
    }
  }
  while (!atomic_load(&done)) {
    if (epoll_mode) {
      if (epoll_wait(epfd, &ev, 1, -1) != 1) {
        _exit(7);
      }
    } else {
      sched_yield();
    }
    atomic_fetch_add(&yields, 1);
  }
  return NULL;
}

int main(int argc, char** argv) {
  const char* mode = argc > 1 ? argv[1] : "";
  read_mode = strcmp(mode, "read") == 0;
  epoll_mode = strcmp(mode, "epoll") == 0;
  int kill_mode = read_mode || epoll_mode || strcmp(mode, "kill") == 0 ||
      strcmp(mode, "kill-unblocked") == 0;
  int suspend_mode = strcmp(mode, "sigsuspend") == 0;
  sibling_unblocked = strcmp(mode, "kill-unblocked") == 0 ||
      strcmp(mode, "exit-unblocked") == 0;
  if (!kill_mode && !suspend_mode && strcmp(mode, "exit") != 0 &&
      strcmp(mode, "exit-unblocked") != 0) {
    fprintf(stderr,
            "usage: %s <kill|read|epoll|exit|sigsuspend|kill-unblocked|exit-unblocked>\n",
            argv[0]);
    return 2;
  }
  if (read_mode && pipe(pipefd) != 0) {
    perror("pipe");
    return 2;
  }
  sigset_t unblocked;
  if (suspend_mode) {
    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = on_sigchld;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGCHLD, &sa, NULL) != 0) {
      perror("sigaction");
      return 2;
    }
    sigset_t chld;
    sigemptyset(&chld);
    sigaddset(&chld, SIGCHLD);
    if (sigprocmask(SIG_BLOCK, &chld, &unblocked) != 0) {
      perror("sigprocmask");
      return 2;
    }
  }
  pthread_t thread;
  if (pthread_create(&thread, NULL, sibling, NULL) != 0) {
    perror("pthread_create");
    return 2;
  }
  while (!atomic_load(&sibling_started)) {
    sched_yield();
  }
  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return 2;
  }
  if (child == 0) {
    if (kill_mode) {
      for (;;) {
        pause();
      }
    }
    _exit(0);
  }
  read_child = child;
  if (read_mode) {
    char c = 0;
    atomic_store(&parent_in_read, 1);
    if (read(pipefd[0], &c, 1) != 1 || c != 'x') {
      perror("read");
      return 2;
    }
  } else if (kill_mode && kill(child, SIGKILL) != 0) {
    perror("kill");
    return 2;
  }
  if (suspend_mode) {
    while (!atomic_load(&handled)) {
      sigsuspend(&unblocked);
    }
  }
  int status = 0;
  if (waitpid(child, &status, 0) != child) {
    perror("waitpid");
    return 2;
  }
  atomic_store(&done, 1);
  pthread_join(thread, NULL);
  printf("mode=%s exited=%d code=%d signaled=%d sig=%d handled=%d\n", mode,
         WIFEXITED(status), WIFEXITED(status) ? WEXITSTATUS(status) : 0,
         WIFSIGNALED(status), WIFSIGNALED(status) ? WTERMSIG(status) : 0,
         atomic_load(&handled));
  return 0;
}
