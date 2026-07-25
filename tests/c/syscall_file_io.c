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
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

static void require_zero(long result, const char *name) {
  if (result != 0) {
    perror(name);
    exit(1);
  }
}

int main(void) {
  char original[128];
  char renamed[128];
  char renamed_at[128];
  char link_path[128];
  long pid = (long)getpid();
  snprintf(original, sizeof(original), "/tmp/hermit-file-io-%ld-a", pid);
  snprintf(renamed, sizeof(renamed), "/tmp/hermit-file-io-%ld-b", pid);
  snprintf(renamed_at, sizeof(renamed_at), "/tmp/hermit-file-io-%ld-c", pid);
  snprintf(link_path, sizeof(link_path), "/tmp/hermit-file-io-%ld-link", pid);

  int fd = open(original, O_CREAT | O_TRUNC | O_RDWR, 0600);
  if (fd < 0 || write(fd, "file-io", 7) != 7) {
    perror("open/write");
    return 1;
  }

  struct stat status;
  long fallocate_result = syscall(SYS_fallocate, fd, 0, 0, 8192);
  if (fallocate_result == 0) {
    require_zero(fstat(fd, &status), "fstat after fallocate");
    if (status.st_size != 8192) {
      fprintf(stderr, "fallocate size mismatch: %ld\n", (long)status.st_size);
      return 1;
    }
  } else if (errno != EOPNOTSUPP) {
    perror("fallocate");
    return 1;
  }
  require_zero(close(fd), "close");

  require_zero(syscall(SYS_truncate, original, 4096), "truncate");
  require_zero(stat(original, &status), "stat");
  if (status.st_size != 4096) {
    fprintf(stderr, "truncate size mismatch: %ld\n", (long)status.st_size);
    return 1;
  }

  require_zero(syscall(SYS_rename, original, renamed), "rename");
  require_zero(syscall(SYS_renameat, AT_FDCWD, renamed, AT_FDCWD, renamed_at),
               "renameat");
  require_zero(symlinkat(renamed_at, AT_FDCWD, link_path), "symlinkat");

  char target[128] = {0};
  long length = syscall(SYS_readlinkat, AT_FDCWD, link_path, target,
                        sizeof(target) - 1);
  if (length < 0) {
    perror("readlinkat");
    return 1;
  }
  target[length] = '\0';
  if (strcmp(target, renamed_at) != 0) {
    fprintf(stderr, "readlinkat target mismatch: %s\n", target);
    return 1;
  }

  require_zero(unlink(link_path), "unlink link");
  require_zero(unlink(renamed_at), "unlink file");
  puts("syscall-file-io-ok count=5");
  return 0;
}
