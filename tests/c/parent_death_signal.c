/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A parent-death signal armed with prctl(PR_SET_PDEATHSIG)
// (https://github.com/rrnewton/hermit/issues/4440). Modes:
//   term      a second thread forks the child, which arms SIGTERM and
//             pauses; that thread exits (its process lives on), so Linux
//             sends the child SIGTERM: "term: child signal 15"
//   caught    the same with a caught SIGUSR1: the child's pause returns
//             after the handler ran, and it exits 7: "caught: child exit 7"
//   get       PR_GET_PDEATHSIG reads back what PR_SET_PDEATHSIG armed, and a
//             forked child starts with none: "get: 10, child 0"
//   subreaper the main process is a child subreaper; a middle process forks
//             the child, which arms SIGTERM and pauses, then exits: the child
//             gets SIGTERM and is adopted: "subreaper: child signal 15"
//   group     the same, but the middle process's worker thread forks the
//             child and the main thread calls exit_group while the worker
//             lives: the worker's exit sends it: "group: child signal 15"
//   exec      a second thread forks the child, which arms a caught SIGUSR1
//             (exit 3 if it comes) and sleeps a second, then the thread
//             execs: it survives as the leader, so nothing is sent and the
//             new image reaps "exec: child exit 0"
//   invalid   the child arms SIGTERM, then tries 99, which Linux refuses
//             with EINVAL leaving SIGTERM armed: "invalid: get 15 einval 1",
//             then "invalid: child signal 15"
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/wait.h>
#include <unistd.h>

static pid_t child;
static int ready[2];
static volatile sig_atomic_t handled;

static void on_usr1(int sig) {
  (void)sig;
  handled = 1;
}

/* The child's body: arm `signal`, report readiness, and pause until a
 * handler has run (or the signal's default action ends it). */
static void armed_child(int signal) {
  if (signal == SIGUSR1) {
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_handler = on_usr1;
    sigaction(SIGUSR1, &action, NULL);
  }
  prctl(PR_SET_PDEATHSIG, signal);
  char byte = 'x';
  if (write(ready[1], &byte, 1) != 1) {
    _exit(9);
  }
  while (!handled) {
    pause();
  }
  _exit(7);
}

static void* creator(void* arg) {
  int signal = *(int*)arg;
  child = fork();
  if (child == 0) {
    armed_child(signal);
  }
  char byte;
  if (read(ready[0], &byte, 1) != 1) {
    _exit(1);
  }
  return NULL;
}

static void on_usr1_exit(int sig) {
  (void)sig;
  _exit(3);
}

static void* exec_creator(void* arg) {
  char* self = arg;
  child = fork();
  if (child == 0) {
    signal(SIGUSR1, on_usr1_exit);
    prctl(PR_SET_PDEATHSIG, SIGUSR1);
    char byte = 'x';
    if (write(ready[1], &byte, 1) != 1) {
      _exit(9);
    }
    sleep(1);
    _exit(0);
  }
  char byte;
  if (read(ready[0], &byte, 1) != 1) {
    _exit(8);
  }
  char* args[] = {self, "exec-after", NULL};
  execv(self, args);
  _exit(7);
}

static void* group_worker(void* arg) {
  (void)arg;
  child = fork();
  if (child == 0) {
    armed_child(SIGTERM);
  }
  for (;;) {
    pause();
  }
  return NULL;
}

static void* invalid_creator(void* arg) {
  (void)arg;
  child = fork();
  if (child == 0) {
    prctl(PR_SET_PDEATHSIG, SIGTERM);
    int rc = prctl(PR_SET_PDEATHSIG, 99);
    int error = errno, armed = 0;
    prctl(PR_GET_PDEATHSIG, &armed);
    printf("invalid: get %d einval %d\n", armed, rc == -1 && error == EINVAL);
    fflush(stdout);
    char byte = 'x';
    if (write(ready[1], &byte, 1) != 1) {
      _exit(9);
    }
    for (;;) {
      pause();
    }
  }
  char byte;
  if (read(ready[0], &byte, 1) != 1) {
    _exit(8);
  }
  return NULL;
}

static void report(const char* mode, int status) {
  printf("%s: child %s %d\n", mode, WIFSIGNALED(status) ? "signal" : "exit",
         WIFSIGNALED(status) ? WTERMSIG(status) : WEXITSTATUS(status));
}

int main(int argc, char** argv) {
  const char* mode = argc > 1 ? argv[1] : "term";
  if (!strcmp(mode, "exec-after")) {
    int status = 0;
    if (waitpid(-1, &status, 0) <= 0) {
      perror("waitpid");
      return 1;
    }
    report("exec", status);
    return 0;
  }
  if (pipe(ready) != 0) {
    return 1;
  }
  if (!strcmp(mode, "exec")) {
    pthread_t thread;
    if (pthread_create(&thread, NULL, exec_creator, argv[0]) != 0) {
      return 1;
    }
    for (;;) {
      pause();
    }
  }
  if (!strcmp(mode, "group")) {
    prctl(PR_SET_CHILD_SUBREAPER, 1);
    pid_t middle = fork();
    if (middle == 0) {
      pthread_t worker;
      if (pthread_create(&worker, NULL, group_worker, NULL) != 0) {
        _exit(1);
      }
      char byte;
      if (read(ready[0], &byte, 1) != 1) {
        _exit(1);
      }
      _exit(0);
    }
    int status = 0;
    waitpid(middle, &status, 0);
    if (waitpid(-1, &status, 0) <= 0) {
      perror("waitpid");
      return 1;
    }
    report(mode, status);
    return 0;
  }
  if (!strcmp(mode, "invalid")) {
    pthread_t thread;
    if (pthread_create(&thread, NULL, invalid_creator, NULL) != 0) {
      return 1;
    }
    pthread_join(thread, NULL);
    int status = 0;
    if (waitpid(child, &status, 0) != child) {
      perror("waitpid");
      return 1;
    }
    report(mode, status);
    return 0;
  }
  if (!strcmp(mode, "get")) {
    prctl(PR_SET_PDEATHSIG, SIGUSR1);
    int armed = -1;
    prctl(PR_GET_PDEATHSIG, &armed);
    pid_t pid = fork();
    if (pid == 0) {
      int inherited = -1;
      prctl(PR_GET_PDEATHSIG, &inherited);
      _exit(inherited);
    }
    int status = 0;
    waitpid(pid, &status, 0);
    printf("get: %d, child %d\n", armed, WEXITSTATUS(status));
    return 0;
  }
  if (!strcmp(mode, "subreaper")) {
    prctl(PR_SET_CHILD_SUBREAPER, 1);
    pid_t middle = fork();
    if (middle == 0) {
      child = fork();
      if (child == 0) {
        armed_child(SIGTERM);
      }
      char byte;
      if (read(ready[0], &byte, 1) != 1 ||
          write(ready[1], &child, sizeof(child)) != (ssize_t)sizeof(child)) {
        _exit(1);
      }
      _exit(0);
    }
    waitpid(middle, NULL, 0);
    if (read(ready[0], &child, sizeof(child)) != (ssize_t)sizeof(child)) {
      return 1;
    }
    int status = 0;
    if (waitpid(child, &status, 0) != child) {
      perror("waitpid");
      return 1;
    }
    report(mode, status);
    return 0;
  }
  int signal = strcmp(mode, "caught") ? SIGTERM : SIGUSR1;
  pthread_t thread;
  if (pthread_create(&thread, NULL, creator, &signal) != 0) {
    return 1;
  }
  pthread_join(thread, NULL);
  int status = 0;
  if (waitpid(child, &status, 0) != child) {
    perror("waitpid");
    return 1;
  }
  report(mode, status);
  return 0;
}
