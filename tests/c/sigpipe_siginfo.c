/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/uio.h>
#include <sys/signalfd.h>
#include <unistd.h>

static volatile sig_atomic_t sigpipe_code = -1;
static volatile sig_atomic_t sigpipe_errno = -1;
static volatile sig_atomic_t sigpipe_pid = -1;
static volatile sig_atomic_t sigpipe_uid = -1;
static volatile sig_atomic_t sigpipe_count = 0;

static void handle_sigpipe(int signal, siginfo_t *info, void *context) {
  (void)context;
  if (signal == SIGPIPE) {
    sigpipe_count++;
    sigpipe_code = info->si_code;
    sigpipe_errno = info->si_errno;
    sigpipe_pid = info->si_pid;
    sigpipe_uid = info->si_uid;
  }
}

static int check_siginfo(const char *operation) {
  if (sigpipe_code == SI_USER && sigpipe_errno == 0 &&
      sigpipe_pid == getpid() && sigpipe_uid == getuid()) {
    return 0;
  }
  fprintf(stderr, "%s SIGPIPE info was code=%d errno=%d pid=%d uid=%d\n",
          operation, sigpipe_code, sigpipe_errno, sigpipe_pid, sigpipe_uid);
  return -1;
}

int main(void) {
  struct sigaction action = {
      .sa_sigaction = handle_sigpipe,
      .sa_flags = SA_SIGINFO,
  };
  if (sigemptyset(&action.sa_mask) != 0 ||
      sigaction(SIGPIPE, &action, NULL) != 0) {
    perror("sigaction");
    return EXIT_FAILURE;
  }
  sigset_t mask;
  if (sigemptyset(&mask) != 0 || sigaddset(&mask, SIGPIPE) != 0) {
    perror("SIGPIPE mask");
    return EXIT_FAILURE;
  }
  int signal_fd = signalfd(-1, &mask, SFD_CLOEXEC);
  if (signal_fd < 0) {
    perror("signalfd");
    return EXIT_FAILURE;
  }

  int pipefd[2];
  if (pipe(pipefd) != 0) {
    perror("pipe");
    return EXIT_FAILURE;
  }
  if (close(pipefd[0]) != 0) {
    perror("close read end");
    return EXIT_FAILURE;
  }

  errno = 0;
  ssize_t result = write(pipefd[1], "x", 1);
  int error = errno;
  if (result != -1 || error != EPIPE) {
    fprintf(stderr, "write returned %zd with errno %d, expected EPIPE\n", result,
            error);
    return EXIT_FAILURE;
  }
  if (check_siginfo("write") != 0) {
    return EXIT_FAILURE;
  }

  sigpipe_code = sigpipe_errno = sigpipe_pid = sigpipe_uid = -1;
  struct iovec iov = {
      .iov_base = (void *)"y",
      .iov_len = 1,
  };
  errno = 0;
  result = pwritev2(pipefd[1], &iov, 1, -1, 0);
  error = errno;
  if (result != -1 || error != EPIPE) {
    fprintf(stderr, "pwritev2 returned %zd with errno %d, expected EPIPE\n",
            result, error);
    return EXIT_FAILURE;
  }
  if (check_siginfo("pwritev2") != 0) {
    return EXIT_FAILURE;
  }

  if (sigprocmask(SIG_BLOCK, &mask, NULL) != 0) {
    perror("block SIGPIPE");
    return EXIT_FAILURE;
  }

  errno = 0;
  result = write(pipefd[1], "z", 1);
  error = errno;
  if (result != -1 || error != EPIPE || sigpipe_count != 2) {
    fprintf(stderr,
            "blocked write returned %zd errno=%d handler-count=%d, expected "
            "EPIPE with no handler\n",
            result, error, sigpipe_count);
    return EXIT_FAILURE;
  }

  struct signalfd_siginfo fd_info;
  result = read(signal_fd, &fd_info, sizeof(fd_info));
  if (result != sizeof(fd_info) || fd_info.ssi_signo != SIGPIPE ||
      fd_info.ssi_code != SI_USER || fd_info.ssi_pid != (unsigned)getpid() ||
      fd_info.ssi_uid != getuid()) {
    fprintf(stderr,
            "signalfd returned %zd bytes signo=%u code=%d pid=%u uid=%u\n",
            result, fd_info.ssi_signo, fd_info.ssi_code, fd_info.ssi_pid,
            fd_info.ssi_uid);
    return EXIT_FAILURE;
  }

  sigset_t pending;
  if (sigpending(&pending) != 0 || sigismember(&pending, SIGPIPE) != 0) {
    fprintf(stderr, "SIGPIPE remained pending after signalfd read\n");
    return EXIT_FAILURE;
  }
  errno = 0;
  result = write(pipefd[1], "v", 1);
  error = errno;
  if (result != -1 || error != EPIPE || sigpipe_count != 2) {
    fprintf(stderr,
            "blocked readv write returned %zd errno=%d handler-count=%d\n",
            result, error, sigpipe_count);
    return EXIT_FAILURE;
  }

  struct iovec signal_iov = {
      .iov_base = &fd_info,
      .iov_len = sizeof(fd_info),
  };
  result = readv(signal_fd, &signal_iov, 1);
  if (result != sizeof(fd_info) || fd_info.ssi_signo != SIGPIPE ||
      fd_info.ssi_code != SI_USER || fd_info.ssi_pid != (unsigned)getpid() ||
      fd_info.ssi_uid != getuid()) {
    fprintf(stderr,
            "signalfd readv returned %zd bytes signo=%u code=%d pid=%u uid=%u\n",
            result, fd_info.ssi_signo, fd_info.ssi_code, fd_info.ssi_pid,
            fd_info.ssi_uid);
    return EXIT_FAILURE;
  }
  if (sigpending(&pending) != 0 || sigismember(&pending, SIGPIPE) != 0) {
    fprintf(stderr, "SIGPIPE remained pending after signalfd readv\n");
    return EXIT_FAILURE;
  }

  errno = 0;
  result = write(pipefd[1], "m", 1);
  error = errno;
  if (result != -1 || error != EPIPE || sigpipe_count != 2) {
    fprintf(stderr,
            "blocked multi-signal write returned %zd errno=%d "
            "handler-count=%d\n",
            result, error, sigpipe_count);
    return EXIT_FAILURE;
  }
  if (kill(getpid(), SIGPIPE) != 0) {
    perror("queue process SIGPIPE");
    return EXIT_FAILURE;
  }

  struct signalfd_siginfo fd_infos[2] = {0};
  result = read(signal_fd, fd_infos, sizeof(fd_infos));
  if (result != sizeof(fd_infos)) {
    fprintf(stderr, "multi-signal signalfd returned %zd bytes, expected %zu\n",
            result, sizeof(fd_infos));
    return EXIT_FAILURE;
  }
  for (size_t index = 0; index < 2; ++index) {
    if (fd_infos[index].ssi_signo != SIGPIPE ||
        fd_infos[index].ssi_code != SI_USER ||
        fd_infos[index].ssi_pid != (unsigned)getpid() ||
        fd_infos[index].ssi_uid != getuid()) {
      fprintf(stderr,
              "multi-signal record %zu was signo=%u code=%d pid=%u uid=%u\n",
              index, fd_infos[index].ssi_signo, fd_infos[index].ssi_code,
              fd_infos[index].ssi_pid, fd_infos[index].ssi_uid);
      return EXIT_FAILURE;
    }
  }
  if (sigpending(&pending) != 0 || sigismember(&pending, SIGPIPE) != 0) {
    fprintf(stderr, "SIGPIPE remained pending after multi-signal read\n");
    return EXIT_FAILURE;
  }

  if (sigprocmask(SIG_UNBLOCK, &mask, NULL) != 0) {
    perror("unblock SIGPIPE");
    return EXIT_FAILURE;
  }
  if (sigpipe_count != 2) {
    fprintf(stderr, "SIGPIPE handler ran again after signalfd consumption\n");
    return EXIT_FAILURE;
  }
  if (close(signal_fd) != 0) {
    perror("close signalfd");
    return EXIT_FAILURE;
  }
  if (close(pipefd[1]) != 0) {
    perror("close write end");
    return EXIT_FAILURE;
  }
  printf("sigpipe-si-code=%d\n", sigpipe_code);
  return EXIT_SUCCESS;
}
