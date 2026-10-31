/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A signal sent to a thread blocked reading an empty blocking inotify
// descriptor (https://github.com/rrnewton/hermit/issues/4768). Every line
// printed is what Linux guarantees, so it equals the native run's; the guest
// exits 9 if any check fails.
//
// - eintr, readv-eintr: the reader's SIGUSR1 handler has no SA_RESTART, so
//   its read (or readv) fails with EINTR after the handler runs.
// - restart: the handler has SA_RESTART, so the read restarts and returns
//   the event the main thread creates only after the handler has run.
// - readv-boundary-eintr: a queued event exactly fills the first of a
//   readv's two iovecs, so the readv waits on into the second, and the
//   signal arrives there. Linux's readv then returns the first iovec's bytes,
//   not EINTR, as it had read something, and the handler runs.
//
// With an argument, only that case runs.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <time.h>
#include <unistd.h>

// Every name here is two characters, which Linux pads to 16 bytes, so every
// event is 32 bytes.
#define EVENT 32

static char root[] = "/tmp/inotify-blocking-signal-XXXXXX";
static int all = 1;
static volatile sig_atomic_t handled;
// The reader writes one byte here just before its read, so the signal is
// sent while it waits in the read, not before.
static int ready[2];

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

static void handler(int signal) {
  (void)signal;
  handled++;
}

static void install(int flags) {
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_handler = handler;
  sa.sa_flags = flags;
  sigemptyset(&sa.sa_mask);
  if (sigaction(SIGUSR1, &sa, NULL) != 0) fail("sigaction");
}

struct blocked_read {
  int fd;
  // 0: read; 1: readv of the whole buffer; 2: readv of two one-event iovecs.
  int vectored;
  char buffer[4 * EVENT] __attribute__((aligned(8)));
  ssize_t got;
  int error;
};

static void *reader(void *argument) {
  struct blocked_read *r = argument;
  if (write(ready[1], "r", 1) != 1) fail("write");
  struct iovec iov[2] = {{r->buffer, sizeof r->buffer}, {NULL, 0}};
  if (r->vectored == 2) {
    iov[0].iov_len = EVENT;
    iov[1] = (struct iovec){r->buffer + EVENT, EVENT};
  }
  r->got = r->vectored ? readv(r->fd, iov, r->vectored) : read(r->fd, r->buffer, sizeof r->buffer);
  r->error = r->got < 0 ? errno : 0;
  return NULL;
}

// A sibling blocked in a read (or readv) of an empty blocking instance
// watching `dir` is sent SIGUSR1, whose handler has SA_RESTART when
// `restart` is set.
static void signal_case(const char *label, int vectored, int restart, const char *dir) {
  char path[96];
  snprintf(path, sizeof path, "%s/%s", root, dir);
  if (mkdir(path, 0700) != 0) fail("mkdir");
  handled = 0;
  install(restart ? SA_RESTART : 0);
  struct blocked_read r = {.fd = inotify_init1(IN_CLOEXEC), .vectored = vectored};
  if (r.fd < 0) fail("inotify_init1");
  if (inotify_add_watch(r.fd, path, IN_CREATE) < 0) fail("inotify_add_watch");
  if (vectored == 2) {
    // The event that fills the first iovec, queued before the readv.
    snprintf(path, sizeof path, "%s/%s/b1", root, dir);
    int fd = open(path, O_CREAT | O_WRONLY | O_CLOEXEC, 0600);
    if (fd < 0 || close(fd) != 0) fail("create");
  }
  pthread_t thread;
  if (pthread_create(&thread, NULL, reader, &r) != 0) fail("pthread_create");
  char byte;
  if (read(ready[0], &byte, 1) != 1) fail("read");
  // Under Hermit the sleep is virtual and the reader blocks meanwhile;
  // natively it makes the race unlikely.
  sleep_ms(1);
  if (pthread_kill(thread, SIGUSR1) != 0) fail("pthread_kill");
  if (restart) {
    // The event comes only after the handler has run, so the read can
    // return it only by restarting.
    for (int i = 0; i < 1000 && !handled; i++) sleep_ms(1);
    snprintf(path, sizeof path, "%s/%s/c1", root, dir);
    int fd = open(path, O_CREAT | O_WRONLY | O_CLOEXEC, 0600);
    if (fd < 0 || close(fd) != 0) fail("create");
  }
  if (pthread_join(thread, NULL) != 0) fail("pthread_join");
  const struct inotify_event *event = (const struct inotify_event *)r.buffer;
  printf("%s read %zd bytes (%s), handled %d\n", label, r.got,
         r.got < 0 ? strerrorname_np(r.error) : "ok", (int)handled);
  char what[128];
  if (vectored == 2) {
    snprintf(what, sizeof what,
             "%s: the readv returns the first iovec's event, not EINTR, after the handler",
             label);
    report(what, handled == 1 && r.got == EVENT && event->mask == IN_CREATE &&
                     event->len == 16 && strcmp(event->name, "b1") == 0);
  } else if (restart) {
    snprintf(what, sizeof what,
             "%s: with SA_RESTART the read restarts and returns the event made after the handler",
             label);
    report(what, handled == 1 && r.got == EVENT && event->mask == IN_CREATE &&
                     event->len == 16 && strcmp(event->name, "c1") == 0);
  } else {
    snprintf(what, sizeof what, "%s: without SA_RESTART the read fails with EINTR after the handler",
             label);
    report(what, handled == 1 && r.got == -1 && r.error == EINTR);
  }
  close(r.fd);
}

static int selected(const char *only, const char *name) {
  return only == NULL || strcmp(only, name) == 0;
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  const char *only = argc > 1 ? argv[1] : NULL;
  if (!mkdtemp(root)) return 1;
  if (pipe(ready) != 0) fail("pipe");
  if (selected(only, "eintr")) signal_case("eintr", 0, 0, "e");
  if (selected(only, "restart")) signal_case("restart", 0, 1, "s");
  if (selected(only, "readv-eintr")) signal_case("readv-eintr", 1, 0, "v");
  if (selected(only, "readv-boundary-eintr")) signal_case("readv-boundary-eintr", 2, 0, "b");
  // Leave nothing behind (the watches are gone with their instances).
  char path[96];
  snprintf(path, sizeof path, "%s/s/c1", root);
  unlink(path);
  snprintf(path, sizeof path, "%s/b/b1", root);
  unlink(path);
  const char *dirs[] = {"e", "s", "v", "b"};
  for (size_t i = 0; i < sizeof dirs / sizeof dirs[0]; i++) {
    snprintf(path, sizeof path, "%s/%s", root, dirs[i]);
    rmdir(path);
  }
  rmdir(root);
  printf("all: %s\n", all ? "yes" : "no");
  return all ? 0 : 9;
}
