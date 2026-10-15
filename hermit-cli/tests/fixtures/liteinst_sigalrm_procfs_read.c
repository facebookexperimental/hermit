/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Reads each procfs file named on the command line from a process that
 * handles SIGALRM, where signal phase 1 (in-guest LiteInst with SIGALRM
 * handlers admitted) refuses any read that might sleep interruptibly. Each
 * file prints as "<path>: <contents>", or as "<path>: <call> errno <n>" when
 * a call fails; the exit status is 0 unless the handler or a write to stdout
 * fails. Only open, read, close and write follow the handler's installation.
 */

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static void on_alarm(int signal_number) {
  (void)signal_number;
}

static int print_line(const char* line, int length) {
  if (length < 0) {
    return 1;
  }
  return write(STDOUT_FILENO, line, (size_t)length) == length ? 0 : 1;
}

static int report(const char* path) {
  /* Returns nonzero only when stdout cannot be written. */
  char contents[256];
  char line[512];
  int fd = open(path, O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    int saved = errno;
    return print_line(line, snprintf(line, sizeof(line), "%s: open errno %d\n", path, saved));
  }
  ssize_t count = read(fd, contents, sizeof(contents) - 1);
  int saved = errno;
  close(fd);
  if (count < 0) {
    return print_line(line, snprintf(line, sizeof(line), "%s: read errno %d\n", path, saved));
  }
  contents[count] = '\0';
  return print_line(line, snprintf(line, sizeof(line), "%s: %s", path, contents));
}

int main(int argc, char** argv) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = on_alarm;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGALRM, &action, NULL) != 0) {
    return 2;
  }
  int failures = 0;
  for (int index = 1; index < argc; index++) {
    failures += report(argv[index]);
  }
  return failures == 0 ? 0 : 1;
}
