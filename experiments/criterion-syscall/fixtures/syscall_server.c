/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

static int read_fd = -1;
static int write_fd = -1;

static int ensure_fd(int *fd, int flags) {
  if (*fd >= 0) {
    return 0;
  }
  *fd = open("/dev/null", flags | O_CLOEXEC);
  return *fd < 0 ? -1 : 0;
}

static int run_getpid(uint64_t iterations, uint64_t *accumulator) {
  uint64_t value = 0;
  for (uint64_t index = 0; index < iterations; ++index) {
    long result = syscall(SYS_getpid);
    if (result < 0) {
      return -1;
    }
    value += (uint64_t)result;
  }
  *accumulator = value;
  return 0;
}

static int run_read(uint64_t iterations, uint64_t *accumulator) {
  if (ensure_fd(&read_fd, O_RDONLY) != 0) {
    return -1;
  }
  uint64_t value = 0;
  char byte = 0;
  for (uint64_t index = 0; index < iterations; ++index) {
    long result = syscall(SYS_read, read_fd, &byte, 1);
    if (result < 0) {
      return -1;
    }
    value += (uint64_t)result;
  }
  *accumulator = value;
  return 0;
}

static int run_write(uint64_t iterations, uint64_t *accumulator) {
  if (ensure_fd(&write_fd, O_WRONLY) != 0) {
    return -1;
  }
  uint64_t value = 0;
  const char byte = 0;
  for (uint64_t index = 0; index < iterations; ++index) {
    long result = syscall(SYS_write, write_fd, &byte, 1);
    if (result < 0) {
      return -1;
    }
    value += (uint64_t)result;
  }
  *accumulator = value;
  return 0;
}

static int run_clock_gettime(uint64_t iterations, uint64_t *accumulator) {
  uint64_t value = 0;
  struct timespec timestamp = {0};
  for (uint64_t index = 0; index < iterations; ++index) {
    long result = syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &timestamp);
    if (result < 0) {
      return -1;
    }
    value += (uint64_t)timestamp.tv_nsec;
  }
  *accumulator = value;
  return 0;
}

static int execute(const char *operation, uint64_t iterations,
                   uint64_t *accumulator) {
  if (strcmp(operation, "getpid") == 0) {
    return run_getpid(iterations, accumulator);
  }
  if (strcmp(operation, "read") == 0) {
    return run_read(iterations, accumulator);
  }
  if (strcmp(operation, "write") == 0) {
    return run_write(iterations, accumulator);
  }
  if (strcmp(operation, "clock_gettime") == 0) {
    return run_clock_gettime(iterations, accumulator);
  }
  errno = EINVAL;
  return -1;
}

static int connect_control(const char *path) {
  if (strlen(path) >= sizeof(((struct sockaddr_un *)0)->sun_path)) {
    errno = ENAMETOOLONG;
    return -1;
  }
  int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
  if (fd < 0) {
    return -1;
  }
  struct sockaddr_un address = {.sun_family = AF_UNIX};
  strcpy(address.sun_path, path);
  if (connect(fd, (const struct sockaddr *)&address, sizeof(address)) != 0) {
    int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    return -1;
  }
  return fd;
}

int main(int argc, char **argv) {
  if (argc == 4 && strcmp(argv[1], "--run") == 0) {
    char *end = NULL;
    errno = 0;
    uint64_t iterations = strtoull(argv[3], &end, 10);
    if (errno != 0 || end == argv[3] || *end != '\0') {
      fprintf(stderr, "invalid iteration count: %s\n", argv[3]);
      return 2;
    }
    uint64_t accumulator = 0;
    if (execute(argv[2], iterations, &accumulator) != 0) {
      perror(argv[2]);
      return 1;
    }
    if (accumulator == UINT64_MAX) {
      fputs("unreachable accumulator value\n", stderr);
      return 1;
    }
    return 0;
  }

  FILE *input = stdin;
  FILE *output = stdout;
  if (argc == 3 && strcmp(argv[1], "--socket") == 0) {
    int control_fd = connect_control(argv[2]);
    if (control_fd < 0) {
      perror("connect control socket");
      return 1;
    }
    int input_fd = dup(control_fd);
    if (input_fd < 0 || (input = fdopen(input_fd, "r")) == NULL ||
        (output = fdopen(control_fd, "w")) == NULL) {
      perror("open control socket streams");
      return 1;
    }
  } else if (argc != 2 || strcmp(argv[1], "--server") != 0) {
    fprintf(stderr, "usage: %s --server | --socket PATH | --run OP N\n", argv[0]);
    return 2;
  }

  setvbuf(input, NULL, _IONBF, 0);
  setvbuf(output, NULL, _IONBF, 0);
  fputs("READY syscall-server-v1\n", output);

  char line[256];
  char operation[64];
  uint64_t iterations = 0;
  while (fgets(line, sizeof(line), input) != NULL) {
    if (sscanf(line, "%63s %" SCNu64, operation, &iterations) != 2) {
      fputs("ERR protocol 0\n", output);
      continue;
    }
    if (strcmp(operation, "quit") == 0) {
      fputs("BYE\n", output);
      return 0;
    }

    uint64_t accumulator = 0;
    errno = 0;
    if (execute(operation, iterations, &accumulator) != 0) {
      fprintf(output, "ERR %s %d\n", operation, errno);
      continue;
    }
    fprintf(output, "OK %s %" PRIu64 " %" PRIu64 "\n", operation,
            iterations, accumulator);
  }

  return ferror(input) ? 1 : 0;
}
