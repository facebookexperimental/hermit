/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * epoll_ctl answers by the kind of the target descriptor: Linux refuses a
 * regular file with EPERM, which cannot be polled, and accepts a pipe
 * (https://github.com/rrnewton/hermit/issues/31).
 *
 * Go's os.Open does what this guest does: it adds every file it opens to its
 * epoll set and, on EPERM, puts the descriptor back into blocking mode with
 * fcntl. Linux checks that the target can be polled before it looks the
 * target up, so deleting a regular file that was never added is EPERM too, not
 * ENOENT. Under record and replay, a regular file the guest opens but did not
 * create is a placeholder descriptor during replay, which epoll can poll. A
 * replay that asked the kernel again got 0 where the recording got EPERM, so
 * the guest took the other branch (fstat here) and replay diverged.
 *
 * The guest checks each answer against Linux's and exits nonzero on a
 * mismatch.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/stat.h>
#include <unistd.h>

static const char* name_of(int result, int error) {
  if (result == 0) {
    return "0";
  }
  switch (error) {
    case EPERM:
      return "EPERM";
    case ENOENT:
      return "ENOENT";
    case EEXIST:
      return "EEXIST";
    default:
      return strerror(error);
  }
}

static int control(int epfd, int op, int fd, const char** answer) {
  struct epoll_event event = {.events = EPOLLIN, .data = {.fd = fd}};
  errno = 0;
  int result = epoll_ctl(epfd, op, fd, &event);
  *answer = name_of(result, errno);
  return result;
}

int main(int argc, char** argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s <existing regular file>\n", argv[0]);
    return 2;
  }
  /* A regular file that exists before the run. */
  int file = open(argv[1], O_RDONLY | O_CLOEXEC);
  int pipe_fds[2];
  int epfd = epoll_create1(EPOLL_CLOEXEC);
  if (file < 0 || epfd < 0 || pipe2(pipe_fds, O_CLOEXEC) != 0) {
    perror("setup");
    return 1;
  }

  const char* add_file;
  int branch_result;
  if (control(epfd, EPOLL_CTL_ADD, file, &add_file) != 0) {
    /* What Go does on EPERM: put the descriptor back into blocking mode. */
    int flags = fcntl(file, F_GETFL);
    branch_result = fcntl(file, F_SETFL, flags & ~O_NONBLOCK);
  } else {
    struct stat status;
    branch_result = fstat(file, &status);
  }
  const char* add_pipe;
  control(epfd, EPOLL_CTL_ADD, pipe_fds[0], &add_pipe);
  const char* add_pipe_again;
  control(epfd, EPOLL_CTL_ADD, pipe_fds[0], &add_pipe_again);
  const char* del_pipe;
  control(epfd, EPOLL_CTL_DEL, pipe_fds[0], &del_pipe);
  const char* del_file;
  control(epfd, EPOLL_CTL_DEL, file, &del_file);

  printf(
      "add file=%s branch=%d add pipe=%s again=%s del pipe=%s del file=%s\n",
      add_file,
      branch_result,
      add_pipe,
      add_pipe_again,
      del_pipe,
      del_file);
  int ok = strcmp(add_file, "EPERM") == 0 && branch_result == 0 &&
      strcmp(add_pipe, "0") == 0 && strcmp(add_pipe_again, "EEXIST") == 0 &&
      strcmp(del_pipe, "0") == 0 && strcmp(del_file, "EPERM") == 0;
  if (!ok) {
    fprintf(stderr, "epoll answers differ from Linux's\n");
    return 1;
  }
  return 0;
}
