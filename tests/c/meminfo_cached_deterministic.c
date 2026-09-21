/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static long read_kb(const char *wanted) {
  FILE *file = fopen("/proc/meminfo", "r");
  if (file == NULL)
    return -1;
  char *line = NULL;
  size_t capacity = 0;
  long value = -1;
  while (getline(&line, &capacity, file) >= 0) {
    if (strncmp(line, wanted, strlen(wanted)) == 0 &&
        sscanf(line + strlen(wanted), "%ld", &value) == 1)
      break;
  }
  free(line);
  fclose(file);
  return value;
}

int main(void) {
  long total = read_kb("MemTotal:");
  long cached = read_kb("Cached:");
  if (total != 976562 || cached != 0) {
    fprintf(stderr, "MemTotal=%ld Cached=%ld, expected 976562/0\n", total,
            cached);
    return 1;
  }
  puts("Cached is deterministic");
  return 0;
}
