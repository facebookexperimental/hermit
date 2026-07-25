/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/mman.h>
#include <sys/select.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

struct writer_args {
  int write_fd;
  int ack_fd;
};

static void* delayed_writer(void* argument) {
  struct writer_args* args = argument;
  struct timespec delay = {.tv_sec = 0, .tv_nsec = 1000000};
  char ack;
  if (nanosleep(&delay, NULL) != 0 || write(args->write_fd, "w", 1) != 1 ||
      read(args->ack_fd, &ack, 1) != 1 || ack != 'a') {
    return (void*)(uintptr_t)1;
  }
  return NULL;
}

static volatile sig_atomic_t signal_received;

static void signal_handler(int signal) {
  (void)signal;
  signal_received = 1;
}

struct signal_args {
  pthread_t target;
  int ack_fd;
};

static void* delayed_signal(void* argument) {
  struct signal_args* args = argument;
  struct timespec delay = {.tv_sec = 0, .tv_nsec = 1000000};
  char ack;
  if (nanosleep(&delay, NULL) != 0 ||
      pthread_kill(args->target, SIGUSR1) != 0 ||
      read(args->ack_fd, &ack, 1) != 1 || ack != 's') {
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

  fd_set ready;
  FD_ZERO(&ready);
  FD_SET(pipefd[0], &ready);
  sigset_t mask;
  sigemptyset(&mask);
  sigaddset(&mask, SIGUSR1);
  struct timespec ready_timeout = {.tv_sec = 1, .tv_nsec = 0};
  if (pselect(pipefd[0] + 1, &ready, NULL, NULL, &ready_timeout, &mask) != 1 ||
      !FD_ISSET(pipefd[0], &ready)) {
    perror("ready pselect");
    return 1;
  }

  char byte;
  if (read(pipefd[0], &byte, 1) != 1) {
    perror("drain ready pipe");
    return 1;
  }

  FD_ZERO(&ready);
  FD_SET(pipefd[0], &ready);
  struct timespec zero = {.tv_sec = 0, .tv_nsec = 0};
  if (pselect(pipefd[0] + 1, &ready, NULL, NULL, &zero, NULL) != 0 ||
      FD_ISSET(pipefd[0], &ready)) {
    perror("zero pselect");
    return 1;
  }

  FD_ZERO(&ready);
  FD_SET(pipefd[0], &ready);
  struct timespec finite = {.tv_sec = 0, .tv_nsec = 20000000};
  if (pselect(pipefd[0] + 1, &ready, NULL, NULL, &finite, NULL) != 0 ||
      FD_ISSET(pipefd[0], &ready)) {
    perror("finite pselect");
    return 1;
  }

  struct timespec invalid = {.tv_sec = 0, .tv_nsec = 1000000000};
  errno = 0;
  if (syscall(SYS_pselect6, 0, NULL, NULL, NULL, &invalid, NULL) != -1 ||
      errno != EINVAL) {
    fprintf(stderr, "invalid pselect timeout: errno=%d\n", errno);
    return 1;
  }

  errno = 0;
  if (syscall(SYS_pselect6, 0, NULL, NULL, NULL, (void*)1, NULL) != -1 ||
      errno != EFAULT) {
    fprintf(stderr, "bad pselect timeout pointer: errno=%d\n", errno);
    return 1;
  }

  struct timespec fdset_fault_timeout = {.tv_sec = 1, .tv_nsec = 0};
  errno = 0;
  if (syscall(SYS_pselect6, 1, (void*)1, NULL, NULL,
              &fdset_fault_timeout, NULL) != -1 ||
      errno != EFAULT) {
    fprintf(stderr,
            "bad fdset pselect timeout failed: errno=%d remain=%ld.%09ld\n", errno,
            fdset_fault_timeout.tv_sec, fdset_fault_timeout.tv_nsec);
    return 1;
  }

  struct timespec raw_timeout = {.tv_sec = 0, .tv_nsec = 1000000};
  if (syscall(SYS_pselect6, 0, (void*)1, (void*)1, (void*)1,
              &raw_timeout, NULL) != 0 ||
      raw_timeout.tv_sec != 0 || raw_timeout.tv_nsec != 0) {
    fprintf(stderr, "nfds=0 pselect or timeout writeback failed: %ld.%09ld\n",
            raw_timeout.tv_sec, raw_timeout.tv_nsec);
    return 1;
  }

  raw_timeout = (struct timespec){.tv_sec = 0, .tv_nsec = 1000000};
  errno = 0;
  if (syscall(SYS_pselect6, -1, (void*)1, (void*)1, (void*)1,
              &raw_timeout, NULL) != -1 ||
      errno != EINVAL) {
    fprintf(stderr, "negative nfds pselect: errno=%d\n", errno);
    return 1;
  }

  long page_size = sysconf(_SC_PAGESIZE);
  void* mapping = mmap(NULL, (size_t)page_size * 2, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (mapping == MAP_FAILED ||
      mprotect((char*)mapping + page_size, (size_t)page_size, PROT_NONE) != 0) {
    perror("pselect fdset mapping");
    return 1;
  }
  unsigned long* compact_set =
      (unsigned long*)((char*)mapping + page_size - sizeof(unsigned long));
  *compact_set = 0;
  raw_timeout = (struct timespec){.tv_sec = 0, .tv_nsec = 1000000};
  if (syscall(SYS_pselect6, 1, compact_set, NULL, NULL, &raw_timeout, NULL) !=
          0 ||
      raw_timeout.tv_sec != 0 || raw_timeout.tv_nsec != 0) {
    perror("compact pselect fdset");
    return 1;
  }
  if (munmap(mapping, (size_t)page_size * 2) != 0) {
    perror("munmap pselect fdset");
    return 1;
  }

  FD_ZERO(&ready);
  FD_SET(pipefd[0], &ready);
  struct timespec masked_timeout = {.tv_sec = 0, .tv_nsec = 1000000};
  errno = 0;
  if (pselect(pipefd[0] + 1, &ready, NULL, NULL, &masked_timeout, &mask) != 0 ||
      FD_ISSET(pipefd[0], &ready)) {
    fprintf(stderr, "masked pselect timeout failed: errno=%d\n", errno);
    return 1;
  }

  struct sigaction action = {0};
  action.sa_handler = signal_handler;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGUSR1, &action, NULL) != 0) {
    perror("sigaction");
    return 1;
  }
  int signal_ack[2];
  if (pipe(signal_ack) != 0) {
    perror("signal ack pipe");
    return 1;
  }
  struct signal_args signal_args = {
      .target = pthread_self(),
      .ack_fd = signal_ack[0],
  };
  pthread_t signal_thread;
  if (pthread_create(&signal_thread, NULL, delayed_signal, &signal_args) != 0) {
    perror("signal pthread_create");
    return 1;
  }
  struct timespec signal_timeout = {.tv_sec = 1, .tv_nsec = 0};
  errno = 0;
  if (syscall(SYS_pselect6, 0, NULL, NULL, NULL, &signal_timeout, NULL) != -1 ||
      errno != EINTR || !signal_received ||
      (signal_timeout.tv_sec == 1 && signal_timeout.tv_nsec == 0) ||
      signal_timeout.tv_sec < 0 || signal_timeout.tv_nsec < 0 ||
      signal_timeout.tv_nsec >= 1000000000) {
    fprintf(stderr,
            "interrupted pselect timeout failed: errno=%d seen=%d remain=%ld.%09ld\n",
            errno, signal_received, signal_timeout.tv_sec,
            signal_timeout.tv_nsec);
    return 1;
  }
  if (write(signal_ack[1], "s", 1) != 1) {
    perror("ack signal sender");
    return 1;
  }
  void* signal_result = NULL;
  if (pthread_join(signal_thread, &signal_result) != 0 || signal_result != NULL) {
    fputs("signal sender failed\n", stderr);
    return 1;
  }

  int delayed_pipe[2];
  int ack_pipe[2];
  if (pipe(delayed_pipe) != 0 || pipe(ack_pipe) != 0) {
    perror("delayed pipe");
    return 1;
  }
  struct writer_args writer_args = {
      .write_fd = delayed_pipe[1],
      .ack_fd = ack_pipe[0],
  };
  pthread_t writer;
  if (pthread_create(&writer, NULL, delayed_writer, &writer_args) != 0) {
    perror("pthread_create");
    return 1;
  }
  FD_ZERO(&ready);
  FD_SET(delayed_pipe[0], &ready);
  if (pselect(delayed_pipe[0] + 1, &ready, NULL, NULL, NULL, NULL) != 1 ||
      !FD_ISSET(delayed_pipe[0], &ready)) {
    perror("infinite pselect");
    return 1;
  }
  if (read(delayed_pipe[0], &byte, 1) != 1 || byte != 'w') {
    perror("read delayed pipe");
    return 1;
  }
  if (write(ack_pipe[1], "a", 1) != 1) {
    perror("ack delayed writer");
    return 1;
  }
  void* writer_result = NULL;
  if (pthread_join(writer, &writer_result) != 0 || writer_result != NULL) {
    fputs("delayed writer failed\n", stderr);
    return 1;
  }

  puts("pselect6-simulation-ok");
  return 0;
}
