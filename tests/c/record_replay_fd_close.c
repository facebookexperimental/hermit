/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/mman.h>
#include <unistd.h>

static int fail(const char* operation) {
  fprintf(stderr, "%s failed: %s\n", operation, strerror(errno));
  return 1;
}

static int after_exec(
    const char* inherited_arg,
    const char* cloexec_arg,
    const char* setfd_arg) {
  const int inherited_fd = atoi(inherited_arg);
  const int cloexec_fd = atoi(cloexec_arg);
  const int setfd_fd = atoi(setfd_arg);

  const int memfd = memfd_create("record-replay-fd-exec", MFD_CLOEXEC);
  if (memfd < 0) {
    return fail("memfd_create(after exec)");
  }
  if (memfd != cloexec_fd) {
    fprintf(
        stderr,
        "CLOEXEC slot was not reused: expected=%d actual=%d\n",
        cloexec_fd,
        memfd);
    return 1;
  }
  const int second_memfd =
      memfd_create("record-replay-fd-setfd-exec", MFD_CLOEXEC);
  if (second_memfd < 0) {
    return fail("memfd_create(after F_SETFD exec)");
  }
  if (second_memfd != setfd_fd) {
    fprintf(
        stderr,
        "F_SETFD slot was not reused: expected=%d actual=%d\n",
        setfd_fd,
        second_memfd);
    return 1;
  }
  if (close(inherited_fd) != 0) {
    return fail("close(inherited_fd after exec)");
  }
  if (close(memfd) != 0) {
    return fail("close(memfd after exec)");
  }
  if (close(second_memfd) != 0) {
    return fail("close(second_memfd after exec)");
  }

  printf("descriptor namespace preserved across exec\n");
  return 0;
}

int main(int argc, char** argv) {
  if (argc == 5 && strcmp(argv[1], "--after-exec") == 0) {
    return after_exec(argv[2], argv[3], argv[4]);
  }

  const int file_fd = open("/dev/null", O_RDONLY);
  if (file_fd < 0) {
    return fail("open(/dev/null)");
  }
  const int displaced_memfd =
      memfd_create("record-replay-fd-displaced", MFD_CLOEXEC);
  if (displaced_memfd < 0) {
    return fail("memfd_create(displaced)");
  }
  if (displaced_memfd == file_fd) {
    fprintf(stderr, "simultaneous descriptors unexpectedly match: %d\n", file_fd);
    return 1;
  }
  if (close(file_fd) != 0) {
    return fail("close(file_fd)");
  }
  if (fcntl(displaced_memfd, F_GETFD) < 0) {
    return fail("fcntl(displaced_memfd, F_GETFD)");
  }
  if (close(displaced_memfd) != 0) {
    return fail("close(displaced_memfd)");
  }

  const int memfd = memfd_create("record-replay-fd-close", MFD_CLOEXEC);
  if (memfd < 0) {
    return fail("memfd_create");
  }
  if (close(memfd) != 0) {
    return fail("close(memfd)");
  }

  const int epoll_fd = epoll_create1(EPOLL_CLOEXEC);
  if (epoll_fd < 0) {
    return fail("epoll_create1");
  }
  if (epoll_fd != memfd) {
    fprintf(
        stderr,
        "closed descriptor was not reused: memfd=%d epoll=%d\n",
        memfd,
        epoll_fd);
    return 1;
  }
  if (fcntl(epoll_fd, F_GETFD) < 0) {
    return fail("fcntl(epoll_fd, F_GETFD)");
  }
  if (close(epoll_fd) != 0) {
    return fail("close(epoll_fd)");
  }

  const int aliased_epoll = epoll_create1(EPOLL_CLOEXEC);
  if (aliased_epoll < 0) {
    return fail("epoll_create1(alias)");
  }
  const int epoll_alias = dup(aliased_epoll);
  if (epoll_alias < 0) {
    return fail("dup(epoll)");
  }
  if (close(aliased_epoll) != 0) {
    return fail("close(aliased_epoll)");
  }
  const int event_fd = eventfd(0, EFD_CLOEXEC);
  if (event_fd < 0) {
    return fail("eventfd");
  }
  struct epoll_event event = {.events = EPOLLIN, .data.fd = event_fd};
  if (epoll_ctl(epoll_alias, EPOLL_CTL_ADD, event_fd, &event) != 0) {
    return fail("epoll_ctl(alias)");
  }
  if (close(event_fd) != 0) {
    return fail("close(event_fd)");
  }
  if (close(epoll_alias) != 0) {
    return fail("close(epoll_alias)");
  }

  if (close(STDOUT_FILENO) != 0) {
    return fail("close(stdout)");
  }
  const int output_alias = open("/proc/self/fd/2", O_WRONLY);
  if (output_alias < 0) {
    return fail("open(/proc/self/fd/2)");
  }
  if (output_alias != STDOUT_FILENO) {
    fprintf(stderr, "stderr alias did not reuse stdout: %d\n", output_alias);
    return 1;
  }
  static const char alias_output[] = "descriptor output alias preserved\n";
  if (write(output_alias, alias_output, sizeof(alias_output) - 1) < 0) {
    return fail("write(output_alias)");
  }

  const int inherited_fd = open("/dev/null", O_RDONLY);
  if (inherited_fd < 0) {
    return fail("open(inherited)");
  }
  const int cloexec_fd = open("/dev/null", O_RDONLY | O_CLOEXEC);
  if (cloexec_fd < 0) {
    return fail("open(CLOEXEC)");
  }
  const int setfd_fd = open("/dev/null", O_RDONLY);
  if (setfd_fd < 0) {
    return fail("open(F_SETFD)");
  }
  if (fcntl(setfd_fd, F_SETFD, FD_CLOEXEC) != 0) {
    return fail("fcntl(F_SETFD)");
  }

  char inherited_arg[32];
  char cloexec_arg[32];
  char setfd_arg[32];
  if (snprintf(inherited_arg, sizeof(inherited_arg), "%d", inherited_fd) < 0 ||
      snprintf(cloexec_arg, sizeof(cloexec_arg), "%d", cloexec_fd) < 0 ||
      snprintf(setfd_arg, sizeof(setfd_arg), "%d", setfd_fd) < 0) {
    return fail("snprintf");
  }
  char* const next_argv[] = {
      argv[0],
      "--after-exec",
      inherited_arg,
      cloexec_arg,
      setfd_arg,
      NULL,
  };
  execv(next_argv[0], next_argv);
  return fail("execv(argv[0])");
}
