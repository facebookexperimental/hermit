/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Directory sizes under Hermit (https://github.com/rrnewton/hermit/issues/4162).
//
// Usage: directory_size grow DIR | watch DIR N | proc | seek DIR |
//                       seek-path DIR | seek-stdin | chroot ROOT |
//                       fuse DIR CONTROL
//   grow   stat DIR, create 64 files in it, stat it again, and stat /proc;
//          print each st_size and DIR's st_blocks.
//   watch  stat DIR N times while something else may add and remove entries
//          in it; print the last st_size.
//   proc   fstat an open /proc directory and seek it to SEEK_END; print its
//          st_size and st_blocks and the seek's result.
//   seek   lseek a fresh descriptor of DIR to SEEK_END; print the result or
//          the errno's name.
//   seek-path   as seek, with an O_PATH descriptor of DIR.
//   seek-stdin  as seek, with standard input, which the caller opened.
//   chroot ROOT  chroot to ROOT, then stat /watched, create 64 files in it,
//          stat it again and seek a fresh descriptor of it to SEEK_END.
//   fuse DIR CONTROL  open DIR (tests/c/fuse_directory.c serves it), fstat
//          it and seek it to SEEK_END, write 2222 to CONTROL, which changes
//          the size the server reports, and fstat and seek the same
//          descriptor again. Each line starts with whether fstatfs names FUSE,
//          so a run that missed the mount cannot pass for one that saw it.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/vfs.h>
#include <unistd.h>

static void print_seek_end(int fd) {
  off_t end = lseek(fd, 0, SEEK_END);
  if (end < 0) {
    const char* name = errno == EINVAL ? "EINVAL" : errno == EBADF ? "EBADF" : strerror(errno);
    printf("seek end: %s\n", name);
  } else {
    printf("seek end: %lld\n", (long long)end);
  }
}

static struct stat must_stat(const char* path) {
  struct stat st;
  if (stat(path, &st) != 0) {
    perror(path);
    exit(1);
  }
  return st;
}

int main(int argc, char** argv) {
  if (argc >= 3 && !strcmp(argv[1], "grow")) {
    const char* dir = argv[2];
    struct stat before = must_stat(dir);
    char path[4096];
    for (int i = 0; i < 64; i++) {
      snprintf(path, sizeof(path), "%s/entry-with-a-long-name-%02d", dir, i);
      int fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
      if (fd < 0) {
        perror(path);
        return 1;
      }
      close(fd);
    }
    struct stat after = must_stat(dir);
    struct stat proc = must_stat("/proc");
    printf("dir %lld then %lld, blocks %lld; /proc %lld\n", (long long)before.st_size,
           (long long)after.st_size, (long long)after.st_blocks, (long long)proc.st_size);
    return 0;
  }
  if (argc >= 4 && !strcmp(argv[1], "watch")) {
    int n = atoi(argv[3]);
    struct stat st = must_stat(argv[2]);
    for (int i = 1; i < n; i++) {
      st = must_stat(argv[2]);
    }
    printf("watched %d times; size %lld\n", n, (long long)st.st_size);
    return 0;
  }
  if (argc >= 2 && !strcmp(argv[1], "proc")) {
    int fd = open("/proc", O_RDONLY | O_DIRECTORY);
    struct stat st;
    if (fd < 0 || fstat(fd, &st) != 0) {
      perror("/proc");
      return 1;
    }
    printf("proc %lld %lld; ", (long long)st.st_size, (long long)st.st_blocks);
    print_seek_end(fd);
    return 0;
  }
  if (argc >= 3 && (!strcmp(argv[1], "seek") || !strcmp(argv[1], "seek-path"))) {
    int flags = !strcmp(argv[1], "seek") ? O_RDONLY | O_DIRECTORY : O_PATH | O_DIRECTORY;
    int fd = open(argv[2], flags);
    if (fd < 0) {
      perror(argv[2]);
      return 1;
    }
    print_seek_end(fd);
    return 0;
  }
  if (argc >= 3 && !strcmp(argv[1], "chroot")) {
    if (chroot(argv[2]) != 0 || chdir("/") != 0) {
      perror(argv[2]);
      return 1;
    }
    struct stat before = must_stat("/watched");
    for (int i = 0; i < 64; i++) {
      char name[64];
      snprintf(name, sizeof(name), "/watched/entry-with-a-longer-name-%02d", i);
      int fd = open(name, O_CREAT | O_WRONLY, 0644);
      if (fd < 0) {
        perror(name);
        return 1;
      }
      close(fd);
    }
    struct stat after = must_stat("/watched");
    printf("chroot dir %lld then %lld, blocks %lld; ", (long long)before.st_size,
           (long long)after.st_size, (long long)after.st_blocks);
    int fd = open("/watched", O_RDONLY | O_DIRECTORY);
    if (fd < 0) {
      perror("/watched");
      return 1;
    }
    print_seek_end(fd);
    return 0;
  }
  if (argc >= 4 && !strcmp(argv[1], "fuse")) {
    int fd = open(argv[2], O_RDONLY | O_DIRECTORY);
    if (fd < 0) {
      perror(argv[2]);
      return 1;
    }
    for (int round = 0; round < 2; round++) {
      if (round == 1) {
        int control = open(argv[3], O_WRONLY | O_CREAT | O_TRUNC, 0644);
        if (control < 0 || write(control, "2222\n", 5) != 5 || close(control) != 0) {
          perror(argv[3]);
          return 1;
        }
      }
      struct stat st;
      struct statfs fs;
      if (fstat(fd, &st) != 0 || fstatfs(fd, &fs) != 0) {
        perror("fstat");
        return 1;
      }
      printf("%s fuse %d size %lld blocks %lld; ", round ? "after" : "before",
             fs.f_type == 0x65735546, (long long)st.st_size, (long long)st.st_blocks);
      print_seek_end(fd);
    }
    return 0;
  }
  if (argc >= 2 && !strcmp(argv[1], "seek-stdin")) {
    print_seek_end(STDIN_FILENO);
    return 0;
  }
  fprintf(
      stderr,
      "usage: %s grow DIR | watch DIR N | proc | seek DIR | seek-path DIR | seek-stdin | "
      "chroot ROOT | fuse DIR CONTROL\n",
      argv[0]);
  return 64;
}
