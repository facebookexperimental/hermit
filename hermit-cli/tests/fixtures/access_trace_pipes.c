// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

// The pipes whose data edges analyze's race finder leaves out
// (races::data_edges), made for real so the access trace test
// the_trace_shows_what_leaves_a_pipes_data_edges_out can check their records:
// a packet pipe (pipe2 with O_DIRECT), whose first read takes half a packet
// and drops the rest; a pipe whose write end fcntl F_SETFL changes; a pipe
// end reopened through /proc/self/fd; and a write larger than the pipe,
// which a child drains while it blocks.

#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static char big[3 * 65536];

int main(void) {
  int p[2];
  char buf[8];

  if (pipe2(p, O_DIRECT) != 0)
    return 1;
  if (write(p[1], "AAAAaaaa", 8) != 8)
    return 2;
  if (read(p[0], buf, 4) != 4)
    return 3;
  if (write(p[1], "BBBB", 4) != 4)
    return 4;
  if (read(p[0], buf, 4) != 4)
    return 5;
  printf("packet pipe second read: %.4s\n", buf);
  close(p[0]);
  close(p[1]);

  if (pipe2(p, 0) != 0)
    return 6;
  if (fcntl(p[1], F_SETFL, O_NONBLOCK) != 0)
    return 7;
  if (write(p[1], "CCCC", 4) != 4)
    return 8;
  if (read(p[0], buf, 4) != 4)
    return 9;
  close(p[0]);
  close(p[1]);

  if (pipe2(p, 0) != 0)
    return 14;
  char path[64];
  snprintf(path, sizeof path, "/proc/self/fd/%d", p[1]);
  int reopened = open(path, O_WRONLY);
  if (reopened < 0)
    return 15;
  if (write(reopened, "XXXX", 4) != 4)
    return 16;
  if (write(p[1], "EEEE", 4) != 4)
    return 17;
  if (read(p[0], buf, 4) != 4)
    return 18;
  printf("reopened pipe first read: %.4s\n", buf);
  close(reopened);
  close(p[0]);
  close(p[1]);

  if (pipe2(p, 0) != 0)
    return 10;
  pid_t child = fork();
  if (child < 0)
    return 11;
  if (child == 0) {
    close(p[1]);
    size_t total = 0;
    ssize_t n;
    while ((n = read(p[0], big, sizeof big)) > 0)
      total += (size_t)n;
    _exit(total == sizeof big ? 0 : 1);
  }
  close(p[0]);
  memset(big, 'D', sizeof big);
  if (write(p[1], big, sizeof big) != (ssize_t)sizeof big)
    return 12;
  close(p[1]);
  int status;
  if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0)
    return 13;
  printf("large write: %zu bytes\n", sizeof big);
  return 0;
}
