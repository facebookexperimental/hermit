/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Reads of a blocking inotify descriptor that wait for another thread's
// filesystem operation (https://github.com/rrnewton/hermit/issues/4768).
// Every line printed is what Linux guarantees, so it equals the native run's;
// the guest exits 9 if any check fails. Rename cookies come from a host-wide
// counter, so their values go to stderr only (under Hermit, --verify-strict
// also compares them, and the read buffers, between the two runs).
//
// - readv, read, preadv2: a sibling blocks reading an instance made by
//   inotify_init1(0); the main thread creates a file 20 ms later, and the
//   read returns that one 32-byte event. Under ptrace a blocking readv once
//   waited in the kernel while holding the scheduler's turn, so the file was
//   never created and the guest hung.
// - readv-boundary: an event is queued, and a sibling's blocking readv has
//   two iovecs, the first exactly that event's size. Linux's readv reads one
//   iovec at a time and each read waits while nothing is queued, so it
//   returns only after filling the second iovec with the event the main
//   thread creates 20 ms later: 64 bytes.
// - two-readers: two siblings block on instances watching different
//   directories; the main thread renames a file in each, and each reader
//   gets its own rename's two events, the pair sharing a cookie that differs
//   from the other rename's.
// - flags: what F_GETFL shows of an instance's blocking mode after
//   inotify_init, inotify_init1(0) and inotify_init1(IN_NONBLOCK), and after
//   F_SETFL and FIONBIO change it, on the descriptor and on a dup of it,
//   which shares its open file description; a read of a nonblocking view
//   with nothing queued fails with EAGAIN, and after the switch back to
//   blocking the main thread's read waits for a sibling's event.
// - readv-array-in-buffer, readv-array-overlay: Linux copies a readv's iovec
//   array when the call begins (import_iovec), so a readv whose array lies in
//   its own first buffer, or whose second iovec is where the first event's
//   header lands, still reads its second iovec as the call named it, after
//   the first event overwrote the array: 64 bytes, both events.
// - readv-array-rewritten-first, readv-array-rewritten-later: likewise when
//   the main thread rewrites the array while the readv waits, in its first
//   iovec or on into its second: the event goes to the buffer the call
//   named, and the new one stays untouched.
// - readv-array-rewritten-ahead: likewise when the main thread rewrites the
//   second iovec while the readv still waits in its first, then makes one
//   event and, once the readv waits on into its second iovec, another: the
//   second event goes to the second buffer the call named, 64 bytes.
// - zero, small-read, readv-zero-first: a read whose buffer no event fits (a
//   zero count, 8 bytes, or a readv whose first iovec is empty) waits for an
//   event, then fails with EINVAL, leaving it queued.
// - readv-small-later: a readv whose second iovec is shorter than an event
//   returns the first iovec's event once the next event comes, which stays
//   queued.
// - readv-three: a readv of three iovecs reads two queued events and waits on
//   into the third: 96 bytes.
// - preadv2-hipri, preadv2-nowait: preadv2 at the current offset with
//   RWF_HIPRI waits as readv does; with RWF_NOWAIT it fails with EOPNOTSUPP at
//   once (do_loop_readv_writev).
// - received: a blocking instance sent over a socketpair and received back
//   (one open file description) is blocking for its receiver too, whose read
//   waits for the main thread's event.
// - received-unmatched, received-outside: see the arguments below.
// Signals sent to a blocked reader are tests/c/inotify_blocking_signal.c's.
//
// Arguments: an optional --no-zero-read, which leaves out the zero case
// (KVM's executor refuses a zero-count inotify read by name), and an optional
// --no-received, which leaves out the received case (DBT gives a received
// descriptor the O_NONBLOCK Detcore set on its sender's open file
// description), in that order; then an optional case name, which runs only
// that case. Two cases run only when
// named, as Hermit refuses their reads by name; natively each read waits for
// the main thread's event:
// - received-unmatched: a blocking instance sent over a datagram socket to a
//   name, which Hermit cannot match to its sender's open file description.
// - received-outside NAME DIR: an instance from outside the run, blocking on
//   the host: the process that started this one serves it, watching DIR, on
//   the abstract unix socket NAME. A sibling's read of it waits for the file
//   the main thread makes in DIR.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

// Every name here is two characters, which Linux pads to 16 bytes, so every
// event is 32 bytes.
#define EVENT 32

static char root[] = "/tmp/inotify-blocking-read-XXXXXX";
static int all = 1;

static void fail(const char *operation) {
  fprintf(stderr, "%s: %s\n", operation, strerror(errno));
  exit(2);
}

static void report(const char *what, int ok) {
  printf("%s: %s\n", what, ok ? "yes" : "no");
  all = all && ok;
}

static void sleep_ms(long ms) {
  struct timespec pause = {ms / 1000, (ms % 1000) * 1000000};
  while (nanosleep(&pause, &pause) != 0 && errno == EINTR) {
  }
}

static void path_in(char *out, size_t size, const char *dir, const char *name) {
  snprintf(out, size, "%s/%s/%s", root, dir, name);
}

static void make_dir(const char *dir) {
  char path[96];
  snprintf(path, sizeof path, "%s/%s", root, dir);
  if (mkdir(path, 0700) != 0) fail("mkdir");
}

static void create_file(const char *dir, const char *name) {
  char path[96];
  path_in(path, sizeof path, dir, name);
  int fd = open(path, O_CREAT | O_WRONLY | O_CLOEXEC, 0600);
  if (fd < 0 || close(fd) != 0) fail("create");
}

static void rename_file(const char *dir, const char *from, const char *to) {
  char old[96], new[96];
  path_in(old, sizeof old, dir, from);
  path_in(new, sizeof new, dir, to);
  if (rename(old, new) != 0) fail("rename");
}

// An instance watching `dir` for `mask`, made by inotify_init1(flags).
static int watch(const char *dir, int flags, uint32_t mask) {
  char path[96];
  snprintf(path, sizeof path, "%s/%s", root, dir);
  int fd = inotify_init1(flags | IN_CLOEXEC);
  if (fd < 0) fail("inotify_init1");
  if (inotify_add_watch(fd, path, mask) < 0) fail("inotify_add_watch");
  return fd;
}

static int nonblocking(int fd) {
  int flags = fcntl(fd, F_GETFL);
  if (flags < 0) fail("F_GETFL");
  return (flags & O_NONBLOCK) != 0;
}

static const struct inotify_event *event_at(const char *buffer, int index) {
  return (const struct inotify_event *)(buffer + index * EVENT);
}

static int is_event(const char *buffer, int index, uint32_t mask, const char *name) {
  const struct inotify_event *event = event_at(buffer, index);
  return event->mask == mask && event->len == 16 && strcmp(event->name, name) == 0;
}

// One blocked sibling: reads `fd` with `how` into its buffer.
enum how { READ, READV, PREADV2, PREADV2_HIPRI };

struct blocked_read {
  int fd;
  enum how how;
  char buffer[4 * EVENT] __attribute__((aligned(8)));
  ssize_t got;
  int error;
};

static ssize_t read_once(int fd, enum how how, char *buffer, size_t size) {
  struct iovec iov = {buffer, size};
  switch (how) {
    case READ:
      return read(fd, buffer, size);
    case READV:
      return readv(fd, &iov, 1);
    case PREADV2:
      return preadv2(fd, &iov, 1, -1, 0);
    case PREADV2_HIPRI:
      return preadv2(fd, &iov, 1, -1, RWF_HIPRI);
  }
  return -1;
}

static void *blocked_reader(void *argument) {
  struct blocked_read *r = argument;
  r->got = read_once(r->fd, r->how, r->buffer, sizeof r->buffer);
  r->error = r->got < 0 ? errno : 0;
  return NULL;
}

// A sibling blocks reading a blocking instance with `how`; the main thread
// creates a file 20 ms later, and the read returns its one event.
static void sibling_case(const char *label, enum how how, const char *dir, const char *name) {
  make_dir(dir);
  struct blocked_read r = {.fd = watch(dir, 0, IN_CREATE), .how = how};
  pthread_t thread;
  if (pthread_create(&thread, NULL, blocked_reader, &r) != 0) fail("pthread_create");
  sleep_ms(20);
  create_file(dir, name);
  if (pthread_join(thread, NULL) != 0) fail("pthread_join");
  char what[128];
  snprintf(what, sizeof what, "%s: a sibling's blocking %s returns the event the main thread made",
           label, label);
  printf("%s read %zd bytes (%s), first event mask 0x%x name %s\n", label, r.got,
         r.got < 0 ? strerrorname_np(r.error) : "ok", r.got >= EVENT ? event_at(r.buffer, 0)->mask : 0,
         r.got >= EVENT ? event_at(r.buffer, 0)->name : "-");
  report(what, r.got == EVENT && is_event(r.buffer, 0, IN_CREATE, name));
  close(r.fd);
}

// A queued event exactly fills a blocking readv's first iovec, so the readv
// waits on into its second one, for the event the main thread makes 20 ms
// later.
struct boundary_read {
  int fd;
  char first[EVENT] __attribute__((aligned(8)));
  char second[EVENT] __attribute__((aligned(8)));
  ssize_t got;
  int error;
};

static void *boundary_reader(void *argument) {
  struct boundary_read *r = argument;
  struct iovec iov[2] = {{r->first, sizeof r->first}, {r->second, sizeof r->second}};
  r->got = readv(r->fd, iov, 2);
  r->error = r->got < 0 ? errno : 0;
  return NULL;
}

static void boundary_case(void) {
  make_dir("b");
  struct boundary_read r = {.fd = watch("b", 0, IN_CREATE)};
  create_file("b", "b1");
  pthread_t thread;
  if (pthread_create(&thread, NULL, boundary_reader, &r) != 0) fail("pthread_create");
  sleep_ms(20);
  create_file("b", "b2");
  if (pthread_join(thread, NULL) != 0) fail("pthread_join");
  printf("readv-boundary read %zd bytes (%s), events %s and %s\n", r.got,
         r.got < 0 ? strerrorname_np(r.error) : "ok",
         r.got >= EVENT ? ((const struct inotify_event *)r.first)->name : "-",
         r.got >= 2 * EVENT ? ((const struct inotify_event *)r.second)->name : "-");
  report("readv-boundary: a blocking readv whose first iovec an event fills waits on into its second",
         r.got == 2 * EVENT && is_event(r.first, 0, IN_CREATE, "b1") &&
             is_event(r.second, 0, IN_CREATE, "b2"));
  close(r.fd);
}

// A sibling that reads until it holds both events of one rename.
struct rename_reader {
  int fd;
  char buffer[2 * EVENT] __attribute__((aligned(8)));
  ssize_t got;
  int error;
};

static void *rename_reader(void *argument) {
  struct rename_reader *r = argument;
  r->got = 0;
  while (r->got < 2 * EVENT) {
    ssize_t n = read(r->fd, r->buffer + r->got, sizeof r->buffer - (size_t)r->got);
    if (n <= 0) {
      r->error = n < 0 ? errno : 0;
      return NULL;
    }
    r->got += n;
  }
  return NULL;
}

static int whole_rename(const struct rename_reader *r, const char *from, const char *to) {
  return r->got == 2 * EVENT && is_event(r->buffer, 0, IN_MOVED_FROM, from) &&
         is_event(r->buffer, 1, IN_MOVED_TO, to) && event_at(r->buffer, 0)->cookie != 0 &&
         event_at(r->buffer, 0)->cookie == event_at(r->buffer, 1)->cookie;
}

static void two_readers_case(void) {
  make_dir("p");
  make_dir("q");
  create_file("p", "p0");
  create_file("q", "q0");
  struct rename_reader p = {.fd = watch("p", 0, IN_MOVED_FROM | IN_MOVED_TO)};
  struct rename_reader q = {.fd = watch("q", 0, IN_MOVED_FROM | IN_MOVED_TO)};
  pthread_t tp, tq;
  if (pthread_create(&tp, NULL, rename_reader, &p) != 0 ||
      pthread_create(&tq, NULL, rename_reader, &q) != 0) {
    fail("pthread_create");
  }
  sleep_ms(20);
  rename_file("p", "p0", "p1");
  rename_file("q", "q0", "q1");
  if (pthread_join(tp, NULL) != 0 || pthread_join(tq, NULL) != 0) fail("pthread_join");
  printf("two readers read %zd and %zd bytes\n", p.got, q.got);
  report("two readers: each gets its own rename whole, the pair sharing a nonzero cookie",
         whole_rename(&p, "p0", "p1") && whole_rename(&q, "q0", "q1"));
  report("two readers: the two renames' cookies differ",
         p.got == 2 * EVENT && q.got == 2 * EVENT &&
             event_at(p.buffer, 0)->cookie != event_at(q.buffer, 0)->cookie);
  // Host-wide values: stderr only.
  fprintf(stderr, "cookies: p %u q %u\n", p.got >= EVENT ? event_at(p.buffer, 0)->cookie : 0,
          q.got >= EVENT ? event_at(q.buffer, 0)->cookie : 0);
  close(p.fd);
  close(q.fd);
}

static int fionbio(int fd, int value) { return ioctl(fd, FIONBIO, &value); }

// A read of an instance whose view is nonblocking, with nothing queued.
static int eagain(int fd) {
  char buffer[EVENT] __attribute__((aligned(8)));
  return read(fd, buffer, sizeof buffer) == -1 && errno == EAGAIN;
}

struct delayed_create {
  const char *dir;
  const char *name;
};

static void *create_later(void *argument) {
  struct delayed_create *c = argument;
  sleep_ms(20);
  create_file(c->dir, c->name);
  return NULL;
}

static void flags_case(void) {
  make_dir("f");
  int plain = (int)syscall(SYS_inotify_init);
  if (plain < 0) fail("inotify_init");
  report("inotify_init: F_GETFL shows a blocking descriptor", !nonblocking(plain));
  close(plain);

  int x = watch("f", 0, IN_CREATE);
  report("inotify_init1(0): F_GETFL shows a blocking descriptor", !nonblocking(x));
  if (fcntl(x, F_SETFL, O_NONBLOCK) != 0) fail("F_SETFL");
  report("F_SETFL O_NONBLOCK: F_GETFL shows a nonblocking descriptor", nonblocking(x));
  report("F_SETFL O_NONBLOCK: a read with nothing queued fails with EAGAIN", eagain(x));
  if (fcntl(x, F_SETFL, 0) != 0) fail("F_SETFL");
  report("F_SETFL 0: F_GETFL shows a blocking descriptor", !nonblocking(x));
  if (fionbio(x, 1) != 0) fail("FIONBIO");
  report("FIONBIO 1: F_GETFL shows a nonblocking descriptor", nonblocking(x));
  report("FIONBIO 1: a read with nothing queued fails with EAGAIN", eagain(x));
  if (fionbio(x, 0) != 0) fail("FIONBIO");
  report("FIONBIO 0: F_GETFL shows a blocking descriptor", !nonblocking(x));

  int alias = dup(x);
  if (alias < 0) fail("dup");
  if (fcntl(alias, F_SETFL, O_NONBLOCK) != 0) fail("F_SETFL");
  report("F_SETFL O_NONBLOCK on a dup: the original shows a nonblocking descriptor",
         nonblocking(x));
  if (fionbio(alias, 0) != 0) fail("FIONBIO");
  report("FIONBIO 0 on the dup: the original shows a blocking descriptor", !nonblocking(x));
  close(alias);

  // Blocking again: the main thread's read waits for a sibling's event.
  struct delayed_create c = {"f", "f1"};
  pthread_t thread;
  if (pthread_create(&thread, NULL, create_later, &c) != 0) fail("pthread_create");
  char buffer[2 * EVENT] __attribute__((aligned(8)));
  ssize_t got = read(x, buffer, sizeof buffer);
  if (pthread_join(thread, NULL) != 0) fail("pthread_join");
  report("blocking again: the main thread's read waits for the sibling's event",
         got == EVENT && is_event(buffer, 0, IN_CREATE, "f1"));
  close(x);

  int y = watch("f", IN_NONBLOCK, IN_CREATE);
  report("inotify_init1(IN_NONBLOCK): F_GETFL shows a nonblocking descriptor", nonblocking(y));
  report("inotify_init1(IN_NONBLOCK): a read with nothing queued fails with EAGAIN", eagain(y));
  if (fionbio(y, 0) != 0) fail("FIONBIO");
  report("FIONBIO 0 after IN_NONBLOCK: F_GETFL shows a blocking descriptor", !nonblocking(y));
  close(y);
}


// A sibling's readv of `count` iovecs at `iov`, which the main thread may
// overwrite or rewrite while it waits.
struct array_read {
  int fd;
  struct iovec *iov;
  int count;
  ssize_t got;
  int error;
};

static void *array_reader(void *argument) {
  struct array_read *r = argument;
  r->got = readv(r->fd, r->iov, r->count);
  r->error = r->got < 0 ? errno : 0;
  return NULL;
}

static const char *name_or_dash(const char *buffer, ssize_t got, ssize_t at_least) {
  return got >= at_least ? ((const struct inotify_event *)buffer)->name : "-";
}

static int untouched(const char *buffer, size_t size) {
  for (size_t i = 0; i < size; i++) {
    if (buffer[i] != 0) return 0;
  }
  return 1;
}

// The array lies in the readv's own first buffer (`overlay` 0), or its
// second iovec is where the first event's header lands (`overlay` 16): the
// first event overwrites the array before the readv reads its second iovec.
static char overlap_area[16 + EVENT] __attribute__((aligned(16)));
static char overlap_second[EVENT] __attribute__((aligned(16)));

static void array_overlap_case(const char *label, size_t overlay, const char *dir) {
  memset(overlap_area, 0, sizeof overlap_area);
  memset(overlap_second, 0, sizeof overlap_second);
  char *first = overlap_area + overlay;
  struct iovec *iov = (struct iovec *)overlap_area;
  iov[0] = (struct iovec){first, EVENT};
  iov[1] = (struct iovec){overlap_second, EVENT};
  make_dir(dir);
  struct array_read r = {.fd = watch(dir, 0, IN_CREATE), .iov = iov, .count = 2};
  char one[3] = {dir[0], '1', 0}, two[3] = {dir[0], '2', 0};
  create_file(dir, one);
  pthread_t thread;
  if (pthread_create(&thread, NULL, array_reader, &r) != 0) fail("pthread_create");
  sleep_ms(20);
  create_file(dir, two);
  if (pthread_join(thread, NULL) != 0) fail("pthread_join");
  printf("%s read %zd bytes (%s), events %s and %s\n", label, r.got,
         r.got < 0 ? strerrorname_np(r.error) : "ok", name_or_dash(first, r.got, EVENT),
         name_or_dash(overlap_second, r.got, 2 * EVENT));
  char what[160];
  snprintf(what, sizeof what,
           "%s: the readv reads its second iovec as the call named it, after the first event "
           "overwrote the array",
           label);
  report(what, r.got == 2 * EVENT && is_event(first, 0, IN_CREATE, one) &&
                   is_event(overlap_second, 0, IN_CREATE, two));
  close(r.fd);
}

// The main thread rewrites iovec `which` of a waiting readv to name a third
// buffer, then makes the event the readv waits for there.
static char rewrite_buffers[3][EVENT] __attribute__((aligned(16)));
static struct iovec rewrite_iov[2];

static void array_rewritten_case(const char *label, int which, const char *dir) {
  memset(rewrite_buffers, 0, sizeof rewrite_buffers);
  rewrite_iov[0] = (struct iovec){rewrite_buffers[0], EVENT};
  rewrite_iov[1] = (struct iovec){rewrite_buffers[1], EVENT};
  make_dir(dir);
  struct array_read r = {.fd = watch(dir, 0, IN_CREATE), .iov = rewrite_iov, .count = which + 1};
  char one[3] = {dir[0], '1', 0}, two[3] = {dir[0], '2', 0};
  // Waiting in its second iovec: an event first fills its first one.
  if (which == 1) create_file(dir, one);
  pthread_t thread;
  if (pthread_create(&thread, NULL, array_reader, &r) != 0) fail("pthread_create");
  sleep_ms(20);
  rewrite_iov[which] = (struct iovec){rewrite_buffers[2], EVENT};
  create_file(dir, which == 1 ? two : one);
  if (pthread_join(thread, NULL) != 0) fail("pthread_join");
  printf("%s read %zd bytes (%s), event in the buffer the call named: %s, new buffer %s\n", label,
         r.got, r.got < 0 ? strerrorname_np(r.error) : "ok",
         name_or_dash(rewrite_buffers[which], r.got, (which + 1) * EVENT),
         untouched(rewrite_buffers[2], EVENT) ? "untouched" : "written");
  char what[160];
  snprintf(what, sizeof what,
           "%s: the event goes to the buffer the call named, not the one the array names later",
           label);
  report(what, r.got == (which + 1) * EVENT &&
                   is_event(rewrite_buffers[which], 0, IN_CREATE, which == 1 ? two : one) &&
                   untouched(rewrite_buffers[2], EVENT));
  close(r.fd);
}

// The main thread rewrites the second iovec of a readv that still waits in
// its first, then makes the event that fills the first and, 20 ms later, the
// one the readv waits for in its second.
static void array_rewritten_ahead_case(const char *label, const char *dir) {
  memset(rewrite_buffers, 0, sizeof rewrite_buffers);
  rewrite_iov[0] = (struct iovec){rewrite_buffers[0], EVENT};
  rewrite_iov[1] = (struct iovec){rewrite_buffers[1], EVENT};
  make_dir(dir);
  struct array_read r = {.fd = watch(dir, 0, IN_CREATE), .iov = rewrite_iov, .count = 2};
  char one[3] = {dir[0], '1', 0}, two[3] = {dir[0], '2', 0};
  pthread_t thread;
  if (pthread_create(&thread, NULL, array_reader, &r) != 0) fail("pthread_create");
  sleep_ms(20);
  rewrite_iov[1] = (struct iovec){rewrite_buffers[2], EVENT};
  create_file(dir, one);
  sleep_ms(20);
  create_file(dir, two);
  if (pthread_join(thread, NULL) != 0) fail("pthread_join");
  printf("%s read %zd bytes (%s), events %s and %s, new buffer %s\n", label, r.got,
         r.got < 0 ? strerrorname_np(r.error) : "ok", name_or_dash(rewrite_buffers[0], r.got, EVENT),
         name_or_dash(rewrite_buffers[1], r.got, 2 * EVENT),
         untouched(rewrite_buffers[2], EVENT) ? "untouched" : "written");
  char what[160];
  snprintf(what, sizeof what,
           "%s: the second event goes to the buffer the call named, not the one the array names "
           "later",
           label);
  report(what, r.got == 2 * EVENT && is_event(rewrite_buffers[0], 0, IN_CREATE, one) &&
                   is_event(rewrite_buffers[1], 0, IN_CREATE, two) &&
                   untouched(rewrite_buffers[2], EVENT));
  close(r.fd);
}

static int queued_bytes(int fd) {
  int queued = -1;
  if (ioctl(fd, FIONREAD, &queued) != 0) fail("FIONREAD");
  return queued;
}

// A sibling's read whose buffer no event fits: `count` bytes, or a readv of
// an empty iovec and then a 32-byte one (`count` -1).
struct unfit_read {
  int fd;
  int count;
  char buffer[EVENT] __attribute__((aligned(8)));
  ssize_t got;
  int error;
};

static void *unfit_reader(void *argument) {
  struct unfit_read *r = argument;
  if (r->count < 0) {
    struct iovec iov[2] = {{r->buffer, 0}, {r->buffer, EVENT}};
    r->got = readv(r->fd, iov, 2);
  } else {
    r->got = read(r->fd, r->buffer, (size_t)r->count);
  }
  r->error = r->got < 0 ? errno : 0;
  return NULL;
}

static void unfit_case(const char *label, int count, const char *dir) {
  make_dir(dir);
  struct unfit_read r = {.fd = watch(dir, 0, IN_CREATE), .count = count};
  pthread_t thread;
  if (pthread_create(&thread, NULL, unfit_reader, &r) != 0) fail("pthread_create");
  sleep_ms(20);
  char one[3] = {dir[0], '1', 0};
  create_file(dir, one);
  if (pthread_join(thread, NULL) != 0) fail("pthread_join");
  int queued = queued_bytes(r.fd);
  printf("%s read %zd (%s), queued after: %d\n", label, r.got,
         r.got < 0 ? strerrorname_np(r.error) : "ok", queued);
  char what[160];
  snprintf(what, sizeof what,
           "%s: the read waits for an event, then fails with EINVAL, leaving it queued", label);
  report(what, r.got == -1 && r.error == EINVAL && queued == EVENT);
  close(r.fd);
}

// A queued event fills the first iovec; the second is shorter than an event.
static void small_later_case(void) {
  make_dir("s");
  static char first[EVENT] __attribute__((aligned(8)));
  static char second[16] __attribute__((aligned(8)));
  struct iovec iov[2] = {{first, EVENT}, {second, sizeof second}};
  struct array_read r = {.fd = watch("s", 0, IN_CREATE), .iov = iov, .count = 2};
  create_file("s", "s1");
  pthread_t thread;
  if (pthread_create(&thread, NULL, array_reader, &r) != 0) fail("pthread_create");
  sleep_ms(20);
  create_file("s", "s2");
  if (pthread_join(thread, NULL) != 0) fail("pthread_join");
  int queued = queued_bytes(r.fd);
  printf("readv-small-later read %zd bytes (%s), event %s, queued after: %d\n", r.got,
         r.got < 0 ? strerrorname_np(r.error) : "ok", name_or_dash(first, r.got, EVENT), queued);
  report("readv-small-later: the readv returns its first iovec's event once the next comes, "
         "which stays queued",
         r.got == EVENT && is_event(first, 0, IN_CREATE, "s1") && queued == EVENT);
  close(r.fd);
}

// Two queued events fill the first two of three iovecs; the third waits.
static void three_case(void) {
  make_dir("t");
  static char buffers[3][EVENT] __attribute__((aligned(8)));
  struct iovec iov[3] = {
      {buffers[0], EVENT}, {buffers[1], EVENT}, {buffers[2], EVENT}};
  struct array_read r = {.fd = watch("t", 0, IN_CREATE), .iov = iov, .count = 3};
  create_file("t", "t1");
  create_file("t", "t2");
  pthread_t thread;
  if (pthread_create(&thread, NULL, array_reader, &r) != 0) fail("pthread_create");
  sleep_ms(20);
  create_file("t", "t3");
  if (pthread_join(thread, NULL) != 0) fail("pthread_join");
  printf("readv-three read %zd bytes (%s), events %s %s %s\n", r.got,
         r.got < 0 ? strerrorname_np(r.error) : "ok", name_or_dash(buffers[0], r.got, EVENT),
         name_or_dash(buffers[1], r.got, 2 * EVENT), name_or_dash(buffers[2], r.got, 3 * EVENT));
  report("readv-three: a readv of three iovecs reads two queued events and waits on into the "
         "third",
         r.got == 3 * EVENT && is_event(buffers[0], 0, IN_CREATE, "t1") &&
             is_event(buffers[1], 0, IN_CREATE, "t2") && is_event(buffers[2], 0, IN_CREATE, "t3"));
  close(r.fd);
}

static void nowait_case(void) {
  make_dir("n");
  int fd = watch("n", 0, IN_CREATE);
  char buffer[EVENT] __attribute__((aligned(8)));
  struct iovec iov = {buffer, sizeof buffer};
  ssize_t got = preadv2(fd, &iov, 1, -1, RWF_NOWAIT);
  int error = got < 0 ? errno : 0;
  printf("preadv2-nowait read %zd (%s)\n", got, got < 0 ? strerrorname_np(error) : "ok");
  report("preadv2-nowait: RWF_NOWAIT fails with EOPNOTSUPP at once",
         got == -1 && error == EOPNOTSUPP);
  close(fd);
}

static void send_fd(int socket, int fd, const struct sockaddr_un *to, socklen_t to_length) {
  char byte = 0;
  struct iovec data = {&byte, 1};
  union {
    struct cmsghdr header;
    char space[CMSG_SPACE(sizeof(int))];
  } control;
  memset(&control, 0, sizeof control);
  struct msghdr message = {.msg_name = (void *)to,
                           .msg_namelen = to_length,
                           .msg_iov = &data,
                           .msg_iovlen = 1,
                           .msg_control = &control,
                           .msg_controllen = sizeof control.space};
  struct cmsghdr *header = CMSG_FIRSTHDR(&message);
  header->cmsg_level = SOL_SOCKET;
  header->cmsg_type = SCM_RIGHTS;
  header->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(header), &fd, sizeof fd);
  if (sendmsg(socket, &message, 0) != 1) fail("sendmsg");
}

static int receive_fd(int socket) {
  char byte;
  struct iovec data = {&byte, 1};
  union {
    struct cmsghdr header;
    char space[CMSG_SPACE(sizeof(int))];
  } control;
  memset(&control, 0, sizeof control);
  struct msghdr message = {
      .msg_iov = &data, .msg_iovlen = 1, .msg_control = &control, .msg_controllen = sizeof control};
  if (recvmsg(socket, &message, MSG_CMSG_CLOEXEC) != 1) fail("recvmsg");
  struct cmsghdr *header = CMSG_FIRSTHDR(&message);
  if (header == NULL || header->cmsg_type != SCM_RIGHTS) fail("SCM_RIGHTS");
  int fd;
  memcpy(&fd, CMSG_DATA(header), sizeof fd);
  return fd;
}

// A blocking instance travels over a socket and is read by a sibling while
// the main thread makes its event 20 ms later: over a socketpair
// (`unmatched` 0), or over a datagram socket to a name (1).
static void received_case(const char *label, int unmatched, const char *dir) {
  make_dir(dir);
  int original = watch(dir, 0, IN_CREATE);
  int received;
  if (unmatched) {
    struct sockaddr_un name = {.sun_family = AF_UNIX};
    // An abstract name: a NUL, then text.
    int length = snprintf(name.sun_path + 1, sizeof name.sun_path - 1,
                          "inotify-blocking-read-%d", (int)getpid());
    socklen_t name_length = (socklen_t)(offsetof(struct sockaddr_un, sun_path) + 1 + length);
    int receiver = socket(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    int sender = socket(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    if (receiver < 0 || sender < 0) fail("socket");
    if (bind(receiver, (struct sockaddr *)&name, name_length) != 0) fail("bind");
    send_fd(sender, original, &name, name_length);
    received = receive_fd(receiver);
    close(sender);
    close(receiver);
  } else {
    int pair[2];
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair) != 0) fail("socketpair");
    send_fd(pair[0], original, NULL, 0);
    received = receive_fd(pair[1]);
    close(pair[0]);
    close(pair[1]);
  }
  close(original);
  struct blocked_read r = {.fd = received, .how = READ};
  if (!unmatched) {
    char what[160];
    snprintf(what, sizeof what, "%s: F_GETFL shows the received descriptor blocking", label);
    report(what, !nonblocking(received));
  }
  pthread_t thread;
  if (pthread_create(&thread, NULL, blocked_reader, &r) != 0) fail("pthread_create");
  sleep_ms(20);
  char one[3] = {dir[0], '1', 0};
  create_file(dir, one);
  if (pthread_join(thread, NULL) != 0) fail("pthread_join");
  printf("%s read %zd bytes (%s), event %s\n", label, r.got,
         r.got < 0 ? strerrorname_np(r.error) : "ok", name_or_dash(r.buffer, r.got, EVENT));
  char what[160];
  snprintf(what, sizeof what,
           "%s: a blocking read of a received instance waits for the main thread's event", label);
  report(what, r.got == EVENT && is_event(r.buffer, 0, IN_CREATE, one));
  close(received);
}

// An instance from outside the run, served on the abstract unix socket
// `name` and watching `dir` (see the top of this file).
static void received_outside_case(const char *name, const char *dir) {
  int s = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
  if (s < 0) fail("socket");
  struct sockaddr_un address = {.sun_family = AF_UNIX};
  // An abstract name: a NUL, then text.
  int length = snprintf(address.sun_path + 1, sizeof address.sun_path - 1, "%s", name);
  socklen_t address_length = (socklen_t)(offsetof(struct sockaddr_un, sun_path) + 1 + length);
  if (connect(s, (struct sockaddr *)&address, address_length) != 0) fail("connect");
  int received = receive_fd(s);
  close(s);
  report("received-outside: F_GETFL shows the received descriptor blocking", !nonblocking(received));
  struct blocked_read r = {.fd = received, .how = READ};
  pthread_t thread;
  if (pthread_create(&thread, NULL, blocked_reader, &r) != 0) fail("pthread_create");
  sleep_ms(20);
  char path[512];
  snprintf(path, sizeof path, "%s/o1", dir);
  int fd = open(path, O_CREAT | O_WRONLY | O_CLOEXEC, 0600);
  if (fd < 0 || close(fd) != 0) fail("create");
  if (pthread_join(thread, NULL) != 0) fail("pthread_join");
  printf("received-outside read %zd bytes (%s), event %s\n", r.got,
         r.got < 0 ? strerrorname_np(r.error) : "ok", name_or_dash(r.buffer, r.got, EVENT));
  report("received-outside: a blocking read of an instance from outside the run waits for the main "
         "thread's event",
         r.got == EVENT && is_event(r.buffer, 0, IN_CREATE, "o1"));
  close(received);
  unlink(path);
}

static int selected(const char *only, const char *name) {
  return only == NULL || strcmp(only, name) == 0;
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  int arg = 1;
  int zero_read = 1;
  if (argc > arg && strcmp(argv[arg], "--no-zero-read") == 0) {
    zero_read = 0;
    arg++;
  }
  int received_case_runs = 1;
  if (argc > arg && strcmp(argv[arg], "--no-received") == 0) {
    received_case_runs = 0;
    arg++;
  }
  const char *only = argc > arg ? argv[arg] : NULL;
  if (!mkdtemp(root)) return 1;
  if (selected(only, "readv")) sibling_case("readv", READV, "v", "v1");
  if (selected(only, "read")) sibling_case("read", READ, "r", "r1");
  if (selected(only, "preadv2")) sibling_case("preadv2", PREADV2, "w", "w1");
  if (selected(only, "readv-boundary")) boundary_case();
  if (selected(only, "two-readers")) two_readers_case();
  if (selected(only, "flags")) flags_case();
  if (selected(only, "readv-array-in-buffer")) array_overlap_case("readv-array-in-buffer", 0, "i");
  if (selected(only, "readv-array-overlay")) array_overlap_case("readv-array-overlay", 16, "o");
  if (selected(only, "readv-array-rewritten-first")) {
    array_rewritten_case("readv-array-rewritten-first", 0, "g");
  }
  if (selected(only, "readv-array-rewritten-later")) {
    array_rewritten_case("readv-array-rewritten-later", 1, "m");
  }
  if (selected(only, "readv-array-rewritten-ahead")) {
    array_rewritten_ahead_case("readv-array-rewritten-ahead", "a");
  }
  if (zero_read && selected(only, "zero")) unfit_case("zero", 0, "z");
  if (selected(only, "small-read")) unfit_case("small-read", 8, "e");
  if (selected(only, "readv-zero-first")) unfit_case("readv-zero-first", -1, "k");
  if (selected(only, "readv-small-later")) small_later_case();
  if (selected(only, "readv-three")) three_case();
  if (selected(only, "preadv2-hipri")) sibling_case("preadv2-hipri", PREADV2_HIPRI, "h", "h1");
  if (selected(only, "preadv2-nowait")) nowait_case();
  if (received_case_runs && selected(only, "received")) received_case("received", 0, "x");
  if (only != NULL && strcmp(only, "received-unmatched") == 0) {
    received_case("received-unmatched", 1, "u");
  }
  if (only != NULL && strcmp(only, "received-outside") == 0) {
    if (argc <= arg + 2) fail("received-outside: no socket name or directory");
    received_outside_case(argv[arg + 1], argv[arg + 2]);
  }
  // Leave nothing behind (the watches are gone with their instances).
  const char *dirs[] = {"v", "r", "w", "b", "b", "p", "q", "f", "i", "i", "o", "o", "g", "m", "m",
                        "a", "a", "z", "e", "k", "s", "s", "t", "t", "t", "h", "n", "x", "u"};
  const char *files[] = {"v1", "r1", "w1", "b1", "b2", "p1", "q1", "f1", "i1", "i2",
                         "o1", "o2", "g1", "m1", "m2", "a1", "a2", "z1", "e1", "k1",
                         "s1", "s2", "t1", "t2", "t3", "h1", "-",  "x1", "u1"};
  for (size_t i = 0; i < sizeof dirs / sizeof dirs[0]; i++) {
    char path[96];
    path_in(path, sizeof path, dirs[i], files[i]);
    unlink(path);
    snprintf(path, sizeof path, "%s/%s", root, dirs[i]);
    rmdir(path);
  }
  rmdir(root);
  printf("all: %s\n", all ? "yes" : "no");
  return all ? 0 : 9;
}
