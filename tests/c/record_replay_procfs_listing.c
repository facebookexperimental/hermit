/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Which names a raw getdents64 of each spelling of the caller's own task
 * directories lists, and of an ordinary directory given as argv[1]. It
 * prints only whether each looked-for name is present, so a recording and
 * its replay print the same lines. argv[2] may name a symlink to the
 * caller's task directory and argv[3] a bind mount of one; a fifth
 * argument "fill" first takes every free descriptor slot but one.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

struct linux_dirent64 {
  unsigned long long d_ino;
  long long d_off;
  unsigned short d_reclen;
  unsigned char d_type;
  char d_name[];
};

static int listed(const char *path, const char *name) {
  static char buffer[65536];
  int fd = open(path, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  if (fd < 0) {
    return -errno;
  }
  int found = 0;
  for (;;) {
    long count = syscall(SYS_getdents64, fd, buffer, sizeof(buffer));
    if (count < 0) {
      int error = errno;
      close(fd);
      return -error;
    }
    if (count == 0) {
      break;
    }
    for (long offset = 0; offset < count;) {
      struct linux_dirent64 *entry =
          (struct linux_dirent64 *)(buffer + offset);
      if (strcmp(entry->d_name, name) == 0) {
        found = 1;
      }
      offset += entry->d_reclen;
    }
  }
  close(fd);
  return found;
}

static int report_task(const char *label, const char *path, int process) {
  int numa_maps = listed(path, "numa_maps");
  if (numa_maps < 0) {
    printf("%s error=%s\n", label, strerrorname_np(-numa_maps));
    return 1;
  }
  int stat = listed(path, "stat");
  if (stat < 0) {
    printf("%s error=%s\n", label, strerrorname_np(-stat));
    return 1;
  }
  int task = process ? listed(path, "task") : 1;
  if (task < 0) {
    printf("%s error=%s\n", label, strerrorname_np(-task));
    return 1;
  }
  printf("%s numa_maps=%d stat=%d task=%d\n", label, numa_maps, stat, task);
  return numa_maps != 0 || stat != 1 || task != 1;
}

int main(int argc, char **argv) {
  if (argc < 2 || argc > 5 || (argc == 5 && strcmp(argv[4], "fill") != 0)) {
    return 2;
  }
  char path[128];
  int failed = report_task("self", "/proc/self", 1);
  failed |= report_task("thread-self", "/proc/thread-self", 0);

  snprintf(path, sizeof(path), "/proc/%ld", (long)getpid());
  failed |= report_task("pid", path, 1);
  snprintf(
      path,
      sizeof(path),
      "/proc/%ld/task/%ld",
      (long)getpid(),
      (long)syscall(SYS_gettid));
  failed |= report_task("pid-thread", path, 0);

  int ordinary_numa = listed(argv[1], "numa_maps");
  int ordinary_other = listed(argv[1], "other");
  if (ordinary_numa < 0 || ordinary_other < 0) {
    printf("ordinary error\n");
    return 1;
  }
  printf(
      "ordinary numa_maps=%d other=%d\n", ordinary_numa, ordinary_other);
  failed |= ordinary_numa != 1 || ordinary_other != 1;

  // The same task directory reached through an ordinary symlink: the
  // listing is the object's, not the spelling's, so it omits the name
  // exactly as the direct spellings do.
  if (argc >= 3) {
    failed |= report_task("alias", argv[2], 1);
  }
  // And through a bind mount at an ordinary path, whose canonical link
  // names the bind, not the task: still the task directory's listing.
  // With "fill", first take every free descriptor slot and release
  // exactly one, so the bind open succeeds with no slot to spare: the
  // listing must not depend on auxiliary opens the tool cannot make
  // then either.
  if (argc >= 4) {
    static int held[65536];
    int held_count = 0;
    if (argc == 5) {
      int slot;
      while (
          held_count < 65536 &&
          (slot = open("/dev/null", O_RDONLY | O_CLOEXEC)) >= 0) {
        held[held_count++] = slot;
      }
      if (errno != EMFILE || held_count == 0) {
        printf("fill error=%s\n", strerrorname_np(errno));
        failed = 1;
      } else {
        close(held[--held_count]);
      }
    }
    failed |= report_task("bind", argv[3], 1);
    while (held_count > 0) {
      close(held[--held_count]);
    }
  }

  errno = 0;
  int fd = open("/proc/self/numa_maps", O_RDONLY | O_CLOEXEC);
  if (fd >= 0) {
    close(fd);
    printf("open numa_maps=ok\n");
    failed = 1;
  } else {
    printf("open numa_maps=%s\n", strerrorname_np(errno));
    failed |= errno != ENOENT;
  }
  return failed;
}
