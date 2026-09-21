/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <fcntl.h>
#include <linux/stat.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <unistd.h>

#include <stdio.h>
#include <string.h>

int main(void) {
  FILE *mountinfo = fopen("/proc/self/mountinfo", "r");
  if (mountinfo == NULL) {
    return 1;
  }
  char row[4096];
  if (fgets(row, sizeof(row), mountinfo) == NULL || fclose(mountinfo) != 0) {
    return 1;
  }

  struct stat stat_result;
  if (stat("/", &stat_result) != 0) {
    return 1;
  }

  struct statx statx_result;
  memset(&statx_result, 0, sizeof(statx_result));
  if (syscall(SYS_statx, AT_FDCWD, "/", 0, STATX_BASIC_STATS,
              &statx_result) != 0) {
    return 1;
  }

  printf("MOUNTINFO %s", row);
  printf("STAT %u:%u\n", major(stat_result.st_dev), minor(stat_result.st_dev));
  printf("STATX %u:%u\n", statx_result.stx_dev_major,
         statx_result.stx_dev_minor);
  return 0;
}
