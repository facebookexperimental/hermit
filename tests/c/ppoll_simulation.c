/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static void* delayed_writer(void* argument) {
  int fd = *(int*)argument;
  struct timespec delay = {.tv_sec = 0, .tv_nsec = 1000000};
  if (nanosleep(&delay, NULL) != 0 || write(fd, "w", 1) != 1) {
    return (void*)(uintptr_t)1;
  }
  return NULL;
}

int main(void) {
  int pipefd[2];
  if (pipe(pipefd) != 0 || write(pipefd[1], "r", 1) != 1) {
    perror("ready pipe");
    return 1;
  }

  sigset_t mask;
  sigemptyset(&mask);
  sigaddset(&mask, SIGUSR1);
  struct pollfd ready = {.fd = pipefd[0], .events = POLLIN};
  struct timespec ready_timeout = {.tv_sec = 1, .tv_nsec = 0};
  if (ppoll(&ready, 1, &ready_timeout, &mask) != 1 ||
      !(ready.revents & POLLIN)) {
    perror("ready ppoll");
    return 1;
  }

  char byte;
  if (read(pipefd[0], &byte, 1) != 1) {
    perror("drain ready pipe");
    return 1;
  }

  ready.revents = 0;
  struct timespec zero = {.tv_sec = 0, .tv_nsec = 0};
  if (ppoll(&ready, 1, &zero, NULL) != 0) {
    perror("zero ppoll");
    return 1;
  }

  struct timespec finite = {.tv_sec = 0, .tv_nsec = 20000000};
  if (ppoll(&ready, 1, &finite, NULL) != 0) {
    perror("finite ppoll");
    return 1;
  }

  struct timespec raw_timeout = {.tv_sec = 0, .tv_nsec = 5000000};
  if (syscall(SYS_ppoll, NULL, 0, &raw_timeout, NULL, sizeof(uint64_t)) != 0 ||
      raw_timeout.tv_sec != 0 || raw_timeout.tv_nsec != 0) {
    fprintf(
        stderr,
        "raw ppoll timeout was not consumed: %ld.%09ld\n",
        raw_timeout.tv_sec,
        raw_timeout.tv_nsec);
    return 1;
  }

  struct timespec invalid = {.tv_sec = 0, .tv_nsec = 1000000000};
  errno = 0;
  if (syscall(SYS_ppoll, NULL, 0, &invalid, NULL, sizeof(uint64_t)) != -1 ||
      errno != EINVAL) {
    fprintf(stderr, "invalid ppoll timeout: errno=%d\n", errno);
    return 1;
  }

  errno = 0;
  if (syscall(SYS_ppoll, NULL, 0, (void*)1, NULL, sizeof(uint64_t)) != -1 ||
      errno != EFAULT) {
    fprintf(stderr, "bad ppoll timeout pointer: errno=%d\n", errno);
    return 1;
  }

  struct timespec masked_timeout = {.tv_sec = 0, .tv_nsec = 1000000};
  errno = 0;
  if (ppoll(&ready, 1, &masked_timeout, &mask) != -1 || errno != ENOSYS) {
    fprintf(
        stderr, "blocking masked ppoll did not fail closed: errno=%d\n", errno);
    return 1;
  }

  int delayed_pipe[2];
  if (pipe(delayed_pipe) != 0) {
    perror("delayed pipe");
    return 1;
  }
  pthread_t writer;
  if (pthread_create(&writer, NULL, delayed_writer, &delayed_pipe[1]) != 0) {
    perror("pthread_create");
    return 1;
  }
  struct pollfd delayed = {.fd = delayed_pipe[0], .events = POLLIN};
  if (ppoll(&delayed, 1, NULL, NULL) != 1 || !(delayed.revents & POLLIN)) {
    perror("infinite ppoll");
    return 1;
  }
  void* writer_result = NULL;
  if (pthread_join(writer, &writer_result) != 0 || writer_result != NULL) {
    fputs("delayed writer failed\n", stderr);
    return 1;
  }

  puts("ppoll-simulation-ok");
  return 0;
}
