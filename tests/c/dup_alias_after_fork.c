/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Two descriptors that alias one open file description (after dup) must
 * still share it in a forked child: reads through either advance one
 * cursor. The child reads 8 bytes through each alias of /dev/urandom and
 * prints them; they continue one stream, so they differ, and the output
 * equals the reference backend's.
 */

#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static void print_bytes(const char* label, const unsigned char* bytes) {
  printf("%s", label);
  for (int i = 0; i < 8; i++) {
    printf("%02x", bytes[i]);
  }
  printf("\n");
}

int main(void) {
  int first = open("/dev/urandom", O_RDONLY);
  if (first < 0) {
    perror("open");
    return 1;
  }
  int second = dup(first);
  if (second < 0) {
    perror("dup");
    return 1;
  }
  fflush(stdout);
  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return 1;
  }
  if (child == 0) {
    unsigned char a[8], b[8];
    if (read(first, a, sizeof(a)) != (ssize_t)sizeof(a) ||
        read(second, b, sizeof(b)) != (ssize_t)sizeof(b)) {
      perror("read");
      _exit(1);
    }
    print_bytes("child first alias  ", a);
    print_bytes("child second alias ", b);
    printf("aliases continue one stream=%d\n", memcmp(a, b, sizeof(a)) != 0);
    fflush(stdout);
    _exit(0);
  }
  int status = 0;
  if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    fprintf(stderr, "child failed\n");
    return 1;
  }
  return 0;
}
