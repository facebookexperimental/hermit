/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Two threads race over the resources the access trace names: a contended
// mutex (futex wake and wait), a pipe closed by close_range, a file opened,
// duplicated (once onto an open number), seeked and closed, an anonymous
// mapping, a POSIX timer, a handled signal, a thread joined while it still
// runs and a forked child that is waited for. Which thread writes each line first depends on
// the schedule, so the output is racy; everything else is checked.

#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define ROUNDS 20

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static int pipe_fds[2];
static int shared_counter;
static volatile sig_atomic_t usr1_seen;

static void on_usr1(int signo) {
  (void)signo;
  usr1_seen = 1;
}

static void must(int ok, const char* what) {
  if (!ok) {
    perror(what);
    exit(2);
  }
}

static void* worker(void* name) {
  for (int round = 0; round < ROUNDS; round++) {
    pthread_mutex_lock(&lock);
    shared_counter++;
    pthread_mutex_unlock(&lock);
    if (round % 5 == 0) {
      char line[32];
      int len = snprintf(line, sizeof line, "%s %d\n", (char*)name, round);
      must(write(STDOUT_FILENO, line, len) == len, "write stdout");
    }
  }
  char byte = ((char*)name)[0];
  must(write(pipe_fds[1], &byte, 1) == 1, "write pipe");
  if (byte == 'a') {
    // Outlive main's pthread_join call, so the join waits on the futex the
    // kernel clears and wakes at this thread's exit.
    struct timespec linger = {.tv_nsec = 20000000};
    nanosleep(&linger, NULL);
  }
  return NULL;
}

int main(void) {
  must(pipe(pipe_fds) == 0, "pipe");
  must(signal(SIGUSR1, on_usr1) != SIG_ERR, "signal");

  // In the run's own /tmp, never the directory Hermit was started in: every
  // run draws the same name here, so concurrent runs sharing a working
  // directory found one another's file, and the O_EXCL create failed in one
  // of them and retried with another name.
  char path[] = "/tmp/access_trace_mix.XXXXXX";
  int fd = mkstemp(path);
  must(fd >= 0, "mkstemp");
  must(unlink(path) == 0, "unlink");
  must(write(fd, "0123456789", 10) == 10, "write file");
  int copy = dup(fd);
  must(copy >= 0, "dup");
  must(dup2(copy, fd + 10) == fd + 10, "dup2");
  must(lseek(fd + 10, 2, SEEK_SET) == 2, "lseek");
  char got[4] = {0};
  must(read(copy, got, 3) == 3 && memcmp(got, "234", 3) == 0, "read file");
  must(dup2(fd, copy) == copy, "dup2 onto an open number");
  must(close(fd + 10) == 0 && close(copy) == 0, "close");

  char* page = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  must(page != MAP_FAILED, "mmap");
  page[0] = 1;
  must(munmap(page, 8192) == 0, "munmap");

  // The trace checks a clock output explicitly; timer setup need not call it.
  struct timespec now = {0};
  must(syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &now) == 0, "clock_gettime");

  timer_t timer;
  struct sigevent event = {.sigev_notify = SIGEV_SIGNAL, .sigev_signo = SIGUSR1};
  must(timer_create(CLOCK_MONOTONIC, &event, &timer) == 0, "timer_create");
  struct itimerspec soon = {.it_value = {.tv_nsec = 1000}};
  must(timer_settime(timer, 0, &soon, NULL) == 0, "timer_settime");

  pthread_t a, b;
  must(pthread_create(&a, NULL, worker, "a") == 0, "pthread_create a");
  must(pthread_create(&b, NULL, worker, "b") == 0, "pthread_create b");
  must(pthread_join(a, NULL) == 0 && pthread_join(b, NULL) == 0, "pthread_join");
  char bytes[2];
  must(read(pipe_fds[0], bytes, 2) == 2, "read pipe");
  // pipe() returned the two lowest free numbers, so this closes just them.
  must(pipe_fds[1] == pipe_fds[0] + 1, "adjacent pipe ends");
  must(syscall(SYS_close_range, pipe_fds[0], pipe_fds[1], 0) == 0, "close_range");

  while (!usr1_seen) {
    struct timespec tick = {.tv_nsec = 1000000};
    nanosleep(&tick, NULL);
  }
  must(timer_delete(timer) == 0, "timer_delete");

  pid_t child = fork();
  must(child >= 0, "fork");
  if (child == 0) {
    _exit(7);
  }
  int status = 0;
  must(waitpid(child, &status, 0) == child, "waitpid");
  must(WIFEXITED(status) && WEXITSTATUS(status) == 7, "child status");
  must(close(fd) == 0, "close file");

  printf("counter %d, pipe %c%c\n", shared_counter, bytes[0], bytes[1]);
  return 0;
}
