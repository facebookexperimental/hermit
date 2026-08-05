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
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/xattr.h>
#include <unistd.h>

#ifndef SYS_fchmodat2
#define SYS_fchmodat2 452
#endif

#ifndef SYNC_FILE_RANGE_WRITE
#define SYNC_FILE_RANGE_WRITE 2
#endif

static void require_zero(long result, const char *name) {
  if (result != 0) {
    perror(name);
    exit(1);
  }
}

static int expected_xattr_error(int error) {
  return error == EACCES || error == ENODATA || error == ENOTSUP ||
         error == EOPNOTSUPP || error == EPERM;
}

static void require_xattr_result(ssize_t result, const char *name) {
  if (result < 0 && !expected_xattr_error(errno)) {
    perror(name);
    exit(1);
  }
}

static void require_xattr_value(ssize_t result, const char *name,
                                const char *actual, const char *expected,
                                size_t expected_len, int must_exist) {
  require_xattr_result(result, name);
  if (must_exist && (result != (ssize_t)expected_len ||
                     memcmp(actual, expected, expected_len) != 0)) {
    fprintf(stderr, "%s returned unexpected extended-attribute value\n", name);
    exit(1);
  }
}

static int xattr_list_contains(const char *list, size_t list_len,
                               const char *name) {
  size_t offset = 0;
  size_t name_len = strlen(name);
  while (offset < list_len) {
    size_t remaining = list_len - offset;
    size_t entry_len = strnlen(list + offset, remaining);
    if (entry_len == remaining) {
      return 0;
    }
    if (entry_len == name_len && memcmp(list + offset, name, name_len) == 0) {
      return 1;
    }
    offset += entry_len + 1;
  }
  return 0;
}

static void require_xattr_list(ssize_t result, const char *call_name,
                               const char *list, const char *xattr_name,
                               int must_exist) {
  require_xattr_result(result, call_name);
  if (must_exist &&
      (result < 0 || !xattr_list_contains(list, (size_t)result, xattr_name))) {
    fprintf(stderr, "%s omitted %s\n", call_name, xattr_name);
    exit(1);
  }
}

int main(void) {
  char path[128];
  char hardlink_path[128];
  char symlink_path[128];
  long pid = (long)getpid();
  snprintf(path, sizeof(path), "/tmp/hermit-file-metadata-%ld", pid);
  snprintf(hardlink_path, sizeof(hardlink_path),
           "/tmp/hermit-file-metadata-%ld-hard", pid);
  snprintf(symlink_path, sizeof(symlink_path),
           "/tmp/hermit-file-metadata-%ld-sym", pid);

  unlink(symlink_path);
  unlink(hardlink_path);
  unlink(path);

  int fd = open(path, O_CREAT | O_TRUNC | O_RDWR, 0600);
  if (fd < 0) {
    perror("open");
    return 1;
  }
  require_zero(ftruncate(fd, 4096), "ftruncate");
  off_t offset_before = lseek(fd, 0, SEEK_CUR);
  if (offset_before < 0 || pwrite(fd, "metadata", 8, 0) != 8) {
    perror("pwrite");
    return 1;
  }
  off_t offset_after = lseek(fd, 0, SEEK_CUR);
  if (offset_after != offset_before) {
    fprintf(stderr, "pwrite changed file offset: %ld -> %ld\n",
            (long)offset_before, (long)offset_after);
    return 1;
  }
  char readback[8] = {0};
  if (pread(fd, readback, sizeof(readback), 0) != (ssize_t)sizeof(readback) ||
      memcmp(readback, "metadata", sizeof(readback)) != 0) {
    perror("pread after pwrite");
    return 1;
  }

  uid_t uid = getuid();
  gid_t gid = getgid();
  require_zero(fchmod(fd, 0600), "fchmod");
  require_zero(fchown(fd, uid, gid), "fchown");
  require_zero(syscall(SYS_fchownat, AT_FDCWD, path, uid, gid, 0), "fchownat");
  require_zero(syscall(SYS_faccessat, AT_FDCWD, path, R_OK | W_OK),
               "faccessat");

  long fchmodat2_result = syscall(SYS_fchmodat2, AT_FDCWD, path, 0600, 0);
  if (fchmodat2_result != 0 && errno != ENOSYS) {
    perror("fchmodat2");
    return 1;
  }

  require_zero(link(path, hardlink_path), "link");
  require_zero(symlink(path, symlink_path), "symlink");
  require_zero(lchown(symlink_path, uid, gid), "lchown");

  const char *name = "user.hermit";
  const char value[] = "metadata";
  char value_buffer[32] = {0};
  char list_buffer[128] = {0};
  ssize_t result = setxattr(path, name, value, sizeof(value), 0);
  require_xattr_result(result, "setxattr");
  int must_exist = result == 0;
  memset(value_buffer, 0, sizeof(value_buffer));
  result = getxattr(path, name, value_buffer, sizeof(value_buffer));
  require_xattr_value(result, "getxattr", value_buffer, value, sizeof(value),
                      must_exist);
  memset(list_buffer, 0, sizeof(list_buffer));
  result = listxattr(path, list_buffer, sizeof(list_buffer));
  require_xattr_list(result, "listxattr", list_buffer, name, must_exist);
  result = removexattr(path, name);
  must_exist ? require_zero(result, "removexattr")
             : require_xattr_result(result, "removexattr");

  result = fsetxattr(fd, name, value, sizeof(value), 0);
  require_xattr_result(result, "fsetxattr");
  must_exist = result == 0;
  memset(value_buffer, 0, sizeof(value_buffer));
  result = fgetxattr(fd, name, value_buffer, sizeof(value_buffer));
  require_xattr_value(result, "fgetxattr", value_buffer, value, sizeof(value),
                      must_exist);
  memset(list_buffer, 0, sizeof(list_buffer));
  result = flistxattr(fd, list_buffer, sizeof(list_buffer));
  require_xattr_list(result, "flistxattr", list_buffer, name, must_exist);
  result = fremovexattr(fd, name);
  must_exist ? require_zero(result, "fremovexattr")
             : require_xattr_result(result, "fremovexattr");

  result = lsetxattr(symlink_path, name, value, sizeof(value), 0);
  require_xattr_result(result, "lsetxattr");
  must_exist = result == 0;
  memset(value_buffer, 0, sizeof(value_buffer));
  result = lgetxattr(symlink_path, name, value_buffer, sizeof(value_buffer));
  require_xattr_value(result, "lgetxattr", value_buffer, value, sizeof(value),
                      must_exist);
  memset(list_buffer, 0, sizeof(list_buffer));
  result = llistxattr(symlink_path, list_buffer, sizeof(list_buffer));
  require_xattr_list(result, "llistxattr", list_buffer, name, must_exist);
  result = lremovexattr(symlink_path, name);
  must_exist ? require_zero(result, "lremovexattr")
             : require_xattr_result(result, "lremovexattr");

  void *mapping = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  if (mapping == MAP_FAILED) {
    perror("mmap");
    return 1;
  }
  memcpy(mapping, "sync", 4);
  require_zero(msync(mapping, 4096, MS_SYNC), "msync");
  require_zero(munmap(mapping, 4096), "munmap");

  require_zero(syscall(SYS_readahead, fd, 0, 4096), "readahead");
  require_zero(syscall(SYS_sync_file_range, fd, 0, 4096, SYNC_FILE_RANGE_WRITE),
               "sync_file_range");

  require_zero(close(fd), "close");
  require_zero(unlink(symlink_path), "unlink symlink");
  require_zero(unlink(hardlink_path), "unlink hardlink");
  require_zero(unlink(path), "unlink file");
  puts("syscall-file-metadata-ok count=20");
  return 0;
}
