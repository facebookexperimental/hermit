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
// Signals sent to a blocked reader are tests/c/inotify_blocking_signal.c's.
//
// With an argument, only that case runs.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
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
enum how { READ, READV, PREADV2 };

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

static int selected(const char *only, const char *name) {
  return only == NULL || strcmp(only, name) == 0;
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  const char *only = argc > 1 ? argv[1] : NULL;
  if (!mkdtemp(root)) return 1;
  if (selected(only, "readv")) sibling_case("readv", READV, "v", "v1");
  if (selected(only, "read")) sibling_case("read", READ, "r", "r1");
  if (selected(only, "preadv2")) sibling_case("preadv2", PREADV2, "w", "w1");
  if (selected(only, "readv-boundary")) boundary_case();
  if (selected(only, "two-readers")) two_readers_case();
  if (selected(only, "flags")) flags_case();
  // Leave nothing behind (the watches are gone with their instances).
  const char *dirs[] = {"v", "r", "w", "b", "b", "p", "q", "f"};
  const char *files[] = {"v1", "r1", "w1", "b1", "b2", "p1", "q1", "f1"};
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
