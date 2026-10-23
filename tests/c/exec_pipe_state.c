/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Blocking pipes must cooperate across two execs, while genuinely nonblocking
 * pipes must return EAGAIN without changing status flags. The default runs
 * both contracts; explicit modes bracket inherited descriptor kinds and aliases.
 * Regression for https://github.com/rrnewton/hermit/issues/4028.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

extern char **environ;

static void require(int condition, const char *message) {
  if (!condition) {
    fprintf(stderr, "probe failed: %s (errno %d)\n", message, errno);
    exit(1);
  }
}

static int fail_before_exec;

static void failed_exec_checks(const char *self, const char *mode) {
  char *const next[] = {(char *)self, (char *)mode, NULL};
  int input_flags = fcntl(0, F_GETFL);
  int output_flags = fcntl(1, F_GETFL);
  require(input_flags >= 0 && output_flags >= 0, "flags before failed exec");
  for (int attempt = 0; attempt < 2; ++attempt) {
    errno = 0;
    require(execve("/definitely/missing-exec-pipe-state", next, environ) == -1 &&
                errno == ENOENT,
            "failed exec errno");
    require(fcntl(0, F_GETFL) == input_flags && fcntl(1, F_GETFL) == output_flags,
            "failed exec preserves descriptor flags");
  }
}

static void replace(const char *self, const char *mode) {
  char *const next[] = {(char *)self, (char *)mode, NULL};
  if (fail_before_exec)
    failed_exec_checks(self, mode);
  execve(self, next, environ);
  require(0, "execve");
}

static void reap(pid_t child) {
  int status = 0;
  require(waitpid(child, &status, 0) == child, "waitpid");
  require(WIFEXITED(status) && WEXITSTATUS(status) == 0, "child status");
}

static int closed_stdio_checks(int expected_closed) {
  for (int fd = 0; fd != 3; ++fd) {
    if (expected_closed == 3 || expected_closed == fd) {
      errno = 0;
      if (fcntl(fd, F_GETFD) != -1 || errno != EBADF)
        return 21;
      char byte = 0;
      errno = 0;
      ssize_t result = fd == 0 ? read(fd, &byte, 1) : write(fd, &byte, 1);
      if (result != -1 || errno != EBADF)
        return 22;
    } else if (fcntl(fd, F_GETFD) < 0) {
      return 23;
    }
  }
  return 0;
}

static int alias_checks(void) {
  int original = fcntl(10, F_GETFL);
  require(original >= 0 && fcntl(11, F_GETFL) == original, "inherited alias flags");
  require(fcntl(10, F_SETFL, original | O_NONBLOCK) == 0, "alias set nonblocking");
  require((fcntl(11, F_GETFL) & O_NONBLOCK) != 0, "other alias sees nonblocking");
  require(fcntl(11, F_SETFL, original & ~O_NONBLOCK) == 0, "alias clear nonblocking");
  require((fcntl(10, F_GETFL) & O_NONBLOCK) == 0, "other alias sees blocking");
  return 0;
}

static int nonblocking_checks(int socket_pair) {
  int flags_read = fcntl(0, F_GETFL);
  int flags_write = fcntl(1, F_GETFL);
  require(flags_read >= 0 && (flags_read & O_NONBLOCK), "read O_NONBLOCK");
  require(flags_write >= 0 && (flags_write & O_NONBLOCK), "write O_NONBLOCK");
  struct stat input;
  struct stat output;
  require(fstat(0, &input) == 0 && fstat(1, &output) == 0, "stdio fstat");
  require(socket_pair ? S_ISSOCK(input.st_mode) && S_ISSOCK(output.st_mode)
                      : S_ISFIFO(input.st_mode) && S_ISFIFO(output.st_mode),
          "stdio types");

  char byte = 0;
  errno = 0;
  require(read(0, &byte, 1) == -1 && errno == EAGAIN, "empty nonblocking read");
  struct iovec vector = {.iov_base = &byte, .iov_len = 1};
  errno = 0;
  require(readv(0, &vector, 1) == -1 && errno == EAGAIN, "empty nonblocking readv");
  if (socket_pair) {
    require(write(1, "x", 1) == 1, "socket peer write");
    require(read(0, &byte, 1) == 1 && byte == 'x', "socket peer read");
  } else {
    char block[4096] = {0};
    size_t written = 0;
    for (;;) {
      ssize_t count = write(1, block, sizeof(block));
      if (count < 0) {
        require(errno == EAGAIN, "full nonblocking write");
        break;
      }
      require(count > 0, "write progress");
      written += (size_t)count;
      require(written <= 1024 * 1024, "bounded pipe capacity");
    }
    errno = 0;
    require(writev(1, &vector, 1) == -1 && errno == EAGAIN,
            "full nonblocking writev");
  }
  require(fcntl(0, F_GETFL) == flags_read && fcntl(1, F_GETFL) == flags_write,
          "I/O preserves status flags");
  return 0;
}

int main(int argc, char **argv) {
  if (argc == 1) {
    const char *cases[] = {"pipeline", "nonblock-local", "nonblock-exec"};
    for (size_t index = 0; index < sizeof(cases) / sizeof(cases[0]); ++index) {
      pid_t child = fork();
      require(child >= 0, "regression fork");
      if (child == 0)
        replace(argv[0], cases[index]);
      reap(child);
    }
    puts("blocking exec pipes and nonblocking errors preserved");
    return 0;
  }
  require(argc == 2, "at most one mode required");
  const char *mode = argv[1];
  if (strcmp(mode, "pipeline-failed-exec") == 0) {
    fail_before_exec = 1;
    mode = "pipeline";
  } else if (strcmp(mode, "nonblock-failed-exec") == 0) {
    fail_before_exec = 1;
    mode = "nonblock-exec";
  } else if (strcmp(mode, "nonblock-failed-only") == 0) {
    fail_before_exec = 1;
    mode = "nonblock-local";
  }
  if (strcmp(mode, "--reader") == 0) {
    require((fcntl(0, F_GETFL) & O_NONBLOCK) == 0, "blocking reader flags");
    uint8_t buffer[31];
    size_t total = 0;
    for (;;) {
      ssize_t count = read(0, buffer, sizeof(buffer));
      require(count >= 0, "blocking read");
      if (count == 0)
        break;
      require(total + (size_t)count <= 100, "payload length");
      for (ssize_t i = 0; i < count; ++i)
        require(buffer[i] == (uint8_t)(total + (size_t)i), "payload byte order");
      total += (size_t)count;
    }
    require(total == 100, "payload through EOF");
    require((fcntl(0, F_GETFL) & O_NONBLOCK) == 0, "blocking flags after read");
    return 0;
  }
  if (strcmp(mode, "--writer") == 0) {
    require((fcntl(1, F_GETFL) & O_NONBLOCK) == 0, "blocking writer flags");
    struct timespec delay = {.tv_nsec = 10000000};
    require(nanosleep(&delay, NULL) == 0, "writer delay");
    uint8_t bytes[100];
    for (size_t i = 0; i < sizeof(bytes); ++i)
      bytes[i] = (uint8_t)i;
    require(write(1, bytes, sizeof(bytes)) == (ssize_t)sizeof(bytes), "payload write");
    /* Returning must release the final writer so the peer reaches real EOF. */
    return 0;
  }
  if (strcmp(mode, "--check-nonblock") == 0)
    return nonblocking_checks(0);
  if (strcmp(mode, "--check-socket") == 0)
    return nonblocking_checks(1);
  if (strcmp(mode, "--check-alias") == 0)
    return alias_checks();
  if (strncmp(mode, "--closed-", 9) == 0 && strlen(mode) == 10 &&
      mode[9] >= '0' && mode[9] <= '3')
    return closed_stdio_checks(mode[9] - '0');
  if (strcmp(mode, "closed-exec") == 0) {
    for (int closed_fd = 0; closed_fd != 4; ++closed_fd) {
      pid_t child = fork();
      require(child >= 0, "closed stdio fork");
      if (child == 0) {
        if (closed_fd == 3) {
          for (int fd = 0; fd != 3; ++fd)
            if (close(fd) != 0)
              _exit(24);
        } else if (close(closed_fd) != 0) {
          _exit(25);
        }
        char next_mode[] = "--closed-0";
        next_mode[9] = (char)('0' + closed_fd);
        replace(argv[0], next_mode);
      }
      reap(child);
    }
    puts("closed stdio remains absent across exec");
    return 0;
  }
  if (strcmp(mode, "alias-exec") == 0 || strcmp(mode, "alias-local") == 0) {
    int aliases[2];
    require(pipe(aliases) == 0, "alias pipe");
    require(dup2(aliases[0], 10) == 10 && dup2(aliases[0], 11) == 11,
            "pipe aliases");
    require(close(aliases[0]) == 0, "original alias close");
    if (strcmp(mode, "alias-local") == 0)
      return alias_checks();
    replace(argv[0], "--check-alias");
  }
  if (strcmp(mode, "--check-regular") == 0) {
    struct stat input;
    struct stat output;
    require(fstat(0, &input) == 0 && fstat(1, &output) == 0, "regular stdio stat");
    require(S_ISREG(input.st_mode) && S_ISREG(output.st_mode), "regular stdio types");
    require((fcntl(0, F_GETFL) & O_ACCMODE) == O_RDONLY, "regular input access");
    require((fcntl(1, F_GETFL) & O_ACCMODE) == O_WRONLY, "regular output access");
    char bytes[4] = {0};
    require(read(0, bytes, sizeof(bytes)) == 4 && memcmp(bytes, "data", 4) == 0,
            "regular input bytes");
    require(read(0, bytes, 1) == 0, "regular input EOF");
    require(write(1, "done", 4) == 4, "regular output bytes");
    return 0;
  }
  if (strcmp(mode, "regular-exec") == 0) {
    /* Fixed relative names keep fixture paths stable across verification runs. */
    int initial = open("regular-input", O_CREAT | O_TRUNC | O_WRONLY, 0600);
    require(initial >= 0 && write(initial, "data", 4) == 4 && close(initial) == 0,
            "prepare regular input");
    int input = open("regular-input", O_RDONLY);
    int output = open("regular-output", O_CREAT | O_TRUNC | O_WRONLY, 0600);
    require(input >= 0 && output >= 0, "regular open");
    pid_t child = fork();
    require(child >= 0, "regular stdio fork");
    if (child == 0) {
      require(dup2(input, 0) == 0 && dup2(output, 1) == 1, "regular stdio dup2");
      require(close(input) == 0 && close(output) == 0, "regular unused descriptors");
      replace(argv[0], "--check-regular");
    }
    require(close(input) == 0 && close(output) == 0, "regular parent descriptors");
    reap(child);
    int result_fd = open("regular-output", O_RDONLY);
    char bytes[4] = {0};
    require(result_fd >= 0 && read(result_fd, bytes, sizeof(bytes)) == 4 &&
                memcmp(bytes, "done", 4) == 0,
            "regular output contract");
    require(read(result_fd, bytes, 1) == 0 && close(result_fd) == 0,
            "regular output EOF");
    require(unlink("regular-input") == 0 && unlink("regular-output") == 0,
            "regular fixture cleanup");
    puts("regular stdio preserved across exec");
    return 0;
  }
  if (strcmp(mode, "pipeline") == 0) {
    int pipe_fds[2];
    require(pipe(pipe_fds) == 0, "pipe");
    pid_t reader = fork();
    require(reader >= 0, "reader fork");
    if (reader == 0) {
      require(dup2(pipe_fds[0], 0) == 0, "reader dup2");
      require(close(pipe_fds[0]) == 0 && close(pipe_fds[1]) == 0,
              "reader close unused ends");
      replace(argv[0], "--reader");
    }
    pid_t writer = fork();
    require(writer >= 0, "writer fork");
    if (writer == 0) {
      require(dup2(pipe_fds[1], 1) == 1, "writer dup2");
      require(close(pipe_fds[0]) == 0 && close(pipe_fds[1]) == 0,
              "writer close unused ends");
      replace(argv[0], "--writer");
    }
    require(close(pipe_fds[0]) == 0 && close(pipe_fds[1]) == 0,
            "parent close unused ends");
    reap(reader);
    reap(writer);
    puts("pipeline payload and EOF preserved");
    return 0;
  }
  int socket_pair = strcmp(mode, "socket-exec") == 0;
  require(socket_pair || strcmp(mode, "nonblock-exec") == 0 ||
              strcmp(mode, "nonblock-local") == 0,
          "known mode");
  int descriptors[2];
  require(socket_pair ? socketpair(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0,
                                  descriptors) == 0
                      : pipe2(descriptors, O_NONBLOCK) == 0,
          "nonblocking endpoint creation");
  require(dup2(descriptors[0], 0) == 0 && dup2(descriptors[1], 1) == 1,
          "nonblocking stdio dup2");
  require(close(descriptors[0]) == 0 && close(descriptors[1]) == 0,
          "nonblocking unused ends");
  if (strcmp(mode, "nonblock-local") == 0) {
    if (fail_before_exec)
      failed_exec_checks(argv[0], "--check-nonblock");
    return nonblocking_checks(0);
  }
  replace(argv[0], socket_pair ? "--check-socket" : "--check-nonblock");
  return 1;
}
