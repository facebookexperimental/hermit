/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static void require(int condition, const char *operation) {
  if (!condition) {
    dprintf(STDERR_FILENO, "terminal-read failure: %s errno=%d\n", operation,
            errno);
    _exit(91);
  }
}

static void emit(const char *message) {
  size_t length = strlen(message);
  require(write(STDOUT_FILENO, message, length) == (ssize_t)length, "write marker");
}

static void reap(pid_t child) {
  int status = 0;
  require(waitpid(child, &status, 0) == child, "wait exact child");
  require(WIFEXITED(status) && WEXITSTATUS(status) == 0, "child exit status");
}

int main(int argc, char **argv) {
  require(argc >= 2, "mode argument");
  int input = STDIN_FILENO;
  if (strcmp(argv[1], "exec") == 0) {
    require(argc == 2, "exec arguments");
    input = dup(STDIN_FILENO);
    require(input > STDERR_FILENO, "duplicate inherited terminal");
    char descriptor[32];
    require(snprintf(descriptor, sizeof(descriptor), "%d", input) > 0,
            "format descriptor");
    char *missing = malloc(strlen(argv[0]) + sizeof(".missing"));
    require(missing != NULL, "allocate missing exec pathname");
    strcpy(missing, argv[0]);
    strcat(missing, ".missing");
    char *failed[] = {missing, NULL};
    execv(missing, failed);
    require(errno == ENOENT, "failed exec preserves terminal alias");
    free(missing);
    emit("pty-failed-exec-ok\n");
    char *next[] = {argv[0], "after-exec", descriptor, NULL};
    execv(argv[0], next);
    require(0, "successful self exec");
  } else if (strcmp(argv[1], "after-exec") == 0) {
    require(argc == 3, "after-exec arguments");
    char *end = NULL;
    long value = strtol(argv[2], &end, 10);
    require(end != NULL && *end == '\0' && value > STDERR_FILENO && value < 1024,
            "inherited alias argument");
    input = (int)value;
    emit("pty-successful-exec-ok\n");
  } else {
    require(argc == 2 && strcmp(argv[1], "direct") == 0, "direct mode");
  }

  require(isatty(input) == 1, "input remains a terminal");
  int original_flags = fcntl(input, F_GETFL);
  require(original_flags >= 0 && !(original_flags & O_NONBLOCK),
          "blocking terminal flags");
  int release_b[2];
  require(pipe(release_b) == 0, "internal pipe");

  pid_t first = fork();
  require(first >= 0, "fork first child");
  if (first == 0) {
    require(close(release_b[0]) == 0 && close(release_b[1]) == 0,
            "first child pipe closure");
    unsigned char bytes[8];
    memset(bytes, 0xa5, sizeof(bytes));
    ssize_t count = read(input, bytes, sizeof(bytes));
    require(count == 1 && bytes[0] == 'X', "terminal short read");
    for (size_t i = 1; i < sizeof(bytes); ++i) {
      require(bytes[i] == 0xa5, "short read leaves remaining bytes untouched");
    }
    require(fcntl(input, F_GETFL) == original_flags, "first child flags preserved");
    emit("pty-read-a=1:X\n");
    _exit(0);
  }

  pid_t second = fork();
  require(second >= 0, "fork second child");
  if (second == 0) {
    require(close(release_b[1]) == 0, "second child write closure");
    require(close(input) == 0, "second child terminal closure");
    if (input != STDIN_FILENO) {
      require(close(STDIN_FILENO) == 0, "second child stdin closure");
    }
    char byte = 0;
    require(read(release_b[0], &byte, 1) == 1 && byte == 'B', "internal pipe read");
    emit("pty-read-b=1:B\n");
    _exit(0);
  }

  require(first != second, "distinct children");
  require(close(release_b[0]) == 0, "parent read-end closure");
  // The host must see this before supplying terminal data. On the broken
  // path child A blocks in the kernel while the parent still needs its second
  // ParentContinue grant, so neither this line nor the host byte can occur.
  emit("pty-parent-ready\n");
  reap(first);
  require(fcntl(input, F_GETFL) == original_flags, "shared terminal flags preserved");
  emit("pty-reaped-a\n");
  require(write(release_b[1], "B", 1) == 1, "release second child");
  require(close(release_b[1]) == 0, "parent write-end closure");
  reap(second);
  emit("pty-complete\n");
  return 0;
}
