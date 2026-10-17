/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * rendered_process_auxv: a process's auxv file, which Detcore renders per
 * process from the auxiliary vector each new image finds on its initial
 * stack (https://github.com/rrnewton/hermit/issues/4213, package K slice K2),
 * answers as Linux's /proc/<pid>/auxv does:
 * - it reads the key and value pairs on the process's initial stack, past
 *   argc, the arguments and the environment, through the AT_NULL pair, read
 *   whole or in parts, and getauxval agrees for the entries glibc keeps as
 *   they are (AT_PHDR, AT_ENTRY, AT_PAGESZ, AT_RANDOM);
 * - it is 0400, of size 0, owned by the caller;
 * - it is a plain file, not a seq_file: pread at any position (0 past the
 *   end, EINVAL at -1), SEEK_END from size 0, SEEK_DATA ENXIO, a read after
 *   a seek; a mapping is ENODEV, RWF_NOWAIT EOPNOTSUPP, fsync EINVAL, and
 *   posix_fadvise succeeds; an O_PATH descriptor neither reads nor seeks;
 * - the calls that write follow the caller's capabilities (capget): with
 *   CAP_DAC_OVERRIDE a write-mode open succeeds and every write through it
 *   is EINVAL (a positioned one too: the file keeps FMODE_PWRITE), ftruncate
 *   and truncate succeed, fallocate is EOPNOTSUPP, and none changes what it
 *   reads; without, a write-mode open, truncate, access(W_OK) and setting a
 *   user. attribute are EACCES; access mode 3 neither reads nor writes;
 * - a fork's child reads its creator's vector, at /proc/self/auxv and at its
 *   creator's number, and its creator reads the child's;
 * - a descriptor opened before the process's exit keeps reading its vector
 *   after the process is reaped (Linux pins the memory map at the open);
 *   the reaped process has no file (ENOENT);
 * - a zombie's file belongs to global root, which only the initial user
 *   namespace's root owns: there an open is ESRCH (no memory map) and
 *   access(R_OK) succeeds; anywhere else both are EACCES;
 * - a process that made itself not dumpable (PR_SET_DUMPABLE 0) opens its
 *   own file when it is root in its namespace (the file's owner then);
 *   another process opens it only with CAP_SYS_PTRACE (EACCES without), and
 *   without CAP_SYS_PTRACE again once the process is dumpable;
 * - across exec, a descriptor opened before it reads the old image's vector,
 *   and the new image reads its own, another.
 *
 * usage: rendered_process_auxv [--hermit]
 *        rendered_process_auxv --kernel-file
 *        rendered_process_auxv --after-exec <fd> <hex> <padding> [--hermit]
 *   --hermit: under Hermit, which renders the file: its identity is
 *   Detcore's procfs device and a per-process inode (its top bit set), and
 *   it prints what it checked, which must be the same on every backend.
 *   Without it (natively) the same relations are checked against the
 *   kernel's file, and nothing is printed.
 *   --kernel-file: under a backend that reports no initial stack pointer,
 *   where Detcore renders no auxv: check that /proc/self/auxv is the
 *   kernel's procfs file (not a rendered one, whose inode has its top bit
 *   set), 0400, and a
 *   well-formed vector (AT_PAGESZ present, AT_NULL last), and print that.
 *   --after-exec is the program after it execs itself: <fd> is the
 *   descriptor opened before the exec, <hex> the old image's vector.
 * The program must be started by its absolute path, which it execs again.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/capability.h>
#include <sched.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <sys/xattr.h>
#include <unistd.h>

#define RENDERED_DEVICE 0xf000ffULL
/* Linux's saved_auxv: AT_VECTOR_SIZE words on x86_64. */
#define MAX_AUXV_WORDS 52

extern char** environ;

static int failures;
static int hermit;
static const char* program;

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
#define RETURNS(call, expected, what)                                                  \
  do {                                                                                 \
    errno = 0;                                                                         \
    long returns_result = (long)(call);                                                \
    int returns_errno = errno;                                                         \
    CHECK(returns_result == (long)(expected), "%s: %s gave %ld (%s), not %ld", (what), \
          #call, returns_result, returns_result < 0 ? strerror(returns_errno) : "",    \
          (long)(expected));                                                           \
  } while (0)

/* The call gives `expected`, 0 for success or an errno. */
#define ANSWERS(call, expected, what)  \
  do {                                 \
    if ((expected) == 0) {             \
      RETURNS(call, 0, what);          \
    } else {                           \
      REFUSED(call, (expected), what); \
    }                                  \
  } while (0)

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
  fflush(stdout);
}

/* The auxiliary vector on this image's initial stack, as bytes: the pairs
 * past the environment's NULL, through AT_NULL. */
static unsigned char stack_auxv[MAX_AUXV_WORDS * 8];
static size_t stack_auxv_length;

/* Reads the vector off the stack; `environ` still points at the initial
 * stack's environment, as nothing has changed it yet. */
static void capture_stack_auxv(void) {
  char** entry = environ;
  while (*entry != NULL) {
    entry++;
  }
  const unsigned long* pairs = (const unsigned long*)(entry + 1);
  size_t words = 0;
  while (words + 2 <= MAX_AUXV_WORDS) {
    words += 2;
    if (pairs[words - 2] == AT_NULL) {
      break;
    }
  }
  CHECK(pairs[words - 2] == AT_NULL, "no AT_NULL within %d words of the stack", MAX_AUXV_WORDS);
  stack_auxv_length = words * 8;
  memcpy(stack_auxv, pairs, stack_auxv_length);
}

static ssize_t read_fd(int fd, unsigned char* buffer, size_t capacity, size_t part) {
  size_t total = 0;
  while (total < capacity) {
    size_t want = capacity - total < part ? capacity - total : part;
    ssize_t n = read(fd, buffer + total, want);
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

/* Reads `path` whole, in parts of `part` bytes. */
static ssize_t read_path(const char* path, unsigned char* buffer, size_t capacity, size_t part) {
  int fd = open(path, O_RDONLY);
  if (fd < 0) {
    return -1;
  }
  ssize_t length = read_fd(fd, buffer, capacity, part);
  close(fd);
  return length;
}

static int is_vector(const unsigned char* bytes, ssize_t length, const unsigned char* expected,
                     size_t expected_length) {
  return length >= 0 && (size_t)length == expected_length &&
         memcmp(bytes, expected, expected_length) == 0;
}

/* The value of `key` in the vector `bytes`. */
static unsigned long vector_value(const unsigned char* bytes, size_t length, unsigned long key) {
  for (size_t at = 0; at + 16 <= length; at += 16) {
    unsigned long pair[2];
    memcpy(pair, bytes + at, sizeof(pair));
    if (pair[0] == key) {
      return pair[1];
    }
  }
  return 0;
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

static int has(unsigned long long capabilities, int capability) {
  return (int)((capabilities >> capability) & 1);
}

/* The bytes, the getauxval entries glibc keeps, and the reads in parts. */
static void check_contents(void) {
  unsigned char bytes[1024];
  ssize_t whole = read_path("/proc/self/auxv", bytes, sizeof(bytes), sizeof(bytes));
  CHECK(is_vector(bytes, whole, stack_auxv, stack_auxv_length),
        "/proc/self/auxv read %zd bytes that are not the stack's %zu", whole, stack_auxv_length);
  for (size_t part = 1; part <= 17; part += 8) {
    ssize_t length = read_path("/proc/self/auxv", bytes, sizeof(bytes), part);
    CHECK(is_vector(bytes, length, stack_auxv, stack_auxv_length),
          "read in parts of %zu: %zd bytes that are not the stack's", part, length);
  }
  char path[64];
  snprintf(path, sizeof(path), "/proc/%d/auxv", getpid());
  ssize_t numbered = read_path(path, bytes, sizeof(bytes), sizeof(bytes));
  CHECK(is_vector(bytes, numbered, stack_auxv, stack_auxv_length), "%s differs", path);
  const unsigned long kept[] = {AT_PHDR, AT_ENTRY, AT_PAGESZ, AT_RANDOM};
  for (size_t i = 0; i < sizeof(kept) / sizeof(kept[0]); i++) {
    unsigned long value = vector_value(stack_auxv, stack_auxv_length, kept[i]);
    CHECK(value != 0 && getauxval(kept[i]) == value, "getauxval(%lu) is %#lx, the file's %#lx",
          kept[i], getauxval(kept[i]), value);
  }
  say("contents: /proc/self/auxv and /proc/<pid>/auxv are the initial stack's auxiliary "
      "vector through AT_NULL, read whole or in parts, and getauxval agrees for AT_PHDR, "
      "AT_ENTRY, AT_PAGESZ and AT_RANDOM");
}

static void check_metadata(void) {
  int fd = open("/proc/self/auxv", O_RDONLY);
  struct stat by_fd;
  struct stat by_path;
  CHECK(fd >= 0 && fstat(fd, &by_fd) == 0, "fstat: %s", strerror(errno));
  CHECK(stat("/proc/self/auxv", &by_path) == 0, "stat: %s", strerror(errno));
  CHECK(by_fd.st_dev == by_path.st_dev && by_fd.st_ino == by_path.st_ino,
        "fstat and stat disagree");
  CHECK(by_fd.st_mode == (S_IFREG | 0400), "mode %o", by_fd.st_mode);
  CHECK(by_fd.st_size == 0 && by_fd.st_nlink == 1, "size %lld, %lu links",
        (long long)by_fd.st_size, (unsigned long)by_fd.st_nlink);
  CHECK(by_fd.st_uid == geteuid() && by_fd.st_gid == getegid(), "owner %u:%u, caller %u:%u",
        by_fd.st_uid, by_fd.st_gid, geteuid(), getegid());
  if (hermit) {
    CHECK(by_fd.st_dev == RENDERED_DEVICE && (by_fd.st_ino >> 63) == 1,
          "identity %#lx:%lu is not Detcore's rendered procfs file", (unsigned long)by_fd.st_dev,
          (unsigned long)by_fd.st_ino);
  }
  close(fd);
  int path_only = open("/proc/self/auxv", O_PATH);
  unsigned char byte;
  CHECK(path_only >= 0, "O_PATH: %s", strerror(errno));
  REFUSED(read(path_only, &byte, 1), EBADF, "O_PATH");
  REFUSED(lseek(path_only, 0, SEEK_SET), EBADF, "O_PATH");
  CHECK(fstat(path_only, &by_path) == 0 && by_path.st_ino == by_fd.st_ino, "O_PATH fstat");
  close(path_only);
  say("metadata: mode 0400, size 0, one link, owned by the caller; an O_PATH descriptor "
      "neither reads nor seeks");
}

static void check_reads(void) {
  int fd = open("/proc/self/auxv", O_RDONLY);
  CHECK(fd >= 0, "open: %s", strerror(errno));
  unsigned char bytes[1024];
  ssize_t length = (ssize_t)stack_auxv_length;
  RETURNS(pread(fd, bytes, sizeof(bytes), 8), length - 8, "pread at 8");
  CHECK(memcmp(bytes, stack_auxv + 8, (size_t)length - 8) == 0, "pread at 8 reads other bytes");
  RETURNS(pread(fd, bytes, sizeof(bytes), length), 0, "pread at the end");
  RETURNS(pread(fd, bytes, sizeof(bytes), length + 100), 0, "pread past the end");
  REFUSED(pread(fd, bytes, 8, -1), EINVAL, "pread at -1");
  RETURNS(lseek(fd, 0, SEEK_END), 0, "SEEK_END");
  RETURNS(lseek(fd, 5, SEEK_END), 5, "SEEK_END + 5");
  REFUSED(lseek(fd, 0, SEEK_DATA), ENXIO, "SEEK_DATA");
  REFUSED(lseek(fd, -1, SEEK_SET), EINVAL, "SEEK_SET -1");
  RETURNS(lseek(fd, 16, SEEK_SET), 16, "SEEK_SET 16");
  RETURNS(read(fd, bytes, 16), 16, "read at 16");
  CHECK(memcmp(bytes, stack_auxv + 16, 16) == 0, "read at 16 reads other bytes");
  RETURNS(lseek(fd, length + 64, SEEK_SET), length + 64, "SEEK_SET past the end");
  RETURNS(read(fd, bytes, 16), 0, "read past the end");
  struct iovec iov = {bytes, sizeof(bytes)};
  REFUSED(preadv2(fd, &iov, 1, 0, RWF_NOWAIT), EOPNOTSUPP, "RWF_NOWAIT");
  RETURNS(preadv2(fd, &iov, 1, 0, RWF_HIPRI), length, "RWF_HIPRI");
  REFUSED(mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, fd, 0) == MAP_FAILED ? -1 : 0, ENODEV,
          "mmap");
  REFUSED(fsync(fd), EINVAL, "fsync");
  REFUSED(fdatasync(fd), EINVAL, "fdatasync");
  RETURNS(posix_fadvise(fd, 0, 0, POSIX_FADV_DONTNEED), 0, "posix_fadvise");
  char value[8];
  REFUSED(fgetxattr(fd, "user.x", value, sizeof(value)), EOPNOTSUPP, "fgetxattr user.x");
  /* A read-only descriptor writes nothing; a negative position is refused
   * first. */
  REFUSED(write(fd, "x", 1), EBADF, "write read-only");
  REFUSED(pwrite(fd, "x", 1, 0), EBADF, "pwrite read-only");
  REFUSED(pwrite(fd, "x", 1, -1), EINVAL, "pwrite read-only at -1");
  struct iovec one = {"x", 1};
  REFUSED(pwritev2(fd, &one, 1, -1, 0), EBADF, "pwritev2 read-only at -1");
  close(fd);
  say("reads: pread at any position (0 past the end, EINVAL at -1), SEEK_END from size 0, "
      "SEEK_DATA ENXIO, a read after a seek; mmap ENODEV, RWF_NOWAIT EOPNOTSUPP, fsync "
      "EINVAL, posix_fadvise 0; a read-only descriptor writes nothing");
}

/* The calls that write, with or without CAP_DAC_OVERRIDE. */
static void check_writes(unsigned long long capabilities) {
  int dac = has(capabilities, CAP_DAC_OVERRIDE);
  int refusal = dac ? 0 : EACCES;
  ANSWERS(access("/proc/self/auxv", W_OK), refusal, "access(W_OK)");
  RETURNS(access("/proc/self/auxv", R_OK), 0, "access(R_OK)");
  REFUSED(access("/proc/self/auxv", X_OK), EACCES, "access(X_OK)");
  REFUSED(chmod("/proc/self/auxv", 0600), EPERM, "chmod");
  ANSWERS(truncate("/proc/self/auxv", 0), refusal, "truncate");
  ANSWERS(setxattr("/proc/self/auxv", "user.x", "1", 1, 0), dac ? EOPNOTSUPP : EACCES,
          "setxattr user.x");
  char value[8];
  REFUSED(getxattr("/proc/self/auxv", "user.x", value, sizeof(value)), EOPNOTSUPP,
          "getxattr user.x");
  if (!dac) {
    REFUSED(open("/proc/self/auxv", O_RDWR), EACCES, "O_RDWR");
    REFUSED(open("/proc/self/auxv", O_WRONLY), EACCES, "O_WRONLY");
    REFUSED(open("/proc/self/auxv", O_ACCMODE), EACCES, "access mode 3");
  } else {
    int rw = open("/proc/self/auxv", O_RDWR);
    CHECK(rw >= 0, "O_RDWR: %s", strerror(errno));
    REFUSED(write(rw, "x", 1), EINVAL, "write");
    REFUSED(pwrite(rw, "x", 1, 0), EINVAL, "pwrite");
    REFUSED(pwrite(rw, "x", 1, -1), EINVAL, "pwrite at -1");
    struct iovec one = {"x", 1};
    REFUSED(pwritev2(rw, &one, 1, -1, 0), EINVAL, "pwritev2 at -1");
    RETURNS(ftruncate(rw, 0), 0, "ftruncate");
    REFUSED(fallocate(rw, 0, 0, 1), EOPNOTSUPP, "fallocate");
    unsigned char bytes[1024];
    ssize_t length = read_fd(rw, bytes, sizeof(bytes), sizeof(bytes));
    CHECK(is_vector(bytes, length, stack_auxv, stack_auxv_length),
          "after the writes, the read-write descriptor read %zd other bytes", length);
    close(rw);
    int neither = open("/proc/self/auxv", O_ACCMODE);
    CHECK(neither >= 0, "access mode 3: %s", strerror(errno));
    REFUSED(read(neither, bytes, 1), EBADF, "access mode 3 read");
    REFUSED(write(neither, "x", 1), EBADF, "access mode 3 write");
    REFUSED(pwrite(neither, "x", 1, 0), EBADF, "access mode 3 pwrite");
    REFUSED(ftruncate(neither, 0), EINVAL, "access mode 3 ftruncate");
    close(neither);
  }
  unsigned char bytes[1024];
  ssize_t length = read_path("/proc/self/auxv", bytes, sizeof(bytes), sizeof(bytes));
  CHECK(is_vector(bytes, length, stack_auxv, stack_auxv_length), "the writes changed the file");
  say("writes: with CAP_DAC_OVERRIDE %d: write-mode opens, truncate and access(W_OK) %s; "
      "chmod EPERM, access(X_OK) EACCES; every write leaves the bytes",
      dac, dac ? "succeed, writes through them EINVAL, fallocate EOPNOTSUPP" : "are EACCES");
}

/* A pipe each way between a parent and a child, for one byte at a time. */
struct channel {
  int to_child[2];
  int to_parent[2];
};

static void open_channel(struct channel* channel) {
  CHECK(pipe(channel->to_child) == 0 && pipe(channel->to_parent) == 0, "pipe: %s",
        strerror(errno));
}

static void send_byte(int fd) {
  char byte = 'x';
  CHECK(write(fd, &byte, 1) == 1, "send: %s", strerror(errno));
}

static void receive_byte(int fd) {
  char byte;
  CHECK(read(fd, &byte, 1) == 1, "receive: %s", strerror(errno));
}

static int child_status(pid_t child) {
  int status = 0;
  CHECK(waitpid(child, &status, 0) == child, "waitpid: %s", strerror(errno));
  return WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status);
}

static void check_fork(void) {
  pid_t parent = getpid();
  struct channel channel;
  open_channel(&channel);
  pid_t child = fork();
  if (child == 0) {
    unsigned char bytes[1024];
    ssize_t own = read_path("/proc/self/auxv", bytes, sizeof(bytes), sizeof(bytes));
    int failed = !is_vector(bytes, own, stack_auxv, stack_auxv_length);
    char path[64];
    snprintf(path, sizeof(path), "/proc/%d/auxv", parent);
    ssize_t creators = read_path(path, bytes, sizeof(bytes), sizeof(bytes));
    failed |= !is_vector(bytes, creators, stack_auxv, stack_auxv_length);
    send_byte(channel.to_parent[1]);
    receive_byte(channel.to_child[0]);
    _exit(failed);
  }
  receive_byte(channel.to_parent[0]);
  char path[64];
  snprintf(path, sizeof(path), "/proc/%d/auxv", child);
  unsigned char bytes[1024];
  ssize_t length = read_path(path, bytes, sizeof(bytes), sizeof(bytes));
  CHECK(is_vector(bytes, length, stack_auxv, stack_auxv_length),
        "the child's file read %zd other bytes", length);
  send_byte(channel.to_child[1]);
  CHECK(child_status(child) == 0, "the child read other bytes");
  say("fork: a child reads its creator's vector at /proc/self/auxv and at its creator's "
      "number, and its creator reads the child's");
}

/* A descriptor opened while the process lives reads its vector after the
 * reap; a zombie's file is global root's. */
static void check_exit(unsigned long long capabilities) {
  struct channel channel;
  open_channel(&channel);
  pid_t child = fork();
  if (child == 0) {
    receive_byte(channel.to_child[0]);
    _exit(0);
  }
  char path[64];
  snprintf(path, sizeof(path), "/proc/%d/auxv", child);
  int kept = open(path, O_RDONLY);
  CHECK(kept >= 0, "open the live child's file: %s", strerror(errno));
  send_byte(channel.to_child[1]);
  siginfo_t info;
  memset(&info, 0, sizeof(info));
  CHECK(waitid(P_PID, (id_t)child, &info, WEXITED | WNOWAIT) == 0, "waitid: %s",
        strerror(errno));
  /* A zombie: its memory map is gone, and its file is global root's. */
  int initial_root = in_initial_user_namespace() && has(capabilities, CAP_DAC_OVERRIDE);
  ANSWERS(open(path, O_RDONLY), initial_root ? ESRCH : EACCES, "open the zombie's file");
  ANSWERS(access(path, R_OK), initial_root ? 0 : EACCES, "access(R_OK) the zombie's file");
  char value[8];
  REFUSED(getxattr(path, "user.x", value, sizeof(value)), initial_root ? EOPNOTSUPP : EACCES,
          "getxattr user.x of the zombie's file");
  REFUSED(getxattr(path, "security.x", value, sizeof(value)), EOPNOTSUPP,
          "getxattr security.x of the zombie's file");
  CHECK(child_status(child) == 0, "the exiting child failed");
  unsigned char bytes[1024];
  ssize_t length = read_fd(kept, bytes, sizeof(bytes), sizeof(bytes));
  CHECK(is_vector(bytes, length, stack_auxv, stack_auxv_length),
        "the kept descriptor read %zd other bytes after the reap", length);
  RETURNS(pread(kept, bytes, 16, 0), 16, "pread of the kept descriptor after the reap");
  close(kept);
  struct stat ignored;
  REFUSED(open(path, O_RDONLY), ENOENT, "open the reaped child's file");
  REFUSED(stat(path, &ignored), ENOENT, "stat the reaped child's file");
  say("exit: a zombie's file cannot be opened, read-probed or have a user. attribute read "
      "(%s); a descriptor opened before the exit reads the vector after the reap, and the "
      "reaped process has no file",
      initial_root ? "ESRCH, access 0" : "EACCES");
}

/* Removes `capability` from the caller's effective set, and from its
 * permitted set too where `permitted`. */
static int drop_capability(int capability, int permitted) {
  struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
  struct __user_cap_data_struct data[2];
  memset(data, 0, sizeof(data));
  if (syscall(SYS_capget, &header, data) != 0) {
    return -1;
  }
  data[capability / 32].effective &= ~(1U << (capability % 32));
  if (permitted) {
    data[capability / 32].permitted &= ~(1U << (capability % 32));
  }
  return (int)syscall(SYS_capset, &header, data);
}

/* The caller's permitted capabilities, as capget reports them. */
static unsigned long long permitted_capabilities(void) {
  struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
  struct __user_cap_data_struct data[2];
  memset(data, 0, sizeof(data));
  CHECK(syscall(SYS_capget, &header, data) == 0, "capget: %s", strerror(errno));
  return (unsigned long long)data[0].permitted | ((unsigned long long)data[1].permitted << 32);
}

/* 0 if `path` opens and reads `expected`, else the errno of the open. */
static int open_answer(const char* path, const unsigned char* expected, size_t expected_length) {
  unsigned char bytes[1024];
  ssize_t length = read_path(path, bytes, sizeof(bytes), sizeof(bytes));
  if (length < 0) {
    return errno;
  }
  CHECK(is_vector(bytes, length, expected, expected_length), "%s read %zd other bytes", path,
        length);
  return 0;
}

static const char* answer_name(int answer) {
  return answer == 0 ? "opened" : answer == EACCES ? "EACCES" : strerror(answer);
}

/* What a target process reports to its creator. */
struct target_report {
  unsigned long long permitted;
  int dumpable;
  int own;
};

/* Linux's answer to another process with the effective capabilities
 * `effective` opening the auxv of a process with the permitted set
 * `permitted` whose memory map is `dumpable`, both with this process's
 * credentials (ptrace_may_access in PTRACE_MODE_READ_FSCREDS, measured on
 * Linux 7.1): a map that is not dumpable belongs to the namespace's root,
 * which a root caller is; CAP_SYS_PTRACE allows any map, and without it the
 * map must be dumpable and the target's permitted set within the caller's
 * effective set. -1 where this kernel answers otherwise in the initial user
 * namespace, which CAP_SYS_ADMIN or CAP_PERFMON there (measured: each alone
 * opens) decides: not asserted. */
static int expected_answer(unsigned long long effective, unsigned long long permitted,
                           int dumpable) {
  int owner = dumpable || geteuid() == 0;
  if (!owner) {
    return EACCES;
  }
  if (has(effective, CAP_SYS_PTRACE)) {
    return 0;
  }
  if (in_initial_user_namespace() &&
      (has(effective, CAP_SYS_ADMIN) || has(effective, CAP_PERFMON))) {
    return -1;
  }
  return dumpable && (permitted & ~effective) == 0 ? 0 : EACCES;
}

/* One case of another process's open of a target's file: the target drops
 * CAP_SYS_PTRACE from its permitted set (`drop` 1) or from its bounding and
 * permitted sets (`drop` 2), makes its memory map not dumpable, or not
 * dumpable and then dumpable again (`dumpable`), and execs itself after its
 * changes (`exec`), which makes the map dumpable unless the exec raised its
 * permitted set; its creator, which holds every capability the target
 * started with, and a helper without CAP_SYS_PTRACE in its effective set open
 * its file, and the target its own. Returns the line that names the target's
 * dumpability and the three answers. */
static void access_case(const char* name, int drop, int dumpable, int exec, char* line,
                        size_t line_size) {
  struct channel channel;
  open_channel(&channel);
  pid_t target = fork();
  if (target == 0) {
    /* The child reports only its own failures. */
    failures = 0;
    close(channel.to_child[1]);
    close(channel.to_parent[0]);
    /* Changing the bounding set needs CAP_SETPCAP; a caller without it (a
     * user) has no capability there to drop. */
    int failed = drop == 2 && has(effective_capabilities(), CAP_SETPCAP) &&
                 prctl(PR_CAPBSET_DROP, CAP_SYS_PTRACE, 0, 0, 0) != 0;
    failed |= drop && drop_capability(CAP_SYS_PTRACE, 1) != 0;
    failed |= prctl(PR_SET_DUMPABLE, 0, 0, 0, 0) != 0;
    if (dumpable) {
      failed |= prctl(PR_SET_DUMPABLE, 1, 0, 0, 0) != 0;
    }
    if (failed) {
      _exit(3);
    }
    if (exec) {
      char out[16];
      char in[16];
      snprintf(out, sizeof(out), "%d", channel.to_parent[1]);
      snprintf(in, sizeof(in), "%d", channel.to_child[0]);
      char* const target_argv[] = {(char*)program, "--target", out, in, NULL};
      execv(program, target_argv);
      _exit(4);
    }
    struct target_report report = {permitted_capabilities(), prctl(PR_GET_DUMPABLE) == 1,
                                   open_answer("/proc/self/auxv", stack_auxv,
                                               stack_auxv_length)};
    CHECK(write(channel.to_parent[1], &report, sizeof(report)) == sizeof(report), "report");
    receive_byte(channel.to_child[0]);
    _exit(failures != 0);
  }
  close(channel.to_child[0]);
  close(channel.to_parent[1]);
  struct target_report report;
  memset(&report, 0, sizeof(report));
  CHECK(read(channel.to_parent[0], &report, sizeof(report)) == sizeof(report),
        "%s: the target reported nothing", name);
  char path[64];
  snprintf(path, sizeof(path), "/proc/%d/auxv", target);
  /* After its exec the target's vector is another image's; compare only
   * where it is this one's. */
  const unsigned char* expected = exec ? NULL : stack_auxv;
  unsigned char exec_bytes[1024];
  ssize_t exec_length = 0;
  if (exec) {
    int fd = open(path, O_RDONLY);
    exec_length = fd < 0 ? -1 : read_fd(fd, exec_bytes, sizeof(exec_bytes), sizeof(exec_bytes));
    if (fd >= 0) {
      close(fd);
    }
    expected = exec_bytes;
  }
  int creator = open_answer(path, expected, exec ? (size_t)exec_length : stack_auxv_length);
  struct channel helper_channel;
  open_channel(&helper_channel);
  pid_t helper = fork();
  if (helper == 0) {
    failures = 0;
    int answers[3] = {drop_capability(CAP_SYS_PTRACE, 0) == 0 ? 0 : errno, 0, 0};
    unsigned long long effective = effective_capabilities();
    answers[1] = open_answer(path, expected, exec ? (size_t)exec_length : stack_auxv_length);
    /* Whether this kernel reports another process's capabilities, which
     * the open's check needs: Linux does; KVM's provider answers capget for
     * the caller only (ESRCH), and Detcore then refuses the open. */
    struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, target};
    struct __user_cap_data_struct data[2];
    answers[2] = syscall(SYS_capget, &header, data) == 0;
    CHECK(write(helper_channel.to_parent[1], answers, sizeof(answers)) == sizeof(answers) &&
              write(helper_channel.to_parent[1], &effective, sizeof(effective)) ==
                  sizeof(effective),
          "helper report");
    _exit(failures != 0);
  }
  int answers[3] = {-1, -1, 0};
  unsigned long long helper_effective = 0;
  CHECK(read(helper_channel.to_parent[0], answers, sizeof(answers)) == sizeof(answers) &&
            read(helper_channel.to_parent[0], &helper_effective, sizeof(helper_effective)) ==
                sizeof(helper_effective),
        "%s: the helper reported nothing", name);
  CHECK(child_status(helper) == 0, "%s: the helper failed", name);
  send_byte(channel.to_child[1]);
  CHECK(child_status(target) == 0, "%s: the target failed", name);
  CHECK(answers[0] == 0 && !has(helper_effective, CAP_SYS_PTRACE),
        "%s: the helper could not drop CAP_SYS_PTRACE: %s", name, strerror(answers[0]));
  int target_dumpable = report.dumpable;
  CHECK(exec || target_dumpable == dumpable, "%s: the target's map is %sdumpable", name,
        target_dumpable ? "" : "not ");
  int expected_creator =
      expected_answer(effective_capabilities(), report.permitted, target_dumpable);
  /* A set that cannot be read is taken as one the helper's does not cover. */
  int expected_helper = expected_answer(
      helper_effective, answers[2] ? report.permitted : ~0ULL, target_dumpable);
  int expected_own = target_dumpable || geteuid() == 0 ? 0 : EACCES;
  CHECK(expected_creator < 0 || creator == expected_creator, "%s: the creator's open %s, not %s",
        name, answer_name(creator), answer_name(expected_creator));
  CHECK(expected_helper < 0 || answers[1] == expected_helper,
        "%s: the helper's open %s, not %s", name, answer_name(answers[1]),
        answer_name(expected_helper));
  CHECK(report.own == expected_own, "%s: its own open %s, not %s", name, answer_name(report.own),
        answer_name(expected_own));
  snprintf(line, line_size,
           "%s: dumpable %d, creator %s, helper without CAP_SYS_PTRACE %s, own %s", name,
           target_dumpable, answer_name(creator), answer_name(answers[1]),
           answer_name(report.own));
}

/* The target after its exec: reports its permitted set and its own open,
 * then waits. */
static int target_after_exec(int out, int in) {
  struct target_report report = {permitted_capabilities(), prctl(PR_GET_DUMPABLE) == 1,
                                 open_answer("/proc/self/auxv", stack_auxv, stack_auxv_length)};
  CHECK(write(out, &report, sizeof(report)) == sizeof(report), "report after exec");
  receive_byte(in);
  return failures != 0;
}

/* Another process's open of a process's file, as ptrace_may_access decides
 * it, across dumpability, capabilities and exec. */
static void check_access(void) {
  struct {
    const char* name;
    int drop;
    int dumpable;
    int exec;
  } cases[] = {
      /* Not a target that drops CAP_SYS_PTRACE from its permitted set only
       * and then execs: that exec raises its permitted set again, so Linux
       * clears ADDR_NO_RANDOMIZE from its personality (PER_CLEAR_ON_SETID),
       * and under Hermit its new image runs with a randomized layout, which
       * no comparison of two runs survives. */
      {"not dumpable", 0, 0, 0},
      {"dumpable again", 0, 1, 0},
      {"without CAP_SYS_PTRACE, dumpable", 1, 1, 0},
      {"without CAP_SYS_PTRACE, not dumpable", 1, 0, 0},
      {"without CAP_SYS_PTRACE in its bounding set, then exec", 2, 0, 1},
  };
  for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); i++) {
    char line[256];
    access_case(cases[i].name, cases[i].drop, cases[i].dumpable, cases[i].exec, line,
                sizeof(line));
    say("access: %s", line);
  }
}

/* Under a backend where Detcore renders no auxv: the kernel's file. */
static int check_kernel_file(void) {
  int fd = open("/proc/self/auxv", O_RDONLY);
  CHECK(fd >= 0, "open: %s", strerror(errno));
  if (fd < 0) {
    return 1;
  }
  struct stat metadata;
  CHECK(fstat(fd, &metadata) == 0, "fstat: %s", strerror(errno));
  /* Under Hermit a kernel procfs file has Detcore's procfs device too, but a
   * rendered per-process file's inode has its top bit set. */
  CHECK((metadata.st_ino >> 63) == 0, "identity %#lx:%lu is Detcore's rendered file",
        (unsigned long)metadata.st_dev, (unsigned long)metadata.st_ino);
  CHECK(metadata.st_mode == (S_IFREG | 0400), "mode %o", metadata.st_mode);
  unsigned char bytes[1024];
  ssize_t length = read_fd(fd, bytes, sizeof(bytes), sizeof(bytes));
  close(fd);
  CHECK(length >= 32 && length % 16 == 0, "%zd bytes", length);
  int page_size = 0;
  for (ssize_t at = 0; at + 16 <= length; at += 16) {
    unsigned long pair[2];
    memcpy(pair, bytes + at, sizeof(pair));
    page_size |= pair[0] == AT_PAGESZ;
    CHECK((pair[0] == AT_NULL) == (at + 16 == length), "AT_NULL at %zd of %zd", at, length);
  }
  CHECK(page_size, "no AT_PAGESZ");
  if (failures == 0) {
    printf("kernel: /proc/self/auxv is the kernel's procfs file, 0400, a whole vector with "
           "AT_PAGESZ ending in AT_NULL\n");
  }
  return failures == 0 ? 0 : 1;
}

/* A helper process, forked now, that drops CAP_SYS_PTRACE from its effective
 * set and opens /proc/<target>/auxv: 0 or the open's errno. */
static int helper_answer(pid_t target) {
  int channel[2];
  CHECK(pipe(channel) == 0, "pipe: %s", strerror(errno));
  pid_t helper = fork();
  if (helper == 0) {
    char path[64];
    snprintf(path, sizeof(path), "/proc/%d/auxv", target);
    int answer = drop_capability(CAP_SYS_PTRACE, 0) != 0 ? -1 : 0;
    if (answer == 0) {
      int fd = open(path, O_RDONLY);
      answer = fd < 0 ? errno : 0;
      if (fd >= 0) {
        close(fd);
      }
    }
    CHECK(write(channel[1], &answer, sizeof(answer)) == sizeof(answer), "helper report");
    _exit(0);
  }
  int answer = -1;
  CHECK(read(channel[0], &answer, sizeof(answer)) == sizeof(answer), "helper reported nothing");
  child_status(helper);
  close(channel[0]);
  close(channel[1]);
  return answer;
}

/* The answer to opening /proc/<target>/auxv from this process. */
static int open_errno(pid_t target) {
  char path[64];
  snprintf(path, sizeof(path), "/proc/%d/auxv", target);
  int fd = open(path, O_RDONLY);
  if (fd < 0) {
    return errno;
  }
  close(fd);
  return 0;
}

static char vm_child_stack[1 << 16];
static int vm_channel[2][2];

/* A child that shares its creator's memory map (CLONE_VM) makes the map not
 * dumpable, reports, and waits. Raw syscalls only: it runs on the creator's
 * memory. */
static int vm_child(void* argument) {
  (void)argument;
  char byte = 'r';
  syscall(SYS_prctl, PR_SET_DUMPABLE, 0, 0, 0, 0);
  syscall(SYS_write, vm_channel[1][1], &byte, 1);
  syscall(SYS_read, vm_channel[0][0], &byte, 1);
  return 0;
}

/* Another user namespace and a shared memory map:
 * - a child made with clone(CLONE_NEWUSER) has its creator's credentials in
 *   a namespace its creator owns: the creator opens its file, also without
 *   CAP_SYS_PTRACE (the owner holds every capability there), and the child,
 *   which has no capability in its creator's namespace, is refused its
 *   creator's (EACCES), but opens its own; measured on Linux 7.1 as a user,
 *   as root in a user namespace and as root. A backend that refuses the
 *   clone (KVM: EOPNOTSUPP) says so.
 * - a CLONE_VM child that sets PR_SET_DUMPABLE(0) makes its creator's map,
 *   which it shares, not dumpable: a helper without CAP_SYS_PTRACE is
 *   refused the creator's file (as root in the initial namespace
 *   CAP_SYS_ADMIN or CAP_PERFMON opens it: not asserted there). A backend
 *   that refuses the clone (KVM: EOPNOTSUPP) says so. */
static void check_namespaces_and_shared_maps(void) {
  pid_t creator = getpid();
  struct channel channel;
  open_channel(&channel);
  long child = syscall(SYS_clone, CLONE_NEWUSER | SIGCHLD, 0, 0, 0, 0);
  if (child == 0) {
    failures = 0;
    receive_byte(channel.to_child[0]);
    int answers[2] = {open_errno(creator), open_errno(getpid())};
    CHECK(write(channel.to_parent[1], answers, sizeof(answers)) == sizeof(answers), "report");
    _exit(failures != 0);
  }
  if (child < 0) {
    say("userns: clone(CLONE_NEWUSER) is refused (%s)", strerror(errno));
  } else {
    int creator_answer = open_errno((pid_t)child);
    int helper = helper_answer((pid_t)child);
    send_byte(channel.to_child[1]);
    int answers[2] = {-1, -1};
    CHECK(read(channel.to_parent[0], answers, sizeof(answers)) == sizeof(answers),
          "the user-namespace child reported nothing");
    CHECK(child_status((pid_t)child) == 0, "the user-namespace child failed");
    CHECK(creator_answer == 0, "the creator's open of its user-namespace child's file: %s",
          answer_name(creator_answer));
    CHECK(helper == 0, "the helper's open of the user-namespace child's file: %s",
          answer_name(helper));
    CHECK(answers[0] == EACCES, "the user-namespace child's open of its creator's file: %s",
          answer_name(answers[0]));
    CHECK(answers[1] == 0, "the user-namespace child's open of its own file: %s",
          answer_name(answers[1]));
    say("userns: the creator opens its CLONE_NEWUSER child's file (%s), and so does a helper "
        "without CAP_SYS_PTRACE (%s); the child is refused its creator's (%s) and opens its "
        "own (%s)",
        answer_name(creator_answer), answer_name(helper), answer_name(answers[0]),
        answer_name(answers[1]));
  }
  CHECK(pipe(vm_channel[0]) == 0 && pipe(vm_channel[1]) == 0, "pipe: %s", strerror(errno));
  pid_t vm = clone(vm_child, vm_child_stack + sizeof(vm_child_stack), CLONE_VM | SIGCHLD, NULL);
  if (vm < 0) {
    say("clone_vm: clone(CLONE_VM) is refused (%s)", strerror(errno));
    return;
  }
  char byte;
  CHECK(read(vm_channel[1][0], &byte, 1) == 1, "the CLONE_VM child reported nothing");
  int dumpable = prctl(PR_GET_DUMPABLE);
  int helper = helper_answer(creator);
  CHECK(write(vm_channel[0][1], "x", 1) == 1, "release the CLONE_VM child");
  CHECK(child_status(vm) == 0, "the CLONE_VM child failed");
  CHECK(dumpable == 0, "after its CLONE_VM child's PR_SET_DUMPABLE(0) the creator's map is %d",
        dumpable);
  unsigned long long effective = effective_capabilities();
  int lenient = in_initial_user_namespace() &&
                (has(effective, CAP_SYS_ADMIN) || has(effective, CAP_PERFMON));
  CHECK(lenient || helper == EACCES,
        "the helper's open of the creator's file after the CLONE_VM child's change: %s",
        answer_name(helper));
  /* The map is the creator's again to restore. */
  CHECK(prctl(PR_SET_DUMPABLE, 1, 0, 0, 0) == 0, "PR_SET_DUMPABLE 1: %s", strerror(errno));
  say("clone_vm: a CLONE_VM child's PR_SET_DUMPABLE(0) makes its creator's shared map not "
      "dumpable (%d), and a helper without CAP_SYS_PTRACE is refused the creator's file (%s)",
      dumpable, answer_name(helper));
}

static void to_hex(const unsigned char* bytes, size_t length, char* out) {
  for (size_t i = 0; i < length; i++) {
    sprintf(out + 2 * i, "%02x", bytes[i]);
  }
  out[2 * length] = '\0';
}

static int after_exec(int fd, const char* hex) {
  size_t old_length = strlen(hex) / 2;
  unsigned char old[MAX_AUXV_WORDS * 8];
  CHECK(old_length <= sizeof(old), "the old vector is %zu bytes", old_length);
  for (size_t i = 0; i < old_length && i < sizeof(old); i++) {
    unsigned int byte = 0;
    sscanf(hex + 2 * i, "%2x", &byte);
    old[i] = (unsigned char)byte;
  }
  unsigned char bytes[1024];
  ssize_t length = read_fd(fd, bytes, sizeof(bytes), 24);
  CHECK(is_vector(bytes, length, old, old_length),
        "after exec the descriptor opened before it read %zd bytes, not the old image's %zu",
        length, old_length);
  RETURNS(lseek(fd, 0, SEEK_SET), 0, "rewind after exec");
  length = read_fd(fd, bytes, sizeof(bytes), sizeof(bytes));
  CHECK(is_vector(bytes, length, old, old_length), "after a rewind, %zd other bytes", length);
  ssize_t own = read_path("/proc/self/auxv", bytes, sizeof(bytes), sizeof(bytes));
  CHECK(is_vector(bytes, own, stack_auxv, stack_auxv_length),
        "after exec /proc/self/auxv read %zd bytes that are not the new stack's %zu", own,
        stack_auxv_length);
  CHECK(!is_vector(stack_auxv, (ssize_t)stack_auxv_length, old, old_length),
        "the new image's vector is the old one's");
  say("exec: a descriptor opened before exec reads the old image's vector, also after a "
      "rewind; the new image reads its own, another");
  return failures == 0 ? 0 : 1;
}

int main(int argc, char** argv) {
  capture_stack_auxv();
  program = argv[0];
  if (argc == 2 && strcmp(argv[1], "--kernel-file") == 0) {
    return check_kernel_file();
  }
  if (argc == 4 && strcmp(argv[1], "--target") == 0) {
    return target_after_exec(atoi(argv[2]), atoi(argv[3]));
  }
  if (argc >= 5 && strcmp(argv[1], "--after-exec") == 0) {
    hermit = argc >= 6 && strcmp(argv[5], "--hermit") == 0;
    return after_exec(atoi(argv[2]), argv[3]);
  }
  for (int arg = 1; arg < argc; arg++) {
    if (strcmp(argv[arg], "--hermit") == 0) {
      hermit = 1;
    } else {
      fprintf(stderr, "usage: %s [--hermit]\n", argv[0]);
      return 2;
    }
  }
  unsigned long long capabilities = effective_capabilities();
  say("capabilities: dac_override=%d sys_ptrace=%d", has(capabilities, CAP_DAC_OVERRIDE),
      has(capabilities, CAP_SYS_PTRACE));
  check_contents();
  check_metadata();
  check_reads();
  check_writes(capabilities);
  check_fork();
  check_exit(capabilities);
  check_access();
  check_namespaces_and_shared_maps();
  if (failures != 0) {
    return 1;
  }
  /* Across exec: a descriptor opened before it, not yet read, reads the old
   * image's vector. A longer argument moves the new image's stack, so its
   * vector (AT_RANDOM, AT_EXECFN) is another. */
  int fd = open("/proc/self/auxv", O_RDONLY);
  CHECK(fd >= 0, "open before exec: %s", strerror(errno));
  char fd_text[16];
  snprintf(fd_text, sizeof(fd_text), "%d", fd);
  char hex[MAX_AUXV_WORDS * 16 + 1];
  to_hex(stack_auxv, stack_auxv_length, hex);
  char padding[257];
  memset(padding, 'p', sizeof(padding) - 1);
  padding[sizeof(padding) - 1] = '\0';
  char* const next[] = {argv[0],  "--after-exec", fd_text, hex, padding,
                        hermit ? "--hermit" : NULL, NULL};
  execv(argv[0], next);
  fprintf(stderr, "FAIL exec %s: %s\n", argv[0], strerror(errno));
  return 1;
}
