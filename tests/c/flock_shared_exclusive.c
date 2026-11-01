/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * flock(2) between processes, each lock taken through an open file
 * description Detcore can follow (the kvm lane's slice SP2): shared and
 * exclusive locks, LOCK_NB conflicts, a conversion that drops the held lock
 * first, blocking waiters woken at a release, at a last close and at an exit,
 * a fork's copy of a locked description, the wake order of queued waiters, the
 * rows /proc/locks shows, waiters (`->`) included, a lock taken before
 * children are forked (the way rustup holds its lock while cargo runs), a
 * signal that ends or restarts a waiting call, and a waiting thread woken by a
 * release in another thread of its process.
 *
 * Every line printed is the same natively and under Hermit, on every backend:
 * all output comes from the parent, at points its pipes order, and no outcome
 * depends on timing (a sleep only makes a waiter likely to block before the
 * release natively; it acquires after the release either way). The wake
 * order is Linux's too: a second exclusive waiter blocks under the first
 * (`__locks_insert_block`), so the first is granted first.
 *
 * Scenarios (argv[1]):
 *   all          every scenario of the table before main, in order; the
 *                default. Each also runs alone by its name there.
 *   mmap-close   a mapping keeps the lock after the last close, until munmap.
 *                Hermit on KVM refuses the close by name.
 *   shell        a descriptor opened before a fork and locked in the child
 *                (`exec 9>lockfile; flock -n 9`). Hermit leaves it to the
 *                kernel where the backend has host descriptions, and refuses
 *                it by name on KVM.
 *   probe PATH   (internal) a LOCK_EX|LOCK_NB probe of PATH by an exec'd
 *                child.
 *
 * Exit status: 0 when every check printed the expected answer, 1 when one
 * did not (a line beginning FAIL), 2 for a harness failure.
 *
 * _GNU_SOURCE is defined here: the manifest runner compiles this file with
 * -std=c11 and no -D_GNU_SOURCE (ci/manifest-plan/src/runner.rs).
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define HARNESS_FAILURE 2

static char lock_dir[256];
static char lock_path[300];
static const char* self_path;
static int failures;

/* Abort the whole run on a harness failure: nothing after it is meaningful. */
static void harness(const char* what) {
  printf("HARNESS: %s (errno=%d)\n", what, errno);
  fflush(stdout);
  exit(HARNESS_FAILURE);
}

static int open_lock(void) {
  int fd = open(lock_path, O_RDWR | O_CLOEXEC);
  if (fd < 0) {
    harness("open the lock file");
  }
  return fd;
}

/* The answer of a flock call, as a word. The buffer is per thread: the
   waiting threads below answer too. */
static const char* attempt(int fd, int operation) {
  static _Thread_local char text[32];
  errno = 0;
  if (flock(fd, operation) == 0) {
    return "ok";
  }
  if (errno == EWOULDBLOCK) {
    return "EWOULDBLOCK";
  }
  snprintf(text, sizeof(text), "errno=%d", errno);
  return text;
}

static void expect(const char* scenario, const char* what, const char* got, const char* want) {
  if (strcmp(got, want) == 0) {
    printf("%s: %s = %s\n", scenario, what, got);
  } else {
    printf("FAIL %s: %s = %s, expected %s\n", scenario, what, got, want);
    failures++;
  }
}

static void sleep_ms(long ms) {
  struct timespec delay = {ms / 1000, (ms % 1000) * 1000000L};
  while (nanosleep(&delay, &delay) != 0 && errno == EINTR) {
  }
}

/* A pipe that carries one-line reports from a child to the parent. */
struct channel {
  int read_end;
  int write_end;
};

static struct channel channel(void) {
  int ends[2];
  if (pipe2(ends, O_CLOEXEC) != 0) {
    harness("pipe2");
  }
  struct channel made = {ends[0], ends[1]};
  return made;
}

static void say(struct channel to, const char* text) {
  char line[64];
  snprintf(line, sizeof(line), "%s\n", text);
  size_t length = strlen(line);
  if (write(to.write_end, line, length) != (ssize_t)length) {
    _exit(HARNESS_FAILURE);
  }
}

/* One line from `from`, without its newline, into `line`. */
static void hear(struct channel from, char* line, size_t size) {
  size_t used = 0;
  while (used + 1 < size) {
    char byte;
    ssize_t got = read(from.read_end, &byte, 1);
    if (got < 0 && errno == EINTR) {
      continue;
    }
    if (got != 1) {
      harness("read a child's report");
    }
    if (byte == '\n') {
      break;
    }
    line[used++] = byte;
  }
  line[used] = '\0';
}

static void close_channel(struct channel c) {
  close(c.read_end);
  close(c.write_end);
}

static void reap(pid_t child) {
  int status;
  while (waitpid(child, &status, 0) < 0) {
    if (errno != EINTR) {
      harness("waitpid");
    }
  }
  if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    printf("FAIL: child exited with status %#x\n", status);
    failures++;
  }
}

static pid_t start(void) {
  fflush(stdout);
  pid_t child = fork();
  if (child < 0) {
    harness("fork");
  }
  return child;
}

/* A child that opens its own description and reports a LOCK_NB attempt. */
static void probe_into(struct channel report, int operation) {
  pid_t child = start();
  if (child == 0) {
    int fd = open_lock();
    say(report, attempt(fd, operation));
    _exit(0);
  }
  reap(child);
}

static void probe(const char* scenario, const char* what, int operation, const char* want) {
  struct channel report = channel();
  char line[64];
  probe_into(report, operation);
  hear(report, line, sizeof(line));
  expect(scenario, what, line, want);
  close_channel(report);
}

static void scenario_exclusive_excludes(void) {
  const char* s = "exclusive-excludes";
  int held = open_lock();
  expect(s, "parent LOCK_EX", attempt(held, LOCK_EX), "ok");
  int second = open_lock();
  expect(s, "second open in the parent LOCK_EX|LOCK_NB", attempt(second, LOCK_EX | LOCK_NB),
         "EWOULDBLOCK");
  probe(s, "child LOCK_EX|LOCK_NB", LOCK_EX | LOCK_NB, "EWOULDBLOCK");
  probe(s, "child LOCK_SH|LOCK_NB", LOCK_SH | LOCK_NB, "EWOULDBLOCK");
  expect(s, "parent LOCK_UN", attempt(held, LOCK_UN), "ok");
  probe(s, "child LOCK_EX|LOCK_NB after the unlock", LOCK_EX | LOCK_NB, "ok");
  close(second);
  close(held);
}

static void scenario_shared_shares(void) {
  const char* s = "shared-shares";
  int held = open_lock();
  expect(s, "parent LOCK_SH", attempt(held, LOCK_SH), "ok");
  struct channel report = channel();
  struct channel go = channel();
  pid_t child = start();
  if (child == 0) {
    int fd = open_lock();
    char line[64];
    say(report, attempt(fd, LOCK_SH | LOCK_NB));
    /* A conversion deletes the held lock before it looks for a conflict. */
    say(report, attempt(fd, LOCK_EX | LOCK_NB));
    hear(go, line, sizeof(line));
    _exit(0);
  }
  char line[64];
  hear(report, line, sizeof(line));
  expect(s, "child LOCK_SH|LOCK_NB", line, "ok");
  hear(report, line, sizeof(line));
  expect(s, "child converts with LOCK_EX|LOCK_NB", line, "EWOULDBLOCK");
  /* The child's description, still open, holds nothing now. */
  expect(s, "parent converts with LOCK_EX|LOCK_NB", attempt(held, LOCK_EX | LOCK_NB), "ok");
  say(go, "exit");
  reap(child);
  close_channel(report);
  close_channel(go);
  close(held);
}

static void scenario_waiter_woken_at_release(void) {
  const char* s = "waiter-release";
  int held = open_lock();
  expect(s, "parent LOCK_EX", attempt(held, LOCK_EX), "ok");
  struct channel report = channel();
  pid_t child = start();
  if (child == 0) {
    int fd = open_lock();
    say(report, "blocking");
    say(report, attempt(fd, LOCK_EX));
    _exit(0);
  }
  char line[64];
  hear(report, line, sizeof(line));
  sleep_ms(100);
  printf("%s: parent releases\n", s);
  expect(s, "parent LOCK_UN", attempt(held, LOCK_UN), "ok");
  hear(report, line, sizeof(line));
  expect(s, "child's blocking LOCK_EX after the release", line, "ok");
  reap(child);
  close_channel(report);
  close(held);
}

static void scenario_released_at_last_close(void) {
  const char* s = "close-release";
  int held = open_lock();
  expect(s, "parent LOCK_EX", attempt(held, LOCK_EX), "ok");
  int alias = dup(held);
  if (alias < 0) {
    harness("dup");
  }
  close(held);
  probe(s, "child LOCK_EX|LOCK_NB with a dup still open", LOCK_EX | LOCK_NB, "EWOULDBLOCK");
  close(alias);
  probe(s, "child LOCK_EX|LOCK_NB after the last close", LOCK_EX | LOCK_NB, "ok");
}

static void scenario_released_at_exit(void) {
  const char* s = "exit-release";
  struct channel holder_report = channel();
  struct channel holder_go = channel();
  pid_t holder = start();
  if (holder == 0) {
    int fd = open_lock();
    char line[64];
    say(holder_report, attempt(fd, LOCK_EX));
    hear(holder_go, line, sizeof(line));
    /* Exits holding the lock. */
    _exit(0);
  }
  char line[64];
  hear(holder_report, line, sizeof(line));
  expect(s, "holder LOCK_EX", line, "ok");
  struct channel waiter_report = channel();
  pid_t waiter = start();
  if (waiter == 0) {
    int fd = open_lock();
    say(waiter_report, attempt(fd, LOCK_EX));
    _exit(0);
  }
  sleep_ms(100);
  printf("%s: holder exits\n", s);
  say(holder_go, "exit");
  reap(holder);
  hear(waiter_report, line, sizeof(line));
  expect(s, "waiter's blocking LOCK_EX after the holder's exit", line, "ok");
  reap(waiter);
  close_channel(holder_report);
  close_channel(holder_go);
  close_channel(waiter_report);
}

static void scenario_fork_shares_the_description(void) {
  const char* s = "fork-shares";
  int held = open_lock();
  expect(s, "parent LOCK_EX", attempt(held, LOCK_EX), "ok");
  struct channel report = channel();
  pid_t child = start();
  if (child == 0) {
    /* The fork's copy is the same description: its lock is this one. */
    say(report, attempt(held, LOCK_EX | LOCK_NB));
    int own = open_lock();
    say(report, attempt(own, LOCK_EX | LOCK_NB));
    _exit(0);
  }
  char line[64];
  hear(report, line, sizeof(line));
  expect(s, "child LOCK_EX|LOCK_NB through the inherited descriptor", line, "ok");
  hear(report, line, sizeof(line));
  expect(s, "child LOCK_EX|LOCK_NB through its own open", line, "EWOULDBLOCK");
  reap(child);
  /* The child's exit dropped its reference; the parent's holds the lock. */
  probe(s, "another child LOCK_EX|LOCK_NB after that child's exit", LOCK_EX | LOCK_NB,
        "EWOULDBLOCK");
  close(held);
  probe(s, "another child LOCK_EX|LOCK_NB after the parent's close", LOCK_EX | LOCK_NB, "ok");
  close_channel(report);
}

static void scenario_downgrade_wakes_a_shared_waiter(void) {
  const char* s = "downgrade-wakes";
  int held = open_lock();
  expect(s, "parent LOCK_EX", attempt(held, LOCK_EX), "ok");
  struct channel report = channel();
  struct channel go = channel();
  pid_t child = start();
  if (child == 0) {
    int fd = open_lock();
    char line[64];
    say(report, attempt(fd, LOCK_SH));
    hear(go, line, sizeof(line));
    _exit(0);
  }
  sleep_ms(100);
  expect(s, "parent converts to LOCK_SH", attempt(held, LOCK_SH), "ok");
  char line[64];
  hear(report, line, sizeof(line));
  expect(s, "child's blocking LOCK_SH after the downgrade", line, "ok");
  probe(s, "another child LOCK_EX|LOCK_NB beside two shared locks", LOCK_EX | LOCK_NB,
        "EWOULDBLOCK");
  probe(s, "another child LOCK_SH|LOCK_NB beside two shared locks", LOCK_SH | LOCK_NB, "ok");
  say(go, "exit");
  reap(child);
  close_channel(report);
  close_channel(go);
  close(held);
}

/* The counter at the start of the lock file, which a waiter holding the
   exclusive lock bumps: the order in which the waiters were granted. */
static int bump_counter(int fd) {
  unsigned char value = 0;
  if (pread(fd, &value, 1, 0) < 0) {
    _exit(HARNESS_FAILURE);
  }
  value++;
  if (pwrite(fd, &value, 1, 0) != 1) {
    _exit(HARNESS_FAILURE);
  }
  return value;
}

static void scenario_wake_order(void) {
  const char* s = "wake-order";
  int held = open_lock();
  unsigned char zero = 0;
  if (pwrite(held, &zero, 1, 0) != 1) {
    harness("reset the counter");
  }
  expect(s, "parent LOCK_EX", attempt(held, LOCK_EX), "ok");
  struct channel reports[2] = {channel(), channel()};
  pid_t waiters[2];
  for (int index = 0; index < 2; index++) {
    waiters[index] = start();
    if (waiters[index] == 0) {
      int fd = open_lock();
      if (flock(fd, LOCK_EX) != 0) {
        _exit(HARNESS_FAILURE);
      }
      char text[16];
      snprintf(text, sizeof(text), "#%d", bump_counter(fd));
      sleep_ms(10);
      flock(fd, LOCK_UN);
      say(reports[index], text);
      _exit(0);
    }
    /* The first waiter blocks before the second starts. */
    sleep_ms(100);
  }
  expect(s, "parent LOCK_UN", attempt(held, LOCK_UN), "ok");
  char line[64];
  hear(reports[0], line, sizeof(line));
  expect(s, "first waiter granted", line, "#1");
  hear(reports[1], line, sizeof(line));
  expect(s, "second waiter granted", line, "#2");
  for (int index = 0; index < 2; index++) {
    reap(waiters[index]);
    close_channel(reports[index]);
  }
  close(held);
}

static void scenario_mixed_waiters(void) {
  const char* s = "mixed-waiters";
  int held = open_lock();
  expect(s, "parent LOCK_EX", attempt(held, LOCK_EX), "ok");
  const int modes[3] = {LOCK_SH, LOCK_EX, LOCK_SH};
  struct channel reports[3] = {channel(), channel(), channel()};
  struct channel goes[3] = {channel(), channel(), channel()};
  pid_t waiters[3];
  for (int index = 0; index < 3; index++) {
    waiters[index] = start();
    if (waiters[index] == 0) {
      int fd = open_lock();
      char line[64];
      say(reports[index], attempt(fd, modes[index]));
      hear(goes[index], line, sizeof(line));
      flock(fd, LOCK_UN);
      _exit(0);
    }
    sleep_ms(100);
  }
  expect(s, "parent LOCK_UN", attempt(held, LOCK_UN), "ok");
  char line[64];
  /* Both shared waiters are granted together; the exclusive one waits. */
  hear(reports[0], line, sizeof(line));
  expect(s, "first shared waiter granted", line, "ok");
  hear(reports[2], line, sizeof(line));
  expect(s, "second shared waiter granted", line, "ok");
  probe(s, "a child LOCK_SH|LOCK_NB beside the shared waiters", LOCK_SH | LOCK_NB, "ok");
  say(goes[0], "release");
  say(goes[2], "release");
  hear(reports[1], line, sizeof(line));
  expect(s, "exclusive waiter granted after both shared releases", line, "ok");
  say(goes[1], "release");
  for (int index = 0; index < 3; index++) {
    reap(waiters[index]);
    close_channel(reports[index]);
    close_channel(goes[index]);
  }
  close(held);
}

/* Whether every FLOCK row /proc/locks shows is this program's: no row was
   there before its first lock. Natively other processes' rows usually are,
   and this program's rows are told apart by the lock file's inode number;
   Hermit shows only the guest's rows, and numbers their identities itself,
   so there the inode number cannot tell them apart and need not. */
static int all_flock_rows_are_ours;

/* The access of each /proc/locks row on the lock file, in row order, a
   waiter's (`->`) after `->`. */
static void proc_locks_rows(char* out, size_t size) {
  struct stat file;
  if (stat(lock_path, &file) != 0) {
    harness("stat the lock file");
  }
  char suffix[32];
  snprintf(suffix, sizeof(suffix), ":%lu", (unsigned long)file.st_ino);
  FILE* locks = fopen("/proc/locks", "re");
  if (locks == NULL) {
    harness("open /proc/locks");
  }
  out[0] = '\0';
  char row[256];
  while (fgets(row, sizeof(row), locks) != NULL) {
    char kind[16];
    char waiter[4];
    char mode[16];
    char access[16];
    char object[64];
    /* N: [->] FLOCK ADVISORY ACCESS PID MAJ:MIN:INO START END */
    int waits = sscanf(row, "%*s %3s", waiter) == 1 && strcmp(waiter, "->") == 0;
    const char* format = waits ? "%*s %*s %15s %15s %15s %*s %63s" : "%*s %15s %15s %15s %*s %63s";
    if (sscanf(row, format, kind, mode, access, object) != 4) {
      continue;
    }
    size_t length = strlen(object);
    size_t tail = strlen(suffix);
    if (strcmp(kind, "FLOCK") != 0 ||
        (!all_flock_rows_are_ours &&
         (length < tail || strcmp(object + length - tail, suffix) != 0))) {
      continue;
    }
    if (out[0] != '\0') {
      strncat(out, " ", size - strlen(out) - 1);
    }
    if (waits) {
      strncat(out, "->", size - strlen(out) - 1);
    }
    strncat(out, access, size - strlen(out) - 1);
  }
  fclose(locks);
  if (out[0] == '\0') {
    snprintf(out, size, "none");
  }
}

/* proc_locks_rows once they are `want`, or after 5 seconds: natively a
   waiter blocks when the kernel's scheduler runs it; under Hermit in its turn,
   which the reads' sleeps let come. */
static void proc_locks_rows_once(char* out, size_t size, const char* want) {
  for (int attempt = 0; attempt < 500; attempt++) {
    proc_locks_rows(out, size);
    if (strcmp(out, want) == 0) {
      return;
    }
    sleep_ms(10);
  }
}

/* Whether every FLOCK row is this program's (all_flock_rows_are_ours),
   learned while it holds no lock. */
static void learn_whose_rows_show(void) {
  char rows[128];
  all_flock_rows_are_ours = 1;
  proc_locks_rows(rows, sizeof(rows));
  all_flock_rows_are_ours = strcmp(rows, "none") == 0;
}

static void scenario_proc_locks(void) {
  const char* s = "proc-locks";
  char rows[128];
  learn_whose_rows_show();
  int held = open_lock();
  expect(s, "parent LOCK_SH", attempt(held, LOCK_SH), "ok");
  struct channel report = channel();
  struct channel go = channel();
  pid_t child = start();
  if (child == 0) {
    int fd = open_lock();
    char line[64];
    say(report, attempt(fd, LOCK_SH));
    hear(go, line, sizeof(line));
    _exit(0);
  }
  char line[64];
  hear(report, line, sizeof(line));
  expect(s, "child LOCK_SH", line, "ok");
  proc_locks_rows(rows, sizeof(rows));
  expect(s, "rows with two shared holders", rows, "READ READ");
  say(go, "exit");
  reap(child);
  proc_locks_rows(rows, sizeof(rows));
  expect(s, "rows after the child's exit", rows, "READ");
  expect(s, "parent converts to LOCK_EX", attempt(held, LOCK_EX), "ok");
  proc_locks_rows(rows, sizeof(rows));
  expect(s, "rows after the conversion", rows, "WRITE");
  close(held);
  proc_locks_rows(rows, sizeof(rows));
  expect(s, "rows after the last close", rows, "none");
  close_channel(report);
  close_channel(go);
}

/* An exec'd child that probes the lock through its own open. */
static void exec_probe(const char* s, const char* what, const char* want) {
  struct channel report = channel();
  pid_t child = start();
  if (child == 0) {
    char fd_text[16];
    snprintf(fd_text, sizeof(fd_text), "%d", report.write_end);
    /* The probe writes to the report pipe, which must survive the exec. */
    int flags = fcntl(report.write_end, F_GETFD);
    fcntl(report.write_end, F_SETFD, flags & ~FD_CLOEXEC);
    /* argv[0] names this program where it has a slash; otherwise the
       kernel's link to the running image does. */
    const char* image = strchr(self_path, '/') != NULL ? self_path : "/proc/self/exe";
    char* argv[] = {(char*)self_path, "probe", lock_path, fd_text, NULL};
    execv(image, argv);
    _exit(HARNESS_FAILURE);
  }
  char line[64];
  hear(report, line, sizeof(line));
  expect(s, what, line, want);
  reap(child);
  close_channel(report);
}

/* /proc/locks lists a blocked request under the lock it waits for (`->`,
   `locks_show`), until the release grants it. */
static void scenario_proc_locks_waiter(void) {
  const char* s = "proc-locks-waiter";
  char rows[128];
  learn_whose_rows_show();
  int held = open_lock();
  expect(s, "parent LOCK_SH", attempt(held, LOCK_SH), "ok");
  struct channel report = channel();
  struct channel go = channel();
  pid_t child = start();
  if (child == 0) {
    int fd = open_lock();
    char line[64];
    say(report, attempt(fd, LOCK_EX));
    hear(go, line, sizeof(line));
    _exit(0);
  }
  proc_locks_rows_once(rows, sizeof(rows), "READ ->WRITE");
  expect(s, "rows while the child waits for LOCK_EX", rows, "READ ->WRITE");
  expect(s, "parent LOCK_UN", attempt(held, LOCK_UN), "ok");
  char line[64];
  hear(report, line, sizeof(line));
  expect(s, "child's LOCK_EX after the release", line, "ok");
  proc_locks_rows(rows, sizeof(rows));
  expect(s, "rows while the child holds LOCK_EX", rows, "WRITE");
  say(go, "exit");
  reap(child);
  proc_locks_rows(rows, sizeof(rows));
  expect(s, "rows after the child's exit", rows, "none");
  close(held);
  close_channel(report);
  close_channel(go);
}

/* The exec'd children lock a file of their own, which no other scenario
   shares: where an exec rebuilds Detcore's view of a descriptor table (DBT,
   SaBRe), Detcore leaves a file a process that exec'd held a lock on to the
   kernel for the rest of the run. */
static void scenario_lock_before_children(void) {
  const char* s = "lock-before-children";
  char shared_path[sizeof(lock_path)];
  memcpy(shared_path, lock_path, sizeof(lock_path));
  snprintf(lock_path, sizeof(lock_path), "%s/lock-exec", lock_dir);
  int created = open(lock_path, O_RDWR | O_CREAT | O_EXCL | O_CLOEXEC, 0600);
  if (created < 0) {
    harness("create the exec'd children's lock file");
  }
  close(created);
  int held = open_lock();
  expect(s, "parent LOCK_EX", attempt(held, LOCK_EX), "ok");
  exec_probe(s, "exec'd child LOCK_EX|LOCK_NB", "EWOULDBLOCK");
  exec_probe(s, "second exec'd child LOCK_EX|LOCK_NB", "EWOULDBLOCK");
  expect(s, "parent LOCK_UN", attempt(held, LOCK_UN), "ok");
  exec_probe(s, "exec'd child LOCK_EX|LOCK_NB after the unlock", "ok");
  close(held);
  unlink(lock_path);
  memcpy(lock_path, shared_path, sizeof(lock_path));
}

static void on_signal(int signal) {
  (void)signal;
}

struct thread_wait {
  struct channel report;
};

static void* wait_for_lock(void* argument) {
  struct thread_wait* wait = argument;
  int fd = open_lock();
  say(wait->report, "blocking");
  say(wait->report, attempt(fd, LOCK_EX));
  close(fd);
  return NULL;
}

static pthread_t start_waiting_thread(struct thread_wait* wait) {
  pthread_t thread;
  if (pthread_create(&thread, NULL, wait_for_lock, wait) != 0) {
    harness("pthread_create");
  }
  char line[64];
  hear(wait->report, line, sizeof(line));
  return thread;
}

static void finish_waiting_thread(pthread_t thread, struct thread_wait* wait) {
  if (pthread_join(thread, NULL) != 0) {
    harness("pthread_join");
  }
  close_channel(wait->report);
}

/* A waiter a signal reaches: without SA_RESTART its flock fails with EINTR
   and it leaves the queue; with it, the call starts again and is granted at
   the holder's release. The waiter is another thread of the holder's process
   with a description of its own, and the signal is directed at that thread
   (pthread_kill). */
static void scenario_signal_ends_or_restarts_a_wait(int restart) {
  const char* s = restart ? "signal-restarts" : "signal-interrupts";
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = on_signal;
  action.sa_flags = restart ? SA_RESTART : 0;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGUSR1, &action, NULL) != 0) {
    harness("sigaction");
  }
  int held = open_lock();
  expect(s, "main thread LOCK_EX", attempt(held, LOCK_EX), "ok");
  struct thread_wait wait = {channel()};
  pthread_t thread = start_waiting_thread(&wait);
  sleep_ms(100);
  if (pthread_kill(thread, SIGUSR1) != 0) {
    harness("pthread_kill");
  }
  char line[64];
  if (restart) {
    /* The restarted call blocks again before the release. */
    sleep_ms(100);
    expect(s, "main thread LOCK_UN", attempt(held, LOCK_UN), "ok");
    hear(wait.report, line, sizeof(line));
    expect(s, "the other thread's restarted LOCK_EX after the release", line, "ok");
  } else {
    hear(wait.report, line, sizeof(line));
    expect(s, "the other thread's interrupted LOCK_EX", line, "errno=4");
    expect(s, "main thread LOCK_UN", attempt(held, LOCK_UN), "ok");
    /* The interrupted waiter left the queue: nothing holds the lock. */
    probe(s, "a child's LOCK_EX|LOCK_NB after the release", LOCK_EX | LOCK_NB, "ok");
  }
  finish_waiting_thread(thread, &wait);
  close(held);
  action.sa_handler = SIG_DFL;
  action.sa_flags = 0;
  if (sigaction(SIGUSR1, &action, NULL) != 0) {
    harness("sigaction");
  }
}

/* A thread of the holder's own process waits through a description of its
   own, and the holder's thread releases. */
static void scenario_thread_waiter(void) {
  const char* s = "thread-waiter";
  int held = open_lock();
  expect(s, "main thread LOCK_EX", attempt(held, LOCK_EX), "ok");
  struct thread_wait wait = {channel()};
  pthread_t thread = start_waiting_thread(&wait);
  sleep_ms(100);
  expect(s, "main thread LOCK_UN", attempt(held, LOCK_UN), "ok");
  char line[64];
  hear(wait.report, line, sizeof(line));
  expect(s, "the other thread's blocking LOCK_EX after the release", line, "ok");
  finish_waiting_thread(thread, &wait);
  close(held);
}

static int scenario_probe(const char* path, const char* fd_text) {
  int report = atoi(fd_text);
  int fd = open(path, O_RDWR | O_CLOEXEC);
  if (fd < 0) {
    return HARNESS_FAILURE;
  }
  const char* got = attempt(fd, LOCK_EX | LOCK_NB);
  char line[64];
  snprintf(line, sizeof(line), "%s\n", got);
  if (write(report, line, strlen(line)) != (ssize_t)strlen(line)) {
    return HARNESS_FAILURE;
  }
  return 0;
}

static void scenario_mmap_close(void) {
  const char* s = "mmap-close";
  int held = open_lock();
  if (ftruncate(held, 4096) != 0) {
    harness("ftruncate");
  }
  expect(s, "parent LOCK_EX", attempt(held, LOCK_EX), "ok");
  void* mapping = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, held, 0);
  if (mapping == MAP_FAILED) {
    harness("mmap");
  }
  /* The mapping keeps the description, and its lock, after the last close. */
  close(held);
  probe(s, "child LOCK_EX|LOCK_NB with the mapping left", LOCK_EX | LOCK_NB, "EWOULDBLOCK");
  if (munmap(mapping, 4096) != 0) {
    harness("munmap");
  }
  probe(s, "child LOCK_EX|LOCK_NB after munmap", LOCK_EX | LOCK_NB, "ok");
}

static void scenario_shell(void) {
  const char* s = "shell";
  /* exec 9>lockfile */
  int opened = open_lock();
  if (dup2(opened, 9) != 9) {
    harness("dup2 to 9");
  }
  close(opened);
  struct channel report = channel();
  struct channel go = channel();
  pid_t child = start();
  if (child == 0) {
    /* flock -n 9, holding the lock until told to exit. */
    char line[64];
    say(report, attempt(9, LOCK_EX | LOCK_NB));
    hear(go, line, sizeof(line));
    _exit(0);
  }
  char line[64];
  hear(report, line, sizeof(line));
  expect(s, "child flock -n 9", line, "ok");
  probe(s, "another child LOCK_EX|LOCK_NB while it holds the lock", LOCK_EX | LOCK_NB,
        "EWOULDBLOCK");
  say(go, "exit");
  reap(child);
  /* The parent's descriptor 9 still holds the description: the lock stays. */
  probe(s, "another child LOCK_EX|LOCK_NB after it exits", LOCK_EX | LOCK_NB, "EWOULDBLOCK");
  close(9);
  probe(s, "another child LOCK_EX|LOCK_NB after the parent closes 9", LOCK_EX | LOCK_NB, "ok");
  close_channel(report);
  close_channel(go);
}

static void scenario_signal_interrupts(void) {
  scenario_signal_ends_or_restarts_a_wait(0);
}

static void scenario_signal_restarts(void) {
  scenario_signal_ends_or_restarts_a_wait(1);
}

/* The scenarios `all` runs, in order; each also runs alone by its name. */
static const struct {
  const char* name;
  void (*run)(void);
} scenarios[] = {
    {"exclusive", scenario_exclusive_excludes},
    {"shared", scenario_shared_shares},
    {"waiter", scenario_waiter_woken_at_release},
    {"last-close", scenario_released_at_last_close},
    {"exit", scenario_released_at_exit},
    {"fork", scenario_fork_shares_the_description},
    {"downgrade", scenario_downgrade_wakes_a_shared_waiter},
    {"wake-order", scenario_wake_order},
    {"mixed-waiters", scenario_mixed_waiters},
    {"proc-locks", scenario_proc_locks},
    {"proc-locks-waiter", scenario_proc_locks_waiter},
    {"lock-before-children", scenario_lock_before_children},
    {"signal-interrupts", scenario_signal_interrupts},
    {"signal-restarts", scenario_signal_restarts},
    {"thread-waiter", scenario_thread_waiter},
};

int main(int argc, char** argv) {
  setvbuf(stdout, NULL, _IOLBF, 0);
  self_path = argv[0];
  const char* scenario = argc > 1 ? argv[1] : "all";
  if (strcmp(scenario, "probe") == 0) {
    return argc > 3 ? scenario_probe(argv[2], argv[3]) : HARNESS_FAILURE;
  }
  const char* tmp = getenv("TMPDIR");
  snprintf(lock_dir, sizeof(lock_dir), "%s/flock-se-XXXXXX", tmp != NULL && tmp[0] != '\0' ? tmp : "/tmp");
  if (mkdtemp(lock_dir) == NULL) {
    harness("mkdtemp");
  }
  snprintf(lock_path, sizeof(lock_path), "%s/lock", lock_dir);
  int created = open(lock_path, O_RDWR | O_CREAT | O_EXCL | O_CLOEXEC, 0600);
  if (created < 0) {
    harness("create the lock file");
  }
  close(created);
  int ran = 0;
  for (size_t i = 0; i < sizeof(scenarios) / sizeof(scenarios[0]); i++) {
    if (strcmp(scenario, "all") == 0 || strcmp(scenario, scenarios[i].name) == 0) {
      scenarios[i].run();
      ran = 1;
    }
  }
  if (!ran && strcmp(scenario, "mmap-close") == 0) {
    scenario_mmap_close();
  } else if (!ran && strcmp(scenario, "shell") == 0) {
    scenario_shell();
  } else if (!ran) {
    printf("HARNESS: unknown scenario %s\n", scenario);
    failures = -1;
  }
  unlink(lock_path);
  rmdir(lock_dir);
  if (failures < 0) {
    return HARNESS_FAILURE;
  }
  printf("flock-shared-exclusive %s: %s\n", scenario, failures == 0 ? "ok" : "FAILED");
  return failures == 0 ? 0 : 1;
}
