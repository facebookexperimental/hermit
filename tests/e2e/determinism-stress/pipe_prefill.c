/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static void fail(const char *operation) {
  fprintf(stderr, "%s: %s\n", operation, strerror(errno));
  exit(EXIT_FAILURE);
}

int main(void) {
  static const char input[] = "kvm pipe inheritance";
  static const char expected[] = "KVM PIPE INHERITANCE";
  int inbound[2];
  int outbound[2];

  if (pipe(inbound) != 0 || pipe(outbound) != 0) {
    fail("pipe");
  }
  if (write(inbound[1], input, sizeof(input)) != (ssize_t)sizeof(input)) {
    fail("write input");
  }
  close(inbound[1]);

  const pid_t child = fork();
  if (child < 0) {
    fail("fork");
  }
  if (child == 0) {
    char buffer[sizeof(input)];
    close(outbound[0]);
    const ssize_t length = read(inbound[0], buffer, sizeof(buffer));
    close(inbound[0]);
    if (length != (ssize_t)sizeof(buffer)) {
      _exit(20);
    }
    for (size_t index = 0; index + 1 < sizeof(buffer); ++index) {
      if (buffer[index] >= 'a' && buffer[index] <= 'z') {
        buffer[index] -= 'a' - 'A';
      }
    }
    if (write(outbound[1], buffer, sizeof(buffer)) != (ssize_t)sizeof(buffer)) {
      _exit(21);
    }
    close(outbound[1]);
    _exit(37);
  }

  close(inbound[0]);
  close(outbound[1]);
  char output[sizeof(expected)];
  const ssize_t length = read(outbound[0], output, sizeof(output));
  close(outbound[0]);
  int status = 0;
  if (waitpid(child, &status, 0) != child || length != (ssize_t)sizeof(output) ||
      !WIFEXITED(status) || WEXITSTATUS(status) != 37 ||
      memcmp(output, expected, sizeof(expected)) != 0) {
    return EXIT_FAILURE;
  }

  printf("fork-pipe bytes=%zu child-exit=%d payload=%s\n", sizeof(output),
         WEXITSTATUS(status), output);
  return EXIT_SUCCESS;
}
