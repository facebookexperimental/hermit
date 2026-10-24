/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * procfs_listings: the /proc directories Detcore lists from its own model
 * (https://github.com/rrnewton/hermit/issues/4266, slice B3a-3) list what
 * Linux lists, as the guest numbers its processes and threads:
 * - the root lists `self`, `thread-self`, the caller's pid and a live child,
 *   a child that exited until it is reaped, but no thread that leads no
 *   process, no child once it is reaped (with or without its status), and
 *   no child reaped automatically;
 * - a process's `task` lists exactly its threads, also read through a
 *   descriptor a forked child inherited (the directory is the opener's);
 * - a process's directory has `task`, a thread's has neither `task` nor
 *   `timerslack_ns`, and both have `status` and `fd`;
 * - every listed entry exists, with the inode a stat of it reports (only a
 *   thread may go between the listing and the stat);
 * - an `O_PATH` descriptor lists nothing (EBADF);
 * - the legacy getdents, as the first read of the root, lists it.
 *
 * usage: procfs_listings [--host] [--no-process-directories] [--no-autoreap]
 *                        [--recorded]
 *   --host: natively, check the same relations and print nothing. A child
 *   reaped automatically may still be listed briefly after wait reports no
 *   child, so natively the guest waits (up to 2 s) for it to go first.
 *   --no-process-directories: check only the root, whose process entries
 *   cannot be looked up either (a backend whose own /proc cannot open a
 *   process's directory yet, KVM).
 *   --no-autoreap: skip the automatically reaped child (a backend that does
 *   not reap them, DBT: https://github.com/rrnewton/hermit/issues/4413).
 *   --recorded: Hermit records and replays the run, where listings are the
 *   kernel's and not filtered by the --proc level while lookups are (as in
 *   omitted_procfs_names.c), so listed entries are not looked up.
 *
 * Under Hermit it prints each listing in order, with the caller's pid, its
 * child's and its threads' shown by role, which must be the same on every
 * backend.
 */

#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static int failures;
static int host;
static int process_directories = 1;
static int autoreap = 1;
static int recorded;

#define CHECK(condition, ...)                \
  do {                                       \
    if (!(condition)) {                      \
      fprintf(stderr, "FAIL: " __VA_ARGS__); \
      fprintf(stderr, "\n");                 \
      failures++;                            \
    }                                        \
  } while (0)

struct linux_dirent64 {
  unsigned long long d_ino;
  long long d_off;
  unsigned short d_reclen;
  unsigned char d_type;
  char d_name[];
};

#define MAX_ENTRIES 16384

struct listing {
  int count;
  char names[MAX_ENTRIES][40];
  unsigned long long inodes[MAX_ENTRIES];
  unsigned char types[MAX_ENTRIES];
};

/* The entries of the directory at `path`, in the order getdents returns
 * them. */
static void list(const char* path, struct listing* listing) {
  listing->count = 0;
  int fd = open(path, O_RDONLY | O_DIRECTORY);
  CHECK(fd >= 0, "open %s: %s", path, strerror(errno));
  if (fd < 0) {
    return;
  }
  char buf[8192];
  for (;;) {
    long n = syscall(SYS_getdents64, fd, buf, sizeof buf);
    CHECK(n >= 0, "getdents64 %s: %s", path, strerror(errno));
    if (n <= 0) {
      break;
    }
    for (long off = 0; off < n;) {
      struct linux_dirent64* d = (struct linux_dirent64*)(buf + off);
      if (listing->count < MAX_ENTRIES) {
        snprintf(listing->names[listing->count], 40, "%s", d->d_name);
        listing->inodes[listing->count] = d->d_ino;
        listing->types[listing->count] = d->d_type;
        listing->count++;
      }
      off += d->d_reclen;
    }
  }
  close(fd);
}

static int has(const struct listing* listing, const char* name) {
  for (int i = 0; i < listing->count; i++) {
    if (strcmp(listing->names[i], name) == 0) {
      return 1;
    }
  }
  return 0;
}

static int is_process_entry(const char* name) {
  return strcmp(name, "self") == 0 || strcmp(name, "thread-self") == 0 ||
      (name[0] >= '1' && name[0] <= '9');
}

/* Every listed entry exists, and its `d_ino` is what lstat of it reports.
 * Only an entry of a `task` directory (`threads`) may go between the
 * listing and the lstat, and natively one of the host's other processes; a
 * backend whose own /proc cannot open process directories refuses to look up
 * a directory or a link of the root too (KVM), but not a file. */
static void inodes_agree(const char* path, const struct listing* listing, int threads) {
  if (recorded) {
    return;
  }
  for (int i = 0; i < listing->count; i++) {
    const char* name = listing->names[i];
    if (strcmp(name, "..") == 0) {
      continue;
    }
    char entry[192];
    snprintf(entry, sizeof entry, "%s/%s", path, name);
    struct stat st;
    if (lstat(entry, &st) != 0) {
      int tolerated = (threads && errno == ENOENT) ||
          (host && errno == ENOENT && is_process_entry(name)) ||
          (!process_directories && errno == EACCES && listing->types[i] != DT_REG);
      CHECK(tolerated, "%s is listed but lstat fails: %s", entry, strerror(errno));
      continue;
    }
    CHECK(st.st_ino == listing->inodes[i], "%s is listed with inode %llu, lstat %llu",
        entry, listing->inodes[i], (unsigned long long)st.st_ino);
  }
}

/* Print `listing` as `label: name...`, with the numbers in `roles` (pairs
 * of a number and its role) shown by role. */
static void print(const char* label, const struct listing* listing, int count,
    const long* numbers, const char* const* roles) {
  if (host) {
    return;
  }
  printf("%s:", label);
  for (int i = 0; i < listing->count; i++) {
    const char* shown = listing->names[i];
    for (int r = 0; r < count; r++) {
      char number[24];
      snprintf(number, sizeof number, "%ld", numbers[r]);
      if (strcmp(shown, number) == 0) {
        shown = roles[r];
      }
    }
    printf(" %s", shown);
  }
  printf("\n");
}

static int gate[2];
static volatile long thread_tid;

static void* thread_main(void* unused) {
  (void)unused;
  thread_tid = syscall(SYS_gettid);
  char c;
  read(gate[0], &c, 1);
  return NULL;
}

int main(int argc, char** argv) {
  for (int i = 1; i < argc; i++) {
    if (strcmp(argv[i], "--host") == 0) {
      host = 1;
    } else if (strcmp(argv[i], "--no-process-directories") == 0) {
      process_directories = 0;
    } else if (strcmp(argv[i], "--no-autoreap") == 0) {
      autoreap = 0;
    } else if (strcmp(argv[i], "--recorded") == 0) {
      recorded = 1;
    } else {
      fprintf(stderr,
          "usage: %s [--host] [--no-process-directories] [--no-autoreap] "
          "[--recorded]\n",
          argv[0]);
      return 2;
    }
  }
  long pid = getpid();
  int child_gate[2], child_done[2];
  CHECK(pipe(child_gate) == 0 && pipe(child_done) == 0, "pipe: %s", strerror(errno));
  pid_t child = fork();
  if (child == 0) {
    char c;
    close(child_gate[1]);
    close(child_done[0]);
    read(child_gate[0], &c, 1);
    _exit(0);
  }
  close(child_gate[0]);
  close(child_done[1]);
  CHECK(child > 0, "fork: %s", strerror(errno));

  static struct listing root, again, directory, tasks;
  char name[24];
  list("/proc", &root);
  CHECK(has(&root, "self") && has(&root, "thread-self"), "/proc lists no self or thread-self");
  snprintf(name, sizeof name, "%ld", pid);
  CHECK(has(&root, name), "/proc does not list the caller, %ld", pid);
  snprintf(name, sizeof name, "%d", child);
  CHECK(has(&root, name), "/proc does not list the live child, %d", child);
  inodes_agree("/proc", &root, 0);
  /* The legacy getdents lists the root too, as the first read of it. */
  {
    int fd = open("/proc", O_RDONLY | O_DIRECTORY);
    char buf[8192];
    long n = syscall(SYS_getdents, fd, buf, sizeof buf);
    CHECK(n > 0, "the legacy getdents of /proc: %ld (%s)", n, strerror(errno));
    close(fd);
  }
  {
    long numbers[] = {pid, child};
    const char* roles[] = {"<pid>", "<child>"};
    print("root", &root, 2, numbers, roles);
  }

  /* A second thread: listed in `task`, not in the root. */
  CHECK(pipe(gate) == 0, "pipe: %s", strerror(errno));
  pthread_t thread;
  CHECK(pthread_create(&thread, NULL, thread_main, NULL) == 0, "pthread_create");
  while (thread_tid == 0) {
    sched_yield();
  }
  long tid = thread_tid;
  list("/proc", &again);
  snprintf(name, sizeof name, "%ld", tid);
  CHECK(!has(&again, name),
      "/proc lists the thread %ld, which leads no process", tid);
  if (process_directories) {
    list("/proc/self/task", &tasks);
    snprintf(name, sizeof name, "%ld", pid);
    CHECK(has(&tasks, name), "/proc/self/task does not list the main thread");
    snprintf(name, sizeof name, "%ld", tid);
    CHECK(has(&tasks, name), "/proc/self/task does not list the thread %ld", tid);
    CHECK(tasks.count == 4, "/proc/self/task lists %d entries, not ., .. and two threads",
        tasks.count);
    inodes_agree("/proc/self/task", &tasks, 1);
    long numbers[] = {pid, tid};
    const char* roles[] = {"<main>", "<thread>"};
    print("task", &tasks, 2, numbers, roles);

    list("/proc/self", &directory);
    CHECK(has(&directory, "task") && has(&directory, "status") && has(&directory, "fd"),
        "/proc/self lacks task, status or fd");
    inodes_agree("/proc/self", &directory, 0);
    print("process", &directory, 0, NULL, NULL);

    list("/proc/thread-self", &directory);
    CHECK(!has(&directory, "task") && !has(&directory, "timerslack_ns"),
        "/proc/thread-self lists task or timerslack_ns");
    CHECK(has(&directory, "status") && has(&directory, "fd"),
        "/proc/thread-self lacks status or fd");
    inodes_agree("/proc/thread-self", &directory, 0);
    print("thread", &directory, 0, NULL, NULL);

    /* An O_PATH descriptor lists nothing. */
    int path_fd = open("/proc/self", O_PATH | O_DIRECTORY);
    CHECK(path_fd >= 0, "open /proc/self O_PATH: %s", strerror(errno));
    char buf[512];
    errno = 0;
    CHECK(syscall(SYS_getdents64, path_fd, buf, sizeof buf) == -1 && errno == EBADF,
        "getdents64 of an O_PATH descriptor: %s", strerror(errno));
    close(path_fd);
    if (!host) {
      printf("an O_PATH descriptor lists nothing\n");
    }
  }
  write(gate[1], "x", 1);
  pthread_join(thread, NULL);

  /* A descriptor a forked child inherits lists the opener's threads, not
   * the reader's. (Forked once the second thread is gone: DBT cannot fork a
   * process with another thread blocked in a call.) */
  if (process_directories) {
    int task_fd = open("/proc/self/task", O_RDONLY | O_DIRECTORY);
    CHECK(task_fd >= 0, "open /proc/self/task: %s", strerror(errno));
    pid_t reader = fork();
    if (reader == 0) {
      char buf[4096];
      long n = syscall(SYS_getdents64, task_fd, buf, sizeof buf);
      int listed = 0, opener = 0, own = 0;
      for (long off = 0; off < n;) {
        struct linux_dirent64* d = (struct linux_dirent64*)(buf + off);
        listed++;
        opener |= atol(d->d_name) == pid;
        own |= atol(d->d_name) == getpid();
        off += d->d_reclen;
      }
      _exit(listed == 3 && opener && !own ? 0 : 1);
    }
    int reader_status = 0;
    CHECK(waitpid(reader, &reader_status, 0) == reader, "waitpid: %s", strerror(errno));
    CHECK(WIFEXITED(reader_status) && WEXITSTATUS(reader_status) == 0,
        "an inherited /proc/self/task descriptor does not list the opener's thread alone");
    close(task_fd);
    if (!host) {
      printf("an inherited task descriptor lists the opener's threads\n");
    }
  }

  /* The child, once reaped, is gone: reaped with no status, and a second
   * one with. */
  write(child_gate[1], "x", 1);
  /* Once the child's end of `child_done` is closed, it has exited: a
   * zombie, listed until it is reaped. */
  char c;
  CHECK(read(child_done[0], &c, 1) == 0, "read child_done: %s", strerror(errno));
  list("/proc", &again);
  snprintf(name, sizeof name, "%d", child);
  CHECK(has(&again, name), "/proc does not list the zombie child %d",
      child);
  CHECK(waitpid(child, NULL, 0) == child, "waitpid: %s", strerror(errno));
  list("/proc", &again);
  snprintf(name, sizeof name, "%d", child);
  CHECK(!has(&again, name),
      "/proc still lists the child %d reaped with no status", child);
  pid_t second = fork();
  if (second == 0) {
    _exit(0);
  }
  int status = 0;
  CHECK(waitpid(second, &status, 0) == second, "waitpid: %s", strerror(errno));
  list("/proc", &again);
  snprintf(name, sizeof name, "%d", second);
  CHECK(!has(&again, name),
      "/proc still lists the child %d reaped with its status", second);
  /* A child of a parent that ignores SIGCHLD is reaped at its exit: once
   * wait finds no child left, it is gone. */
  if (autoreap) {
    signal(SIGCHLD, SIG_IGN);
    pid_t ignored = fork();
    if (ignored == 0) {
      _exit(0);
    }
    errno = 0;
    CHECK(wait(NULL) == -1 && errno == ECHILD, "wait with SIGCHLD ignored: %s",
        strerror(errno));
    signal(SIGCHLD, SIG_DFL);
    snprintf(name, sizeof name, "%d", ignored);
    /* Linux reaps such a child asynchronously to its parent's wait: exit
     * marks it EXIT_DEAD, which wait skips (so it reports ECHILD), and only
     * then release_task unhashes its pid (Linux v7.1.3 kernel/exit.c:766,
     * :777, :1465, :265). So natively it can still be listed for a moment;
     * wait for that to end. Under Hermit the first listing must not list
     * it. */
    for (int tries = 0; host && tries < 2000; tries++) {
      list("/proc", &again);
      if (!has(&again, name)) {
        break;
      }
      usleep(1000);
    }
    list("/proc", &again);
    CHECK(!has(&again, name),
        "/proc still lists the child %d reaped automatically", ignored);
  }
  if (!host) {
    printf("a reaped child is not listed\n");
  }

  if (failures) {
    fprintf(stderr, "%d checks failed\n", failures);
    return 1;
  }
  return 0;
}
