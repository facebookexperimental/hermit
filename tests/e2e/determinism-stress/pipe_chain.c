/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

enum {
  STAGE_COUNT = 5,
  LINK_COUNT = STAGE_COUNT + 1,
  BUFFER_CAPACITY = 4096,
  STAGE_EXIT_BASE = 30,
};

static const char seed[] = "seed=ptrace-compat\n";

static void fail(const char *operation) {
  fprintf(stderr, "%s: %s\n", operation, strerror(errno));
  exit(EXIT_FAILURE);
}

static ssize_t read_all(int fd, char *buffer, size_t capacity) {
  size_t length = 0;
  while (length < capacity) {
    const ssize_t bytes = read(fd, buffer + length, capacity - length);
    if (bytes < 0 && errno == EINTR) {
      continue;
    }
    if (bytes < 0) {
      return -1;
    }
    if (bytes == 0) {
      return (ssize_t)length;
    }
    length += (size_t)bytes;
  }
  errno = EOVERFLOW;
  return -1;
}

static int write_all(int fd, const char *buffer, size_t length) {
  size_t written = 0;
  while (written < length) {
    const ssize_t bytes = write(fd, buffer + written, length - written);
    if (bytes < 0 && errno == EINTR) {
      continue;
    }
    if (bytes <= 0) {
      return -1;
    }
    written += (size_t)bytes;
  }
  return 0;
}

static void close_unneeded_fds(int links[LINK_COUNT][2], unsigned stage) {
  const int input = links[stage][0];
  const int output = links[stage + 1][1];
  for (unsigned link = 0; link < LINK_COUNT; ++link) {
    for (unsigned end = 0; end < 2; ++end) {
      const int fd = links[link][end];
      if (fd != input && fd != output) {
        close(fd);
      }
    }
  }
}

static void run_stage(int links[LINK_COUNT][2], unsigned stage) {
  char buffer[BUFFER_CAPACITY];
  char addition[128];

  close_unneeded_fds(links, stage);
  const int input = links[stage][0];
  const int output = links[stage + 1][1];
  const ssize_t input_length = read_all(input, buffer, sizeof(buffer));
  close(input);
  if (input_length < 0) {
    _exit(200 + stage);
  }

  const int addition_length =
      snprintf(addition, sizeof(addition),
               "stage=%u payload=%08x\n", stage, 0x13579bdfU ^ stage);
  if (addition_length < 0 || (size_t)addition_length >= sizeof(addition) ||
      (size_t)input_length + (size_t)addition_length > sizeof(buffer) ||
      write_all(output, buffer, (size_t)input_length) != 0 ||
      write_all(output, addition, (size_t)addition_length) != 0) {
    _exit(210 + stage);
  }
  close(output);
  _exit(STAGE_EXIT_BASE + stage);
}

static int wait_for_exit(pid_t process, int expected_exit) {
  int status;
  pid_t waited;
  do {
    waited = waitpid(process, &status, 0);
  } while (waited < 0 && errno == EINTR);

  return waited == process && WIFEXITED(status) &&
         WEXITSTATUS(status) == expected_exit;
}

static size_t build_expected(char expected[BUFFER_CAPACITY]) {
  size_t length = sizeof(seed) - 1;
  memcpy(expected, seed, length);
  for (unsigned stage = 0; stage < STAGE_COUNT; ++stage) {
    const int bytes = snprintf(expected + length, BUFFER_CAPACITY - length,
                               "stage=%u payload=%08x\n", stage,
                               0x13579bdfU ^ stage);
    if (bytes < 0 || (size_t)bytes >= BUFFER_CAPACITY - length) {
      fail("snprintf(expected)");
    }
    length += (size_t)bytes;
  }
  return length;
}

int main(void) {
  int links[LINK_COUNT][2];
  pid_t stages[STAGE_COUNT];

  for (unsigned link = 0; link < LINK_COUNT; ++link) {
    if (pipe(links[link]) != 0) {
      fail("pipe");
    }
  }
  for (unsigned stage = 0; stage < STAGE_COUNT; ++stage) {
    const pid_t child = fork();
    if (child < 0) {
      fail("fork(stage)");
    }
    if (child == 0) {
      run_stage(links, stage);
    }
    stages[stage] = child;
  }

  for (unsigned link = 0; link < LINK_COUNT; ++link) {
    if (link != 0) {
      close(links[link][1]);
    }
    if (link != STAGE_COUNT) {
      close(links[link][0]);
    }
  }

  if (write_all(links[0][1], seed, sizeof(seed) - 1) != 0) {
    fail("write(seed)");
  }
  close(links[0][1]);

  char output[BUFFER_CAPACITY];
  const ssize_t output_length =
      read_all(links[STAGE_COUNT][0], output, sizeof(output));
  close(links[STAGE_COUNT][0]);
  if (output_length < 0) {
    fail("read(output)");
  }

  for (unsigned stage = 0; stage < STAGE_COUNT; ++stage) {
    if (!wait_for_exit(stages[stage], STAGE_EXIT_BASE + stage)) {
      fprintf(stderr, "stage %u exit mismatch\n", stage);
      return EXIT_FAILURE;
    }
  }

  char expected[BUFFER_CAPACITY];
  const size_t expected_length = build_expected(expected);
  if ((size_t)output_length != expected_length ||
      memcmp(output, expected, expected_length) != 0) {
    fprintf(stderr, "pipe output mismatch: got=%zd expected=%zu\n",
            output_length, expected_length);
    return EXIT_FAILURE;
  }

  if (write_all(STDOUT_FILENO, output, (size_t)output_length) != 0) {
    fail("write(stdout)");
  }
  printf("pipe-chain stages=%u bytes=%zu exits=", STAGE_COUNT,
         expected_length);
  for (unsigned stage = 0; stage < STAGE_COUNT; ++stage) {
    printf("%s%u", stage == 0 ? "" : ",", STAGE_EXIT_BASE + stage);
  }
  putchar('\n');
  return EXIT_SUCCESS;
}
