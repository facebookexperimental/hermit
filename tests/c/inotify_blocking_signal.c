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
// - read-blocked, read-ignored, readv-boundary-blocked,
//   readv-boundary-ignored: SIGUSR2, which the reader blocks or the process
//   ignores, does not end the wait: no signal is pending for the reader
//   (an ignored one is discarded when it is sent), so the read returns the
//   event the main thread makes after it, and on into the readv's second
//   iovec, 64 bytes, once it read the first. No handler runs while it
//   waits; a blocked SIGUSR2 is still pending once the read returns, and its
//   handler runs once when the reader unblocks it.
// - readv-boundary-procdir: SIGUSR2 sent to the process, which the reader
//   blocks and the main thread catches: the main thread takes it, and the
//   readv waits on into its second iovec as before, 64 bytes.
// - mode-setfl-signal, mode-fionbio-signal: the main thread makes the
//   reader's descriptor nonblocking (F_SETFL or FIONBIO), which alone leaves
//   the read waiting, then sends SIGUSR1 (no SA_RESTART). inotify_read tests
//   O_NONBLOCK before a pending signal, so the read fails with EAGAIN, not
//   EINTR, and the handler runs.
// - mode-setfl-blocked, mode-setfl-ignored: the same mode change, then
//   SIGUSR2, which the reader blocks or the process ignores. Neither wakes
//   the reader, so the read returns the event the main thread makes 20 ms
//   later, and a blocked SIGUSR2 is still pending once it returns.
// - readv-event-signal, readv3-boundary-event-signal,
//   readv3-boundary-event-signal-restart: the main thread makes one event
//   while the reader waits in a readv of two one-event iovecs (or of three,
//   with an event queued before the readv filling the first), and at once
//   sends SIGUSR1. The readv takes the event into the iovec it waits in;
//   the next iovec's read finds the signal pending before it waits, so the
//   readv returns the bytes read so far (32, or 64 of three), the handler
//   runs, and an event made 20 ms later stays queued. A count is never
//   restarted, so SA_RESTART changes nothing.
// - pipe-replaced-eintr: the main thread puts a nonblocking inotify instance
//   at the descriptor number of a reader blocked on an empty blocking pipe
//   (dup3), then sends SIGUSR1 (no SA_RESTART). The read keeps the pipe it
//   began on, so it fails with EINTR, not the instance's EAGAIN, after the
//   handler.
//
// Arguments: an optional --no-process-signal, which leaves out
// readv-boundary-procdir (KVM refuses a process-directed signal to a process
// of several threads by name, https://github.com/rrnewton/hermit/issues/4034),
// then an optional case name, which runs only that case.

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
#include <sys/ioctl.h>
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
  // 0: read; 1: readv of the whole buffer; 2 or 3: readv of two or three
  // one-event iovecs.
  int vectored;
  // A signal the reader blocks before it reads, or 0.
  int block;
  char buffer[4 * EVENT] __attribute__((aligned(8)));
  ssize_t got;
  int error;
  // Handlers run by the time the read returned.
  int handled_at_return;
  // For a reader that blocks `block`: whether it was still pending once the
  // read returned, and how many handlers ran when the reader then unblocked it.
  int pending_after;
  int handled_after_unblock;
  // Set once the read has returned.
  volatile sig_atomic_t returned;
};

static void *reader(void *argument) {
  struct blocked_read *r = argument;
  if (r->block) {
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, r->block);
    if (pthread_sigmask(SIG_BLOCK, &set, NULL) != 0) fail("pthread_sigmask");
  }
  if (write(ready[1], "r", 1) != 1) fail("write");
  struct iovec iov[3] = {{r->buffer, sizeof r->buffer}, {NULL, 0}, {NULL, 0}};
  if (r->vectored >= 2) {
    iov[0].iov_len = EVENT;
    iov[1] = (struct iovec){r->buffer + EVENT, EVENT};
    iov[2] = (struct iovec){r->buffer + 2 * EVENT, EVENT};
  }
  r->got = r->vectored ? readv(r->fd, iov, r->vectored) : read(r->fd, r->buffer, sizeof r->buffer);
  r->error = r->got < 0 ? errno : 0;
  r->handled_at_return = handled;
  if (r->block) {
    sigset_t pending, set;
    if (sigpending(&pending) != 0) fail("sigpending");
    r->pending_after = sigismember(&pending, r->block);
    sigemptyset(&set);
    sigaddset(&set, r->block);
    if (pthread_sigmask(SIG_UNBLOCK, &set, NULL) != 0) fail("pthread_sigmask");
    r->handled_after_unblock = handled - r->handled_at_return;
  }
  r->returned = 1;
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


static void create_in(const char *dir, const char *name) {
  char path[96];
  snprintf(path, sizeof path, "%s/%s/%s", root, dir, name);
  int fd = open(path, O_CREAT | O_WRONLY | O_CLOEXEC, 0600);
  if (fd < 0 || close(fd) != 0) fail("create");
}

// SIGUSR2 that must not end the wait of a sibling blocked in a read
// (`vectored` 0) or in a readv waiting on into its second iovec (2): the
// reader blocks it (`how` 'b'), the process ignores it ('i'), or the reader
// blocks it and the main thread catches it, sent to the process ('p'). The
// main thread makes the event the read waits for 20 ms after the signal.
static void quiet_signal_case(const char *label, int vectored, char how, const char *dir) {
  char path[96];
  snprintf(path, sizeof path, "%s/%s", root, dir);
  if (mkdir(path, 0700) != 0) fail("mkdir");
  handled = 0;
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_handler = how == 'i' ? SIG_IGN : handler;
  sigemptyset(&sa.sa_mask);
  if (sigaction(SIGUSR2, &sa, NULL) != 0) fail("sigaction");
  struct blocked_read r = {.fd = inotify_init1(IN_CLOEXEC),
                           .vectored = vectored,
                           .block = how == 'i' ? 0 : SIGUSR2};
  if (r.fd < 0) fail("inotify_init1");
  if (inotify_add_watch(r.fd, path, IN_CREATE) < 0) fail("inotify_add_watch");
  if (vectored == 2) create_in(dir, "b1");
  pthread_t thread;
  if (pthread_create(&thread, NULL, reader, &r) != 0) fail("pthread_create");
  char byte;
  if (read(ready[0], &byte, 1) != 1) fail("read");
  sleep_ms(1);
  if (how == 'p') {
    if (kill(getpid(), SIGUSR2) != 0) fail("kill");
  } else if (pthread_kill(thread, SIGUSR2) != 0) {
    fail("pthread_kill");
  }
  sleep_ms(20);
  create_in(dir, "c1");
  if (pthread_join(thread, NULL) != 0) fail("pthread_join");
  signal(SIGUSR2, SIG_DFL);
  const struct inotify_event *first = (const struct inotify_event *)r.buffer;
  const struct inotify_event *second = (const struct inotify_event *)(r.buffer + EVENT);
  printf("%s read %zd bytes (%s), handled %d\n", label, r.got,
         r.got < 0 ? strerrorname_np(r.error) : "ok", r.handled_at_return);
  char what[160];
  int caught = how == 'p';
  if (vectored == 2) {
    snprintf(what, sizeof what,
             "%s: the signal leaves the readv waiting on into its second iovec: both events", label);
    report(what, r.handled_at_return == caught && r.got == 2 * EVENT &&
                     strcmp(first->name, "b1") == 0 && strcmp(second->name, "c1") == 0);
  } else {
    snprintf(what, sizeof what,
             "%s: the signal leaves the read waiting for the event made after it", label);
    report(what, r.handled_at_return == caught && r.got == EVENT && strcmp(first->name, "c1") == 0);
  }
  if (how == 'b') {
    printf("%s: pending once the read returned %d, handlers run on unblocking %d\n", label,
           r.pending_after, r.handled_after_unblock);
    snprintf(what, sizeof what,
             "%s: the blocked signal stays pending, and its handler runs once when the reader "
             "unblocks it",
             label);
    report(what, r.pending_after == 1 && r.handled_after_unblock == 1);
  }
  close(r.fd);
}

// The main thread makes a sibling's blocking descriptor nonblocking while the
// sibling waits in a read (with F_SETFL, or FIONBIO when `fionbio`), then
// sends SIGUSR1, whose handler has no SA_RESTART.
static void mode_signal_case(const char *label, int fionbio, const char *dir) {
  char path[96];
  snprintf(path, sizeof path, "%s/%s", root, dir);
  if (mkdir(path, 0700) != 0) fail("mkdir");
  handled = 0;
  install(0);
  struct blocked_read r = {.fd = inotify_init1(IN_CLOEXEC)};
  if (r.fd < 0) fail("inotify_init1");
  if (inotify_add_watch(r.fd, path, IN_CREATE) < 0) fail("inotify_add_watch");
  pthread_t thread;
  if (pthread_create(&thread, NULL, reader, &r) != 0) fail("pthread_create");
  char byte;
  if (read(ready[0], &byte, 1) != 1) fail("read");
  sleep_ms(1);
  if (fionbio) {
    int one = 1;
    if (ioctl(r.fd, FIONBIO, &one) != 0) fail("FIONBIO");
  } else if (fcntl(r.fd, F_SETFL, O_NONBLOCK) != 0) {
    fail("F_SETFL");
  }
  // A mode change wakes no reader: Linux's stays asleep until an event or a
  // signal.
  sleep_ms(20);
  int waiting = !r.returned;
  if (pthread_kill(thread, SIGUSR1) != 0) fail("pthread_kill");
  if (pthread_join(thread, NULL) != 0) fail("pthread_join");
  printf("%s read %zd bytes (%s), handled %d, still waiting after the mode change: %s\n", label,
         r.got, r.got < 0 ? strerrorname_np(r.error) : "ok", (int)handled,
         waiting ? "yes" : "no");
  char what[160];
  snprintf(what, sizeof what, "%s: the mode change alone leaves the read waiting", label);
  report(what, waiting);
  snprintf(what, sizeof what,
           "%s: the signal then ends the read with EAGAIN, not EINTR, after the handler", label);
  report(what, handled == 1 && r.got == -1 && r.error == EAGAIN);
  close(r.fd);
}

// The main thread makes a sibling's blocking descriptor nonblocking (F_SETFL)
// while the sibling waits in a read, then sends SIGUSR2, which the reader
// blocks (`how` 'b') or the process ignores ('i'), and makes the event the
// read waits for 20 ms after the signal.
static void mode_quiet_signal_case(const char *label, char how, const char *dir) {
  char path[96];
  snprintf(path, sizeof path, "%s/%s", root, dir);
  if (mkdir(path, 0700) != 0) fail("mkdir");
  handled = 0;
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_handler = how == 'i' ? SIG_IGN : handler;
  sigemptyset(&sa.sa_mask);
  if (sigaction(SIGUSR2, &sa, NULL) != 0) fail("sigaction");
  struct blocked_read r = {.fd = inotify_init1(IN_CLOEXEC), .block = how == 'b' ? SIGUSR2 : 0};
  if (r.fd < 0) fail("inotify_init1");
  if (inotify_add_watch(r.fd, path, IN_CREATE) < 0) fail("inotify_add_watch");
  pthread_t thread;
  if (pthread_create(&thread, NULL, reader, &r) != 0) fail("pthread_create");
  char byte;
  if (read(ready[0], &byte, 1) != 1) fail("read");
  sleep_ms(1);
  if (fcntl(r.fd, F_SETFL, O_NONBLOCK) != 0) fail("F_SETFL");
  sleep_ms(20);
  int waiting = !r.returned;
  if (pthread_kill(thread, SIGUSR2) != 0) fail("pthread_kill");
  sleep_ms(20);
  create_in(dir, "c1");
  if (pthread_join(thread, NULL) != 0) fail("pthread_join");
  signal(SIGUSR2, SIG_DFL);
  const struct inotify_event *first = (const struct inotify_event *)r.buffer;
  printf("%s read %zd bytes (%s), handled %d, still waiting after the mode change: %s\n", label,
         r.got, r.got < 0 ? strerrorname_np(r.error) : "ok", r.handled_at_return,
         waiting ? "yes" : "no");
  char what[160];
  snprintf(what, sizeof what, "%s: the mode change alone leaves the read waiting", label);
  report(what, waiting);
  snprintf(what, sizeof what, "%s: the signal leaves the read waiting for the event made after it",
           label);
  report(what, r.handled_at_return == 0 && r.got == EVENT && strcmp(first->name, "c1") == 0);
  if (how == 'b') {
    printf("%s: pending once the read returned %d, handlers run on unblocking %d\n", label,
           r.pending_after, r.handled_after_unblock);
    snprintf(what, sizeof what,
             "%s: the blocked signal stays pending, and its handler runs once when the reader "
             "unblocks it",
             label);
    report(what, r.pending_after == 1 && r.handled_after_unblock == 1);
  }
  close(r.fd);
}

// The main thread makes one event while a sibling waits in a readv of
// `iovecs` one-event iovecs (2, or 3 with an event queued before the readv
// filling the first, so that it waits in the second), then at once sends
// SIGUSR1, whose handler has SA_RESTART when `restart` is set, and makes a
// last event 20 ms later.
static void event_signal_case(const char *label, int iovecs, int restart, const char *dir) {
  char path[96];
  snprintf(path, sizeof path, "%s/%s", root, dir);
  if (mkdir(path, 0700) != 0) fail("mkdir");
  handled = 0;
  install(restart ? SA_RESTART : 0);
  struct blocked_read r = {.fd = inotify_init1(IN_CLOEXEC), .vectored = iovecs};
  if (r.fd < 0) fail("inotify_init1");
  if (inotify_add_watch(r.fd, path, IN_CREATE) < 0) fail("inotify_add_watch");
  int queued = iovecs == 3;
  if (queued) create_in(dir, "q1");
  pthread_t thread;
  if (pthread_create(&thread, NULL, reader, &r) != 0) fail("pthread_create");
  char byte;
  if (read(ready[0], &byte, 1) != 1) fail("read");
  sleep_ms(1);
  create_in(dir, "e1");
  if (pthread_kill(thread, SIGUSR1) != 0) fail("pthread_kill");
  sleep_ms(20);
  create_in(dir, "l1");
  if (pthread_join(thread, NULL) != 0) fail("pthread_join");
  // What the readv left queued.
  if (fcntl(r.fd, F_SETFL, O_NONBLOCK) != 0) fail("F_SETFL");
  char rest[2 * EVENT] __attribute__((aligned(8)));
  ssize_t left = read(r.fd, rest, sizeof rest);
  const struct inotify_event *first = (const struct inotify_event *)r.buffer;
  const struct inotify_event *second = (const struct inotify_event *)(r.buffer + EVENT);
  const struct inotify_event *last = (const struct inotify_event *)rest;
  printf("%s read %zd bytes (%s), handled %d, left queued %zd\n", label, r.got,
         r.got < 0 ? strerrorname_np(r.error) : "ok", r.handled_at_return, left);
  char what[160];
  snprintf(what, sizeof what,
           "%s: the readv returns the bytes it read before the signal, after the handler", label);
  int events = queued ? r.got == 2 * EVENT && strcmp(first->name, "q1") == 0 &&
                            strcmp(second->name, "e1") == 0
                      : r.got == EVENT && strcmp(first->name, "e1") == 0;
  report(what, r.handled_at_return == 1 && events);
  snprintf(what, sizeof what, "%s: the event made after the signal stays queued", label);
  report(what, left == EVENT && strcmp(last->name, "l1") == 0);
  close(r.fd);
}

// The main thread puts a nonblocking inotify instance at the descriptor
// number of a sibling blocked reading an empty blocking pipe (dup3), then
// sends SIGUSR1, whose handler has no SA_RESTART.
static void replaced_signal_case(const char *label) {
  handled = 0;
  install(0);
  int pipefd[2];
  if (pipe2(pipefd, O_CLOEXEC) != 0) fail("pipe2");
  struct blocked_read r = {.fd = pipefd[0]};
  pthread_t thread;
  if (pthread_create(&thread, NULL, reader, &r) != 0) fail("pthread_create");
  char byte;
  if (read(ready[0], &byte, 1) != 1) fail("read");
  sleep_ms(1);
  int instance = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
  if (instance < 0) fail("inotify_init1");
  if (dup3(instance, r.fd, O_CLOEXEC) != r.fd) fail("dup3");
  if (close(instance) != 0) fail("close");
  sleep_ms(20);
  if (pthread_kill(thread, SIGUSR1) != 0) fail("pthread_kill");
  if (pthread_join(thread, NULL) != 0) fail("pthread_join");
  printf("%s read %zd bytes (%s), handled %d\n", label, r.got,
         r.got < 0 ? strerrorname_np(r.error) : "ok", r.handled_at_return);
  char what[160];
  snprintf(what, sizeof what,
           "%s: the read keeps the pipe it began on: EINTR, not EAGAIN, after the handler",
           label);
  report(what, r.handled_at_return == 1 && r.got == -1 && r.error == EINTR);
  close(r.fd);
  close(pipefd[1]);
}

static int selected(const char *only, const char *name) {
  return only == NULL || strcmp(only, name) == 0;
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  int arg = 1;
  int process_signal = 1;
  if (argc > arg && strcmp(argv[arg], "--no-process-signal") == 0) {
    process_signal = 0;
    arg++;
  }
  const char *only = argc > arg ? argv[arg] : NULL;
  if (!mkdtemp(root)) return 1;
  if (pipe(ready) != 0) fail("pipe");
  if (selected(only, "eintr")) signal_case("eintr", 0, 0, "e");
  if (selected(only, "restart")) signal_case("restart", 0, 1, "s");
  if (selected(only, "readv-eintr")) signal_case("readv-eintr", 1, 0, "v");
  if (selected(only, "readv-boundary-eintr")) signal_case("readv-boundary-eintr", 2, 0, "b");
  if (selected(only, "read-blocked")) quiet_signal_case("read-blocked", 0, 'b', "rb");
  if (selected(only, "read-ignored")) quiet_signal_case("read-ignored", 0, 'i', "ri");
  if (selected(only, "readv-boundary-blocked")) {
    quiet_signal_case("readv-boundary-blocked", 2, 'b', "bb");
  }
  if (selected(only, "readv-boundary-ignored")) {
    quiet_signal_case("readv-boundary-ignored", 2, 'i', "bi");
  }
  if (process_signal && selected(only, "readv-boundary-procdir")) {
    quiet_signal_case("readv-boundary-procdir", 2, 'p', "bp");
  }
  if (selected(only, "mode-setfl-signal")) mode_signal_case("mode-setfl-signal", 0, "ms");
  if (selected(only, "mode-fionbio-signal")) mode_signal_case("mode-fionbio-signal", 1, "mf");
  if (selected(only, "mode-setfl-blocked")) mode_quiet_signal_case("mode-setfl-blocked", 'b', "mb");
  if (selected(only, "mode-setfl-ignored")) mode_quiet_signal_case("mode-setfl-ignored", 'i', "mi");
  if (selected(only, "readv-event-signal")) event_signal_case("readv-event-signal", 2, 0, "es");
  if (selected(only, "readv3-boundary-event-signal")) {
    event_signal_case("readv3-boundary-event-signal", 3, 0, "e3");
  }
  if (selected(only, "readv3-boundary-event-signal-restart")) {
    event_signal_case("readv3-boundary-event-signal-restart", 3, 1, "e3r");
  }
  if (selected(only, "pipe-replaced-eintr")) replaced_signal_case("pipe-replaced-eintr");
  // Leave nothing behind (the watches are gone with their instances).
  char path[96];
  const char *files[] = {"s/c1",  "b/b1",  "rb/c1", "ri/c1",  "bb/b1",  "bb/c1",  "bi/b1",
                         "bi/c1", "bp/b1", "bp/c1", "mb/c1",  "mi/c1",  "es/e1",  "es/l1",
                         "e3/q1", "e3/e1", "e3/l1", "e3r/q1", "e3r/e1", "e3r/l1"};
  for (size_t i = 0; i < sizeof files / sizeof files[0]; i++) {
    snprintf(path, sizeof path, "%s/%s", root, files[i]);
    unlink(path);
  }
  const char *dirs[] = {"e",  "s",  "v",  "b",  "rb", "ri", "bb", "bi",
                        "bp", "ms", "mf", "mb", "mi", "es", "e3", "e3r"};
  for (size_t i = 0; i < sizeof dirs / sizeof dirs[0]; i++) {
    snprintf(path, sizeof path, "%s/%s", root, dirs[i]);
    rmdir(path);
  }
  rmdir(root);
  printf("all: %s\n", all ? "yes" : "no");
  return all ? 0 : 9;
}
