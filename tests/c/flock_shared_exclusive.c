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
 * signal that ends or restarts a waiting call, a waiting thread woken by a
 * release in another thread of its process, a waiting call whose descriptor a
 * sibling closes or replaces with another file's, which keeps the open file
 * description it took, and the wake order of Linux's tree of blocked requests
 * after a blocking upgrade, an interrupted waiter, a retry that converts its
 * description's lock, and a release through a waiting call's own description.
 *
 * Every line printed is the same natively and under Hermit, on every backend:
 * all output comes from the parent, at points its pipes order, and no outcome
 * depends on timing (a sleep only makes a waiter likely to block before the
 * release natively; it acquires after the release either way; where the
 * order matters, the parent waits until /proc/locks shows the request
 * blocked). Where two outcomes are Linux's, as the schedule has it, the guest
 * prints whether it saw one of them. The wake order is Linux's too: a release
 * wakes the requests blocked directly under the lock, each of which retries,
 * and a request blocks under the first granted lock it conflicts with, below
 * the requests there that conflict with it (`__locks_insert_block`), so a
 * second exclusive waiter is granted after the first.
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
 *   private-table-close  a thread with a descriptor table of its own, the
 *                parent and a forked child close every descriptor of a locked
 *                description, after which a fresh open takes the lock.
 *                Hermit leaves the file to the kernel where the backend has
 *                host descriptions, and KVM refuses the thread by name.
 *   exec-waiter  a thread waits for LOCK_EX behind its process's lock,
 *                another process waits behind that thread's request, and
 *                the first process execs: the exec kills the waiting thread,
 *                which wakes the other process's request, and closes the
 *                lock's descriptor, so the other process takes the lock. KVM
 *                hangs at an exec while another thread of the process is
 *                blocked, flock or not.
 *   exec-report FD  (internal) the new image of exec-waiter's exec, which
 *                reports on FD.
 *   stopped-waiter  a job-control stop takes a waiter out of the queue: a
 *                release grants the other waiter alone, and the stopped one
 *                queues again once SIGCONT continues it. Hermit refuses the
 *                stopped waiter's call by name where the stop reaches it, as
 *                Detcore does not represent job-control stops
 *                (https://github.com/rrnewton/hermit/issues/4944), and KVM
 *                refuses the stop by name.
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
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
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
   and this program's rows are told apart by the lock file's full
   MAJ:MIN:INO object (below); Hermit shows only the guest's rows, and
   numbers their identities itself, so there the object cannot tell them
   apart and need not. */
static int all_flock_rows_are_ours;

/* The lock file's MAJ:MIN:INO as /proc/locks prints it, learned once per
   lock file from a row this test created, whoever holds it: a row whose
   inode is the file's st_ino and whose pid is this process or one of its
   descendants (in exec-waiter only children and threads ever hold or wait
   on the lock, so own-pid rows may never exist). The device cannot come
   from stat(): on btrfs, st_dev is the subvolume's anonymous device while
   /proc/locks prints the superblock's, and an inode number alone also
   matches unrelated files on other filesystems that reuse it (a native
   false red: a foreign shared flock on a same-inode file counted as a
   third holder here). If no descendant row is visible yet and exactly one
   triple carries the file's inode, that triple is unambiguous and is
   learned instead. */
static char lock_object[64];

/* Whether `pid` is this process or a descendant of it, by /proc parentage. */
static int pid_is_ours(long pid) {
  long own = (long)getpid();
  for (int hops = 0; hops < 32 && pid > 1; hops++) {
    if (pid == own) {
      return 1;
    }
    char path[64];
    snprintf(path, sizeof(path), "/proc/%ld/stat", pid);
    FILE* stat_file = fopen(path, "re");
    if (stat_file == NULL) {
      return 0;
    }
    char text[512];
    size_t length = fread(text, 1, sizeof(text) - 1, stat_file);
    fclose(stat_file);
    text[length] = '\0';
    char* close_paren = strrchr(text, ')');
    long ppid = 0;
    if (close_paren == NULL || sscanf(close_paren + 1, "%*s %ld", &ppid) != 1) {
      return 0;
    }
    pid = ppid;
  }
  return pid == own;
}

/* The access of each /proc/locks row on the lock file, in row order, a
   waiter's (`->`) after `->`: one read of /proc/locks. */
static void proc_locks_snapshot(char* out, size_t size) {
  struct stat file;
  if (stat(lock_path, &file) != 0) {
    harness("stat the lock file");
  }
  FILE* locks = fopen("/proc/locks", "re");
  if (locks == NULL) {
    harness("open /proc/locks");
  }
  /* Rows are collected first: this process's own row, which teaches
     lock_object, need not come before the rows it must distinguish. */
  struct parsed_row {
    char access[16];
    char object[64];
    long pid;
    int waits;
  };
  static struct parsed_row parsed[4096];
  int count = 0;
  char row[256];
  while (fgets(row, sizeof(row), locks) != NULL) {
    char kind[16];
    char waiter[4];
    char mode[16];
    char pid[16];
    /* N: [->] FLOCK ADVISORY ACCESS PID MAJ:MIN:INO START END */
    int waits = sscanf(row, "%*s %3s", waiter) == 1 && strcmp(waiter, "->") == 0;
    const char* format =
        waits ? "%*s %*s %15s %15s %15s %15s %63s" : "%*s %15s %15s %15s %15s %63s";
    if (count >= (int)(sizeof(parsed) / sizeof(parsed[0])) ||
        sscanf(row, format, kind, mode, parsed[count].access, pid, parsed[count].object) != 5 ||
        strcmp(kind, "FLOCK") != 0) {
      continue;
    }
    parsed[count].waits = waits;
    parsed[count].pid = strtol(pid, NULL, 10);
    count++;
  }
  fclose(locks);
  if (lock_object[0] == '\0') {
    /* Learn the file's triple: first from a holder row this test created
       (descendant pid, matching inode), then from such a waiter row, and
       finally from the only triple carrying the inode, if there is one. */
    const char* unique = NULL;
    int ambiguous = 0;
    for (int pass = 0; pass < 3 && lock_object[0] == '\0'; pass++) {
      for (int index = 0; index < count; index++) {
        const char* colon = strrchr(parsed[index].object, ':');
        if (colon == NULL ||
            strtoull(colon + 1, NULL, 10) != (unsigned long long)file.st_ino) {
          continue;
        }
        if (pass == 2) {
          if (unique == NULL) {
            unique = parsed[index].object;
          } else if (strcmp(unique, parsed[index].object) != 0) {
            ambiguous = 1;
          }
          continue;
        }
        if ((pass == 0) != (parsed[index].waits != 0) &&
            pid_is_ours(parsed[index].pid)) {
          snprintf(lock_object, sizeof(lock_object), "%s", parsed[index].object);
          break;
        }
      }
    }
    if (lock_object[0] == '\0' && unique != NULL && !ambiguous) {
      snprintf(lock_object, sizeof(lock_object), "%s", unique);
    }
  }
  out[0] = '\0';
  for (int index = 0; index < count; index++) {
    if (!all_flock_rows_are_ours &&
        (lock_object[0] == '\0' || strcmp(parsed[index].object, lock_object) != 0)) {
      continue;
    }
    if (out[0] != '\0') {
      strncat(out, " ", size - strlen(out) - 1);
    }
    if (parsed[index].waits) {
      strncat(out, "->", size - strlen(out) - 1);
    }
    strncat(out, parsed[index].access, size - strlen(out) - 1);
  }
  if (out[0] == '\0') {
    snprintf(out, size, "none");
  }
}

/* proc_locks_rows as read everywhere above: a one-shot read of a long
   /proc/locks (~1,300 rows on a busy host) can race lock changes and tear,
   so re-read until two consecutive snapshots agree on the matched rows.
   Bounded and counted; on exhaustion the last snapshot stands, which is
   what a one-shot read would have returned. */
static void proc_locks_rows(char* out, size_t size) {
  static char first[1024];
  static char second[1024];
  if (size > sizeof(first)) {
    proc_locks_snapshot(out, size);
    return;
  }
  int attempts = 0;
  for (;;) {
    attempts++;
    proc_locks_snapshot(first, size);
    proc_locks_snapshot(second, size);
    if (strcmp(first, second) == 0 || attempts >= 250) {
      size_t length = strnlen(second, size - 1);
      memcpy(out, second, length);
      out[length] = '\0';
      return;
    }
    sleep_ms(10);
  }
}

/* proc_locks_rows once two consecutive scans agree, or after 5 seconds.
   /proc/locks is a seq_file over locks belonging to every process; while it
   is being read, unrelated lock churn can shift the traversal and cause one
   scan to skip or repeat this file's row even though this scenario's locks
   are stable at a channel-synchronized point. Full-object matching (above)
   cannot repair an incoherent scan, so settle on agreement rather than let
   one snapshot decide. The last scan is returned if they never agree.
   (Measured at the full-object head without this: 3 failures in 1000 native
   runs, two missing rows and one duplicated WRITE; with it, 0 in 1000.) */
static void proc_locks_rows_settled(char* out, size_t size) {
  char previous[128];
  proc_locks_rows(previous, sizeof(previous));
  for (int attempt = 0; attempt < 500; attempt++) {
    sleep_ms(10);
    proc_locks_rows(out, size);
    if (strcmp(out, previous) == 0) {
      return;
    }
    size_t length = strnlen(out, sizeof(previous) - 1);
    memcpy(previous, out, length);
    previous[length] = '\0';
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
  proc_locks_rows_settled(rows, sizeof(rows));
  expect(s, "rows with two shared holders", rows, "READ READ");
  say(go, "exit");
  reap(child);
  proc_locks_rows_settled(rows, sizeof(rows));
  expect(s, "rows after the child's exit", rows, "READ");
  expect(s, "parent converts to LOCK_EX", attempt(held, LOCK_EX), "ok");
  proc_locks_rows_settled(rows, sizeof(rows));
  expect(s, "rows after the conversion", rows, "WRITE");
  close(held);
  proc_locks_rows_settled(rows, sizeof(rows));
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

/* A waiting thread that waits on a descriptor the main thread opened for it,
   and leaves the descriptor to the main thread. */
struct fd_wait {
  struct channel report;
  int fd;
};

static void* wait_on_descriptor(void* argument) {
  struct fd_wait* wait = argument;
  say(wait->report, "blocking");
  say(wait->report, attempt(wait->fd, LOCK_EX));
  return NULL;
}

static pthread_t start_descriptor_waiter(struct fd_wait* wait) {
  pthread_t thread;
  if (pthread_create(&thread, NULL, wait_on_descriptor, wait) != 0) {
    harness("pthread_create");
  }
  char line[64];
  hear(wait->report, line, sizeof(line));
  return thread;
}

/* A child's LOCK_NB attempt through its own open of `path`. */
static void probe_path(const char* s, const char* what, const char* path, int operation,
                       const char* want) {
  char saved[sizeof(lock_path)];
  memcpy(saved, lock_path, sizeof(lock_path));
  snprintf(lock_path, sizeof(lock_path), "%s", path);
  probe(s, what, operation, want);
  memcpy(lock_path, saved, sizeof(lock_path));
}

/* An OFD lock on a file of its own, unrelated to the lock file, which
   waiter-closed, waiter-replaced and ebadf hold while they check that its
   /proc/locks row stays. Its range starts at 4096 plus the process's pid,
   so that no other process's lock has it, natively too, where /proc/locks
   lists every process's locks and other runs of this program may hold
   theirs; /proc/locks prints the range as the row's last two fields. */
static long unrelated_start(void) {
  return 4096 + (long)getpid();
}

static int take_unrelated_ofd_lock(char* path, size_t size) {
  snprintf(path, size, "%s/lock-unrelated", lock_dir);
  int unrelated = open(path, O_RDWR | O_CREAT | O_EXCL | O_CLOEXEC, 0600);
  struct flock range = {
      .l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = unrelated_start(), .l_len = 23};
  if (unrelated < 0 || fcntl(unrelated, F_OFD_SETLK, &range) != 0) {
    harness("take the unrelated OFD lock");
  }
  return unrelated;
}

/* The /proc/locks rows of that lock, as a word. */
static void unrelated_ofd_rows(char* out, size_t size) {
  char range[48];
  snprintf(range, sizeof(range), " %ld %ld\n", unrelated_start(), unrelated_start() + 22);
  FILE* locks = fopen("/proc/locks", "re");
  if (locks == NULL) {
    harness("open /proc/locks");
  }
  int count = 0;
  char row[256];
  while (fgets(row, sizeof(row), locks) != NULL) {
    if (strstr(row, "OFDLCK") != NULL && strstr(row, range) != NULL) {
      count++;
    }
  }
  fclose(locks);
  snprintf(out, size, "%d", count);
}

/* A sibling closes, or replaces with another file's, the descriptor a waiting
   call took. Linux's call holds the open file description it took
   (`fdget`): it is granted at the release and returns 0, and its lock goes
   with its own reference, the description's last; the other file is never
   locked. An OFD lock the process holds on an unrelated file keeps its
   /proc/locks row throughout (the Codex review of slice SP2's second round:
   the call's completion read the stale descriptor number, which hid every
   lock's row). */
static void scenario_waiter_descriptor_gone(int replaced) {
  const char* s = replaced ? "waiter-replaced" : "waiter-closed";
  char rows[128];
  char count[16];
  char other[sizeof(lock_path)];
  char unrelated_path[sizeof(lock_path)];
  snprintf(other, sizeof(other), "%s/lock-other", lock_dir);
  int unrelated = take_unrelated_ofd_lock(unrelated_path, sizeof(unrelated_path));
  unrelated_ofd_rows(count, sizeof(count));
  expect(s, "the unrelated OFD lock's rows before the wait", count, "1");
  learn_whose_rows_show();
  int held = open_lock();
  expect(s, "main thread LOCK_EX", attempt(held, LOCK_EX), "ok");
  struct fd_wait wait = {channel(), open_lock()};
  /* The waiting descriptor is a number no open takes meanwhile, as in the
     review's probe (200). The lowest free number goes to the next open,
     Hermit's own included; on ptrace that open masked the defect this
     checks. */
  int high = fcntl(wait.fd, F_DUPFD_CLOEXEC, 200);
  if (high < 0 || close(wait.fd) != 0) {
    harness("move the waiting descriptor to 200");
  }
  wait.fd = high;
  pthread_t thread = start_descriptor_waiter(&wait);
  proc_locks_rows_once(rows, sizeof(rows), "WRITE ->WRITE");
  expect(s, "rows while the other thread waits", rows, "WRITE ->WRITE");
  if (replaced) {
    int replacement = open(other, O_RDWR | O_CREAT | O_EXCL | O_CLOEXEC, 0600);
    if (replacement < 0 || dup2(replacement, wait.fd) != wait.fd) {
      harness("replace the waiting descriptor");
    }
    close(replacement);
    printf("%s: main thread replaced the waiting descriptor with another file's\n", s);
  } else {
    if (close(wait.fd) != 0) {
      harness("close the waiting descriptor");
    }
    printf("%s: main thread closed the waiting descriptor\n", s);
  }
  expect(s, "main thread LOCK_UN", attempt(held, LOCK_UN), "ok");
  char line[64];
  hear(wait.report, line, sizeof(line));
  expect(s, "the waiting call after the release", line, "ok");
  if (pthread_join(thread, NULL) != 0) {
    harness("pthread_join");
  }
  close_channel(wait.report);
  unrelated_ofd_rows(count, sizeof(count));
  expect(s, "the unrelated OFD lock's rows after the waiting call returned", count, "1");
  /* The call's own reference was the description's last. */
  probe(s, "child LOCK_EX|LOCK_NB after the call returned", LOCK_EX | LOCK_NB, "ok");
  if (replaced) {
    probe_path(s, "child LOCK_EX|LOCK_NB on the other file", other, LOCK_EX | LOCK_NB, "ok");
    close(wait.fd);
    unlink(other);
  }
  close(held);
  close(unrelated);
  unlink(unrelated_path);
}

static void scenario_waiter_closed(void) {
  scenario_waiter_descriptor_gone(0);
}

static void scenario_waiter_replaced(void) {
  scenario_waiter_descriptor_gone(1);
}

/* Linux answers a flock before it reaches any lock list where the command
   is LOCK_MAND (0, before anything else), where the command is invalid
   (EINVAL, before the descriptor), and, through a descriptor number that
   names nothing, with EBADF (`SYSCALL_DEFINE2(flock)`). Through descriptor
   250, which names nothing, each changes no lock, and an OFD lock the
   process holds on an unrelated file keeps its /proc/locks row (found in
   slice SP2's third round, the EBADF case on main too: the call's completion
   read the number's fdinfo, found none, and hid every lock's row; the other
   two in the Claude review of that round). Not in `all`: replay refuses a
   flock through a descriptor outside its root, an EBADF one included;
   hermit-cli/tests/flock_exclusion.rs runs it. */
static void scenario_ebadf(void) {
  const char* s = "ebadf";
  char count[16];
  char unrelated_path[sizeof(lock_path)];
  int unrelated = take_unrelated_ofd_lock(unrelated_path, sizeof(unrelated_path));
  unrelated_ofd_rows(count, sizeof(count));
  expect(s, "the unrelated OFD lock's rows", count, "1");
  expect(s, "LOCK_EX|LOCK_NB through descriptor 250, which names nothing",
         attempt(250, LOCK_EX | LOCK_NB), "errno=9");
  unrelated_ofd_rows(count, sizeof(count));
  expect(s, "the unrelated OFD lock's rows after that EBADF", count, "1");
  expect(s, "LOCK_SH|LOCK_EX through descriptor 250", attempt(250, LOCK_SH | LOCK_EX),
         "errno=22");
  unrelated_ofd_rows(count, sizeof(count));
  expect(s, "the unrelated OFD lock's rows after that EINVAL", count, "1");
  expect(s, "LOCK_MAND through descriptor 250", attempt(250, LOCK_MAND), "ok");
  unrelated_ofd_rows(count, sizeof(count));
  expect(s, "the unrelated OFD lock's rows after that LOCK_MAND", count, "1");
  close(unrelated);
  unlink(unrelated_path);
}

/* A sibling closes the descriptor a waiting call took while a mapping of its
   open file description remains (the Claude review of slice SP2's second
   round). Linux's call takes the lock at the release, and the mapping keeps
   the description, and with it the lock, after the call's own reference
   goes (`locks_remove_file` runs at the last `fput`): another description's
   LOCK_EX|LOCK_NB fails until the munmap. Detcore cannot place that lock on
   the host without a descriptor that names the description, so the
   backends with host descriptions refuse the call by name, and KVM refuses
   the thread of a process with a shared file mapping. Not in `all`:
   hermit-cli/tests/flock_exclusion.rs runs it. */
static void scenario_waiter_mapped(void) {
  const char* s = "waiter-mapped";
  char rows[128];
  learn_whose_rows_show();
  int held = open_lock();
  if (ftruncate(held, 4096) != 0) {
    harness("ftruncate");
  }
  expect(s, "main thread LOCK_EX", attempt(held, LOCK_EX), "ok");
  struct fd_wait wait = {channel(), open_lock()};
  void* mapping = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, wait.fd, 0);
  if (mapping == MAP_FAILED) {
    harness("mmap");
  }
  pthread_t thread = start_descriptor_waiter(&wait);
  proc_locks_rows_once(rows, sizeof(rows), "WRITE ->WRITE");
  expect(s, "rows while the other thread waits", rows, "WRITE ->WRITE");
  if (close(wait.fd) != 0) {
    harness("close the waiting descriptor");
  }
  printf("%s: main thread closed the waiting descriptor; its mapping remains\n", s);
  expect(s, "main thread LOCK_UN", attempt(held, LOCK_UN), "ok");
  char line[64];
  hear(wait.report, line, sizeof(line));
  expect(s, "the waiting call after the release", line, "ok");
  if (pthread_join(thread, NULL) != 0) {
    harness("pthread_join");
  }
  close_channel(wait.report);
  /* The mapping keeps the description, and the lock the call took. */
  probe(s, "child LOCK_EX|LOCK_NB while the mapping remains", LOCK_EX | LOCK_NB, "EWOULDBLOCK");
  if (munmap(mapping, 4096) != 0) {
    harness("munmap");
  }
  probe(s, "child LOCK_EX|LOCK_NB after the munmap", LOCK_EX | LOCK_NB, "ok");
  close(held);
}

/* The thread of private-table-close: CLONE_THREAD without CLONE_FILES gives it
   a copy of the descriptor table, in which it closes the parent's
   descriptor. */
static volatile int private_tid = 1;
static int private_close_status = -1;
static char private_stack[256 * 1024] __attribute__((aligned(16)));

static int close_in_private_table(void* argument) {
  private_close_status = close(*(int*)argument);
  return 0;
}

static void scenario_private_table_close(void) {
  const char* s = "private-table-close";
  int held = open_lock();
  expect(s, "parent LOCK_EX", attempt(held, LOCK_EX), "ok");
  struct channel report = channel();
  struct channel go = channel();
  pid_t child = start();
  if (child == 0) {
    char line[64];
    hear(go, line, sizeof(line));
    close(held);
    int fd = open_lock();
    say(report, attempt(fd, LOCK_EX | LOCK_NB));
    _exit(0);
  }
  int created = clone(close_in_private_table, private_stack + sizeof(private_stack),
                      CLONE_VM | CLONE_SIGHAND | CLONE_THREAD | CLONE_CHILD_SETTID |
                          CLONE_CHILD_CLEARTID,
                      &held, NULL, NULL, (pid_t*)&private_tid);
  if (created < 0) {
    harness("clone a thread with a descriptor table of its own");
  }
  for (int current = private_tid; current != 0; current = private_tid) {
    syscall(SYS_futex, (int*)&private_tid, FUTEX_WAIT, current, NULL, NULL, 0);
  }
  expect(s, "the private-table thread's close", private_close_status == 0 ? "ok" : "failed", "ok");
  close(held);
  printf("%s: parent closed its descriptor\n", s);
  say(go, "close");
  char line[64];
  hear(report, line, sizeof(line));
  expect(s, "child's fresh LOCK_EX|LOCK_NB after every descriptor closed", line, "ok");
  reap(child);
  close_channel(report);
  close_channel(go);
}

/* A job-control stop takes a waiter out of the queue (`flock_lock_inode_wait`
   returns -ERESTARTSYS and the stopped task waits for SIGCONT before the call
   restarts): /proc/locks shows the other waiter alone, the release grants it
   alone, and the stopped waiter queues again once SIGCONT continues it. */
static void scenario_stopped_waiter(void) {
  const char* s = "stopped-waiter";
  char rows[128];
  learn_whose_rows_show();
  int held = open_lock();
  expect(s, "parent LOCK_EX", attempt(held, LOCK_EX), "ok");
  struct channel reports[2] = {channel(), channel()};
  struct channel goes[2] = {channel(), channel()};
  pid_t waiters[2];
  const char* queued[2] = {"WRITE ->READ", "WRITE ->READ ->READ"};
  for (int index = 0; index < 2; index++) {
    waiters[index] = start();
    if (waiters[index] == 0) {
      int fd = open_lock();
      char line[64];
      say(reports[index], attempt(fd, LOCK_SH));
      hear(goes[index], line, sizeof(line));
      _exit(0);
    }
    proc_locks_rows_once(rows, sizeof(rows), queued[index]);
    expect(s, index == 0 ? "rows with one waiter" : "rows with two waiters", rows, queued[index]);
  }
  int status;
  if (kill(waiters[1], SIGSTOP) != 0 || waitpid(waiters[1], &status, WUNTRACED) != waiters[1] ||
      !WIFSTOPPED(status)) {
    harness("stop the second waiter");
  }
  proc_locks_rows_once(rows, sizeof(rows), "WRITE ->READ");
  expect(s, "rows while the second waiter is stopped", rows, "WRITE ->READ");
  expect(s, "parent LOCK_UN", attempt(held, LOCK_UN), "ok");
  char line[64];
  hear(reports[0], line, sizeof(line));
  expect(s, "first waiter's LOCK_SH after the release", line, "ok");
  proc_locks_rows_once(rows, sizeof(rows), "READ");
  expect(s, "rows while the second waiter is still stopped", rows, "READ");
  if (kill(waiters[1], SIGCONT) != 0) {
    harness("continue the second waiter");
  }
  hear(reports[1], line, sizeof(line));
  expect(s, "second waiter's LOCK_SH once continued", line, "ok");
  for (int index = 0; index < 2; index++) {
    say(goes[index], "exit");
    reap(waiters[index]);
    close_channel(reports[index]);
    close_channel(goes[index]);
  }
  close(held);
}

/* proc_locks_rows with the waiter rows (`->`) after each granted lock sorted.
   Linux lists a lock's blocked requests depth first, each list in the order
   its requests blocked (`locks_show`), with a waiter's depth in its
   indentation; Detcore's /proc/locks sanitizer orders a lock's waiter rows
   by their fields and indents each by one level (`sanitize_locks`, older
   than the flock model: https://github.com/rrnewton/hermit/issues/5052). So
   the scenarios below with more than one waiter compare this form, whose
   rows show which locks are granted and which requests wait under them, and
   whose events show the order of the grants. */
static void proc_locks_sorted(char* out, size_t size) {
  char rows[256];
  proc_locks_rows(rows, sizeof(rows));
  char* tokens[32];
  int count = 0;
  for (char* token = strtok(rows, " "); token != NULL && count < 32; token = strtok(NULL, " ")) {
    tokens[count++] = token;
  }
  for (int start = 0; start < count; start++) {
    if (strncmp(tokens[start], "->", 2) == 0) {
      continue;
    }
    int end = start + 1;
    while (end < count && strncmp(tokens[end], "->", 2) == 0) {
      end++;
    }
    for (int i = start + 1; i < end; i++) {
      for (int j = i + 1; j < end; j++) {
        if (strcmp(tokens[j], tokens[i]) < 0) {
          char* swap = tokens[i];
          tokens[i] = tokens[j];
          tokens[j] = swap;
        }
      }
    }
  }
  out[0] = '\0';
  for (int index = 0; index < count; index++) {
    if (index > 0) {
      strncat(out, " ", size - strlen(out) - 1);
    }
    strncat(out, tokens[index], size - strlen(out) - 1);
  }
}

/* proc_locks_sorted once it is `want`, or after 5 seconds. */
static void proc_locks_sorted_once(char* out, size_t size, const char* want) {
  for (int attempt = 0; attempt < 500; attempt++) {
    proc_locks_sorted(out, size);
    if (strcmp(out, want) == 0) {
      return;
    }
    sleep_ms(10);
  }
}

/* The number of waiter rows (`->`) /proc/locks shows for the lock file:
   synchronization where more than one lock is granted, whose rows the kernel
   lists in an order of its own (its per-CPU lists of granted locks). */
static int waiter_rows(void) {
  char rows[256];
  proc_locks_rows(rows, sizeof(rows));
  int count = 0;
  for (const char* at = strstr(rows, "->"); at != NULL; at = strstr(at + 2, "->")) {
    count++;
  }
  return count;
}

/* Wait until /proc/locks shows `want` waiter rows for the lock file, for at
   most 5 seconds, as proc_locks_rows_once does. */
static void waiter_rows_once(const char* s, int want) {
  for (int attempt = 0; attempt < 500; attempt++) {
    if (waiter_rows() == want) {
      return;
    }
    sleep_ms(10);
  }
  printf("FAIL %s: /proc/locks never showed %d waiter rows\n", s, want);
  failures++;
}

/* A child with an open file description of the lock file of its own, which
   runs the flock operations the parent orders, one line each ("sh", "ex",
   "un"), and reports each answer as `NAME:ANSWER` on the shared `events`
   channel once the call returns, until "exit". */
struct locker {
  const char* name;
  pid_t pid;
  struct channel orders;
};

static struct locker start_locker(const char* name, struct channel events) {
  struct locker made = {name, 0, channel()};
  made.pid = start();
  if (made.pid == 0) {
    int fd = open_lock();
    char line[64];
    for (;;) {
      hear(made.orders, line, sizeof(line));
      int operation = strcmp(line, "sh") == 0   ? LOCK_SH
                      : strcmp(line, "ex") == 0 ? LOCK_EX
                      : strcmp(line, "un") == 0 ? LOCK_UN
                                                : -1;
      if (operation < 0) {
        _exit(0);
      }
      char event[64];
      snprintf(event, sizeof(event), "%s:%s", name, attempt(fd, operation));
      say(events, event);
    }
  }
  return made;
}

static void order(struct locker* locker, const char* operation) {
  say(locker->orders, operation);
}

static void stop_locker(struct locker* locker) {
  say(locker->orders, "exit");
  reap(locker->pid);
  close_channel(locker->orders);
}

/* The next `count` events, sorted and joined by spaces: the calls a step
   completes, whose reports race natively. */
static void events_of(struct channel events, int count, char* out, size_t size) {
  char lines[4][64];
  for (int index = 0; index < count; index++) {
    hear(events, lines[index], sizeof(lines[index]));
  }
  for (int i = 0; i < count; i++) {
    for (int j = i + 1; j < count; j++) {
      if (strcmp(lines[j], lines[i]) < 0) {
        char swap[64];
        memcpy(swap, lines[i], sizeof(swap));
        memcpy(lines[i], lines[j], sizeof(swap));
        memcpy(lines[j], swap, sizeof(swap));
      }
    }
  }
  out[0] = '\0';
  for (int index = 0; index < count; index++) {
    if (index > 0) {
      strncat(out, " ", size - strlen(out) - 1);
    }
    strncat(out, lines[index], size - strlen(out) - 1);
  }
}

/* The wake order after a blocking conversion (the Claude review of slice
   SP2): A and B hold LOCK_SH, X waits for LOCK_EX under A's lock, and A
   converts with a blocking LOCK_EX. The conversion deletes A's shared lock,
   which wakes X, and blocks A's request under B's lock; X's retry finds B's
   lock and blocks under A's request, which conflicts with it
   (`__locks_insert_block`). B's release then wakes A's request alone, so A
   holds LOCK_EX before X, in every schedule. The parent goes on only once
   /proc/locks shows each request blocked. */
static void scenario_upgrade_order(void) {
  const char* s = "upgrade-order";
  char rows[128];
  char line[128];
  learn_whose_rows_show();
  struct channel events = channel();
  struct locker a = start_locker("A", events);
  struct locker b = start_locker("B", events);
  struct locker x = start_locker("X", events);
  order(&a, "sh");
  hear(events, line, sizeof(line));
  expect(s, "A's LOCK_SH", line, "A:ok");
  order(&b, "sh");
  hear(events, line, sizeof(line));
  expect(s, "B's LOCK_SH", line, "B:ok");
  order(&x, "ex");
  waiter_rows_once(s, 1);
  printf("%s: X waits for LOCK_EX under A's lock\n", s);
  order(&a, "ex");
  proc_locks_rows_once(rows, sizeof(rows), "READ ->WRITE ->WRITE");
  expect(s, "rows once A's conversion and X's retry blocked", rows, "READ ->WRITE ->WRITE");
  order(&b, "un");
  events_of(events, 2, line, sizeof(line));
  expect(s, "the calls B's LOCK_UN completes", line, "A:ok B:ok");
  proc_locks_rows_once(rows, sizeof(rows), "WRITE ->WRITE");
  expect(s, "rows while A holds LOCK_EX", rows, "WRITE ->WRITE");
  order(&a, "un");
  events_of(events, 2, line, sizeof(line));
  expect(s, "the calls A's LOCK_UN completes", line, "A:ok X:ok");
  order(&x, "un");
  hear(events, line, sizeof(line));
  expect(s, "X's LOCK_UN", line, "X:ok");
  stop_locker(&a);
  stop_locker(&b);
  stop_locker(&x);
  close_channel(events);
}

/* A thread of the parent that waits for LOCK_SH through a description of its
   own, and reports its answer. */
static void* wait_for_shared_lock(void* argument) {
  struct thread_wait* wait = argument;
  int fd = open_lock();
  say(wait->report, "blocking");
  say(wait->report, attempt(fd, LOCK_SH));
  close(fd);
  return NULL;
}

/* The wake order after a waiter is interrupted (the Claude review of slice
   SP2): G holds LOCK_EX; X, a thread of the parent, waits for LOCK_SH under
   G; V waits for LOCK_EX under X, which conflicts with it; Y waits for
   LOCK_SH under G, after X, which does not conflict with it. A signal ends
   X's wait with EINTR, which wakes V (`locks_delete_block`), whose retry
   blocks under Y. G's release then wakes Y alone, so Y holds the lock before
   V, in every schedule. */
static void scenario_interrupted_waiter(void) {
  const char* s = "interrupted-waiter";
  char rows[128];
  char line[128];
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = on_signal;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGUSR1, &action, NULL) != 0) {
    harness("sigaction");
  }
  learn_whose_rows_show();
  struct channel events = channel();
  struct locker g = start_locker("G", events);
  struct locker v = start_locker("V", events);
  struct locker y = start_locker("Y", events);
  order(&g, "ex");
  hear(events, line, sizeof(line));
  expect(s, "G's LOCK_EX", line, "G:ok");
  struct thread_wait wait = {channel()};
  pthread_t x;
  if (pthread_create(&x, NULL, wait_for_shared_lock, &wait) != 0) {
    harness("pthread_create");
  }
  hear(wait.report, line, sizeof(line));
  proc_locks_rows_once(rows, sizeof(rows), "WRITE ->READ");
  expect(s, "rows while X waits", rows, "WRITE ->READ");
  order(&v, "ex");
  proc_locks_sorted_once(rows, sizeof(rows), "WRITE ->READ ->WRITE");
  expect(s, "rows while V waits under X", rows, "WRITE ->READ ->WRITE");
  order(&y, "sh");
  proc_locks_sorted_once(rows, sizeof(rows), "WRITE ->READ ->READ ->WRITE");
  expect(s, "rows while Y waits under G, sorted", rows, "WRITE ->READ ->READ ->WRITE");
  if (pthread_kill(x, SIGUSR1) != 0) {
    harness("pthread_kill");
  }
  hear(wait.report, line, sizeof(line));
  expect(s, "X's interrupted LOCK_SH", line, "errno=4");
  proc_locks_sorted_once(rows, sizeof(rows), "WRITE ->READ ->WRITE");
  expect(s, "rows once V's retry blocked under Y", rows, "WRITE ->READ ->WRITE");
  order(&g, "un");
  events_of(events, 2, line, sizeof(line));
  expect(s, "the calls G's LOCK_UN completes", line, "G:ok Y:ok");
  proc_locks_rows_once(rows, sizeof(rows), "READ ->WRITE");
  expect(s, "rows while Y holds LOCK_SH", rows, "READ ->WRITE");
  order(&y, "un");
  events_of(events, 2, line, sizeof(line));
  expect(s, "the calls Y's LOCK_UN completes", line, "V:ok Y:ok");
  order(&v, "un");
  hear(events, line, sizeof(line));
  expect(s, "V's LOCK_UN", line, "V:ok");
  if (pthread_join(x, NULL) != 0) {
    harness("pthread_join");
  }
  close_channel(wait.report);
  stop_locker(&g);
  stop_locker(&v);
  stop_locker(&y);
  close_channel(events);
  action.sa_handler = SIG_DFL;
  if (sigaction(SIGUSR1, &action, NULL) != 0) {
    harness("sigaction");
  }
}

/* A waiting thread of retain-conversion: a blocking flock of `fd` in `mode`,
   reported on `report`. */
struct mode_wait {
  struct channel report;
  int fd;
  int mode;
};

static void* wait_in_mode(void* argument) {
  struct mode_wait* wait = argument;
  say(wait->report, "blocking");
  say(wait->report, attempt(wait->fd, wait->mode));
  return NULL;
}

static pthread_t start_mode_waiter(struct mode_wait* wait) {
  pthread_t thread;
  if (pthread_create(&thread, NULL, wait_in_mode, wait) != 0) {
    harness("pthread_create");
  }
  char line[64];
  hear(wait->report, line, sizeof(line));
  return thread;
}

/* A retry that converts its description's lock (the Claude review of slice
   SP2): threads T2 and T1 share description d and wait for LOCK_EX and
   LOCK_SH behind the parent's LOCK_EX, with W0 (LOCK_SH, a description of its
   own) blocked between them, under T2's request. The release wakes T2 and T1,
   whose retries race. Where T2's comes first, T1's converts d's lock to
   LOCK_SH, which deletes the exclusive lock W0 is blocked under, and W0's
   retry takes LOCK_SH: two shared locks. Where T1's comes first, T2's
   conversion leaves W0 blocked under d's LOCK_EX. Which it is depends on the
   schedule, so the guest prints whether the state it settles in is one of the
   two, and whether a fresh LOCK_SH|LOCK_NB agrees with it. */
static void scenario_retain_conversion(void) {
  const char* s = "retain-conversion";
  char rows[128];
  char line[64];
  learn_whose_rows_show();
  int held = open_lock();
  expect(s, "parent LOCK_EX", attempt(held, LOCK_EX), "ok");
  int shared = open_lock();
  struct mode_wait t2 = {channel(), shared, LOCK_EX};
  struct mode_wait w0 = {channel(), open_lock(), LOCK_SH};
  struct mode_wait t1 = {channel(), shared, LOCK_SH};
  pthread_t t2_thread = start_mode_waiter(&t2);
  proc_locks_rows_once(rows, sizeof(rows), "WRITE ->WRITE");
  expect(s, "rows while T2 waits", rows, "WRITE ->WRITE");
  pthread_t w0_thread = start_mode_waiter(&w0);
  proc_locks_sorted_once(rows, sizeof(rows), "WRITE ->READ ->WRITE");
  expect(s, "rows while W0 waits under T2, sorted", rows, "WRITE ->READ ->WRITE");
  pthread_t t1_thread = start_mode_waiter(&t1);
  proc_locks_sorted_once(rows, sizeof(rows), "WRITE ->READ ->READ ->WRITE");
  expect(s, "rows while T1 waits under the parent, sorted", rows, "WRITE ->READ ->READ ->WRITE");
  expect(s, "parent LOCK_UN", attempt(held, LOCK_UN), "ok");
  hear(t2.report, line, sizeof(line));
  expect(s, "T2's LOCK_EX", line, "ok");
  hear(t1.report, line, sizeof(line));
  expect(s, "T1's LOCK_SH", line, "ok");
  if (pthread_join(t2_thread, NULL) != 0 || pthread_join(t1_thread, NULL) != 0) {
    harness("pthread_join");
  }
  for (int attempt_index = 0; attempt_index < 500; attempt_index++) {
    proc_locks_rows(rows, sizeof(rows));
    if (strcmp(rows, "READ READ") == 0 || strcmp(rows, "WRITE ->READ") == 0) {
      break;
    }
    sleep_ms(10);
  }
  int two_shared = strcmp(rows, "READ READ") == 0;
  int w0_waits = strcmp(rows, "WRITE ->READ") == 0;
  expect(s, "settled in a state Linux reaches", two_shared || w0_waits ? "yes" : rows, "yes");
  int fresh = open_lock();
  const char* got = attempt(fresh, LOCK_SH | LOCK_NB);
  int agrees = (two_shared && strcmp(got, "ok") == 0) || (w0_waits && strcmp(got, "EWOULDBLOCK") == 0);
  expect(s, "a fresh LOCK_SH|LOCK_NB agrees with it", agrees ? "yes" : got, "yes");
  close(fresh);
  /* d's release lets W0 take its lock if it still waits. */
  expect(s, "d's LOCK_UN", attempt(shared, LOCK_UN), "ok");
  hear(w0.report, line, sizeof(line));
  expect(s, "W0's LOCK_SH", line, "ok");
  if (pthread_join(w0_thread, NULL) != 0) {
    harness("pthread_join");
  }
  close_channel(t2.report);
  close_channel(w0.report);
  close_channel(t1.report);
  close(w0.fd);
  close(shared);
  close(held);
}

/* A release through the waiting call's own description, right after the
   release that woke it (the Claude review of slice SP2): a thread waits for
   LOCK_EX through the parent's descriptor d behind the parent's other lock,
   and the parent releases that lock and then d's. The waiting call takes d's
   lock after or before d's release, as the schedule has it: afterwards d
   holds LOCK_EX until the parent closes it, otherwise nothing. Either is
   Linux's, and the guest prints whether a child's probe saw one of them. */
static void scenario_unlock_before_retry(void) {
  const char* s = "unlock-before-retry";
  char rows[128];
  char line[64];
  learn_whose_rows_show();
  int held = open_lock();
  expect(s, "parent LOCK_EX", attempt(held, LOCK_EX), "ok");
  struct fd_wait wait = {channel(), open_lock()};
  pthread_t thread = start_descriptor_waiter(&wait);
  proc_locks_rows_once(rows, sizeof(rows), "WRITE ->WRITE");
  expect(s, "rows while the other thread waits", rows, "WRITE ->WRITE");
  int released = flock(held, LOCK_UN);
  int released_d = flock(wait.fd, LOCK_UN);
  expect(s, "parent LOCK_UN, then d's LOCK_UN", released == 0 && released_d == 0 ? "ok" : "failed",
         "ok");
  hear(wait.report, line, sizeof(line));
  expect(s, "the waiting call", line, "ok");
  if (pthread_join(thread, NULL) != 0) {
    harness("pthread_join");
  }
  struct channel report = channel();
  probe_into(report, LOCK_EX | LOCK_NB);
  hear(report, line, sizeof(line));
  int linux_outcome = strcmp(line, "ok") == 0 || strcmp(line, "EWOULDBLOCK") == 0;
  expect(s, "a child's LOCK_EX|LOCK_NB saw a Linux outcome", linux_outcome ? "yes" : line, "yes");
  close_channel(report);
  close(wait.fd);
  probe(s, "a child's LOCK_EX|LOCK_NB after d's close", LOCK_EX | LOCK_NB, "ok");
  close_channel(wait.report);
  close(held);
}

/* An exec kills the caller's other threads (the Claude review of slice SP2):
   in a child, a thread waits for LOCK_EX behind the lock the child holds
   through a close-on-exec descriptor, and another process, P, waits for
   LOCK_EX behind that thread's request (`__locks_insert_block` puts it
   under the request, which conflicts with it). Then the child's main thread
   execs. The exec kills the waiting thread (`de_thread`), whose request
   leaves the tree and wakes P's (`locks_delete_block`), and closes the
   child's descriptors, which releases its lock, so P takes the lock. The new
   image reports through a pipe that survives the exec. */
static void scenario_exec_waiter(void) {
  const char* s = "exec-waiter";
  char rows[128];
  char line[64];
  learn_whose_rows_show();
  struct channel report = channel();
  struct channel go = channel();
  pid_t child = start();
  if (child == 0) {
    int held = open_lock();
    if (flock(held, LOCK_EX) != 0) {
      _exit(HARNESS_FAILURE);
    }
    struct fd_wait wait = {channel(), open_lock()};
    start_descriptor_waiter(&wait);
    hear(go, line, sizeof(line));
    char fd_text[16];
    snprintf(fd_text, sizeof(fd_text), "%d", report.write_end);
    int flags = fcntl(report.write_end, F_GETFD);
    fcntl(report.write_end, F_SETFD, flags & ~FD_CLOEXEC);
    const char* image = strchr(self_path, '/') != NULL ? self_path : "/proc/self/exe";
    char* argv[] = {(char*)self_path, "exec-report", fd_text, NULL};
    execv(image, argv);
    _exit(HARNESS_FAILURE);
  }
  proc_locks_rows_once(rows, sizeof(rows), "WRITE ->WRITE");
  expect(s, "rows while the child's thread waits", rows, "WRITE ->WRITE");
  struct channel events = channel();
  struct locker p = start_locker("P", events);
  order(&p, "ex");
  proc_locks_rows_once(rows, sizeof(rows), "WRITE ->WRITE ->WRITE");
  expect(s, "rows once P waits under the thread's request", rows, "WRITE ->WRITE ->WRITE");
  say(go, "exec");
  hear(report, line, sizeof(line));
  expect(s, "the new image", line, "running");
  hear(events, line, sizeof(line));
  expect(s, "P's LOCK_EX after the exec", line, "P:ok");
  order(&p, "un");
  hear(events, line, sizeof(line));
  expect(s, "P's LOCK_UN", line, "P:ok");
  stop_locker(&p);
  reap(child);
  close_channel(events);
  close_channel(report);
  close_channel(go);
}

/* (internal, after exec-waiter's exec) Report that the new image runs. */
static int scenario_exec_report(const char* fd_text) {
  int report = atoi(fd_text);
  static const char running[] = "running\n";
  if (write(report, running, sizeof(running) - 1) != (ssize_t)(sizeof(running) - 1)) {
    return HARNESS_FAILURE;
  }
  return 0;
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
    {"waiter-closed", scenario_waiter_closed},
    {"waiter-replaced", scenario_waiter_replaced},
    {"upgrade-order", scenario_upgrade_order},
    {"interrupted-waiter", scenario_interrupted_waiter},
    {"retain-conversion", scenario_retain_conversion},
    {"unlock-before-retry", scenario_unlock_before_retry},
};

int main(int argc, char** argv) {
  setvbuf(stdout, NULL, _IOLBF, 0);
  self_path = argv[0];
  const char* scenario = argc > 1 ? argv[1] : "all";
  if (strcmp(scenario, "probe") == 0) {
    return argc > 3 ? scenario_probe(argv[2], argv[3]) : HARNESS_FAILURE;
  }
  if (strcmp(scenario, "exec-report") == 0) {
    return argc > 2 ? scenario_exec_report(argv[2]) : HARNESS_FAILURE;
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
  } else if (!ran && strcmp(scenario, "private-table-close") == 0) {
    scenario_private_table_close();
  } else if (!ran && strcmp(scenario, "stopped-waiter") == 0) {
    scenario_stopped_waiter();
  } else if (!ran && strcmp(scenario, "exec-waiter") == 0) {
    scenario_exec_waiter();
  } else if (!ran && strcmp(scenario, "waiter-mapped") == 0) {
    scenario_waiter_mapped();
  } else if (!ran && strcmp(scenario, "ebadf") == 0) {
    scenario_ebadf();
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
