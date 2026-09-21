/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

struct handoff {
  int fd;
  long opener_tid;
};

static void *read_status(void *opaque) {
  struct handoff *handoff = opaque;
  char buffer[8192];
  ssize_t count = read(handoff->fd, buffer, sizeof(buffer) - 1);
  if (count < 0) {
    return (void *)(intptr_t)1;
  }
  buffer[count] = '\0';

  char *save = NULL;
  for (char *line = strtok_r(buffer, "\n", &save); line != NULL;
       line = strtok_r(NULL, "\n", &save)) {
    if (strncmp(line, "Pid:", 4) == 0) {
      long observed = strtol(line + 4, NULL, 10);
      return (void *)(intptr_t)(observed == handoff->opener_tid ? 0 : 2);
    }
  }
  return (void *)(intptr_t)3;
}

int main(void) {
  struct handoff handoff = {
      .fd = open("/proc/thread-self/status", O_RDONLY | O_CLOEXEC),
      .opener_tid = syscall(SYS_gettid),
  };
  if (handoff.fd < 0) {
    return EXIT_FAILURE;
  }

  pthread_t reader;
  if (pthread_create(&reader, NULL, read_status, &handoff) != 0) {
    return EXIT_FAILURE;
  }
  void *result = NULL;
  if (pthread_join(reader, &result) != 0 || close(handoff.fd) != 0) {
    return EXIT_FAILURE;
  }
  if ((intptr_t)result != 0) {
    return EXIT_FAILURE;
  }
  puts("thread-self opener identity preserved");
  return EXIT_SUCCESS;
}
