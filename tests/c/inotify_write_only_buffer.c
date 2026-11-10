/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// An inotify read into a write-only page. Linux copies events to user memory
// with copy_to_user, which needs only write access, so the read succeeds; the
// page is made readable afterwards to look at them. Two instances watch one
// directory while a file is renamed: one is read into an ordinary buffer, the
// other into the write-only page, and both must show the rename's pair with
// the same nonzero cookie (Linux draws it from one host-wide counter, so only
// the agreement is printed; under Hermit `--verify-strict` also compares the
// values).

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/mman.h>
#include <unistd.h>

// The pair's two events, each 32 bytes: a two-character name is padded to 16.
#define PAIR 64

// Whether `buffer` holds `length` bytes of exactly one rename pair, and if so
// its cookie in `cookie`.
static int pair(const char *buffer, ssize_t length, uint32_t *cookie) {
  const struct inotify_event *from = (const struct inotify_event *)buffer;
  const struct inotify_event *to = (const struct inotify_event *)(buffer + PAIR / 2);
  if (length != PAIR || from->len != 16 || to->len != 16) return 0;
  *cookie = from->cookie;
  return (from->mask & IN_MOVED_FROM) && (to->mask & IN_MOVED_TO) && !strcmp(from->name, "f0") &&
         !strcmp(to->name, "f1") && from->cookie != 0 && from->cookie == to->cookie;
}

int main(void) {
  char dir[] = "/tmp/inotify-write-only-XXXXXX";
  if (!mkdtemp(dir)) return 1;
  int readable = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
  int write_only = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
  if (readable < 0 || write_only < 0) return 2;
  if (inotify_add_watch(readable, dir, IN_MOVED_FROM | IN_MOVED_TO) < 0 ||
      inotify_add_watch(write_only, dir, IN_MOVED_FROM | IN_MOVED_TO) < 0) {
    return 2;
  }
  char first[64], second[64];
  snprintf(first, sizeof first, "%s/f0", dir);
  snprintf(second, sizeof second, "%s/f1", dir);
  int fd = open(first, O_CREAT | O_WRONLY | O_CLOEXEC, 0644);
  if (fd < 0 || close(fd) != 0 || rename(first, second) != 0) return 3;

  char buffer[4096] __attribute__((aligned(8)));
  ssize_t length = read(readable, buffer, sizeof buffer);
  uint32_t expected = 0;
  int ok = pair(buffer, length, &expected);
  printf("ordinary buffer: %zd bytes, one pair: %s\n", length, ok ? "yes" : "no");

  char *page = mmap(NULL, 4096, PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (page == MAP_FAILED) return 4;
  length = read(write_only, page, 4096);
  int saved = length < 0 ? errno : 0;
  if (mprotect(page, 4096, PROT_READ) != 0) return 5;
  uint32_t cookie = 0;
  int agrees = pair(page, length, &cookie) && cookie == expected;
  printf("write-only page: %zd bytes (errno %d), the same pair and cookie: %s\n", length, saved,
         agrees ? "yes" : "no");

  unlink(second);
  rmdir(dir);
  return ok && agrees ? 0 : 6;
}
