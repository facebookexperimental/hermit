/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Inotify rename cookies. Linux pairs each IN_MOVED_FROM with its IN_MOVED_TO
// by a cookie drawn from one host-wide counter, so the values depend on every
// other rename on the host. The guest prints only what Linux guarantees about
// them; under Hermit, `--verify-strict` also checks the values themselves
// through the logged read buffers. What Linux guarantees, checked here:
// - a pair shares one nonzero cookie, even when its halves come from two reads;
// - two renames have different cookies, even when separate reads return them;
// - every instance, in this process or another, sees a rename's cookie, in
//   whatever order each process observed other renames.
// Four renames are watched by instance A (read with read(2), then preadv2(2)
// at offset -1), instance B (read last, with readv(2) over three iovecs),
// instance C (created after the first two renames), instance D (created and
// read by a forked child, which sees rename 3 first) and instance H (created
// after rename 3, so the parent sees rename 4 before rename 3). First, a
// readv with overlapping iovecs must leave exactly the bytes Linux leaves.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

#define MAX_EVENTS 8
// Every name here is two characters, which Linux pads to 16 bytes, so every
// event is 32 bytes.
#define EVENT 32
#define BUFFER 4096

struct observed {
  uint32_t mask[MAX_EVENTS];
  uint32_t cookie[MAX_EVENTS];
  char name[MAX_EVENTS][16];
  int count;
};

static char dir[] = "/tmp/inotify-cookies-XXXXXX";

static int parse(const char *buffer, ssize_t length, struct observed *out) {
  out->count = 0;
  for (const char *p = buffer; p < buffer + length;) {
    const struct inotify_event *event = (const struct inotify_event *)p;
    if (out->count == MAX_EVENTS) return -1;
    out->mask[out->count] = event->mask;
    out->cookie[out->count] = event->cookie;
    snprintf(out->name[out->count], sizeof out->name[0], "%s", event->len ? event->name : "");
    out->count++;
    p += sizeof *event + event->len;
  }
  return 0;
}

static int watch(void) {
  int fd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
  if (fd < 0 || inotify_add_watch(fd, dir, IN_MOVED_FROM | IN_MOVED_TO) < 0) exit(2);
  return fd;
}

static void rename_in_dir(const char *from, const char *to) {
  char old[64], new[64];
  snprintf(old, sizeof old, "%s/%s", dir, from);
  snprintf(new, sizeof new, "%s/%s", dir, to);
  if (rename(old, new) != 0) exit(3);
}

// One read(2) of at most `size` bytes, parsed into `out`.
static void read_events(int fd, size_t size, struct observed *out) {
  char buffer[BUFFER] __attribute__((aligned(8)));
  ssize_t length = read(fd, buffer, size);
  if (length <= 0) {
    printf("read %zd errno %d\n", length, errno);
    exit(4);
  }
  if (parse(buffer, length, out) != 0) exit(5);
}

// Whether events `i` and `i + 1` of `o` are the pair `from` -> `to`, with one
// nonzero cookie.
static int pair_at(const struct observed *o, int i, const char *from, const char *to) {
  return o->count >= i + 2 && (o->mask[i] & IN_MOVED_FROM) && (o->mask[i + 1] & IN_MOVED_TO) &&
         !strcmp(o->name[i], from) && !strcmp(o->name[i + 1], to) && o->cookie[i] != 0 &&
         o->cookie[i] == o->cookie[i + 1];
}

// Whether `o` holds exactly the pair `from` -> `to`.
static int is_pair(const struct observed *o, const char *from, const char *to) {
  return o->count == 2 && pair_at(o, 0, from, to);
}

static void report(const char *what, int ok, int *all) {
  printf("%s: %s\n", what, ok ? "yes" : "no");
  *all = *all && ok;
}

// A readv with overlapping iovecs. Instance G watches the directory and the
// file `file`, then removes both watches, which queues two IN_IGNORED events
// of 16 bytes with cookie 0. With iovecs {buf, 16} and {buf + 8, 16}, Linux
// reads the first event into the first iovec and then the second into the
// second, over the first event's last 8 bytes. Every byte must be the one
// Linux leaves: nothing there is a cookie, the second event's watch descriptor
// included.
static void overlapping_readv(const char *file, int *all) {
  int g = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
  int w1 = g < 0 ? -1 : inotify_add_watch(g, dir, IN_MOVED_FROM);
  int w2 = g < 0 ? -1 : inotify_add_watch(g, file, IN_ATTRIB);
  if (w1 < 0 || w2 < 0 || w1 == w2 || inotify_rm_watch(g, w1) != 0 || inotify_rm_watch(g, w2) != 0) {
    exit(2);
  }
  unsigned char buf[64] __attribute__((aligned(8)));
  memset(buf, 0xa5, sizeof buf);
  struct iovec iov[2] = {{buf, 16}, {buf + 8, 16}};
  ssize_t length = readv(g, iov, 2);
  unsigned char want[64];
  memset(want, 0xa5, sizeof want);
  struct inotify_event first = {.wd = w1, .mask = IN_IGNORED};
  struct inotify_event second = {.wd = w2, .mask = IN_IGNORED};
  memcpy(want, &first, sizeof first);
  memcpy(want + 8, &second, sizeof second);
  report("an overlapping readv leaves exactly Linux's bytes",
         length == 32 && !memcmp(buf, want, sizeof buf), all);
  close(g);
}

int main(void) {
  int all = 1;
  if (!mkdtemp(dir)) return 1;
  char first[64];
  snprintf(first, sizeof first, "%s/f0", dir);
  int fd = open(first, O_CREAT | O_WRONLY | O_CLOEXEC, 0644);
  if (fd < 0 || close(fd) != 0) return 1;
  overlapping_readv(first, &all);
  int a = watch(), b = watch();

  // Rename 1, its halves read by two 32-byte reads: one event each.
  rename_in_dir("f0", "f1");
  struct observed from, to;
  read_events(a, EVENT, &from);
  read_events(a, EVENT, &to);
  uint32_t c1 = from.cookie[0];
  report("a pair split over two reads shares one cookie",
         from.count == 1 && to.count == 1 && (from.mask[0] & IN_MOVED_FROM) &&
             (to.mask[0] & IN_MOVED_TO) && c1 != 0 && c1 == to.cookie[0],
         &all);

  // Rename 2, read by a separate read.
  rename_in_dir("f1", "f2");
  struct observed second;
  read_events(a, BUFFER, &second);
  uint32_t c2 = second.cookie[0];
  report("a rename read separately has a cookie of its own", is_pair(&second, "f1", "f2") && c2 != c1,
         &all);

  // Instances C (here) and D (in a child) see only renames 3 and 4. The child
  // reads rename 3, before any other observation of it.
  int c = watch();
  int ready[2], go[2], answer[2];
  if (pipe(ready) || pipe(go) || pipe(answer)) return 1;
  // Nothing buffered for the child to print again if it exits early.
  fflush(stdout);
  pid_t child = fork();
  if (child < 0) return 1;
  if (child == 0) {
    int d = watch();
    char byte = 0;
    if (write(ready[1], &byte, 1) != 1 || read(go[0], &byte, 1) != 1) _exit(1);
    struct observed third;
    read_events(d, BUFFER, &third);
    uint32_t cookie = is_pair(&third, "f2", "f3") ? third.cookie[0] : 0;
    _exit(write(answer[1], &cookie, sizeof cookie) == sizeof cookie ? 0 : 1);
  }
  char byte = 0;
  if (read(ready[0], &byte, 1) != 1) return 1;
  // Rename 3.
  rename_in_dir("f2", "f3");
  if (write(go[1], &byte, 1) != 1) return 1;
  uint32_t child_cookie = 0;
  int status = 0;
  if (read(answer[0], &child_cookie, sizeof child_cookie) != sizeof child_cookie ||
      waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    return 6;
  }

  // The parent now observes rename 4, through instance H, before any read shows
  // it rename 3: after the fork the two processes have observed different
  // renames first. One run-wide numbering gives rename 3 the cookie the child
  // saw; numbering copied into each process at the fork would give rename 4,
  // the parent's next new rename, that same value, and rename 3 another.
  int h = watch();
  rename_in_dir("f3", "f4");
  struct observed fourth;
  read_events(h, BUFFER, &fourth);
  uint32_t c4 = fourth.cookie[0];
  report("an instance created after rename 3 sees only rename 4", is_pair(&fourth, "f3", "f4"),
         &all);

  // Instance A reads renames 3 and 4 with preadv2 at the current offset.
  char abuf[BUFFER] __attribute__((aligned(8)));
  struct iovec whole = {abuf, sizeof abuf};
  ssize_t alen = preadv2(a, &whole, 1, -1, 0);
  struct observed both;
  if (alen <= 0 || parse(abuf, alen, &both) != 0) {
    printf("preadv2 %zd errno %d\n", alen, errno);
    return 7;
  }
  uint32_t c3 = both.cookie[0];
  report("preadv2 at offset -1 reads renames 3 and 4",
         both.count == 4 && pair_at(&both, 0, "f2", "f3") && pair_at(&both, 2, "f3", "f4") &&
             both.cookie[2] == c4,
         &all);
  report("four renames have four cookies",
         c3 != c1 && c3 != c2 && c4 != c1 && c4 != c2 && c4 != c3, &all);
  report("rename 3, which the child saw first, has the child's cookie", child_cookie == c3, &all);

  struct observed later;
  read_events(c, BUFFER, &later);
  report("an instance created later agrees",
         later.count == 4 && pair_at(&later, 0, "f2", "f3") && pair_at(&later, 2, "f3", "f4") &&
             later.cookie[0] == c3 && later.cookie[2] == c4,
         &all);

  // Instance B, read last, holds all eight events. Three iovecs: inotify has
  // no read_iter, so readv reads each iovec with its own read, which takes
  // only whole events: one event fills the first iovec, two the second, and
  // the other five go to the third.
  char bbuf[BUFFER] __attribute__((aligned(8)));
  struct iovec iov[3] = {
      {bbuf, EVENT},
      {bbuf + EVENT, 2 * EVENT},
      {bbuf + 3 * EVENT, sizeof bbuf - 3 * EVENT},
  };
  ssize_t blen = readv(b, iov, 3);
  struct observed all_b;
  if (blen <= 0 || parse(bbuf, blen, &all_b) != 0) {
    printf("readv %zd errno %d\n", blen, errno);
    return 8;
  }
  const uint32_t expected[8] = {c1, c1, c2, c2, c3, c3, c4, c4};
  int agrees = all_b.count == 8;
  for (int i = 0; agrees && i < 8; i++) agrees = all_b.cookie[i] == expected[i];
  printf("readv events %d:", all_b.count);
  for (int i = 0; i < all_b.count; i++) {
    printf(" %s %s", all_b.mask[i] & IN_MOVED_FROM ? "from" : "to", all_b.name[i]);
  }
  printf("\n");
  report("an instance read last agrees on every rename", agrees, &all);

  char last[64];
  snprintf(last, sizeof last, "%s/f4", dir);
  unlink(last);
  rmdir(dir);
  return all ? 0 : 9;
}
