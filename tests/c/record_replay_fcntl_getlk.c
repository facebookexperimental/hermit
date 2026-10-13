/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * F_GETLK answers through the struct flock it is given: a child that asks
 * about a region its parent has write-locked gets F_WRLCK and the parent's pid
 * back in that struct. Under record and replay the struct must be written back
 * from the recording; replaying only the call's status left it as the child
 * filled it in (https://github.com/rrnewton/hermit/issues/2289).
 *
 * Usage: record_replay_fcntl_getlk <directory>. The guest creates
 * <directory>/locked, locks it, and checks the answer against Linux's.
 */

#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

int main(int argc, char** argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s <directory>\n", argv[0]);
    return 2;
  }
  char path[4096];
  snprintf(path, sizeof path, "%s/locked", argv[1]);
  int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
  if (fd < 0) {
    perror("open");
    return 1;
  }
  struct flock lock = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
  if (fcntl(fd, F_SETLK, &lock) != 0) {
    perror("F_SETLK");
    return 1;
  }
  int pipe_fds[2];
  if (pipe(pipe_fds) != 0) {
    perror("pipe");
    return 1;
  }
  pid_t child = fork();
  if (child == 0) {
    struct flock query = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
    int rc = fcntl(fd, F_GETLK, &query);
    char line[128];
    int len = snprintf(
        line,
        sizeof line,
        "getlk rc=%d type_is_wrlck=%d pid_is_parent=%d\n",
        rc,
        query.l_type == F_WRLCK,
        query.l_pid == getppid());
    if (write(pipe_fds[1], line, len) != len) {
      _exit(1);
    }
    _exit(0);
  }
  close(pipe_fds[1]);
  char line[128] = {0};
  ssize_t got = read(pipe_fds[0], line, sizeof line - 1);
  int status = 0;
  waitpid(child, &status, 0);
  if (got <= 0) {
    fprintf(stderr, "no answer from the child\n");
    return 1;
  }
  fputs(line, stdout);
  return strcmp(line, "getlk rc=0 type_is_wrlck=1 pid_is_parent=1\n") == 0 ? 0 : 1;
}
