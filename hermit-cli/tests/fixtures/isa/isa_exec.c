// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

/*
 * isa_exec MODE IMAGE: execs IMAGE by execveat(2) in one of these ways, and
 * where the exec fails prints "errno=<n>" and exits 0.
 *
 *   memfd       a copy of IMAGE in a memfd, by AT_EMPTY_PATH: the executed
 *               object has no pathname anywhere
 *   bad-flag    IMAGE's descriptor with an unknown flag (EINVAL)
 *   empty-path  IMAGE's descriptor and "" without AT_EMPTY_PATH (ENOENT)
 *   no-follow   a symbolic link to IMAGE with AT_SYMLINK_NOFOLLOW (ELOOP)
 *   chroot      after a chroot to IMAGE's directory, by the absolute link
 *               /isa-link there, which names IMAGE only inside that root
 */

#define _GNU_SOURCE
#include <errno.h>
#include <libgen.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/sendfile.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

extern char** environ;

static int execveat_(int dirfd, const char* path, int flags) {
  char* argv[] = {(char*)path, NULL};
  return (int)syscall(SYS_execveat, dirfd, path, argv, environ, flags);
}

int main(int argc, char** argv) {
  if (argc != 3) {
    fprintf(stderr, "usage: isa_exec MODE IMAGE\n");
    return 2;
  }
  const char* mode = argv[1];
  const char* image = argv[2];
  int fd = open(image, O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    perror("open");
    return 2;
  }
  if (strcmp(mode, "memfd") == 0) {
    struct stat st;
    int copy = memfd_create("isa-image", MFD_CLOEXEC);
    if (copy < 0 || fstat(fd, &st) != 0 ||
        sendfile(copy, fd, NULL, (size_t)st.st_size) != st.st_size) {
      perror("memfd copy");
      return 2;
    }
    execveat_(copy, "", AT_EMPTY_PATH);
  } else if (strcmp(mode, "bad-flag") == 0) {
    execveat_(fd, "", AT_EMPTY_PATH | 0x40000000);
  } else if (strcmp(mode, "empty-path") == 0) {
    execveat_(fd, "", 0);
  } else if (strcmp(mode, "no-follow") == 0) {
    char link[4096];
    snprintf(link, sizeof link, "%s.link", image);
    unlink(link);
    if (symlink(image, link) != 0) {
      perror("symlink");
      return 2;
    }
    execveat_(AT_FDCWD, link, AT_SYMLINK_NOFOLLOW);
  } else if (strcmp(mode, "chroot") == 0) {
    char directory[4096];
    snprintf(directory, sizeof directory, "%s", image);
    if (chroot(dirname(directory)) != 0 || chdir("/") != 0) {
      perror("chroot");
      return 2;
    }
    execveat_(AT_FDCWD, "/isa-link", 0);
  } else {
    fprintf(stderr, "unknown mode %s\n", mode);
    return 2;
  }
  printf("errno=%d\n", errno);
  return 0;
}
