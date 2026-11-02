/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Inotify rename cookies in the events a read copies before it faults. Linux
// copies whole events in order and stores every byte up to the first one the
// guest cannot write, then fails with EFAULT (or, for a readv whose later
// iovec faults, returns the earlier iovecs' count). The events it copied are
// left in the buffer, with the cookie every other instance sees for their
// rename; that agreement is all the guest prints about rename cookies (Linux
// draws them from one host-wide counter, so their values differ from run to
// run; under Hermit, `--verify-strict` also compares them). Each case puts
// the end of the buffer at a different place: after one event and part of
// the next event's header, inside its cookie, inside its length, in a readv's
// second iovec, and after more than 64 KiB of events. The buffers end at
// read-only pages, which every Hermit backend can read; Linux faults there as
// at an inaccessible page. An IN_IGNORED event, whose cookie is
// 0, cut inside its cookie leaves the zeros Linux wrote. A readv whose second
// iovec overlaps its first but receives nothing leaves its first iovec's
// events as they were returned. A copy that runs on through a write-only page
// keeps a rename's two cookies equal. And a read that Linux refuses before it
// reads the queue (a count past the end of the address space, a bad later
// iovec, an iovec array the guest cannot read) leaves the buffer exactly as
// it was, events already there included, and the queue full.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <unistd.h>

// Every name here is two characters, which Linux pads to 16 bytes, so every
// event is 32 bytes.
#define EVENT 32
// Enough renames that their events fill more than 64 KiB.
#define MANY 1100

static char dir[] = "/tmp/inotify-faults-XXXXXX";
static long page;

static int watch_for(uint32_t mask) {
  int fd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
  if (fd < 0 || inotify_add_watch(fd, dir, mask) < 0) exit(2);
  return fd;
}

static int watch(void) { return watch_for(IN_MOVED_FROM | IN_MOVED_TO); }

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

// Reads that Linux refuses before it reads the queue of `fd`, which holds
// `queued` bytes, into a buffer that holds `stale` (earlier events, cookies
// included): a count that runs past the end of the address space, a second
// iovec there, and an iovec array the guest cannot read. Each must fail with
// EFAULT and leave the buffer and the queue as they were.
static void refused_before_copy(int fd, const char *stale, size_t queued, const char *kind,
                                int *all) {
  char buffer[4 * EVENT] __attribute__((aligned(8)));
  char what[160];
  memcpy(buffer, stale, sizeof buffer);
  // The raw call: the C library's declaration lets the compiler refuse a
  // count larger than the buffer, which is the point here.
  int ok = syscall(SYS_read, fd, buffer, (size_t)1 << 62) == -1 && errno == EFAULT;
  snprintf(what, sizeof what, "%s: a read whose count runs past the address space fails with EFAULT and leaves the buffer", kind);
  report(what, ok && memcmp(buffer, stale, sizeof buffer) == 0, all);

  struct iovec bad[2] = {{buffer, sizeof buffer}, {(void *)0xffff800000000000ull, EVENT}};
  ok = readv(fd, bad, 2) == -1 && errno == EFAULT;
  snprintf(what, sizeof what, "%s: a readv with a second iovec outside the address space fails with EFAULT and leaves the buffer", kind);
  report(what, ok && memcmp(buffer, stale, sizeof buffer) == 0, all);

  struct iovec *hidden = mmap(NULL, page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (hidden == MAP_FAILED) exit(4);
  hidden[0] = (struct iovec){buffer, sizeof buffer};
  if (mprotect(hidden, page, PROT_NONE) != 0) exit(4);
  ok = readv(fd, hidden, 1) == -1 && errno == EFAULT;
  snprintf(what, sizeof what, "%s: a readv whose iovec array the guest cannot read fails with EFAULT and leaves the buffer", kind);
  report(what, ok && memcmp(buffer, stale, sizeof buffer) == 0, all);

  char *all_events = malloc(queued + EVENT);
  snprintf(what, sizeof what, "%s: the queue still holds every event", kind);
  report(what, all_events && read(fd, all_events, queued + EVENT) == (ssize_t)queued, all);
  free(all_events);
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
  // them whole; the others read them into buffers that end early. O sees
  // only their IN_MOVED_FROM halves.
  int a = watch(), b = watch(), c = watch(), d = watch(), v = watch(), e = watch();
  int o = watch_for(IN_MOVED_FROM), x = watch();
  rename_in_dir("f0", "f1");
  rename_in_dir("f1", "f2");
  char whole[4 * EVENT] __attribute__((aligned(8)));
  int ok = read(a, whole, sizeof whole) == 4 * EVENT;
  uint32_t c1 = cookie_at(whole), c2 = cookie_at(whole + 2 * EVENT);
  report("the reference instance reads both renames",
         ok && c1 && c1 == cookie_at(whole + EVENT) && c2 && c2 == cookie_at(whole + 3 * EVENT) &&
             c1 != c2,
         &all);

  // 40 writable bytes: one event, then the next event's wd and mask.
  char *at = edge(2, PROT_READ, 40);
  report("a read that faults after one event fails with EFAULT", faults(b, at, 2 * EVENT), &all);
  report("the event it copied has the rename's cookie", cookie_at(at) == c1, &all);

  // 44 writable bytes: the next event's cookie whole, then its length cut.
  at = edge(2, PROT_READ, 44);
  report("a read that faults in the next event's length fails with EFAULT",
         faults(c, at, 2 * EVENT), &all);
  report("both cookies it copied whole are the rename's",
         cookie_at(at) == c1 && cookie_at(at + EVENT) == c1, &all);

  // 42 writable bytes before a read-only page: the next event's cookie cut.
  at = edge(2, PROT_READ, 42);
  report("a read that faults at a read-only page fails with EFAULT", faults(d, at, 2 * EVENT),
         &all);
  report("the event before the cut cookie has the rename's cookie", cookie_at(at) == c1, &all);

  // A readv whose first iovec faults fails with EFAULT, as a read does.
  at = edge(2, PROT_READ, 40);
  char spare[EVENT];
  struct iovec iov[2] = {{at, 2 * EVENT}, {spare, sizeof spare}};
  report("a readv whose first iovec faults fails with EFAULT",
         readv(v, iov, 2) == -1 && errno == EFAULT, &all);
  report("the event its first iovec holds has the rename's cookie", cookie_at(at) == c1, &all);

  // A readv whose second iovec faults after one event returns the first
  // iovec's event, and the event in the second iovec has the rename's
  // cookie: Linux reads each iovec on its own.
  at = edge(2, PROT_READ, 40);
  char first[EVENT] __attribute__((aligned(8)));
  struct iovec later[2] = {{first, EVENT}, {at, 2 * EVENT}};
  report("a readv whose second iovec faults returns the first iovec",
         readv(e, later, 2) == EVENT && cookie_at(first) == c1, &all);
  report("the event the faulting iovec holds has the rename's cookie", cookie_at(at) == c1, &all);

  // O holds exactly the two IN_MOVED_FROM events. A readv into {buf, 64} and
  // {buf + 8, 16} fills the first iovec and finds nothing more for the
  // second, so both events stay as returned.
  char both[2 * EVENT + 16] __attribute__((aligned(8)));
  struct iovec overlapping[2] = {{both, 2 * EVENT}, {both + 8, 16}};
  report("a readv whose overlapping second iovec receives nothing returns both events",
         readv(o, overlapping, 2) == 2 * EVENT && (mask_at(both) & IN_MOVED_FROM) &&
             (mask_at(both + EVENT) & IN_MOVED_FROM),
         &all);
  report("both have their renames' cookies",
         cookie_at(both) == c1 && cookie_at(both + EVENT) == c2, &all);

  // X holds both renames' four events, and the buffer the events A read.
  refused_before_copy(x, whole, sizeof whole, "nonblocking", &all);

  // An IN_IGNORED event, whose cookie is 0, read into a buffer that ends
  // inside its cookie: the bytes Linux wrote there are zeros.
  int g = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
  int w = g < 0 ? -1 : inotify_add_watch(g, dir, IN_ATTRIB);
  if (w < 0 || inotify_rm_watch(g, w) != 0) return 5;
  at = edge(2, PROT_READ, 10);
  report("a read that faults inside an IN_IGNORED event's cookie fails with EFAULT",
         faults(g, at, 16), &all);
  report("the cookie bytes it wrote are zeros",
         (mask_at(at) & IN_IGNORED) && at[8] == 0 && at[9] == 0, &all);

  // More than 64 KiB of events before the fault: the renames f2 -> f3 and
  // back, MANY times, read whole by A and into 18 pages, the last one
  // read-only, by M.
  int m = watch(), wo = watch();
  for (int i = 0; i < MANY; i++) rename_in_dir(i % 2 ? "f3" : "f2", i % 2 ? "f2" : "f3");
  size_t size = (size_t)2 * MANY * EVENT;
  char *reference = malloc(size);
  ok = reference && read(a, reference, size) == (ssize_t)size;
  at = edge(18, PROT_READ, 17 * page);
  ok = ok && faults(m, at, size);
  // The last event the copy wrote whole, and with it the last whole cookie.
  size_t last = ((size_t)17 * page / EVENT - 1) * EVENT;
  report("a read that faults after more than 64 KiB of events fails with EFAULT", ok, &all);
  report("its first and last whole events have the renames' cookies",
         ok && cookie_at(at) == cookie_at(reference) && cookie_at(at + last) == cookie_at(reference + last),
         &all);

  // 42 writable bytes, then a write-only page, then an inaccessible one: the
  // copy runs on through the write-only page and faults after 4,138 bytes.
  // The second event's cookie, split by the first page boundary, was copied
  // whole, as its rename's first event's was. The check compares the pair's
  // two cookies with each other only, which holds whether Hermit renumbered
  // them (ptrace reads the write-only page) or left them as Linux wrote them
  // (the in-process backends cannot read it, so they cannot tell where the
  // copy ended); what it rules out is a whole cookie taken for a cut one.
  at = edge(3, PROT_NONE, page + 42);
  if (mprotect(at + 42, page, PROT_WRITE) != 0) return 4;
  report("a read that faults after a write-only page fails with EFAULT",
         faults(wo, at, 42 + page + 64), &all);
  report("the rename split by the page boundary keeps one cookie in both events",
         (mask_at(at) & IN_MOVED_FROM) && (mask_at(at + EVENT) & IN_MOVED_TO) &&
             cookie_at(at) == cookie_at(at + EVENT),
         &all);

  snprintf(path, sizeof path, "%s/f2", dir);
  unlink(path);
  rmdir(dir);
  return all ? 0 : 9;
}
