/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * rendered_process_files: a process's cgroup file, which Detcore renders
 * per process without the backend opening a kernel object
 * (https://github.com/rrnewton/hermit/issues/4213, package K slice K1),
 * answers as Linux's /proc/<pid>/cgroup does, for every spelling that
 * reaches it:
 * - /proc/self/cgroup, /proc/<pid>/cgroup and /proc/self/./cgroup are one
 *   file; /proc/thread-self/cgroup, /proc/<pid>/task/<pid>/cgroup and
 *   /proc/self/task/<pid>/cgroup are another; a thread's and a thread's
 *   top-level /proc/<tid>/cgroup are others; each reads the same bytes;
 * - fstat agrees with stat; the file is 0444, owned by the caller's own
 *   identity, of size 0 with one link and 1024-byte blocks;
 * - a descriptor's link names /proc/<pid>/cgroup or
 *   /proc/<pid>/task/<tid>/cgroup, and reopening through /proc/self/fd/N
 *   and /dev/fd/N is the same file;
 * - relative to a /proc, /proc/self, /proc/<pid>, /proc/self/task or
 *   /proc/thread-self directory, and to a /proc/self directory reopened
 *   through its descriptor link, the name is the same file;
 * - a fork's child reads its own file at /proc/self/cgroup, but its parent's
 *   through the /proc/self directory it inherited (the selector resolved at
 *   the parent's open), and at /proc/<parent>/cgroup; the parent reads the
 *   child's at /proc/<child>/cgroup;
 * - a /proc/self directory received over SCM_RIGHTS names its sender's file;
 * - a zombie still has the file, owned by global root (as /proc/version
 *   is) and writable only with CAP_DAC_OVERRIDE where global root is mapped;
 *   a reaped child does not (ENOENT), nor a pid or a task no process holds,
 *   nor a number with a leading zero;
 * - a descriptor kept past its task's reap reads ESRCH wherever the file's
 *   show runs again (a first read, a read after a rewind, a pread), and end
 *   of file after a whole read;
 * - below a task's directory (/proc/thread-self, /proc/<pid>, and a parent's
 *   /proc/self that a child inherited), fd/N is that task's descriptor link;
 *   an O_PATH | O_NOFOLLOW open of /proc/self is the link itself, with
 *   nothing below it (ENOTDIR);
 * - every name below a /proc/<pid> or /proc/self directory whose task was
 *   reaped is ESRCH, for its own process and for one that inherited the
 *   directory;
 * - its listing in /proc/self, its path and a descriptor on it have one
 *   inode, and (--chroot) still after chroot, where the kernel serves it;
 * - it seeks as a seq_file (SEEK_END is EINVAL), reads in parts and at a
 *   position, and continues across exec from the position the old image
 *   left;
 * - the calls that write or ask for its owner answer as Linux answers its
 *   owner, with or without CAP_DAC_OVERRIDE and CAP_SYS_ADMIN (as capget
 *   reports them): a write-mode open, access(W_OK), truncate and a user.
 *   attribute need CAP_DAC_OVERRIDE; a write through a write-mode descriptor
 *   is EINVAL, ftruncate succeeds, fallocate is EOPNOTSUPP, sendfile and
 *   splice into it are EINVAL, and none of them changes what it reads; a
 *   mapping is ENODEV; chmod is EPERM; O_NOATIME is its owner's; a lease is
 *   EINVAL;
 * - access mode 3 (O_ACCMODE) opens with write permission and neither reads
 *   (EBADF) nor writes (EBADF, ftruncate EINVAL, pwrite ESPIPE, a mapping
 *   EACCES);
 * - times its owner sets (utimensat, UTIME_NOW, UTIME_OMIT, futimens) are the
 *   file's, seen through its number and every descriptor, and not the
 *   thread's file's;
 * - F_SET_RW_HINT is the file's: another descriptor and a reopening through
 *   the descriptor link read it back, the thread's file has its own;
 * - a descriptor on the file passed over SCM_RIGHTS is the same open file:
 *   it reads the bytes, shares the sent descriptor's cursor, refuses
 *   SEEK_END, maps ENODEV, refuses every write as the file does, reads ESRCH
 *   at its first read once the file's task is reaped, and reads the bytes
 *   after the sender closed its own.
 *
 * usage: rendered_process_files [--hermit] [--no-process-directories]
 *        [--chroot]
 *        rendered_process_files --inherited <fd> <ino>
 *   --hermit: under Hermit, which renders the file: its bytes are the root
 *   of the cgroup v2 hierarchy, "0::/\n", its identity Detcore's procfs
 *   device and a per-process inode (its top bit set), and it prints what it
 *   checked, with the caller's pid, its threads' and its children's shown by
 *   role, which must be the same on every backend. Without it (natively) the
 *   host's bytes are only compared across spellings, and nothing is printed.
 *   --no-process-directories: skip the names relative to a process's or a
 *   thread's /proc directory (also inherited across fork, reopened and
 *   received over SCM_RIGHTS), named on stderr, for a backend whose own /proc
 *   cannot open those directories yet (KVM), and print none of their lines.
 *   --chroot: also check the file's inode after chroot("/") in a child, which
 *   needs CAP_SYS_CHROOT.
 *   --relaxed: only the checks one thread's own calls decide (bytes, live
 *   owner, an mtime, a write hint), for a run under
 *   --no-sequentialize-threads, where a zombie's owner and a reaped child's
 *   file are host-timed and accepted either way.
 *   --relaxed-transfer: under --no-sequentialize-threads, read through a
 *   rendered descriptor received over SCM_RIGHTS and print the answer
 *   (Detcore refuses the call by name there).
 *   --inherited is the program after it execs itself.
 * The program must be started by its absolute path, which it execs again.
 */

#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/capability.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/sendfile.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <sys/xattr.h>
#include <unistd.h>

#ifndef SYS_faccessat2
#define SYS_faccessat2 439
#endif

#define PROC_SUPER_MAGIC 0x9fa0
#define RENDERED_DEVICE 0xf000ffULL
#define F_SET_RW_HINT_COMMAND 1036
#define F_GET_RW_HINT_COMMAND 1035

static int failures;
static int hermit;
static int process_directories = 1;
static int after_chroot;
static int relaxed;
static int relaxed_transfer;
static const char* program;
static pid_t pid;

#define CHECK(condition, ...)                              \
  do {                                                     \
    if (!(condition)) {                                    \
      fprintf(stderr, "FAIL %s:%d: ", __FILE__, __LINE__); \
      fprintf(stderr, __VA_ARGS__);                        \
      fprintf(stderr, "\n");                               \
      failures++;                                          \
    }                                                      \
  } while (0)

/* The call fails with `expected`. */
#define REFUSED(call, expected, what)                                    \
  do {                                                                   \
    errno = 0;                                                           \
    long refused_result = (long)(call);                                  \
    int refused_errno = errno;                                           \
    CHECK(refused_result == -1 && refused_errno == (expected),           \
          "%s: %s gave %ld (%s), not %s", (what), #call, refused_result, \
          strerror(refused_errno), strerror(expected));                  \
  } while (0)

/* The call succeeds with `expected`. */
#define RETURNS(call, expected, what)                                              \
  do {                                                                             \
    errno = 0;                                                                     \
    long returns_result = (long)(call);                                            \
    int returns_errno = errno;                                                     \
    CHECK(returns_result == (long)(expected), "%s: %s gave %ld (%s), not %ld", (what), \
          #call, returns_result, returns_result < 0 ? strerror(returns_errno) : "", \
          (long)(expected));                                                       \
  } while (0)

/* The call gives `expected`, 0 for success or an errno. */
#define ANSWERS(call, expected, what)       \
  do {                                      \
    if ((expected) == 0) {                  \
      RETURNS(call, 0, what);               \
    } else {                                \
      REFUSED(call, (expected), what);      \
    }                                       \
  } while (0)

/* The bytes /proc/self/cgroup reads, which every spelling must read. */
static char reference[4096];
static ssize_t reference_length;

/* Prints a line of what was checked, under Hermit only: stdout is compared
 * across backends. */
static void say(const char* format, ...) __attribute__((format(printf, 1, 2)));
static void say(const char* format, ...) {
  if (!hermit) {
    return;
  }
  va_list arguments;
  va_start(arguments, format);
  vprintf(format, arguments);
  va_end(arguments);
  printf("\n");
}

static ssize_t read_fd(int fd, char* buffer, size_t capacity) {
  size_t total = 0;
  while (total < capacity) {
    ssize_t n = read(fd, buffer + total, capacity - total);
    if (n < 0) {
      return -1;
    }
    if (n == 0) {
      break;
    }
    total += (size_t)n;
  }
  return (ssize_t)total;
}

/* Opens `path` relative to `dirfd`, reads it whole, and checks that it reads
 * the reference bytes. Returns the open descriptor, or -1. */
static int open_and_compare(int dirfd, const char* path, const char* what) {
  int fd = openat(dirfd, path, O_RDONLY);
  CHECK(fd >= 0, "%s: open %s: %s", what, path, strerror(errno));
  if (fd < 0) {
    return -1;
  }
  char buffer[4096];
  ssize_t length = read_fd(fd, buffer, sizeof(buffer));
  CHECK(length == reference_length && memcmp(buffer, reference, (size_t)length) == 0,
        "%s: %s read %zd bytes that differ from /proc/self/cgroup's %zd", what, path, length,
        reference_length);
  return fd;
}

static int same_file(const struct stat* a, const struct stat* b) {
  return a->st_dev == b->st_dev && a->st_ino == b->st_ino;
}

/* The file's metadata is Linux's for a process's cgroup file its owner
 * reads, and fstat of `fd` agrees with stat of `path`. */
static void check_metadata(int fd, const char* path, const char* what) {
  struct stat by_path;
  struct stat by_fd;
  CHECK(stat(path, &by_path) == 0, "%s: stat %s: %s", what, path, strerror(errno));
  CHECK(fstat(fd, &by_fd) == 0, "%s: fstat: %s", what, strerror(errno));
  CHECK(same_file(&by_path, &by_fd), "%s: fstat %lu:%lu, stat %lu:%lu", what,
        (unsigned long)by_fd.st_dev, (unsigned long)by_fd.st_ino, (unsigned long)by_path.st_dev,
        (unsigned long)by_path.st_ino);
  CHECK(by_fd.st_mode == (S_IFREG | 0444), "%s: mode %o", what, by_fd.st_mode);
  CHECK(by_fd.st_uid == geteuid() && by_fd.st_gid == getegid(), "%s: owner %u:%u, caller %u:%u",
        what, by_fd.st_uid, by_fd.st_gid, geteuid(), getegid());
  CHECK(by_fd.st_size == 0 && by_fd.st_nlink == 1 && by_fd.st_blksize == 1024,
        "%s: size %lld, %lu links, %ld-byte blocks", what, (long long)by_fd.st_size,
        (unsigned long)by_fd.st_nlink, (long)by_fd.st_blksize);
  if (hermit) {
    CHECK(by_fd.st_dev == RENDERED_DEVICE && (by_fd.st_ino >> 63) == 1,
          "%s: identity %#lx:%lu is not Detcore's rendered procfs file", what,
          (unsigned long)by_fd.st_dev, (unsigned long)by_fd.st_ino);
  }
  struct statfs fs;
  CHECK(fstatfs(fd, &fs) == 0 && fs.f_type == PROC_SUPER_MAGIC, "%s: not on procfs", what);
}

/* The descriptor's link reads `expected`. */
static void check_link(int fd, const char* expected, const char* what) {
  char link[64];
  char target[256];
  snprintf(link, sizeof(link), "/proc/self/fd/%d", fd);
  ssize_t length = readlink(link, target, sizeof(target) - 1);
  CHECK(length >= 0, "%s: readlink %s: %s", what, link, strerror(errno));
  if (length < 0) {
    return;
  }
  target[length] = '\0';
  CHECK(strcmp(target, expected) == 0, "%s: link %s, not %s", what, target, expected);
}

static void stat_of(const char* path, struct stat* out, const char* what) {
  CHECK(stat(path, out) == 0, "%s: stat %s: %s", what, path, strerror(errno));
}

static void nonexistent(const char* path, const char* what) {
  struct stat ignored;
  REFUSED(open(path, O_RDONLY), ENOENT, what);
  REFUSED(stat(path, &ignored), ENOENT, what);
}

/* The thread's /proc/thread-self/cgroup and the paths that name the same
 * file, read in the thread. */
struct thread_result {
  pid_t tid;
  struct stat thread_self;
  struct stat numbered;
  struct stat top_level;
  char link[256];
};

static void* thread_main(void* argument) {
  struct thread_result* result = argument;
  result->tid = (pid_t)syscall(SYS_gettid);
  int fd = open_and_compare(AT_FDCWD, "/proc/thread-self/cgroup", "thread");
  if (fd >= 0) {
    CHECK(fstat(fd, &result->thread_self) == 0, "thread: fstat");
    char link[64];
    snprintf(link, sizeof(link), "/proc/self/fd/%d", fd);
    ssize_t length = readlink(link, result->link, sizeof(result->link) - 1);
    result->link[length < 0 ? 0 : length] = '\0';
    close(fd);
  }
  char path[64];
  snprintf(path, sizeof(path), "/proc/%d/task/%d/cgroup", pid, result->tid);
  stat_of(path, &result->numbered, "thread");
  snprintf(path, sizeof(path), "/proc/%d/cgroup", result->tid);
  stat_of(path, &result->top_level, "thread");
  return NULL;
}

/* The caller's effective capabilities, as capget reports them. */
static unsigned long long effective_capabilities(void) {
  struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
  struct __user_cap_data_struct data[2];
  memset(data, 0, sizeof(data));
  CHECK(syscall(SYS_capget, &header, data) == 0, "capget: %s", strerror(errno));
  return (unsigned long long)data[0].effective | ((unsigned long long)data[1].effective << 32);
}

/* Whether the caller is in the initial user namespace: its uid map is the
 * identity over every id. */
static int in_initial_user_namespace(void) {
  FILE* map = fopen("/proc/self/uid_map", "r");
  if (map == NULL) {
    return 0;
  }
  unsigned long inside = 1;
  unsigned long outside = 1;
  unsigned long count = 0;
  int fields = fscanf(map, "%lu %lu %lu", &inside, &outside, &count);
  fclose(map);
  return fields == 3 && inside == 0 && outside == 0 && count == 4294967295UL;
}

/* Checks that the link `name` below `dirfd` reads `expected`. */
static void check_link_at(int dirfd, const char* name, const char* expected, const char* what) {
  char target[256];
  ssize_t length = readlinkat(dirfd, name, target, sizeof(target) - 1);
  CHECK(length >= 0, "%s: readlinkat: %s", what, strerror(errno));
  if (length >= 0) {
    target[length] = '\0';
    CHECK(strcmp(target, expected) == 0, "%s: the link reads %s, not %s", what, target, expected);
  }
}

/* `fd`, passed to this process over `pair` and received as a new
 * descriptor, which Detcore has no record of where its tool does not
 * discover descriptors as they are used. */
static int pass_to_self(int pair[2], int fd) {
  char token = 'p';
  struct iovec iov = {&token, 1};
  char control[CMSG_SPACE(sizeof(int))];
  memset(control, 0, sizeof(control));
  struct msghdr message = {
      .msg_iov = &iov, .msg_iovlen = 1, .msg_control = control, .msg_controllen = sizeof(control)};
  struct cmsghdr* header = CMSG_FIRSTHDR(&message);
  header->cmsg_level = SOL_SOCKET;
  header->cmsg_type = SCM_RIGHTS;
  header->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(header), &fd, sizeof(int));
  CHECK(sendmsg(pair[0], &message, MSG_NOSIGNAL) == 1, "sendmsg: %s", strerror(errno));
  memset(control, 0, sizeof(control));
  message.msg_controllen = sizeof(control);
  int received = -1;
  if (recvmsg(pair[1], &message, 0) == 1) {
    header = CMSG_FIRSTHDR(&message);
    if (header != NULL && header->cmsg_type == SCM_RIGHTS) {
      memcpy(&received, CMSG_DATA(header), sizeof(int));
    }
  }
  CHECK(received >= 0 && received != fd, "no descriptor was received");
  return received;
}

/* A descriptor on the file received over SCM_RIGHTS is the same open file:
 * it reads the bytes, shares the cursor with the descriptor sent, seeks and
 * maps as the file does, refuses writes as the file does, and reads ESRCH
 * once the file's task is reaped, as one Detcore opened. */
static void check_transfer(int dac_override) {
  int pair[2];
  CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0, "socketpair: %s", strerror(errno));
  int sent = open("/proc/self/cgroup", O_RDONLY);
  CHECK(sent >= 0, "transfer: open: %s", strerror(errno));
  int received = pass_to_self(pair, sent);
  if (sent >= 0 && received >= 0) {
    char buffer[sizeof(reference)];
    CHECK(read_fd(received, buffer, sizeof(buffer)) == reference_length &&
              memcmp(buffer, reference, (size_t)reference_length) == 0,
          "transfer: the received descriptor reads other bytes");
    /* One cursor: the sent descriptor is at the end too. */
    RETURNS(read(sent, buffer, sizeof(buffer)), 0, "transfer: the sent descriptor at the end");
    REFUSED(lseek(received, 0, SEEK_END), EINVAL, "transfer: SEEK_END");
    RETURNS(lseek(received, 2, SEEK_SET), 2, "transfer: SEEK_SET 2");
    RETURNS(read(sent, buffer, sizeof(buffer)), reference_length - 2,
            "transfer: the sent descriptor reads on from 2");
    void* mapping = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, received, 0);
    CHECK(mapping == MAP_FAILED && errno == ENODEV, "transfer: a mapping: %s",
          mapping == MAP_FAILED ? strerror(errno) : "mapped");
    struct stat by_sent;
    struct stat by_received;
    CHECK(fstat(sent, &by_sent) == 0 && fstat(received, &by_received) == 0 &&
              same_file(&by_sent, &by_received),
          "transfer: the received descriptor is another file");
    REFUSED(write(received, "x", 1), EBADF, "transfer: write to a read-only descriptor");
  }
  if (received >= 0) {
    close(received);
  }
  if (sent >= 0) {
    close(sent);
  }
  /* A descriptor opened for writing refuses every write as the file does. */
  if (dac_override) {
    sent = open("/proc/self/cgroup", O_RDWR);
    CHECK(sent >= 0, "transfer: open O_RDWR: %s", strerror(errno));
    received = pass_to_self(pair, sent);
    if (sent >= 0 && received >= 0) {
      REFUSED(write(received, "x", 1), EINVAL, "transfer: write");
      REFUSED(fallocate(received, 0, 0, 1), EOPNOTSUPP, "transfer: fallocate");
      RETURNS(ftruncate(received, 0), 0, "transfer: ftruncate");
      /* sendfile and splice into it, in Linux's order: an offset that
       * cannot be read before a closed source, a zero-length splice before
       * anything, an off_out that cannot be read before a write-only
       * source's EBADF, and a readable source this process received over
       * SCM_RIGHTS (which Detcore has no record of) reaching the
       * destination's EINVAL. */
      int write_only = open("/dev/null", O_WRONLY);
      REFUSED(sendfile(received, 999, (off_t*)1, 4), EFAULT,
              "transfer: sendfile, bad offset, closed source");
      RETURNS(splice(999, NULL, received, NULL, 0, 0), 0,
              "transfer: zero-length splice, closed source");
      REFUSED(splice(write_only, NULL, received, (loff_t*)1, 4, 0), EFAULT,
              "transfer: splice, bad off_out, write-only source");
      int source = open(program, O_RDONLY);
      int received_source = pass_to_self(pair, source);
      REFUSED(sendfile(received, received_source, NULL, 4), EINVAL,
              "transfer: sendfile from a received readable source");
      close(received_source);
      close(source);
      close(write_only);
      char buffer[sizeof(reference)];
      CHECK(read_fd(sent, buffer, sizeof(buffer)) == reference_length &&
                memcmp(buffer, reference, (size_t)reference_length) == 0,
            "transfer: the writes changed the file");
    }
    if (received >= 0) {
      close(received);
    }
    if (sent >= 0) {
      close(sent);
    }
  }
  /* O_APPEND set on the shared description by a child is the description's:
   * a zero-count sendfile into it is then EINVAL, not 0. */
  if (dac_override) {
    int appended = open("/proc/self/cgroup", O_RDWR);
    int source = open(program, O_RDONLY);
    CHECK(appended >= 0 && source >= 0, "append: open: %s", strerror(errno));
    RETURNS(sendfile(appended, source, NULL, 0), 0, "append: a zero-count sendfile");
    fflush(stdout);
    pid_t setter = fork();
    if (setter == 0) {
      _exit(fcntl(appended, F_SETFL, O_APPEND) == 0 ? 0 : 1);
    }
    int status;
    CHECK(waitpid(setter, &status, 0) == setter && WIFEXITED(status) && WEXITSTATUS(status) == 0,
          "append: the child's F_SETFL(O_APPEND)");
    CHECK((fcntl(appended, F_GETFL) & O_APPEND) != 0, "append: the flag is not shared");
    REFUSED(sendfile(appended, source, NULL, 0), EINVAL,
            "append: a zero-count sendfile into an O_APPEND description");
    close(source);
    close(appended);
  }
  /* A child's file, received while the child lives and read once it is
   * reaped: its first read is ESRCH. */
  int go[2];
  CHECK(pipe(go) == 0, "pipe");
  fflush(stdout);
  pid_t child = fork();
  if (child == 0) {
    char token;
    _exit(read(go[0], &token, 1) == 1 ? 0 : 1);
  }
  char path[64];
  snprintf(path, sizeof(path), "/proc/%d/cgroup", child);
  sent = open(path, O_RDONLY);
  CHECK(sent >= 0, "transfer: open the child's file: %s", strerror(errno));
  received = pass_to_self(pair, sent);
  CHECK(write(go[1], "g", 1) == 1, "release the child");
  int status;
  CHECK(waitpid(child, &status, 0) == child, "waitpid: %s", strerror(errno));
  if (received >= 0) {
    char byte;
    REFUSED(read(received, &byte, 1), ESRCH, "transfer: the first read after the reap");
    close(received);
  }
  if (sent >= 0) {
    close(sent);
  }
  /* The sender closes its own descriptor before the receiver uses the one it
   * received: the open file lives on in the received one alone. */
  sent = open("/proc/self/cgroup", O_RDONLY);
  received = pass_to_self(pair, sent);
  close(sent);
  if (received >= 0) {
    char buffer[sizeof(reference)];
    CHECK(read_fd(received, buffer, sizeof(buffer)) == reference_length &&
              memcmp(buffer, reference, (size_t)reference_length) == 0,
          "transfer: the received descriptor of a closed sender reads other bytes");
    close(received);
  }
  close(go[0]);
  close(go[1]);
  close(pair[0]);
  close(pair[1]);
  say("transfer: a descriptor received over SCM_RIGHTS reads the bytes, shares the sent "
      "descriptor's cursor, seeks, maps and refuses writes as the file does, reads ESRCH "
      "after its task is reaped, and outlives the sender's");
  if (dac_override) {
    say("transfer: sendfile and splice into the file answer in Linux's order, a received "
        "readable source included, and a child's O_APPEND makes a zero-count sendfile EINVAL");
  }
}

/* Under --no-sequentialize-threads (a declared relaxation): what one thread's
 * own calls decide holds (the bytes, the live owner, a time and a hint it
 * sets); a zombie's owner and a reaped child's file follow when Detcore's
 * process table learns of the child's exit, which there is host-timed, so
 * either answer is accepted and the line says so. */
static int check_relaxed(void) {
  int fd = open("/proc/self/cgroup", O_RDONLY);
  char buffer[sizeof(reference)];
  CHECK(fd >= 0 && read_fd(fd, buffer, sizeof(buffer)) == 5 && memcmp(buffer, "0::/\n", 5) == 0,
        "relaxed: the bytes");
  struct stat metadata;
  CHECK(fd >= 0 && fstat(fd, &metadata) == 0 && metadata.st_uid == geteuid(),
        "relaxed: the live owner");
  struct timespec times[2] = {{0, UTIME_OMIT}, {1234, 5}};
  RETURNS(utimensat(AT_FDCWD, "/proc/self/cgroup", times, 0), 0, "relaxed: utimensat");
  stat_of("/proc/self/cgroup", &metadata, "relaxed");
  CHECK(metadata.st_mtim.tv_sec == 1234 && metadata.st_mtim.tv_nsec == 5, "relaxed: the mtime");
  unsigned long long hint = 2;
  unsigned long long read_back = 0;
  int other = open("/proc/self/cgroup", O_RDONLY);
  CHECK(fd >= 0 && fcntl(fd, F_SET_RW_HINT_COMMAND, &hint) == 0 && other >= 0 &&
            fcntl(other, F_GET_RW_HINT_COMMAND, &read_back) == 0 && read_back == 2,
        "relaxed: the write hint");
  close(other);
  close(fd);
  fflush(stdout);
  pid_t child = fork();
  if (child == 0) {
    _exit(0);
  }
  siginfo_t info;
  CHECK(waitid(P_PID, (id_t)child, &info, WEXITED | WNOWAIT) == 0, "relaxed: waitid");
  char path[64];
  snprintf(path, sizeof(path), "/proc/%d/cgroup", child);
  struct stat zombie;
  struct stat global_root;
  stat_of("/proc/version", &global_root, "relaxed");
  int zombie_stat = stat(path, &zombie);
  int zombie_global = zombie_stat == 0 && zombie.st_uid == global_root.st_uid;
  int zombie_live = zombie_stat == 0 && zombie.st_uid == geteuid();
  CHECK(zombie_global || zombie_live, "relaxed: a zombie's file: %s",
        zombie_stat == 0 ? "another owner" : strerror(errno));
  /* Each accepted outcome, named (stderr, which the run does not compare). */
  fprintf(stderr, "relaxed: OUTCOME zombie-owner=%s\n",
          zombie_global ? "global-root" : zombie_live ? "live-owner" : "other");
  int status;
  CHECK(waitpid(child, &status, 0) == child, "relaxed: waitpid");
  struct stat reaped;
  int reaped_stat = stat(path, &reaped);
  int reaped_errno = errno;
  CHECK(reaped_stat == 0 || reaped_errno == ENOENT, "relaxed: a reaped child's file: %s",
        strerror(reaped_errno));
  fprintf(stderr, "relaxed: OUTCOME reaped-child-file=%s\n",
          reaped_stat == 0 ? "still-listed" : reaped_errno == ENOENT ? "absent" : "other");
  printf("relaxed: bytes, the live owner, the mtime and the write hint hold; a zombie's owner "
         "(global root, or the live owner before Detcore sees the exit) and a reaped child's "
         "file are host-timed and not compared\n");
  return failures == 0 ? 0 : 1;
}

/* Under --no-sequentialize-threads: a rendered descriptor received over
 * SCM_RIGHTS, which Detcore has no record of, cannot be adopted safely, and
 * a call through it is refused by name; with --allow-unsupported-syscalls
 * the call is EOPNOTSUPP. */
static int check_relaxed_transfer(void) {
  int pair[2];
  CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0, "socketpair: %s", strerror(errno));
  int sent = open("/proc/self/cgroup", O_RDONLY);
  int received = pass_to_self(pair, sent);
  char byte;
  errno = 0;
  long result = read(received, &byte, 1);
  printf("relaxed transfer: a read through a received rendered descriptor gave %ld (%s)\n", result,
         result < 0 ? strerror(errno) : "no error");
  return failures == 0 ? 0 : 1;
}

static void check_spellings(void) {
  char path[128];
  char expected[128];
  struct stat self;
  struct stat thread_self;
  struct stat other;
  int fd = open_and_compare(AT_FDCWD, "/proc/self/cgroup", "self");
  if (fd < 0) {
    return;
  }
  check_metadata(fd, "/proc/self/cgroup", "self");
  snprintf(expected, sizeof(expected), "/proc/%d/cgroup", pid);
  check_link(fd, expected, "self");
  CHECK(fstat(fd, &self) == 0, "self: fstat");
  int numbered = open_and_compare(AT_FDCWD, expected, "numbered");
  if (numbered >= 0) {
    check_metadata(numbered, expected, "numbered");
    check_link(numbered, expected, "numbered");
    CHECK(fstat(numbered, &other) == 0 && same_file(&self, &other),
          "/proc/<pid>/cgroup is not /proc/self/cgroup");
    close(numbered);
  }
  stat_of("/proc/self/./cgroup", &other, "dot");
  CHECK(same_file(&self, &other), "/proc/self/./cgroup is not /proc/self/cgroup");
  say("process: /proc/self/cgroup = /proc/<pid>/cgroup = /proc/self/./cgroup");

  /* Reopened through its descriptor links. */
  snprintf(path, sizeof(path), "/proc/self/fd/%d", fd);
  int reopened = open_and_compare(AT_FDCWD, path, "reopened");
  if (reopened >= 0) {
    CHECK(fstat(reopened, &other) == 0 && same_file(&self, &other),
          "/proc/self/fd/N is not the file");
    check_link(reopened, expected, "reopened");
    close(reopened);
  }
  snprintf(path, sizeof(path), "/dev/fd/%d", fd);
  reopened = open_and_compare(AT_FDCWD, path, "/dev/fd");
  if (reopened >= 0) {
    CHECK(fstat(reopened, &other) == 0 && same_file(&self, &other), "/dev/fd/N is not the file");
    close(reopened);
  }
  say("reopened: /proc/self/fd/N = /dev/fd/N = the file, link /proc/<pid>/cgroup");

  int thread_fd = open_and_compare(AT_FDCWD, "/proc/thread-self/cgroup", "thread-self");
  if (thread_fd >= 0) {
    snprintf(expected, sizeof(expected), "/proc/%d/task/%d/cgroup", pid, pid);
    check_metadata(thread_fd, "/proc/thread-self/cgroup", "thread-self");
    check_link(thread_fd, expected, "thread-self");
    CHECK(fstat(thread_fd, &thread_self) == 0, "thread-self: fstat");
    CHECK(!same_file(&self, &thread_self), "/proc/thread-self/cgroup is /proc/self/cgroup");
    stat_of(expected, &other, "task");
    CHECK(same_file(&thread_self, &other), "/proc/<pid>/task/<pid>/cgroup is not thread-self's");
    snprintf(path, sizeof(path), "/proc/self/task/%d/cgroup", pid);
    stat_of(path, &other, "self task");
    CHECK(same_file(&thread_self, &other), "/proc/self/task/<pid>/cgroup is not thread-self's");
    close(thread_fd);
  }
  say("thread: /proc/thread-self/cgroup = /proc/<pid>/task/<pid>/cgroup = "
      "/proc/self/task/<pid>/cgroup, not /proc/self/cgroup");

  struct thread_result result;
  memset(&result, 0, sizeof(result));
  pthread_t thread;
  CHECK(pthread_create(&thread, NULL, thread_main, &result) == 0, "pthread_create");
  pthread_join(thread, NULL);
  snprintf(expected, sizeof(expected), "/proc/%d/task/%d/cgroup", pid, result.tid);
  CHECK(strcmp(result.link, expected) == 0, "thread: link %s, not %s", result.link, expected);
  CHECK(same_file(&result.thread_self, &result.numbered),
        "a thread's thread-self file is not /proc/<pid>/task/<tid>/cgroup");
  CHECK(!same_file(&result.thread_self, &thread_self) && !same_file(&result.thread_self, &self),
        "a thread's file is the leader's");
  CHECK(!same_file(&result.top_level, &self) && !same_file(&result.top_level, &result.numbered),
        "a thread's /proc/<tid>/cgroup is another spelling's");
  say("other thread: its thread-self = /proc/<pid>/task/<tid>/cgroup, and /proc/<tid>/cgroup "
      "is a file of its own");

  /* Relative to /proc directories. */
  int proc_directory = open("/proc", O_RDONLY | O_DIRECTORY);
  CHECK(proc_directory >= 0, "open /proc: %s", strerror(errno));
  int fd2 = open_and_compare(proc_directory, "self/cgroup", "/proc dirfd");
  if (fd2 >= 0) {
    CHECK(fstat(fd2, &other) == 0 && same_file(&self, &other), "/proc + self/cgroup");
    close(fd2);
  }
  snprintf(path, sizeof(path), "%d/cgroup", pid);
  fd2 = open_and_compare(proc_directory, path, "/proc dirfd numbered");
  if (fd2 >= 0) {
    CHECK(fstat(fd2, &other) == 0 && same_file(&self, &other), "/proc + <pid>/cgroup");
    close(fd2);
  }
  say("directories: /proc + self/cgroup, /proc + <pid>/cgroup = the process's file");
  int self_directory = -1;
  int pid_directory = -1;
  int task_directory = -1;
  int thread_directory = -1;
  int relinked = -1;
  if (!process_directories) {
    fprintf(stderr,
            "SKIPPED: names relative to /proc/self, /proc/<pid>, /proc/self/task and "
            "/proc/thread-self directories, also inherited and received "
            "(--no-process-directories)\n");
  } else {
    self_directory = open("/proc/self", O_RDONLY | O_DIRECTORY);
    CHECK(self_directory >= 0, "open /proc/self: %s", strerror(errno));
    fd2 = open_and_compare(self_directory, "cgroup", "/proc/self dirfd");
    if (fd2 >= 0) {
      CHECK(fstat(fd2, &other) == 0 && same_file(&self, &other), "/proc/self + cgroup");
      close(fd2);
    }
    snprintf(path, sizeof(path), "/proc/%d", pid);
    pid_directory = open(path, O_RDONLY | O_DIRECTORY);
    CHECK(pid_directory >= 0, "open %s: %s", path, strerror(errno));
    fd2 = open_and_compare(pid_directory, "cgroup", "/proc/<pid> dirfd");
    if (fd2 >= 0) {
      CHECK(fstat(fd2, &other) == 0 && same_file(&self, &other), "/proc/<pid> + cgroup");
      close(fd2);
    }
    task_directory = open("/proc/self/task", O_RDONLY | O_DIRECTORY);
    CHECK(task_directory >= 0, "open /proc/self/task: %s", strerror(errno));
    snprintf(path, sizeof(path), "%d/cgroup", pid);
    fd2 = open_and_compare(task_directory, path, "/proc/self/task dirfd");
    if (fd2 >= 0) {
      CHECK(fstat(fd2, &other) == 0 && same_file(&thread_self, &other),
            "/proc/self/task + <pid>/cgroup");
      close(fd2);
    }
    thread_directory = open("/proc/thread-self", O_RDONLY | O_DIRECTORY);
    CHECK(thread_directory >= 0, "open /proc/thread-self: %s", strerror(errno));
    fd2 = open_and_compare(thread_directory, "cgroup", "/proc/thread-self dirfd");
    if (fd2 >= 0) {
      CHECK(fstat(fd2, &other) == 0 && same_file(&thread_self, &other),
            "/proc/thread-self + cgroup");
      close(fd2);
    }
    snprintf(path, sizeof(path), "/proc/self/fd/%d", self_directory);
    relinked = open(path, O_RDONLY | O_DIRECTORY);
    CHECK(relinked >= 0, "open %s: %s", path, strerror(errno));
    fd2 = open_and_compare(relinked, "cgroup", "reopened /proc/self dirfd");
    if (fd2 >= 0) {
      CHECK(fstat(fd2, &other) == 0 && same_file(&self, &other),
            "a reopened /proc/self + cgroup");
      close(fd2);
    }
    /* The descriptor links below a task's directory are that task's. */
    snprintf(expected, sizeof(expected), "/proc/%d/cgroup", pid);
    char linked[16];
    snprintf(linked, sizeof(linked), "fd/%d", fd);
    check_link_at(thread_directory, linked, expected, "/proc/thread-self + fd/N");
    check_link_at(pid_directory, linked, expected, "/proc/<pid> + fd/N");
    /* An O_PATH | O_NOFOLLOW open of /proc/self is the link, no directory. */
    int link = open("/proc/self", O_PATH | O_NOFOLLOW);
    CHECK(link >= 0, "open the /proc/self link: %s", strerror(errno));
    if (link >= 0) {
      REFUSED(openat(link, "cgroup", O_RDONLY), ENOTDIR, "a name below the /proc/self link");
      close(link);
    }
    say("process directories: /proc/self + cgroup, /proc/<pid> + cgroup, reopened /proc/self + "
        "cgroup = the process's file; /proc/self/task + <pid>/cgroup, /proc/thread-self + "
        "cgroup = the thread's; fd/N below /proc/thread-self and /proc/<pid> = the process's "
        "descriptor; nothing below the /proc/self link itself");
  }

  /* A fork's child. */
  int ready[2];
  int done[2];
  CHECK(pipe(ready) == 0 && pipe(done) == 0, "pipe");
  fflush(stdout);
  pid_t child = fork();
  if (child == 0) {
    pid_t me = getpid();
    struct stat mine;
    int own = open_and_compare(AT_FDCWD, "/proc/self/cgroup", "child");
    if (own >= 0) {
      snprintf(expected, sizeof(expected), "/proc/%d/cgroup", me);
      check_link(own, expected, "child");
      CHECK(fstat(own, &mine) == 0 && !same_file(&mine, &self),
            "a child's /proc/self/cgroup is its parent's");
      close(own);
    }
    snprintf(expected, sizeof(expected), "/proc/%d/cgroup", pid);
    if (process_directories) {
      /* Below the parent's /proc/self directory, fd/N is the parent's
       * descriptor, whatever the child holds at N. */
      CHECK(dup2(ready[1], fd) == fd, "dup2: %s", strerror(errno));
      char linked[16];
      snprintf(linked, sizeof(linked), "fd/%d", fd);
      check_link_at(self_directory, linked, expected, "child: inherited /proc/self + fd/N");
      int inherited = open_and_compare(self_directory, "cgroup", "child via inherited dirfd");
      if (inherited >= 0) {
        CHECK(fstat(inherited, &other) == 0 && same_file(&other, &self),
              "the inherited /proc/self directory + cgroup is not the parent's file");
        check_link(inherited, expected, "child via inherited dirfd");
        close(inherited);
      }
      inherited = open_and_compare(relinked, "cgroup", "child via inherited reopened dirfd");
      if (inherited >= 0) {
        CHECK(fstat(inherited, &other) == 0 && same_file(&other, &self),
              "the inherited reopened /proc/self directory + cgroup is not the parent's file");
        close(inherited);
      }
      inherited = open_and_compare(thread_directory, "cgroup", "child via thread-self dirfd");
      if (inherited >= 0) {
        CHECK(fstat(inherited, &other) == 0 && same_file(&other, &thread_self),
              "the inherited /proc/thread-self directory + cgroup is not the parent's thread's");
        close(inherited);
      }
    }
    stat_of(expected, &other, "child");
    CHECK(same_file(&other, &self), "a child's /proc/<parent>/cgroup is not the parent's");
    if (write(ready[1], "r", 1) != 1) {
      _exit(2);
    }
    char byte;
    if (read(done[0], &byte, 1) != 1) {
      _exit(2);
    }
    _exit(failures == 0 ? 0 : 1);
  }
  CHECK(child > 0, "fork: %s", strerror(errno));
  char byte;
  CHECK(read(ready[0], &byte, 1) == 1, "the child did not get ready");
  snprintf(path, sizeof(path), "/proc/%d/cgroup", child);
  int childs = open_and_compare(AT_FDCWD, path, "parent reads the child's");
  if (childs >= 0) {
    check_link(childs, path, "the child's file");
    CHECK(fstat(childs, &other) == 0 && !same_file(&other, &self),
          "the child's file is the parent's");
    close(childs);
  }
  /* A task of the child is not a thread of this process. */
  snprintf(path, sizeof(path), "/proc/self/task/%d/cgroup", child);
  nonexistent(path, "the child as a thread of the parent");
  /* Descriptors on the child's file kept past its reap: one never read, one
   * read whole. */
  snprintf(path, sizeof(path), "/proc/%d/cgroup", child);
  int unread = open(path, O_RDONLY);
  int consumed = open(path, O_RDONLY);
  char directory_path[64];
  snprintf(directory_path, sizeof(directory_path), "/proc/%d", child);
  int child_directory = process_directories ? open(directory_path, O_RDONLY | O_DIRECTORY) : -1;
  CHECK(!process_directories || child_directory >= 0, "open the child's directory: %s",
        strerror(errno));
  CHECK(unread >= 0 && consumed >= 0, "open the child's file: %s", strerror(errno));
  char buffer[sizeof(reference)];
  CHECK(read_fd(consumed, buffer, sizeof(buffer)) == reference_length,
        "read the child's file whole");
  CHECK(write(done[1], "d", 1) == 1, "release the child");
  /* The child, exited and not yet reaped, is a zombie and keeps its file,
   * which Linux gives to global root once the task has no memory map: its
   * owner is /proc/version's, and it is writable only with
   * CAP_DAC_OVERRIDE where global root is mapped. */
  siginfo_t info;
  CHECK(waitid(P_PID, (id_t)child, &info, WEXITED | WNOWAIT) == 0, "waitid: %s", strerror(errno));
  int zombie = openat(AT_FDCWD, path, O_RDONLY);
  CHECK(zombie >= 0, "a zombie's cgroup file: %s", strerror(errno));
  if (zombie >= 0) {
    close(zombie);
  }
  if (child_directory >= 0) {
    zombie = openat(child_directory, "cgroup", O_RDONLY);
    CHECK(zombie >= 0, "a zombie's cgroup below its directory: %s", strerror(errno));
    if (zombie >= 0) {
      close(zombie);
    }
  }
  struct stat global_root;
  struct stat zombie_metadata;
  stat_of("/proc/version", &global_root, "global root");
  stat_of(path, &zombie_metadata, "a zombie's file");
  CHECK(zombie_metadata.st_uid == global_root.st_uid &&
            zombie_metadata.st_gid == global_root.st_gid,
        "a zombie's file is owned by %u:%u, not global root's %u:%u", zombie_metadata.st_uid,
        zombie_metadata.st_gid, global_root.st_uid, global_root.st_gid);
  int zombie_writes = (global_root.st_uid == 0 &&
                       ((effective_capabilities() >> CAP_DAC_OVERRIDE) & 1))
                          ? 0
                          : EACCES;
  int zombie_writer = open(path, O_WRONLY);
  CHECK(zombie_writes == 0 ? zombie_writer >= 0 : (zombie_writer == -1 && errno == EACCES),
        "open O_WRONLY of a zombie's file: %d (%s)", zombie_writer, strerror(errno));
  if (zombie_writer >= 0) {
    close(zombie_writer);
  }
  ANSWERS(access(path, W_OK), zombie_writes, "access(W_OK) of a zombie's file");
  int status;
  CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0,
        "the child failed its checks (status %#x)", status);
  nonexistent(path, "a reaped child");
  if (child_directory >= 0) {
    REFUSED(openat(child_directory, "cgroup", O_RDONLY), ESRCH,
            "a name below a reaped child's directory");
    close(child_directory);
  }
  /* Its `show` runs again at a first read, a rewind's read and a positioned
   * read: a task that is gone is ESRCH. A read after the whole file was read
   * is end of file, from the buffer. */
  if (unread >= 0) {
    REFUSED(read(unread, buffer, sizeof(buffer)), ESRCH, "the first read after a reap");
    close(unread);
  }
  if (consumed >= 0) {
    RETURNS(read(consumed, buffer, sizeof(buffer)), 0, "a read at end after a reap");
    REFUSED(pread(consumed, buffer, sizeof(buffer), 0), ESRCH, "a pread after a reap");
    RETURNS(lseek(consumed, 0, SEEK_SET), 0, "a rewind after a reap");
    REFUSED(read(consumed, buffer, sizeof(buffer)), ESRCH, "a read after a rewind after a reap");
    struct stat kept;
    CHECK(fstat(consumed, &kept) == 0, "fstat after a reap: %s", strerror(errno));
    close(consumed);
  }
  say("fork: the child's /proc/self/cgroup is its own; its /proc/<parent>/cgroup is the "
      "parent's; the parent reads /proc/<child>/cgroup; a zombie's exists, owned by global "
      "root and not writable through its owner; a reaped child's does not, and a descriptor "
      "on it reads ESRCH where its show runs again");
  if (process_directories) {
    say("fork directories: the child's inherited /proc/self, reopened /proc/self and "
        "/proc/thread-self directories name the parent's");
  }

  /* A /proc/self directory whose process is gone has nothing below it: a
   * child opens its /proc/self, forks a grandchild that inherits it, and
   * exits; once this process reaps the child, every name the grandchild
   * looks up below the directory is ESRCH, as Linux checks the directory's
   * task at each lookup. */
  if (process_directories) {
    int result[2];
    int go[2];
    CHECK(pipe(result) == 0 && pipe(go) == 0, "pipe");
    fflush(stdout);
    pid_t opener = fork();
    if (opener == 0) {
      int directory = open("/proc/self", O_RDONLY | O_DIRECTORY);
      pid_t grandchild = fork();
      if (grandchild == 0) {
        char token;
        if (read(go[0], &token, 1) != 1) {
          _exit(2);
        }
        errno = 0;
        int opened = openat(directory, "cgroup", O_RDONLY) == -1 && errno == ESRCH;
        struct stat ignored;
        errno = 0;
        int stated = fstatat(directory, "cgroup", &ignored, 0) == -1 && errno == ESRCH;
        errno = 0;
        int accessed = faccessat(directory, "cgroup", R_OK, 0) == -1 && errno == ESRCH;
        char answer = opened && stated && accessed ? 'e' : 'x';
        if (write(result[1], &answer, 1) != 1) {
          _exit(2);
        }
        _exit(0);
      }
      _exit(directory >= 0 && grandchild > 0 ? 0 : 1);
    }
    int opened;
    CHECK(waitpid(opener, &opened, 0) == opener && WIFEXITED(opened) && WEXITSTATUS(opened) == 0,
          "the opener failed (status %#x)", opened);
    CHECK(write(go[1], "g", 1) == 1, "release the grandchild");
    char answer = 0;
    CHECK(read(result[0], &answer, 1) == 1 && answer == 'e',
          "a /proc/self directory of a reaped process answers other than ESRCH below it");
    close(result[0]);
    close(result[1]);
    close(go[0]);
    close(go[1]);
    say("stale: below a /proc/self directory of a reaped process, open, stat and access are "
        "ESRCH");
  }

  /* A /proc/self directory received over SCM_RIGHTS names its sender's. */
  if (process_directories) {
    int pair[2];
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0, "socketpair");
    fflush(stdout);
    pid_t sender = fork();
    if (sender == 0) {
      close(pair[0]);
      int directory = open("/proc/self", O_RDONLY | O_DIRECTORY);
      char token = 's';
      struct iovec iov = {&token, 1};
      char control[CMSG_SPACE(sizeof(int))];
      memset(control, 0, sizeof(control));
      struct msghdr message = {.msg_iov = &iov,
                               .msg_iovlen = 1,
                               .msg_control = control,
                               .msg_controllen = sizeof(control)};
      struct cmsghdr* header = CMSG_FIRSTHDR(&message);
      header->cmsg_level = SOL_SOCKET;
      header->cmsg_type = SCM_RIGHTS;
      header->cmsg_len = CMSG_LEN(sizeof(int));
      memcpy(CMSG_DATA(header), &directory, sizeof(int));
      if (directory < 0 || sendmsg(pair[1], &message, 0) != 1) {
        _exit(2);
      }
      /* Stay until the receiver is done with the directory. */
      if (read(pair[1], &token, 1) != 1) {
        _exit(2);
      }
      _exit(0);
    }
    close(pair[1]);
    char token;
    struct iovec iov = {&token, 1};
    char control[CMSG_SPACE(sizeof(int))];
    struct msghdr message = {.msg_iov = &iov,
                             .msg_iovlen = 1,
                             .msg_control = control,
                             .msg_controllen = sizeof(control)};
    int received = -1;
    if (recvmsg(pair[0], &message, 0) == 1) {
      struct cmsghdr* header = CMSG_FIRSTHDR(&message);
      if (header != NULL && header->cmsg_type == SCM_RIGHTS) {
        memcpy(&received, CMSG_DATA(header), sizeof(int));
      }
    }
    CHECK(received >= 0, "no /proc/self directory was received");
    if (received >= 0) {
      snprintf(expected, sizeof(expected), "/proc/%d/cgroup", sender);
      int senders = open_and_compare(received, "cgroup", "a received /proc/self directory");
      if (senders >= 0) {
        check_link(senders, expected, "a received /proc/self directory");
        stat_of(expected, &other, "the sender's file");
        struct stat theirs;
        CHECK(fstat(senders, &theirs) == 0 && same_file(&theirs, &other) &&
                  !same_file(&theirs, &self),
              "the received /proc/self directory + cgroup is not the sender's file");
        close(senders);
      }
      close(received);
    }
    CHECK(send(pair[0], "d", 1, MSG_NOSIGNAL) == 1, "release the sender");
    CHECK(waitpid(sender, &status, 0) == sender && WIFEXITED(status) && WEXITSTATUS(status) == 0,
          "the sender failed (status %#x)", status);
    close(pair[0]);
    say("scm_rights: a received /proc/self directory + cgroup = the sender's file");
  }

  /* Tasks and spellings no process holds. */
  nonexistent("/proc/999999/cgroup", "no such pid");
  snprintf(path, sizeof(path), "/proc/%d/task/999999/cgroup", pid);
  nonexistent(path, "no such thread");
  snprintf(path, sizeof(path), "/proc/0%d/cgroup", pid);
  nonexistent(path, "a leading zero");
  say("nonexistent: a reaped pid, an unused pid, an unused tid, a leading zero: ENOENT");

  if (process_directories) {
    close(relinked);
    close(thread_directory);
    close(task_directory);
    close(pid_directory);
    close(self_directory);
  }
  close(proc_directory);
  close(fd);
}

static void check_reads(void) {
  int fd = open("/proc/self/cgroup", O_RDONLY);
  CHECK(fd >= 0, "open: %s", strerror(errno));
  if (fd < 0) {
    return;
  }
  REFUSED(lseek(fd, 0, SEEK_END), EINVAL, "SEEK_END before a read");
  char buffer[4096];
  ssize_t head = read(fd, buffer, 2);
  ssize_t rest = read_fd(fd, buffer + 2, sizeof(buffer) - 2);
  CHECK(head == 2 && head + rest == reference_length &&
            memcmp(buffer, reference, (size_t)reference_length) == 0,
        "reading in parts gave %zd+%zd bytes", head, rest);
  REFUSED(lseek(fd, 0, SEEK_END), EINVAL, "SEEK_END after a read");
  REFUSED(lseek(fd, 0, SEEK_DATA), EINVAL, "SEEK_DATA");
  RETURNS(lseek(fd, 0, SEEK_SET), 0, "SEEK_SET");
  ssize_t again = read_fd(fd, buffer, sizeof(buffer));
  CHECK(again == reference_length, "a read after SEEK_SET gave %zd bytes", again);
  ssize_t positioned = pread(fd, buffer, sizeof(buffer), 1);
  CHECK(positioned == reference_length - 1 &&
            memcmp(buffer, reference + 1, (size_t)positioned) == 0,
        "pread at 1 gave %zd bytes", positioned);
  close(fd);
  say("reads: SEEK_END and SEEK_DATA EINVAL; parts, a rewind and pread read the bytes");
}

/* Every call a write-mode descriptor makes is refused or does nothing, and
 * the file still reads its bytes. */
static void check_write_mode_descriptor(int fd, const char* what) {
  char buffer[4096];
  struct iovec vector = {"x", 1};
  RETURNS(fcntl(fd, F_GETFL) & O_ACCMODE, O_RDWR, what);
  REFUSED(write(fd, "x", 1), EINVAL, what);
  REFUSED(writev(fd, &vector, 1), EINVAL, what);
  REFUSED(pwritev2(fd, &vector, 1, -1, 0), EINVAL, what);
  REFUSED(pwrite(fd, "x", 1, 0), ESPIPE, what);
  RETURNS(ftruncate(fd, 5), 0, what);
  REFUSED(fallocate(fd, 0, 0, 10), EOPNOTSUPP, what);
  int zero = open("/dev/zero", O_RDONLY);
  off_t offset = 0;
  REFUSED(sendfile(fd, zero, &offset, 1), EINVAL, what);
  close(zero);
  int pipe_ends[2];
  CHECK(pipe(pipe_ends) == 0, "pipe");
  CHECK(write(pipe_ends[1], "x", 1) == 1, "pipe write");
  REFUSED(splice(pipe_ends[0], NULL, fd, NULL, 1, 0), EINVAL, what);
  close(pipe_ends[0]);
  close(pipe_ends[1]);
  void* mapping = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  CHECK(mapping == MAP_FAILED && errno == ENODEV, "%s: a shared writable mapping: %s", what,
        mapping == MAP_FAILED ? strerror(errno) : "mapped");
  REFUSED(fcntl(fd, F_ADD_SEALS, F_SEAL_SEAL), EINVAL, what);
  /* Nothing changed what the file reads, through this descriptor or another. */
  ssize_t length = pread(fd, buffer, sizeof(buffer), 0);
  CHECK(length == reference_length && memcmp(buffer, reference, (size_t)length) == 0,
        "%s: after the writes the file reads %zd bytes", what, length);
  struct stat metadata;
  CHECK(fstat(fd, &metadata) == 0 && metadata.st_size == 0, "%s: size after the writes", what);
  int fresh = open_and_compare(AT_FDCWD, "/proc/self/cgroup", "after the writes");
  if (fresh >= 0) {
    close(fresh);
  }
}

static void check_owner_calls(void) {
  const char* path = "/proc/self/cgroup";
  unsigned long long capabilities = effective_capabilities();
  int dac_override = (capabilities >> CAP_DAC_OVERRIDE) & 1;
  int sys_admin = (capabilities >> CAP_SYS_ADMIN) & 1;
  say("capabilities: dac_override=%d sys_admin=%d", dac_override, sys_admin);
  int writes = dac_override ? 0 : EACCES;
  int fd = open(path, O_RDWR);
  CHECK(dac_override ? fd >= 0 : (fd == -1 && errno == EACCES), "open O_RDWR: %d (%s)", fd,
        strerror(errno));
  if (fd >= 0) {
    check_write_mode_descriptor(fd, "O_RDWR");
    close(fd);
  }
  fd = open(path, O_WRONLY);
  CHECK(dac_override ? fd >= 0 : (fd == -1 && errno == EACCES), "open O_WRONLY: %d (%s)", fd,
        strerror(errno));
  if (fd >= 0) {
    char byte;
    REFUSED(read(fd, &byte, 1), EBADF, "read of O_WRONLY");
    REFUSED(write(fd, "x", 1), EINVAL, "write of O_WRONLY");
    void* mapping = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, fd, 0);
    CHECK(mapping == MAP_FAILED && errno == EACCES, "a mapping of O_WRONLY: %s",
          mapping == MAP_FAILED ? strerror(errno) : "mapped");
    close(fd);
  }
  fd = open(path, O_RDONLY | O_TRUNC);
  CHECK(dac_override ? fd >= 0 : (fd == -1 && errno == EACCES), "open O_TRUNC: %d (%s)", fd,
        strerror(errno));
  if (fd >= 0) {
    close(fd);
  }
  /* Access mode 3 asks for both permissions and gives neither. */
  fd = open(path, O_ACCMODE);
  CHECK(dac_override ? fd >= 0 : (fd == -1 && errno == EACCES), "open O_ACCMODE: %d (%s)", fd,
        strerror(errno));
  if (fd >= 0) {
    char byte;
    REFUSED(read(fd, &byte, 1), EBADF, "read of mode 3");
    REFUSED(write(fd, "x", 1), EBADF, "write of mode 3");
    REFUSED(ftruncate(fd, 0), EINVAL, "ftruncate of mode 3");
    REFUSED(fallocate(fd, 0, 0, 1), EBADF, "fallocate of mode 3");
    REFUSED(pwrite(fd, "x", 1, 0), ESPIPE, "pwrite of mode 3");
    void* mapping = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, fd, 0);
    CHECK(mapping == MAP_FAILED && errno == EACCES, "a mapping of mode 3: %s",
          mapping == MAP_FAILED ? strerror(errno) : "mapped");
    close(fd);
  }
  ANSWERS(access(path, W_OK), writes, "access(W_OK)");
  ANSWERS(syscall(SYS_faccessat2, AT_FDCWD, path, W_OK, AT_EACCESS), writes,
          "faccessat2(W_OK, AT_EACCESS)");
  RETURNS(access(path, R_OK), 0, "access(R_OK)");
  REFUSED(access(path, X_OK), EACCES, "access(X_OK)");
  ANSWERS(truncate(path, 0), writes, "truncate");
  REFUSED(truncate(path, -1), EINVAL, "truncate(-1)");
  /* Times its owner sets stay on the file, for every spelling and
   * descriptor; another file (the thread's) keeps its own. */
  char numbered[64];
  snprintf(numbered, sizeof(numbered), "/proc/%d/cgroup", pid);
  int held = open(path, O_RDONLY);
  struct stat thread_before;
  stat_of("/proc/thread-self/cgroup", &thread_before, "thread-self");
  struct timespec times[2] = {{1, 0}, {1234, 5}};
  RETURNS(utimensat(AT_FDCWD, path, times, 0), 0, "utimensat(times)");
  struct stat after;
  stat_of(numbered, &after, "mtime by number");
  CHECK(after.st_mtim.tv_sec == 1234 && after.st_mtim.tv_nsec == 5,
        "an explicit mtime reads back as %ld.%09ld", (long)after.st_mtim.tv_sec,
        after.st_mtim.tv_nsec);
  if (held >= 0) {
    CHECK(fstat(held, &after) == 0 && after.st_mtim.tv_sec == 1234 && after.st_mtim.tv_nsec == 5,
          "fstat shows another mtime");
  }
  struct timespec omitted[2] = {{0, UTIME_NOW}, {0, UTIME_OMIT}};
  RETURNS(utimensat(AT_FDCWD, path, omitted, 0), 0, "utimensat(now, omit)");
  stat_of(path, &after, "mtime after UTIME_OMIT");
  CHECK(after.st_mtim.tv_sec == 1234 && after.st_mtim.tv_nsec == 5, "UTIME_OMIT changed mtime");
  RETURNS(utimensat(AT_FDCWD, path, NULL, 0), 0, "utimensat(now)");
  struct stat now;
  stat_of(path, &now, "mtime after now");
  CHECK(now.st_mtim.tv_sec != 1234, "utimensat(now) left the mtime");
  if (held >= 0) {
    struct stat by_fd;
    CHECK(fstat(held, &by_fd) == 0 && by_fd.st_mtim.tv_sec == now.st_mtim.tv_sec &&
              by_fd.st_mtim.tv_nsec == now.st_mtim.tv_nsec,
          "fstat after utimensat(now) shows another mtime");
    RETURNS(futimens(held, times), 0, "futimens(times)");
    stat_of(numbered, &after, "mtime after futimens");
    CHECK(after.st_mtim.tv_sec == 1234, "futimens did not set the mtime");
    close(held);
  }
  struct stat thread_after;
  stat_of("/proc/thread-self/cgroup", &thread_after, "thread-self");
  CHECK(thread_after.st_mtim.tv_sec == thread_before.st_mtim.tv_sec &&
            thread_after.st_mtim.tv_nsec == thread_before.st_mtim.tv_nsec,
        "the process file's times changed the thread file's");
  REFUSED(chmod(path, 0444), EPERM, "chmod");
  ANSWERS(setxattr(path, "user.x", "1", 1, 0), dac_override ? EOPNOTSUPP : EACCES,
          "setxattr(user.x)");
  ANSWERS(removexattr(path, "user.x"), dac_override ? EOPNOTSUPP : EACCES, "removexattr(user.x)");
  ANSWERS(setxattr(path, "security.x", "1", 1, 0), sys_admin ? EOPNOTSUPP : EPERM,
          "setxattr(security.x)");
  /* A trusted. attribute needs CAP_SYS_ADMIN in the initial user namespace
   * (EPERM without it; with it procfs has no handler, EOPNOTSUPP). The
   * caller's uid map tells which namespace it is in: a guest of Hermit runs
   * in a user namespace of its own (EPERM, whatever capget reports there);
   * natively, root in the initial namespace gets EOPNOTSUPP. */
  ANSWERS(setxattr(path, "trusted.x", "1", 1, 0),
          sys_admin && in_initial_user_namespace() ? EOPNOTSUPP : EPERM, "setxattr(trusted.x)");
  char value[16];
  REFUSED(getxattr(path, "user.x", value, sizeof(value)), EOPNOTSUPP, "getxattr(user.x)");
  RETURNS(listxattr(path, value, sizeof(value)), 0, "listxattr");

  fd = open(path, O_RDONLY | O_NOATIME);
  CHECK(fd >= 0, "open O_NOATIME: %s", strerror(errno));
  if (fd >= 0) {
    close(fd);
  }
  fd = open(path, O_RDONLY);
  CHECK(fd >= 0, "open: %s", strerror(errno));
  if (fd >= 0) {
    REFUSED(fchmod(fd, 0444), EPERM, "fchmod");
    RETURNS(fcntl(fd, F_SETFL, O_NOATIME), 0, "F_SETFL O_NOATIME");
    RETURNS(fcntl(fd, F_GETFL) & O_NOATIME, O_NOATIME, "F_GETFL after O_NOATIME");
    REFUSED(fcntl(fd, F_SETFL, O_DIRECT), EINVAL, "F_SETFL O_DIRECT");
    REFUSED(fcntl(fd, F_SETLEASE, F_RDLCK), EINVAL, "F_SETLEASE");
    RETURNS(fcntl(fd, F_GETLEASE), F_UNLCK, "F_GETLEASE");
    unsigned long long hint = 2;
    RETURNS(fcntl(fd, F_SET_RW_HINT_COMMAND, &hint), 0, "F_SET_RW_HINT");
    /* The hint is the inode's: another descriptor, a reopening through the
     * descriptor link, read it; the thread's file has its own. */
    unsigned long long read_back = 0;
    int other = open(path, O_RDONLY);
    CHECK(other >= 0 && fcntl(other, F_GET_RW_HINT_COMMAND, &read_back) == 0 && read_back == 2,
          "F_GET_RW_HINT through another descriptor read %llu", read_back);
    if (other >= 0) {
      close(other);
    }
    char link[64];
    snprintf(link, sizeof(link), "/proc/self/fd/%d", fd);
    other = open(link, O_RDONLY);
    read_back = 0;
    CHECK(other >= 0 && fcntl(other, F_GET_RW_HINT_COMMAND, &read_back) == 0 && read_back == 2,
          "F_GET_RW_HINT through a reopening read %llu", read_back);
    if (other >= 0) {
      close(other);
    }
    other = open("/proc/thread-self/cgroup", O_RDONLY);
    read_back = 9;
    CHECK(other >= 0 && fcntl(other, F_GET_RW_HINT_COMMAND, &read_back) == 0 && read_back == 0,
          "the thread file's hint is %llu", read_back);
    if (other >= 0) {
      close(other);
    }
    hint = 9;
    REFUSED(fcntl(fd, F_SET_RW_HINT_COMMAND, &hint), EINVAL, "F_SET_RW_HINT(9)");
    REFUSED(fcntl(fd, F_ADD_SEALS, F_SEAL_SEAL), EPERM, "F_ADD_SEALS");
    REFUSED(fcntl(fd, F_GET_SEALS), EINVAL, "F_GET_SEALS");
    REFUSED(fsync(fd), EINVAL, "fsync");
    REFUSED(fsetxattr(fd, "user.x", "1", 1, 0), dac_override ? EOPNOTSUPP : EACCES,
            "fsetxattr(user.x)");
    RETURNS(futimens(fd, NULL), 0, "futimens");
    void* mapping = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, fd, 0);
    CHECK(mapping == MAP_FAILED && errno == ENODEV, "a private mapping: %s",
          mapping == MAP_FAILED ? strerror(errno) : "mapped");
    REFUSED(pwrite(fd, "x", 1, 0), ESPIPE, "pwrite of O_RDONLY");
    REFUSED(write(fd, "x", 1), EBADF, "write of O_RDONLY");
    close(fd);
  }
  say("owner: write-mode opens, access(W_OK), truncate and user. attributes follow "
      "CAP_DAC_OVERRIDE; writes EINVAL, ftruncate 0, fallocate EOPNOTSUPP, sendfile and "
      "splice EINVAL, mappings ENODEV, and the file unchanged; mode 3 neither reads nor "
      "writes; times set stay on the file for every alias; chmod EPERM; O_NOATIME allowed; "
      "the write hint is the file's; leases EINVAL");
}

/* The file /proc/self/cgroup names is one file whichever way it is
 * described: its listing in /proc/self, its path and a descriptor on it have
 * one inode. */
static void check_listing(const char* when) {
  DIR* directory = opendir("/proc/self");
  CHECK(directory != NULL, "%s: opendir /proc/self: %s", when, strerror(errno));
  if (directory == NULL) {
    return;
  }
  unsigned long long listed = 0;
  struct dirent* entry;
  while ((entry = readdir(directory)) != NULL) {
    if (strcmp(entry->d_name, "cgroup") == 0) {
      listed = (unsigned long long)entry->d_ino;
    }
  }
  closedir(directory);
  struct stat by_path;
  struct stat by_descriptor;
  stat_of("/proc/self/cgroup", &by_path, when);
  int fd = open("/proc/self/cgroup", O_RDONLY);
  CHECK(fd >= 0 && fstat(fd, &by_descriptor) == 0, "%s: fstat", when);
  if (fd >= 0) {
    close(fd);
  }
  CHECK(listed != 0 && listed == (unsigned long long)by_path.st_ino &&
            by_path.st_ino == by_descriptor.st_ino && by_path.st_dev == by_descriptor.st_dev,
        "%s: listed inode %llu, path %llu, descriptor %llu", when, listed,
        (unsigned long long)by_path.st_ino, (unsigned long long)by_descriptor.st_ino);
}

/* After chroot, where Detcore names no files by path and the kernel serves
 * the file, its metadata is the kernel's file's as well: one inode, still. */
static void check_after_chroot(void) {
  fflush(stdout);
  pid_t child = fork();
  if (child == 0) {
    failures = 0;
    if (chroot("/") != 0) {
      fprintf(stderr, "FAIL chroot: %s\n", strerror(errno));
      _exit(1);
    }
    check_listing("after chroot");
    _exit(failures == 0 ? 0 : 1);
  }
  int status;
  CHECK(child > 0 && waitpid(child, &status, 0) == child && WIFEXITED(status) &&
            WEXITSTATUS(status) == 0,
        "the chroot child failed");
  say("chroot: after chroot, the listing, path and descriptor of /proc/self/cgroup are one "
      "inode");
}

int main(int argc, char** argv) {
  program = argv[0];
  pid = getpid();
  int arg = 1;
  if (argc >= 4 && strcmp(argv[1], "--inherited") == 0) {
    hermit = 0;
    int fd = atoi(argv[2]);
    unsigned long long ino = strtoull(argv[3], NULL, 10);
    char buffer[4096];
    ssize_t rest = read_fd(fd, buffer, sizeof(buffer));
    struct stat metadata;
    CHECK(fstat(fd, &metadata) == 0 && metadata.st_ino == ino,
          "after exec the descriptor is another file");
    int whole = open("/proc/self/cgroup", O_RDONLY);
    char expected[4096];
    ssize_t length = read_fd(whole, expected, sizeof(expected));
    CHECK(length >= 2 && rest == length - 2 && memcmp(buffer, expected + 2, (size_t)rest) == 0,
          "after exec the rest is %zd bytes of %zd", rest, length);
    if (failures == 0) {
      printf("exec: the descriptor continues from its position, 2+%zd bytes\n", rest);
    }
    return failures == 0 ? 0 : 1;
  }
  for (; arg < argc; arg++) {
    if (strcmp(argv[arg], "--hermit") == 0) {
      hermit = 1;
    } else if (strcmp(argv[arg], "--no-process-directories") == 0) {
      process_directories = 0;
    } else if (strcmp(argv[arg], "--chroot") == 0) {
      after_chroot = 1;
    } else if (strcmp(argv[arg], "--relaxed") == 0) {
      relaxed = 1;
    } else if (strcmp(argv[arg], "--relaxed-transfer") == 0) {
      relaxed_transfer = 1;
    } else {
      fprintf(stderr,
              "usage: %s [--hermit] [--no-process-directories] [--chroot] [--relaxed] "
              "[--relaxed-transfer]\n",
              argv[0]);
      return 2;
    }
  }
  int fd = open("/proc/self/cgroup", O_RDONLY);
  CHECK(fd >= 0, "open /proc/self/cgroup: %s", strerror(errno));
  if (fd < 0) {
    return 1;
  }
  reference_length = read_fd(fd, reference, sizeof(reference));
  close(fd);
  CHECK(reference_length > 0, "/proc/self/cgroup is empty");
  if (hermit) {
    CHECK(reference_length == 5 && memcmp(reference, "0::/\n", 5) == 0,
          "/proc/self/cgroup is not the root of the cgroup v2 hierarchy: %.*s",
          (int)reference_length, reference);
    say("contents: 0::/");
  }
  if (relaxed) {
    return check_relaxed();
  }
  if (relaxed_transfer) {
    return check_relaxed_transfer();
  }
  check_spellings();
  check_reads();
  check_owner_calls();
  check_transfer((effective_capabilities() >> CAP_DAC_OVERRIDE) & 1);
  if (process_directories) {
    check_listing("listing");
    say("listing: the listing, path and descriptor of /proc/self/cgroup are one inode");
    if (after_chroot) {
      check_after_chroot();
    }
  }
  /* Across exec: the descriptor read in part continues where it was. */
  fd = open("/proc/self/cgroup", O_RDONLY);
  char head[2];
  struct stat metadata;
  CHECK(fd >= 0 && read(fd, head, 2) == 2 && fstat(fd, &metadata) == 0, "before exec");
  if (failures != 0) {
    return 1;
  }
  fflush(stdout);
  char fd_text[16];
  char ino_text[32];
  snprintf(fd_text, sizeof(fd_text), "%d", fd);
  snprintf(ino_text, sizeof(ino_text), "%llu", (unsigned long long)metadata.st_ino);
  if (hermit) {
    execl(program, program, "--inherited", fd_text, ino_text, (char*)NULL);
  } else {
    /* Natively, say nothing after exec either. */
    int devnull = open("/dev/null", O_WRONLY);
    dup2(devnull, STDOUT_FILENO);
    execl(program, program, "--inherited", fd_text, ino_text, (char*)NULL);
  }
  fprintf(stderr, "FAIL: exec %s: %s\n", program, strerror(errno));
  return 1;
}
