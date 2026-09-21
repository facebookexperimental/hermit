/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_copy_file_range
#define SYS_copy_file_range 326
#endif

int main(void) {
  int input = open("/tmp/hermit-copy-file-range-source",
                   O_RDWR | O_CREAT | O_TRUNC, 0600);
  int output = open("/tmp/hermit-copy-file-range-destination",
                    O_WRONLY | O_CREAT | O_TRUNC, 0600);
  if (input < 0 || output < 0) {
    perror("open");
    return 2;
  }
  if (write(input, "copy-file-range", 15) != 15 ||
      lseek(input, 0, SEEK_SET) != 0) {
    perror("prepare source");
    return 2;
  }

  errno = 0;
  long result =
      syscall(SYS_copy_file_range, input, NULL, output, NULL, 4096, 0);
  if (result == -1 && errno == ENOSYS) {
    puts("copy-file-range-refused-ok");
    return 0;
  }

  fprintf(stderr, "copy_file_range: expected ENOSYS, got result=%ld errno=%d\n",
          result, errno);
  return 1;
}
