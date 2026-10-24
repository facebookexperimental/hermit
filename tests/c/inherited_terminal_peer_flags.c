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
    dprintf(STDERR_FILENO, "terminal-peer failure: %s errno=%d\n", operation, errno);
    _exit(91);
  }
}
static void emit(const char *text) {
  size_t length = strlen(text);
  require(write(STDOUT_FILENO, text, length) == (ssize_t)length, "marker");
}
static void send_byte(int fd, char byte) {
  require(write(fd, &byte, 1) == 1, "send protocol byte");
}
static void receive_byte(int fd, char expected) {
  char byte = 0;
  require(read(fd, &byte, 1) == 1 && byte == expected, "receive protocol byte");
}
static int descriptor(const char *text) {
  char *end = NULL;
  long value = strtol(text, &end, 10);
  require(end && !*end && value > STDERR_FILENO && value < 1024, "descriptor");
  return (int)value;
}
static void reap(pid_t child) {
  int status = 0;
  require(waitpid(child, &status, 0) == child, "wait exact child");
  require(WIFEXITED(status) && WEXITSTATUS(status) == 0, "child status");
}

int main(int argc, char **argv) {
  require(argc >= 2, "mode");
  if (!strcmp(argv[1], "reader-after-exec")) {
    require(argc == 5, "reader arguments");
    int input = descriptor(argv[2]);
    int restored = descriptor(argv[3]);
    int cleared = descriptor(argv[4]);
    int flags = fcntl(input, F_GETFL);
    require(flags >= 0 && (flags & O_NONBLOCK), "restored nonblocking terminal");
    unsigned char bytes[8];
    memset(bytes, 0xa5, sizeof(bytes));
    errno = 0;
    require(read(input, bytes, sizeof(bytes)) == -1 && errno == EAGAIN,
            "empty real nonblocking read before notification");
    for (size_t i = 0; i < sizeof(bytes); ++i)
      require(bytes[i] == 0xa5, "EAGAIN preserves buffer");
    emit("pty-peer-reader-restored\n");
    send_byte(restored, 'R');
    require(close(restored) == 0, "close restored notification");
    receive_byte(cleared, 'C');
    require(close(cleared) == 0, "close clear notification");
    // No flag query between the peer clear and this read: restored local
    // metadata may still cache O_NONBLOCK although Linux's shared OFD does not.
    require(read(input, bytes, sizeof(bytes)) == 1 && bytes[0] == 'X',
            "short read after peer cleared nonblocking");
    for (size_t i = 1; i < sizeof(bytes); ++i)
      require(bytes[i] == 0xa5, "short read preserves remaining buffer");
    emit("pty-read-a=1:X\n");
    return 0;
  }
  require(argc == 2 && !strcmp(argv[1], "peer-flags"), "parent arguments");
  int input = dup(STDIN_FILENO);
  require(input > STDERR_FILENO && isatty(input), "inherited terminal alias");
  int original = fcntl(input, F_GETFL);
  require(original >= 0 && !(original & O_NONBLOCK), "initial blocking flags");
  require(fcntl(input, F_SETFL, original | O_NONBLOCK) == 0, "set nonblocking");
  int restored[2], cleared_a[2], cleared_parent[2], release_b[2];
  require(pipe(restored) == 0 && pipe(cleared_a) == 0 &&
              pipe(cleared_parent) == 0 && pipe(release_b) == 0, "protocol pipes");
  pid_t first = fork();
  require(first >= 0, "fork reader");
  if (!first) {
    require(close(restored[0]) == 0 && close(cleared_a[1]) == 0 &&
                close(cleared_parent[0]) == 0 && close(cleared_parent[1]) == 0 &&
                close(release_b[0]) == 0 && close(release_b[1]) == 0,
            "reader unused endpoints");
    char input_arg[32], restored_arg[32], cleared_arg[32];
    require(snprintf(input_arg, sizeof(input_arg), "%d", input) > 0 &&
                snprintf(restored_arg, sizeof(restored_arg), "%d", restored[1]) > 0 &&
                snprintf(cleared_arg, sizeof(cleared_arg), "%d", cleared_a[0]) > 0,
            "format reader descriptors");
    char *next[] = {argv[0], "reader-after-exec", input_arg, restored_arg, cleared_arg, NULL};
    execv(argv[0], next);
    require(0, "reader self exec");
  }
  pid_t second = fork();
  require(second >= 0, "fork peer");
  if (!second) {
    require(close(restored[1]) == 0 && close(cleared_a[0]) == 0 &&
                close(cleared_parent[0]) == 0 && close(release_b[1]) == 0,
            "peer unused endpoints");
    receive_byte(restored[0], 'R');
    require(fcntl(input, F_SETFL, original) == 0, "peer clear real OFD flag");
    require(fcntl(input, F_GETFL) == original, "clearing peer observes blocking flags");
    emit("pty-peer-cleared\n");
    send_byte(cleared_a[1], 'C');
    send_byte(cleared_parent[1], 'C');
    require(close(restored[0]) == 0 && close(cleared_a[1]) == 0 &&
                close(cleared_parent[1]) == 0, "peer protocol closure");
    receive_byte(release_b[0], 'B');
    emit("pty-read-b=1:B\n");
    return 0;
  }
  require(first != second, "distinct children");
  require(close(restored[0]) == 0 && close(restored[1]) == 0 &&
              close(cleared_a[0]) == 0 && close(cleared_a[1]) == 0 &&
              close(cleared_parent[1]) == 0 && close(release_b[0]) == 0,
          "parent unused endpoints");
  receive_byte(cleared_parent[0], 'C');
  require(close(cleared_parent[0]) == 0, "parent clear notification closure");
  // Synchronization proves the clear precedes A's read. It does not invent a
  // scheduler order between that read and this parent marker; retain actual
  // old-code outcomes instead of claiming every schedule must reproduce.
  emit("pty-parent-ready\n");
  reap(first);
  emit("pty-reaped-a\n");
  send_byte(release_b[1], 'B');
  require(close(release_b[1]) == 0, "release peer endpoint closure");
  reap(second);
  emit("pty-complete\n");
  return 0;
}
