/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Reads a fdinfo/7 whose descriptor table is another task's, while the
 * reader's own descriptor 7 is a different open file description or none,
 * and two of the reader's own through a directory descriptor:
 *
 * Another process's fdinfo, opened by the reader (the parent opens
 * /proc/<child>/fdinfo/7 while the child holds file b at its fd 7):
 * - `posix`: the child holds a POSIX write lock on b; the parent's fd 7 is
 *   file a. Linux lists the child's lock with the child's pid and b's inode.
 * - `flock`: parent and child each hold a shared flock on b, each through its
 *   own open at fd 7. Linux lists the child's flock with the child's pid.
 * - `unlocked`: as `posix`, but the child takes no lock. Linux shows b's
 *   inode and no lock line.
 * - `non-dumpable`: as `posix`, but the child makes itself non-dumpable
 *   first. Linux refuses the parent's open with EACCES
 *   (`proc_fdinfo_access_allowed`).
 * - `rendered-collision`: as `posix`, but the parent's fd 7 is
 *   /proc/self/mountinfo, a file Hermit renders itself.
 * - `placeholder-collision`: as `posix`, but the parent's fd 7 is the
 *   directory /proc, for which Hermit holds a placeholder descriptor in a
 *   container with a procfs of its own.
 * - `no-reader-descriptor`: as `posix`, but the parent has no fd 7 at all.
 *
 * A parent's fdinfo the child reads:
 * - `inherited-posix`: the parent holds a POSIX write lock on b through its
 *   fd 7 and opens /proc/self/fdinfo/7 without reading it, then forks; the
 *   child replaces its fd 7 with file a and reads the inherited descriptor.
 *   Linux resolved `self` at the open, so it lists the parent's lock with the
 *   parent's pid and b's inode.
 * - `inherited-flock`: as `inherited-posix`, with a shared flock on b, and the
 *   child's fd 7 its own open of b holding its own shared flock.
 * - `inherited-directory`: as `inherited-posix`, but the parent opens the
 *   directory /proc/self before the fork, and the child opens fdinfo/7
 *   relative to it. Linux bound the directory to the parent at its open.
 * - `inherited-directory-rendered`: as `inherited-directory`, but the child's
 *   fd 7 is /proc/self/mountinfo, which Hermit renders itself.
 * - `inherited-directory-unlocked`, `inherited-directory-unlocked-rendered`:
 *   the two above without the parent's lock: Linux shows b's inode and no
 *   lock line.
 * - `private-thread`: the process holds a POSIX write lock on b through its
 *   fd 7; a thread created with CLONE_THREAD but without CLONE_FILES puts
 *   file a at its own fd 7 and reads /proc/self/fdinfo/7, which Linux
 *   resolves to the thread group, whose table is the leader's: the lock on
 *   b, with the process's pid.
 *
 * The reader's own fdinfo through a directory descriptor (the reader holds a
 * POSIX write lock on b through its fd 7):
 * - `own-directory`: opened as fdinfo/7 relative to its own /proc/self.
 * - `proc-directory-self`: opened as self/fdinfo/7 relative to /proc, where
 *   Linux resolves `self` at that open, to the reader.
 * - `proc-directory-dot-self`: as `proc-directory-self`, spelled
 *   ./self/fdinfo/7.
 * - `parent-directory-dotdot-self`: a child opens its own as
 *   ../self/fdinfo/7 relative to its parent's /proc/self directory
 *   descriptor: the `..` climbs back to /proc, and Linux resolves that
 *   `self` at this open, to the child.
 *
 * The parent's fdinfo reached through an ordinary symbolic link:
 * - `symlink-named-self`: the parent, lock-free b at its fd 7, puts a
 *   symbolic link called `self` to /proc/<parent>/task/<parent> in its
 *   working directory and opens /proc/self before the fork; the child puts
 *   lock-free a at its own fd 7 and opens cwd/self/../../fdinfo/7 relative
 *   to the parent's /proc/self. Linux follows `cwd` and the link, climbs to
 *   /proc/<parent>, and shows b's inode: the parent's fd 7.
 * - `descriptor-link-directory`: the parent, lock-free b at its fd 7, opens
 *   its /proc/self/fdinfo directory as D before the fork; the child puts
 *   lock-free a at its own fd 7 and opens /proc/self/fd/D/7. Linux follows
 *   the link to the directory D holds, the parent's fdinfo directory, and
 *   shows b's inode.
 * - `descriptor-link-parent-self`: the parent holds a POSIX write lock on b
 *   through its fd 7 and opens the directory /proc/self as N before the
 *   fork; the child puts a at its own fd 7, takes its own POSIX write lock
 *   on it, and opens /proc/self/fd/N/fdinfo/7. Linux follows the link to the
 *   directory N holds, the parent's /proc/<parent>, and lists the parent's
 *   lock with the parent's pid and b's inode, whatever the child's fd 7 is.
 * - `number-through-link`: the parent holds a POSIX write lock on b through
 *   its fd 7 and puts L, a symbolic link to /proc/<parent>/task/<parent>, in
 *   its working directory; the child takes its own lock on a at its fd 7 and
 *   opens /proc/<child>/cwd/L/../../fdinfo/7. Linux follows cwd and L, climbs
 *   to /proc/<parent>, and lists the parent's lock.
 * - `dup-over-own`: the parent holds a POSIX write lock on b through its fd
 *   7; the child takes its own lock on a at its fd 7, opens its own
 *   /proc/self/fdinfo/7 and the parent's fdinfo/7, and moves the parent's over
 *   its own with dup2 before reading: Linux lists the parent's lock.
 * - `directory-swap-race`: the parent holds a POSIX write lock on b through
 *   its fd 7; the child, a lock-free at its fd 7, reads fdinfo/7 relative to a
 *   descriptor a second thread keeps moving between /proc/<child> and
 *   /proc/<parent>. It counts the reads that show a lock line under any inode
 *   but b's ("wrong").
 *
 * Usage: proc_fdinfo_other_process DIRECTORY CASE
 *
 * It prints, for each fdinfo read, its `ino:` checked against stat of b
 * (`ino=INO` when it is b's, else `ino=OTHER-INODE`), then each lock line
 * with its inode checked the same way and its owner checked against the
 * reader (`SELF`) and the other process (`CHILD`, `PARENT`, or the number),
 * so a run shows which descriptor the file describes. An open that fails
 * prints its errno (`open EACCES`) instead.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static void die(const char *what) {
  perror(what);
  exit(2);
}

static const char *errno_name(int error) {
  switch (error) {
    case EACCES:
      return "EACCES";
    case ENOENT:
      return "ENOENT";
    case EBADF:
      return "EBADF";
    case EPERM:
      return "EPERM";
    default:
      return "other";
  }
}

static int open_at_seven(const char *path) {
  int fd = open(path, O_RDWR | O_CREAT | O_CLOEXEC, 0600);
  if (fd < 0) die("open");
  if (fd != 7) {
    if (dup3(fd, 7, O_CLOEXEC) < 0) die("dup3");
    close(fd);
  }
  return 7;
}

static void write_lock(int fd) {
  struct flock lock = {.l_type = F_WRLCK, .l_whence = SEEK_SET, .l_len = 10};
  if (fcntl(fd, F_SETLK, &lock) != 0) die("F_SETLK");
}

/* Print the identity and the lock lines of the fdinfo open at `fdinfo`, or
 * the errno of its open where `fdinfo` is negative (`open_error`). `other` is
 * the other process's pid and `other_name` its name. */
static void show(const char *label, int fdinfo, int open_error, const char *b, pid_t other,
                 const char *other_name) {
  if (fdinfo < 0) {
    printf("%s: open %s\n", label, errno_name(open_error));
    fflush(stdout);
    return;
  }
  struct stat st;
  if (stat(b, &st) != 0) die("stat b");
  char text[4096];
  ssize_t length = pread(fdinfo, text, sizeof text - 1, 0);
  if (length < 0) {
    /* Printed, not fatal: a process that dies here would leave its child
     * waiting on the pipe it holds, and the run would end as a deadlock
     * instead of saying what the read did. */
    printf("%s: read %s\n", label, errno_name(errno));
    fflush(stdout);
    return;
  }
  text[length] = '\0';
  for (char *line = strtok(text, "\n"); line; line = strtok(NULL, "\n")) {
    unsigned long ino;
    if (sscanf(line, "ino:\t%lu", &ino) == 1) {
      printf("%s: ino=%s\n", label, ino == (unsigned long)st.st_ino ? "INO" : "OTHER-INODE");
      continue;
    }
    char class[16], mode[16], access[16];
    long owner;
    unsigned major, minor;
    unsigned long inode;
    if (sscanf(line, "lock:\t%*d: %15s %15s %15s %ld %x:%x:%lu", class, mode, access,
               &owner, &major, &minor, &inode) != 7) {
      continue;
    }
    char who[32];
    if (owner == (long)getpid()) {
      snprintf(who, sizeof who, "SELF");
    } else if (owner == (long)other) {
      snprintf(who, sizeof who, "%s", other_name);
    } else {
      snprintf(who, sizeof who, "%ld", owner);
    }
    printf("%s: %s %s owner=%s inode=%s\n", label, class, access, who,
           inode == (unsigned long)st.st_ino ? "INO" : "OTHER-INODE");
  }
  fflush(stdout);
}

/* Put `path`, opened read-only with `flags` besides, at fd 7. */
static void open_read_only_at_seven(const char *path, int flags) {
  int fd = open(path, O_RDONLY | O_CLOEXEC | flags);
  if (fd < 0) die("open read-only");
  if (fd != 7) {
    if (dup3(fd, 7, O_CLOEXEC) < 0) die("dup3");
    close(fd);
  }
}

/* A pipe whose two descriptors are numbered 20 or above, so that it never
 * takes fd 7. */
static void pipe_above_seven(int fds[2]) {
  int raw[2];
  if (pipe(raw) != 0) die("pipe");
  for (int i = 0; i < 2; i++) {
    fds[i] = fcntl(raw[i], F_DUPFD_CLOEXEC, 20);
    if (fds[i] < 0) die("F_DUPFD_CLOEXEC");
    close(raw[i]);
  }
}

/* What the reader's own fd 7 is while it reads the child's fdinfo/7. */
enum reader_seven {
  /* File a for a POSIX lock or none, its own open of b holding a shared
   * flock for `flock`. */
  READER_FILE,
  /* /proc/self/mountinfo, a file Hermit renders itself under a container
   * mount table. */
  READER_RENDERED,
  /* The directory /proc, for which Hermit holds a placeholder descriptor in
   * a container with a procfs of its own. */
  READER_PLACEHOLDER,
  /* No descriptor at all. */
  READER_NONE,
};

/* The lock the child (or, inherited, the parent) takes on b. */
enum lock_kind { LOCK_POSIX, LOCK_FLOCK, LOCK_NONE };

/* Whether the child makes itself non-dumpable before it says it is ready,
 * and the parent gives up CAP_SYS_PTRACE (which a root in its own user
 * namespace, as under Hermit, would otherwise hold over the child): then the
 * kernel refuses the parent's open of the child's fdinfo
 * (`proc_fdinfo_access_allowed`), EACCES. */
static int child_non_dumpable;

/* Drop CAP_SYS_PTRACE from this process's effective and permitted sets, if
 * it holds it (a no-op for an unprivileged process). */
static void drop_sys_ptrace(void) {
  struct {
    unsigned version;
    int pid;
  } header = {0x20080522 /* _LINUX_CAPABILITY_VERSION_3 */, 0};
  struct {
    unsigned effective, permitted, inheritable;
  } data[2];
  if (syscall(SYS_capget, &header, data) != 0) die("capget");
  unsigned bit = 1u << 19; /* CAP_SYS_PTRACE */
  data[0].effective &= ~bit;
  data[0].permitted &= ~bit;
  if (syscall(SYS_capset, &header, data) != 0) die("capset");
}

static void take_lock(int fd, enum lock_kind lock) {
  if (lock == LOCK_POSIX) {
    write_lock(fd);
  } else if (lock == LOCK_FLOCK && flock(fd, LOCK_SH) != 0) {
    die("flock");
  }
}

static int other_process(enum lock_kind lock, enum reader_seven reader, const char *a,
                         const char *b) {
  int ready[2], done[2];
  pipe_above_seven(ready);
  pipe_above_seven(done);
  switch (reader) {
    case READER_FILE:
      open_at_seven(lock == LOCK_FLOCK ? b : a);
      take_lock(7, lock == LOCK_FLOCK ? LOCK_FLOCK : LOCK_NONE);
      break;
    case READER_RENDERED:
      open_read_only_at_seven("/proc/self/mountinfo", 0);
      break;
    case READER_PLACEHOLDER:
      open_read_only_at_seven("/proc", O_DIRECTORY);
      break;
    case READER_NONE:
      close(7);
      if (fcntl(7, F_GETFD) != -1) die("fd 7 is still open");
      break;
  }
  pid_t child = fork();
  if (child < 0) die("fork");
  if (child == 0) {
    close(7);
    take_lock(open_at_seven(b), lock);
    if (child_non_dumpable && prctl(PR_SET_DUMPABLE, 0) != 0) die("PR_SET_DUMPABLE");
    char byte = 1;
    if (write(ready[1], &byte, 1) != 1) die("write ready");
    if (read(done[0], &byte, 1) != 1) die("read done");
    _exit(0);
  }
  char byte;
  if (read(ready[0], &byte, 1) != 1) die("read ready");
  if (child_non_dumpable) drop_sys_ptrace();
  char path[64];
  snprintf(path, sizeof path, "/proc/%d/fdinfo/7", (int)child);
  int fdinfo = open(path, O_RDONLY | O_CLOEXEC);
  show("child's fd 7", fdinfo, errno, b, child, "CHILD");
  if (fdinfo >= 0) close(fdinfo);
  byte = 1;
  if (write(done[1], &byte, 1) != 1) die("write done");
  int status;
  if (waitpid(child, &status, 0) != child) die("waitpid");
  return WIFEXITED(status) ? WEXITSTATUS(status) : 1;
}

/* `rendered`: the child's fd 7 is /proc/self/mountinfo, which Hermit renders
 * itself, rather than file a (a POSIX lock or none) or its own open of b
 * (`flock`). */
static int inherited(enum lock_kind lock, int through_directory, int rendered, const char *a,
                     const char *b) {
  take_lock(open_at_seven(b), lock);
  /* Opened, not read: Linux resolves `self` here, at the open. */
  int fdinfo = -1, directory = -1, open_error = 0;
  if (through_directory) {
    directory = open("/proc/self", O_RDONLY | O_DIRECTORY);
    if (directory < 0) die("open /proc/self");
  } else {
    fdinfo = open("/proc/self/fdinfo/7", O_RDONLY);
    if (fdinfo < 0) die("open fdinfo");
  }
  pid_t parent = getpid();
  pid_t child = fork();
  if (child < 0) die("fork");
  if (child == 0) {
    close(7);
    if (rendered) {
      open_read_only_at_seven("/proc/self/mountinfo", 0);
    } else {
      take_lock(open_at_seven(lock == LOCK_FLOCK ? b : a), lock == LOCK_FLOCK ? LOCK_FLOCK : LOCK_NONE);
    }
    if (through_directory) {
      fdinfo = openat(directory, "fdinfo/7", O_RDONLY);
      open_error = errno;
    }
    show("inherited fdinfo", fdinfo, open_error, b, parent, "PARENT");
    _exit(0);
  }
  int status;
  if (waitpid(child, &status, 0) != child) die("waitpid");
  return WIFEXITED(status) ? WEXITSTATUS(status) : 1;
}

/* The child's own locked fd 7, opened as ../self/fdinfo/7 relative to the
 * parent's /proc/self directory descriptor: the `..` climbs back to /proc,
 * and Linux resolves that `self` at this open, to the child. */
static int own_through_parents_directory(const char *b) {
  int directory = open("/proc/self", O_RDONLY | O_DIRECTORY);
  if (directory < 0) die("open /proc/self");
  pid_t parent = getpid();
  pid_t child = fork();
  if (child < 0) die("fork");
  if (child == 0) {
    write_lock(open_at_seven(b));
    int fdinfo = openat(directory, "../self/fdinfo/7", O_RDONLY);
    show("child's ../self/fdinfo/7", fdinfo, errno, b, parent, "PARENT");
    _exit(0);
  }
  int status;
  if (waitpid(child, &status, 0) != child) die("waitpid");
  return WIFEXITED(status) ? WEXITSTATUS(status) : 1;
}

/* The parent's lock-free fd 7, reached as cwd/self/../../fdinfo/7 relative
 * to the parent's /proc/self directory descriptor by a child whose own fd 7
 * is another lock-free file. The parent's working directory holds an
 * ordinary symbolic link called `self` to /proc/<parent>/task/<parent>: on
 * Linux `cwd` leads to that directory, the link to the parent's thread
 * directory, and the two `..` to /proc/<parent>, so the file describes the
 * parent's fd 7 (file b), whatever the child's fd 7 is. No component of the
 * child's path is a /proc selector. */
static int symlink_named_self(const char *directory_path, const char *a, const char *b) {
  if (chdir(directory_path) != 0) die("chdir");
  char target[64];
  snprintf(target, sizeof target, "/proc/%d/task/%d", (int)getpid(), (int)getpid());
  if (symlink(target, "self") != 0) die("symlink");
  open_at_seven(b);
  int directory = open("/proc/self", O_RDONLY | O_DIRECTORY);
  if (directory < 0) die("open /proc/self");
  pid_t parent = getpid();
  pid_t child = fork();
  if (child < 0) die("fork");
  if (child == 0) {
    close(7);
    open_at_seven(a);
    int fdinfo = openat(directory, "cwd/self/../../fdinfo/7", O_RDONLY);
    show("child's cwd/self/../../fdinfo/7", fdinfo, errno, b, parent, "PARENT");
    _exit(0);
  }
  int status;
  if (waitpid(child, &status, 0) != child) die("waitpid");
  return WIFEXITED(status) ? WEXITSTATUS(status) : 1;
}

/* The parent's lock-free fd 7, reached as /proc/self/fd/D/7, where D is the
 * parent's descriptor of its own /proc/self/fdinfo directory, inherited by a
 * child whose own fd 7 is another lock-free file. On Linux /proc/self/fd/D
 * is a link to the directory D holds, the parent's fdinfo directory, so the
 * file describes the parent's fd 7 (file b). */
static int descriptor_link_directory(const char *a, const char *b) {
  open_at_seven(b);
  int directory = open("/proc/self/fdinfo", O_RDONLY | O_DIRECTORY);
  if (directory < 0) die("open /proc/self/fdinfo");
  pid_t parent = getpid();
  pid_t child = fork();
  if (child < 0) die("fork");
  if (child == 0) {
    close(7);
    open_at_seven(a);
    char path[64];
    snprintf(path, sizeof path, "/proc/self/fd/%d/7", directory);
    int fdinfo = open(path, O_RDONLY);
    show("child's /proc/self/fd/D/7", fdinfo, errno, b, parent, "PARENT");
    _exit(0);
  }
  int status;
  if (waitpid(child, &status, 0) != child) die("waitpid");
  return WIFEXITED(status) ? WEXITSTATUS(status) : 1;
}

/* The parent's locked fd 7, reached by a child as
 * /proc/<child>/cwd/L/../../fdinfo/7, where the parent's working directory,
 * which the child inherits, holds L, an ordinary symbolic link to
 * /proc/<parent>/task/<parent>. Linux follows cwd and L and climbs to
 * /proc/<parent>, so the file lists the parent's lock, whatever the child's
 * fd 7 holds; the child takes its own lock on a there. The spelling names the
 * child by number and passes through links (cwd, L) that a lexical reading
 * would skip. */
static int number_through_link(const char *directory_path, const char *a, const char *b) {
  if (chdir(directory_path) != 0) die("chdir");
  char target[64];
  snprintf(target, sizeof target, "/proc/%d/task/%d", (int)getpid(), (int)getpid());
  if (symlink(target, "L") != 0) die("symlink");
  write_lock(open_at_seven(b));
  pid_t parent = getpid();
  pid_t child = fork();
  if (child < 0) die("fork");
  if (child == 0) {
    close(7);
    write_lock(open_at_seven(a));
    char path[96];
    snprintf(path, sizeof path, "/proc/%d/cwd/L/../../fdinfo/7", (int)getpid());
    int fdinfo = open(path, O_RDONLY);
    show("child's /proc/<child>/cwd/L/../../fdinfo/7", fdinfo, errno, b, parent, "PARENT");
    _exit(0);
  }
  int status;
  if (waitpid(child, &status, 0) != child) die("waitpid");
  return WIFEXITED(status) ? WEXITSTATUS(status) : 1;
}

/* `dup-over-own`: the parent holds a POSIX write lock on b through its fd 7;
 * the child takes its own lock on a at its fd 7, opens its own
 * /proc/self/fdinfo/7 and the parent's /proc/<parent>/fdinfo/7, and moves the
 * parent's over its own with dup2 before reading. The descriptor now holds
 * the parent's fdinfo, so Linux lists the parent's lock on b. */
static int dup_over_own(const char *a, const char *b) {
  write_lock(open_at_seven(b));
  pid_t parent = getpid();
  pid_t child = fork();
  if (child < 0) die("fork");
  if (child == 0) {
    close(7);
    write_lock(open_at_seven(a));
    int own = open("/proc/self/fdinfo/7", O_RDONLY | O_CLOEXEC);
    char path[64];
    snprintf(path, sizeof path, "/proc/%d/fdinfo/7", (int)parent);
    int other = open(path, O_RDONLY | O_CLOEXEC);
    if (own < 0 || other < 0) die("open fdinfo/7");
    if (dup2(other, own) != own) die("dup2");
    close(other);
    show("child's dup2-replaced fdinfo", own, 0, b, parent, "PARENT");
    _exit(0);
  }
  int status;
  if (waitpid(child, &status, 0) != child) die("waitpid");
  return WIFEXITED(status) ? WEXITSTATUS(status) : 1;
}

/* The descriptors the directory-swap race's second thread moves, and its
 * stop flag. */
static int race_directory, race_own, race_other;
static volatile int race_stop;

static int race_swapper(void *unused) {
  (void)unused;
  while (!__atomic_load_n(&race_stop, __ATOMIC_SEQ_CST)) {
    if (dup2(race_other, race_directory) < 0) die("dup2 other");
    if (dup2(race_own, race_directory) < 0) die("dup2 own");
  }
  __atomic_store_n(&race_stop, 2, __ATOMIC_SEQ_CST);
  syscall(SYS_exit, 0);
  return 0;
}

#define RACE_READS 2000

/* A lookup whose directory descriptor another thread replaces: natively, in a
 * run without sequentialized threads, or in one whose scheduler switches
 * threads at syscalls (Hermit's --chaos with --target-timeslice; at the
 * default timeslice the second thread keeps its slice and the reads barely
 * advance). The parent holds a POSIX write lock on b
 * through its fd 7; the child holds a, lock-free, at its own fd 7. The child
 * opens /proc/<child> and /proc/<parent>, and a second thread of the child
 * (sharing its table) keeps moving a third descriptor D between the two with
 * dup2, while the child opens fdinfo/7 relative to D and reads it,
 * RACE_READS times. A read shows the child's fd 7 (a's inode, no lock line)
 * or the parent's (b's inode and the parent's lock), or is refused (an open
 * or read error, EIO under --allow-unsupported-syscalls). A lock line under
 * any inode but b's is the wrong answer: the parent's lock shown as another
 * file's. It prints every count (reads, wrong, own, other, refused); the
 * check is "wrong=0", with both files met. */
static int directory_swap_race(const char *a, const char *b) {
  write_lock(open_at_seven(b));
  pid_t parent = getpid();
  pid_t child = fork();
  if (child < 0) die("fork");
  if (child == 0) {
    close(7);
    open_at_seven(a);
    struct stat sa, sb;
    if (stat(a, &sa) != 0 || stat(b, &sb) != 0) die("stat");
    char path[64];
    snprintf(path, sizeof path, "/proc/%d", (int)getpid());
    race_own = open(path, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    snprintf(path, sizeof path, "/proc/%d", (int)parent);
    race_other = open(path, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (race_own < 0 || race_other < 0) die("open /proc/<pid>");
    race_directory = fcntl(race_own, F_DUPFD_CLOEXEC, 30);
    if (race_directory < 0) die("F_DUPFD");
    static char stack[256 * 1024] __attribute__((aligned(16)));
    int flags = CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD | CLONE_SYSVSEM;
    if (clone(race_swapper, stack + sizeof stack, flags, NULL) < 0) die("clone");
    int own = 0, other = 0, refused = 0, wrong = 0;
    for (int i = 0; i < RACE_READS; i++) {
      int fdinfo = openat(race_directory, "fdinfo/7", O_RDONLY | O_CLOEXEC);
      char text[4096];
      ssize_t length = fdinfo < 0 ? -1 : pread(fdinfo, text, sizeof text - 1, 0);
      if (fdinfo >= 0) close(fdinfo);
      if (length < 0) {
        refused++;
        continue;
      }
      text[length] = '\0';
      unsigned long ino = 0;
      int locks = 0, foreign_lock = 0;
      for (char *line = strtok(text, "\n"); line; line = strtok(NULL, "\n")) {
        unsigned long value;
        if (sscanf(line, "ino:\t%lu", &value) == 1) {
          ino = value;
          continue;
        }
        unsigned major, minor;
        unsigned long inode;
        if (sscanf(line, "lock:\t%*d: %*s %*s %*s %*d %x:%x:%lu", &major, &minor, &inode) == 3) {
          locks++;
          if (inode != (unsigned long)sb.st_ino) foreign_lock = 1;
        }
      }
      if (locks > 0 && (foreign_lock || ino != (unsigned long)sb.st_ino)) {
        wrong++;
      } else if (locks > 0) {
        other++;
      } else if (ino == (unsigned long)sa.st_ino) {
        own++;
      } else {
        other++;
      }
    }
    __atomic_store_n(&race_stop, 1, __ATOMIC_SEQ_CST);
    while (__atomic_load_n(&race_stop, __ATOMIC_SEQ_CST) != 2) sched_yield();
    printf("directory-swap race: reads=%d wrong=%d own=%d other=%d refused=%d\n", RACE_READS,
           wrong, own, other, refused);
    fflush(stdout);
    _exit(0);
  }
  int status;
  if (waitpid(child, &status, 0) != child) die("waitpid");
  return WIFEXITED(status) ? WEXITSTATUS(status) : 1;
}

/* The parent's locked fd 7, reached by a child whose own fd 7 is another file
 * it write-locks, as /proc/self/fd/N/fdinfo/7, where N is the parent's
 * /proc/self directory descriptor, inherited through the fork: Linux follows
 * the link to the directory N holds, /proc/<parent>, so the file describes
 * the parent's fd 7 (file b) and its lock. */
static int descriptor_link_parent_self(const char *a, const char *b) {
  write_lock(open_at_seven(b));
  int directory = open("/proc/self", O_RDONLY | O_DIRECTORY);
  if (directory < 0) die("open /proc/self");
  pid_t parent = getpid();
  pid_t child = fork();
  if (child < 0) die("fork");
  if (child == 0) {
    close(7);
    write_lock(open_at_seven(a));
    char path[64];
    snprintf(path, sizeof path, "/proc/self/fd/%d/fdinfo/7", directory);
    int fdinfo = open(path, O_RDONLY);
    show("child's /proc/self/fd/N/fdinfo/7", fdinfo, errno, b, parent, "PARENT");
    _exit(0);
  }
  int status;
  if (waitpid(child, &status, 0) != child) die("waitpid");
  return WIFEXITED(status) ? WEXITSTATUS(status) : 1;
}

/* The reader's own fd 7, holding its POSIX write lock on b, reached as
 * /proc/self/fd/D/self/fdinfo/7, where D is its descriptor of the directory
 * /proc: Linux follows the link to /proc, resolves that `self` at this open,
 * to the reader, and shows its own file b and its own lock. */
static int own_through_descriptor_link(const char *b) {
  write_lock(open_at_seven(b));
  int directory = open("/proc", O_RDONLY | O_DIRECTORY);
  if (directory < 0) die("open /proc");
  char path[64];
  snprintf(path, sizeof path, "/proc/self/fd/%d/self/fdinfo/7", directory);
  int fdinfo = open(path, O_RDONLY);
  show("/proc/self/fd/D/self/fdinfo/7", fdinfo, errno, b, -1, "OTHER");
  return 0;
}

/* The private-table thread's arguments and its completion flag. */
static const char *thread_a, *thread_b;
static volatile int thread_done;

static int private_table_thread(void *unused) {
  (void)unused;
  /* This thread's table is its own copy: replacing its fd 7 leaves the
   * leader's alone. */
  close(7);
  open_at_seven(thread_a);
  int fdinfo = open("/proc/self/fdinfo/7", O_RDONLY);
  show("thread's /proc/self/fdinfo/7", fdinfo, errno, thread_b, -1, "OTHER");
  __atomic_store_n(&thread_done, 1, __ATOMIC_SEQ_CST);
  syscall(SYS_exit, 0);
  return 0;
}

static int private_thread(const char *a, const char *b) {
  write_lock(open_at_seven(b));
  thread_a = a;
  thread_b = b;
  static char stack[256 * 1024] __attribute__((aligned(16)));
  int flags = CLONE_VM | CLONE_FS | CLONE_SIGHAND | CLONE_THREAD | CLONE_SYSVSEM;
  if (clone(private_table_thread, stack + sizeof stack, flags, NULL) < 0) die("clone");
  while (!__atomic_load_n(&thread_done, __ATOMIC_SEQ_CST)) {
    sched_yield();
  }
  return 0;
}

/* The reader's own fd 7, locked, read through a directory descriptor: its
 * /proc/self (`directory` "/proc/self", name "fdinfo/7"), or /proc itself
 * (`directory` "/proc", name "self/fdinfo/7"). */
static int own_through_directory(const char *label, const char *directory_path, const char *name,
                                 const char *b) {
  write_lock(open_at_seven(b));
  int directory = open(directory_path, O_RDONLY | O_DIRECTORY);
  if (directory < 0) die("open directory");
  int fdinfo = openat(directory, name, O_RDONLY);
  show(label, fdinfo, errno, b, -1, "OTHER");
  return 0;
}

int main(int argc, char **argv) {
  if (argc != 3) {
    fprintf(stderr, "usage: %s DIRECTORY CASE\n", argv[0]);
    return 2;
  }
  char a[4096], b[4096];
  snprintf(a, sizeof a, "%s/a", argv[1]);
  snprintf(b, sizeof b, "%s/b", argv[1]);
  const char *mode = argv[2];
  if (strcmp(mode, "posix") == 0) return other_process(LOCK_POSIX, READER_FILE, a, b);
  if (strcmp(mode, "flock") == 0) return other_process(LOCK_FLOCK, READER_FILE, a, b);
  if (strcmp(mode, "unlocked") == 0) return other_process(LOCK_NONE, READER_FILE, a, b);
  if (strcmp(mode, "non-dumpable") == 0) {
    child_non_dumpable = 1;
    return other_process(LOCK_POSIX, READER_FILE, a, b);
  }
  if (strcmp(mode, "rendered-collision") == 0) {
    return other_process(LOCK_POSIX, READER_RENDERED, a, b);
  }
  if (strcmp(mode, "placeholder-collision") == 0) {
    return other_process(LOCK_POSIX, READER_PLACEHOLDER, a, b);
  }
  if (strcmp(mode, "no-reader-descriptor") == 0) {
    return other_process(LOCK_POSIX, READER_NONE, a, b);
  }
  if (strcmp(mode, "inherited-posix") == 0) return inherited(LOCK_POSIX, 0, 0, a, b);
  if (strcmp(mode, "inherited-flock") == 0) return inherited(LOCK_FLOCK, 0, 0, a, b);
  if (strcmp(mode, "inherited-directory") == 0) return inherited(LOCK_POSIX, 1, 0, a, b);
  if (strcmp(mode, "inherited-directory-rendered") == 0) return inherited(LOCK_POSIX, 1, 1, a, b);
  if (strcmp(mode, "inherited-directory-unlocked") == 0) return inherited(LOCK_NONE, 1, 0, a, b);
  if (strcmp(mode, "inherited-directory-unlocked-rendered") == 0) {
    return inherited(LOCK_NONE, 1, 1, a, b);
  }
  if (strcmp(mode, "private-thread") == 0) return private_thread(a, b);
  if (strcmp(mode, "own-directory") == 0) {
    return own_through_directory("own /proc/self directory's fdinfo/7", "/proc/self", "fdinfo/7",
                                 b);
  }
  if (strcmp(mode, "proc-directory-self") == 0) {
    return own_through_directory("/proc directory's self/fdinfo/7", "/proc", "self/fdinfo/7", b);
  }
  if (strcmp(mode, "proc-directory-dot-self") == 0) {
    return own_through_directory("/proc directory's ./self/fdinfo/7", "/proc", "./self/fdinfo/7",
                                 b);
  }
  if (strcmp(mode, "parent-directory-dotdot-self") == 0) return own_through_parents_directory(b);
  if (strcmp(mode, "symlink-named-self") == 0) return symlink_named_self(argv[1], a, b);
  if (strcmp(mode, "descriptor-link-directory") == 0) return descriptor_link_directory(a, b);
  if (strcmp(mode, "descriptor-link-proc-self") == 0) return own_through_descriptor_link(b);
  if (strcmp(mode, "descriptor-link-parent-self") == 0) return descriptor_link_parent_self(a, b);
  if (strcmp(mode, "number-through-link") == 0) return number_through_link(argv[1], a, b);
  if (strcmp(mode, "dup-over-own") == 0) return dup_over_own(a, b);
  if (strcmp(mode, "directory-swap-race") == 0) return directory_swap_race(a, b);
  fprintf(stderr, "unknown case %s\n", mode);
  return 2;
}
