/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Inotify rename cookies in the events a read copies before it faults at a
// page the guest cannot read or write: an inaccessible (PROT_NONE) guard
// page, as hardened allocators place after buffers, or no page at all. Linux
// copies whole events in order and stores every byte up to the first one the
// guest cannot write, then fails with EFAULT (or, for a readv whose later
// iovec faults, returns the earlier iovecs' count), leaving the events it
// copied in the buffer with the cookie every other instance sees for their
// rename (https://github.com/rrnewton/hermit/issues/4793).
//
// The guest checks that agreement and prints the cookies themselves. Linux
// draws them from one host-wide counter, so natively they are large and
// differ from run to run; under Hermit they are the run's virtual cookies,
// numbered from 1 in the order the run first reads them: the reference
// instance's two renames are 1 and 2, and the 70 renames of the last case
// are 3 onwards. A host cookie left in place shows as a large number that
// differs between the two runs of a verify pair. Where Linux wrote only part
// of a rename cookie before the fault, Hermit writes zeros, since no virtual
// cookie can be found for part of one; the guest prints those bytes too.
//
// Cases: a read that faults 40, 44 and 42 bytes into the events before a
// PROT_NONE page (40 is issue 4793's reproducer), 40 bytes before an unmapped
// page, a readv whose second iovec faults before a PROT_NONE page, and a read
// whose copy runs on through a write-only page and faults at a PROT_NONE page
// after it, where the second event's cookie is split by the first page
// boundary.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/mman.h>
#include <sys/uio.h>
#include <unistd.h>

// Every name here is two characters, which Linux pads to 16 bytes, so every
// event is 32 bytes.
#define EVENT 32
// Enough renames that their events run past a page: 140 events, 4,480 bytes.
#define MANY 70

static char dir[] = "/tmp/inotify-guard-page-faults-XXXXXX";
static long page;

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

static void report(const char *what, int ok, int *all) {
  printf("%s: %s\n", what, ok ? "yes" : "no");
  *all = *all && ok;
}

static uint32_t cookie_at(const char *p) {
  return ((const struct inotify_event *)p)->cookie;
}

static uint32_t mask_at(const char *p) {
  return ((const struct inotify_event *)p)->mask;
}

// Print the cookie of the event at `p`: under Hermit, a small virtual cookie.
static void show_cookie(const char *what, const char *p) {
  printf("%s: %u\n", what, (unsigned)cookie_at(p));
}

// Print the two bytes at `p`: under Hermit, zeros where they are what Linux
// wrote of a cut rename cookie.
static void show_cut(const char *what, const char *p) {
  printf("%s: %02x %02x\n", what, (unsigned char)p[0], (unsigned char)p[1]);
}

// `pages` pages, the last one protected with `last`, and the address
// `writable` bytes before it, filled with 0xa5.
static char *edge(int pages, int last, int writable) {
  char *map = mmap(NULL, pages * page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (map == MAP_FAILED || mprotect(map + (pages - 1) * page, page, last) != 0) exit(4);
  char *at = map + (pages - 1) * page - writable;
  memset(at, 0xa5, writable);
  return at;
}

// One read of `count` bytes from `fd` into `at` that faults: whether it
// failed with EFAULT.
static int faults(int fd, char *at, size_t count) {
  return read(fd, at, count) == -1 && errno == EFAULT;
}

int main(void) {
  int all = 1;
  page = sysconf(_SC_PAGESIZE);
  if (!mkdtemp(dir)) return 1;
  char path[64];
  snprintf(path, sizeof path, "%s/f0", dir);
  int fd = open(path, O_CREAT | O_WRONLY | O_CLOEXEC, 0644);
  if (fd < 0 || close(fd) != 0) return 1;

  // Six instances see the same two renames (f0 -> f1, f1 -> f2): A reads
  // them whole, and is read first; the others read them into buffers that
  // end early.
  int a = watch(), b = watch(), c = watch(), d = watch(), u = watch(), v = watch();
  rename_in_dir("f0", "f1");
  rename_in_dir("f1", "f2");
  char whole[4 * EVENT] __attribute__((aligned(8)));
  int ok = read(a, whole, sizeof whole) == 4 * EVENT;
  uint32_t c1 = cookie_at(whole), c2 = cookie_at(whole + 2 * EVENT);
  report("the reference instance reads both renames",
         ok && c1 && c1 == cookie_at(whole + EVENT) && c2 && c2 == cookie_at(whole + 3 * EVENT) &&
             c1 != c2,
         &all);
  show_cookie("the first rename's cookie", whole);
  show_cookie("the second rename's cookie", whole + 2 * EVENT);

  // 40 writable bytes before a PROT_NONE page: one event, then the next
  // event's wd and mask (issue 4793's reproducer).
  char *at = edge(2, PROT_NONE, 40);
  report("a read that faults 40 bytes before a PROT_NONE page fails with EFAULT",
         faults(b, at, 2 * EVENT), &all);
  report("the event it copied has the rename's cookie", cookie_at(at) == c1, &all);
  show_cookie("its cookie", at);

  // 44 bytes: the next event's cookie whole, then its length cut.
  at = edge(2, PROT_NONE, 44);
  report("a read that faults 44 bytes before a PROT_NONE page fails with EFAULT",
         faults(c, at, 2 * EVENT), &all);
  report("both cookies it copied whole are the rename's",
         cookie_at(at) == c1 && cookie_at(at + EVENT) == c1, &all);
  show_cookie("the first cookie", at);
  show_cookie("the second cookie", at + EVENT);

  // 42 bytes: the next event's cookie cut after two bytes.
  at = edge(2, PROT_NONE, 42);
  report("a read that faults 42 bytes before a PROT_NONE page fails with EFAULT",
         faults(d, at, 2 * EVENT), &all);
  report("the event before the cut cookie has the rename's cookie", cookie_at(at) == c1, &all);
  show_cookie("its cookie", at);
  show_cut("the cut cookie's bytes", at + EVENT + 8);

  // 40 bytes before a page that is not mapped at all.
  char *map = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (map == MAP_FAILED || munmap(map + page, page) != 0) return 4;
  at = map + page - 40;
  memset(at, 0xa5, 40);
  report("a read that faults 40 bytes before an unmapped page fails with EFAULT",
         faults(u, at, 2 * EVENT), &all);
  report("the event it copied has the rename's cookie", cookie_at(at) == c1, &all);
  show_cookie("its cookie", at);

  // A readv whose second iovec faults 40 bytes before a PROT_NONE page: it
  // returns the first iovec's event, and the event Linux copied into the
  // second iovec before the fault stays there.
  at = edge(2, PROT_NONE, 40);
  char first[EVENT] __attribute__((aligned(8)));
  struct iovec later[2] = {{first, EVENT}, {at, 2 * EVENT}};
  report("a readv whose second iovec faults before a PROT_NONE page returns the first iovec",
         readv(v, later, 2) == EVENT, &all);
  report("both iovecs' events have the rename's cookie",
         cookie_at(first) == c1 && cookie_at(at) == c1, &all);
  show_cookie("the first iovec's cookie", first);
  show_cookie("the faulting iovec's cookie", at);

  // 42 read-write bytes, then a write-only page, then a PROT_NONE page: the
  // copy runs on through the write-only page and faults after 4,138 bytes,
  // in the 130th event's cookie. The second event's cookie, split by the
  // first page boundary, was copied whole. (x86-64 cannot map a page
  // write-only for the CPU, so the guest reads it back directly.)
  int w = watch();
  for (int i = 0; i < MANY; i++) rename_in_dir(i % 2 ? "f3" : "f2", i % 2 ? "f2" : "f3");
  char reference[2 * MANY * EVENT] __attribute__((aligned(8)));
  ok = read(a, reference, sizeof reference) == (ssize_t)sizeof reference;
  at = edge(3, PROT_NONE, page + 42);
  if (mprotect(at + 42, page, PROT_WRITE) != 0) return 4;
  ok = ok && faults(w, at, 42 + page + 64);
  // The last event the copy wrote whole, and the one it cut in its cookie.
  size_t last = ((size_t)(42 + page) / EVENT - 1) * EVENT;
  report("a read that faults after a write-only page fails with EFAULT", ok, &all);
  report("the rename split by the page boundary has its cookie in both events",
         (mask_at(at) & IN_MOVED_FROM) && (mask_at(at + EVENT) & IN_MOVED_TO) &&
             cookie_at(at) == cookie_at(reference) && cookie_at(at + EVENT) == cookie_at(reference),
         &all);
  report("the last whole event has its rename's cookie",
         cookie_at(at + last) == cookie_at(reference + last), &all);
  show_cookie("the first event's cookie", at);
  show_cookie("the split event's cookie", at + EVENT);
  show_cookie("the last whole event's cookie", at + last);
  show_cut("the cut cookie's bytes", at + last + EVENT + 8);

  snprintf(path, sizeof path, "%s/f2", dir);
  unlink(path);
  rmdir(dir);
  return all ? 0 : 9;
}
