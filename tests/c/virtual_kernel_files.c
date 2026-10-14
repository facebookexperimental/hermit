/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * The procfs files that describe the kernel, its fixed limits and its tables
 * (https://github.com/rrnewton/hermit/issues/4266).
 *
 * Checks what Linux guarantees, natively and under Hermit:
 * - kernel/osrelease is uname's release, kernel/ostype its sysname and
 *   kernel/version its version;
 * - /proc/version starts "<sysname> version <release> " and ends with
 *   uname's version;
 * - the tables describe one machine: /proc/interrupts has a column per
 *   online CPU and /proc/softirqs one per possible CPU, /proc/stat and
 *   /proc/schedstat a line per online CPU; /proc/meminfo's MemTotal is
 *   sysinfo's total memory in whole kB; /proc/buddyinfo lists the zones
 *   whose page sets /proc/zoneinfo shows;
 * - none of the files can be opened for writing by a process that is not
 *   global root, and each is a regular file on the procfs device (that of
 *   kernel/random/uuid);
 * - stat by path, statx and fstat agree, also when stat's result overwrites
 *   the path it was given; access(R_OK) succeeds; the descriptor's link in
 *   /proc/self/fd names the file; and its directory lists it under the same
 *   inode (not with --kvm-local-procfs: KVM's own /proc lists nothing until
 *   it is removed, https://github.com/rrnewton/hermit/issues/4266);
 * - an integer sysctl ends after a partial read, while a string sysctl and a
 *   seq_file file serve the rest; a seq_file seeks only from its start or
 *   position, and a sysctl file has size 0;
 * - preadv2 accepts and refuses the flags Linux 7.1 does for these files; a
 *   vectored read refuses a destination outside the address space, also at
 *   end of file or in a later vector (a file without read_iter, /proc/modules
 *   and /proc/swaps, takes RWF_HIPRI alone); and a read whose buffer faults partway
 *   copies up to the fault from a seq_file but fails with EFAULT from a
 *   sysctl, which then keeps its position;
 * - all of that also holds for the file opened through a symbolic link, which
 *   the kernel resolves; and after exec, such a descriptor reads on from where
 *   the old image stopped (neither with --kvm-local-procfs, whose own /proc
 *   cannot be reached through a link,
 *   https://github.com/rrnewton/hermit/issues/4282).
 *
 * Under Hermit every file reads the same each time, which the guest checks.
 * The host's tables change from one read to the next, so with --host (the
 * native control) a check that compares a read with an earlier one runs
 * only for the files the kernel keeps constant.
 *
 * Then prints, for every file, its permission bits, device and inode, and
 * its contents if they are one line, or else its size, line count and an
 * FNV-1a digest (or "volatile" for a file whose two reads disagree), so
 * that a caller can require the same lines from every backend. A file the
 * host does not have prints "absent".
 */

#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysinfo.h>
#include <sys/sysmacros.h>
#include <sys/uio.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <unistd.h>

static const char* const FILES[] = {
    "/proc/version",
    "/proc/cmdline",
    "/proc/sys/kernel/osrelease",
    "/proc/sys/kernel/ostype",
    "/proc/sys/kernel/version",
    "/proc/sys/kernel/random/boot_id",
    "/proc/sys/kernel/random/entropy_avail",
    "/proc/sys/kernel/overflowuid",
    "/proc/sys/kernel/overflowgid",
    "/proc/sys/kernel/pid_max",
    "/proc/sys/kernel/cap_last_cap",
    "/proc/sys/vm/overcommit_memory",
    "/proc/sys/vm/max_map_count",
    "/proc/sys/vm/mmap_min_addr",
    "/proc/modules",
    "/proc/filesystems",
    "/proc/interrupts",
    "/proc/softirqs",
    "/proc/swaps",
    "/proc/buddyinfo",
    "/proc/zoneinfo",
    "/proc/schedstat",
    "/proc/key-users",
    "/proc/vmstat",
    "/proc/cgroups",
    "/proc/tty/drivers",
    "/proc/meminfo",
    "/proc/sys/fs/file-nr",
    "/proc/sys/fs/file-max",
    "/proc/sys/fs/inode-nr",
    "/proc/sys/fs/inode-state",
    "/proc/sys/fs/dentry-state",
    "/proc/sys/fs/aio-nr",
    "/proc/sys/fs/aio-max-nr",
    "/proc/sys/fs/pipe-max-size",
};

#define NFILES (sizeof(FILES) / sizeof(FILES[0]))

static int failures;

#define CHECK(condition, ...)              \
  do {                                     \
    if (!(condition)) {                    \
      fprintf(stderr, "FAIL: " __VA_ARGS__); \
      fprintf(stderr, "\n");               \
      failures++;                          \
    }                                      \
  } while (0)

#ifndef RWF_ATOMIC
#define RWF_ATOMIC 0x40
#endif
#ifndef RWF_DONTCACHE
#define RWF_DONTCACHE 0x80
#endif
#ifndef RWF_NOSIGNAL
#define RWF_NOSIGNAL 0x100
#endif
#define RWF_UNKNOWN 0x200

/* A file as read: its bytes (NUL-terminated for convenience), and whether
 * later reads may be compared with them. */
struct contents {
  char* bytes;
  size_t length;
  int stable;
};

/* Whether this is the native control (--host). */
static int host;

/* The files whose contents a host kernel keeps constant. */
static int host_constant(const char* path) {
  static const char* const VOLATILE[] = {
      "/proc/interrupts", "/proc/softirqs",
      "/proc/swaps",      "/proc/buddyinfo",      "/proc/zoneinfo",
      "/proc/schedstat",  "/proc/key-users",      "/proc/vmstat",
      "/proc/cgroups",    "/proc/meminfo",        "/proc/modules",
      "/proc/sys/fs/file-nr", "/proc/sys/fs/inode-nr", "/proc/sys/fs/inode-state",
      "/proc/sys/fs/dentry-state", "/proc/sys/fs/aio-nr",
      "/proc/sys/kernel/random/entropy_avail",
  };
  for (size_t i = 0; i < sizeof(VOLATILE) / sizeof(VOLATILE[0]); i++) {
    if (strcmp(path, VOLATILE[i]) == 0) {
      return 0;
    }
  }
  return 1;
}

/* The whole file, read in pieces, or NULL (errno set) if it cannot be opened. */
static char* read_whole(const char* path, size_t* length) {
  int fd = open(path, O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    return NULL;
  }
  size_t capacity = 4096, used = 0;
  char* buffer = malloc(capacity + 1);
  for (;;) {
    if (used == capacity) {
      capacity *= 2;
      buffer = realloc(buffer, capacity + 1);
    }
    ssize_t n = read(fd, buffer + used, capacity - used);
    if (n < 0) {
      CHECK(0, "read %s: %s", path, strerror(errno));
      break;
    }
    if (n == 0) {
      break;
    }
    used += n;
  }
  close(fd);
  buffer[used] = '\0';
  *length = used;
  return buffer;
}

/* The file read twice, or `bytes` NULL if it is absent. */
static struct contents read_file(const char* path, struct stat* st) {
  struct contents file = {NULL, 0, 0};
  int fd = open(path, O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    CHECK(errno == ENOENT, "open %s: %s", path, strerror(errno));
    return file;
  }
  CHECK(fstat(fd, st) == 0, "fstat %s: %s", path, strerror(errno));
  close(fd);
  file.bytes = read_whole(path, &file.length);
  size_t again_length = 0;
  char* again = read_whole(path, &again_length);
  int same = again && again_length == file.length && memcmp(again, file.bytes, file.length) == 0;
  free(again);
  CHECK(host || same, "%s changed between two reads", path);
  file.stable = same && (!host || host_constant(path));
  CHECK(file.length == 0 || file.bytes[file.length - 1] == '\n',
        "%s does not end in a newline", path);
  return file;
}

/* Whether `path` is a seq_file (outside /proc/sys) rather than a sysctl. */
static int is_seq_file(const char* path) {
  return strncmp(path, "/proc/sys/", 10) != 0;
}

/* Whether `data`, `length` bytes, is the file at `path` from `offset`: as
 * read earlier, or, in the native control only, for a host table that has
 * changed since, as read now. */
static int is_file_at(const char* path, const struct contents* file, size_t offset,
                      const char* data, size_t length) {
  if (length == 0) {
    return 1;
  }
  if (offset + length <= file->length && memcmp(file->bytes + offset, data, length) == 0) {
    return 1;
  }
  if (!host) {
    return 0;
  }
  size_t now_length;
  char* now = read_whole(path, &now_length);
  int same = now && offset + length <= now_length && memcmp(now + offset, data, length) == 0;
  free(now);
  return same;
}

/* Whether Linux 7.1 reads `path` through `read_iter`: every file here but
 * /proc/modules and /proc/swaps, whose vectored reads take only RWF_HIPRI
 * (do_loop_readv_writev). */
static int has_read_iter(const char* path) {
  return strcmp(path, "/proc/modules") != 0 && strcmp(path, "/proc/swaps") != 0;
}

/* Whether `path` is an integer sysctl: digits separated by blanks. */
static int is_integer(const char* path, const struct contents* file) {
  return !is_seq_file(path) && file->length > 0 &&
         strspn(file->bytes, "0123456789 \t\n") == file->length;
}

/* Every other way to reach the file agrees with `fstat`'s `st`. */
static void check_access_paths(const char* path, const struct stat* st,
                               const struct contents* file, int listings) {
  struct stat by_path;
  CHECK(stat(path, &by_path) == 0 && by_path.st_dev == st->st_dev &&
            by_path.st_ino == st->st_ino && by_path.st_mode == st->st_mode,
        "stat %s disagrees with fstat", path);

  struct statx stx;
  CHECK(statx(AT_FDCWD, path, 0, STATX_BASIC_STATS, &stx) == 0 &&
            stx.stx_ino == st->st_ino && stx.stx_mode == st->st_mode &&
            makedev(stx.stx_dev_major, stx.stx_dev_minor) == st->st_dev,
        "statx %s disagrees with fstat", path);

  /* Linux reads the path before it writes the result over it. */
  union {
    char path[PATH_MAX];
    struct stat st;
  } overlap;
  snprintf(overlap.path, sizeof(overlap.path), "%s", path);
  /* The raw call: the C library's prototype forbids the aliasing. */
  CHECK(syscall(SYS_newfstatat, AT_FDCWD, overlap.path, &overlap.st, 0) == 0 &&
            overlap.st.st_ino == st->st_ino,
        "stat %s into its own path disagrees with fstat", path);
  union {
    char path[PATH_MAX];
    struct statx stx;
  } overlap_x;
  snprintf(overlap_x.path, sizeof(overlap_x.path), "%s", path);
  CHECK(syscall(SYS_statx, AT_FDCWD, overlap_x.path, 0, STATX_BASIC_STATS, &overlap_x.stx) == 0 &&
            overlap_x.stx.stx_ino == st->st_ino,
        "statx %s into its own path disagrees with fstat", path);

  CHECK(access(path, R_OK) == 0, "access %s R_OK: %s", path, strerror(errno));

  int integer = is_integer(path, file);
  int fd = open(path, O_RDONLY | O_CLOEXEC);
  CHECK(fd >= 0, "open %s: %s", path, strerror(errno));
  if (fd >= 0) {
    char link[64], target[PATH_MAX];
    snprintf(link, sizeof(link), "/proc/self/fd/%d", fd);
    ssize_t n = readlink(link, target, sizeof(target) - 1);
    target[n < 0 ? 0 : n] = '\0';
    CHECK(strcmp(target, path) == 0, "%s links to \"%s\"", path, target);

    /* An integer sysctl formats its value only for a read from position 0. */
    if (file->length > 2) {
      char head[2], rest[4096];
      CHECK(read(fd, head, sizeof(head)) == 2, "partial read of %s", path);
      ssize_t more = read(fd, rest, sizeof(rest));
      if (integer) {
        CHECK(more == 0, "%s served %zd bytes after a partial read", path, more);
      } else if (file->stable && file->length - 2 <= sizeof(rest)) {
        CHECK(more == (ssize_t)file->length - 2 && is_file_at(path, file, 2, rest, more),
              "%s served %zd of %zu remaining bytes", path, more, file->length - 2);
      } else if (file->stable) {
        /* A seq_file may stop short of a full buffer: the bytes served are
         * the next ones, however many. */
        CHECK(more > 0 && is_file_at(path, file, 2, rest, more),
              "%s served %zd bytes after a partial read", path, more);
      } else {
        CHECK(more > 0, "%s served %zd bytes after a partial read", path, more);
      }
    }
    close(fd);
  }

  /* A vectored read is one read of the file, over both buffers; a positioned
   * one starts where it says. */
  fd = open(path, O_RDONLY | O_CLOEXEC);
  if (fd >= 0) {
    char first[3], second[256];
    struct iovec parts[2] = {{first, sizeof(first)}, {second, sizeof(second)}};
    ssize_t total = readv(fd, parts, 2);
    size_t room = sizeof(first) + sizeof(second);
    size_t wanted = file->length < room ? file->length : room;
    if (file->stable) {
      char joined[sizeof(first) + sizeof(second)];
      memcpy(joined, first, sizeof(first));
      memcpy(joined + sizeof(first), second, sizeof(second));
      CHECK(total == (ssize_t)wanted && is_file_at(path, file, 0, joined, total),
            "%s readv gave %zd of %zu bytes", path, total, wanted);
    } else {
      CHECK(total > 0, "%s readv gave %zd", path, total);
    }
    char at_one[256];
    struct iovec whole = {at_one, sizeof(at_one)};
    ssize_t positioned = preadv(fd, &whole, 1, 1);
    size_t rest = file->length == 0 ? 0 : file->length - 1;
    rest = rest < sizeof(at_one) ? rest : sizeof(at_one);
    /* An integer sysctl formats nothing for a read past position 0. */
    if (integer) {
      CHECK(positioned == 0, "%s preadv at 1 gave %zd", path, positioned);
    } else if (file->stable) {
      CHECK(positioned == (ssize_t)rest && is_file_at(path, file, 1, at_one, rest),
            "%s preadv at 1 gave %zd", path, positioned);
    }
    /* A seq file seeks only from its start or position; a sysctl file is a
     * file of size 0. */
    if (is_seq_file(path)) {
      CHECK(lseek(fd, 0, SEEK_END) == -1 && errno == EINVAL, "%s SEEK_END: %s", path,
            strerror(errno));
    } else {
      CHECK(lseek(fd, 0, SEEK_END) == 0, "%s SEEK_END", path);
    }
    close(fd);
  }

  if (listings) {
    char directory[PATH_MAX];
    snprintf(directory, sizeof(directory), "%s", path);
    char* slash = strrchr(directory, '/');
    const char* name = slash + 1;
    *slash = '\0';
    char name_copy[NAME_MAX + 1];
    snprintf(name_copy, sizeof(name_copy), "%s", name);
    DIR* dir = opendir(directory);
    CHECK(dir != NULL, "opendir %s: %s", directory, strerror(errno));
    int found = 0;
    for (struct dirent* entry; dir && (entry = readdir(dir)) != NULL;) {
      if (strcmp(entry->d_name, name_copy) == 0) {
        found = 1;
        CHECK(entry->d_ino == st->st_ino, "%s lists %s with inode %lu, not %lu", directory,
              name_copy, (unsigned long)entry->d_ino, (unsigned long)st->st_ino);
      }
    }
    if (dir) {
      closedir(dir);
    }
    CHECK(found, "%s does not list %s", directory, name_copy);
  }
}

/* A call's result, or the negated errno it failed with. */
static ssize_t outcome(ssize_t result) {
  return result < 0 ? -errno : result;
}

/* Reads of `path`, opened as `name`, whose contents are `file`. */
static void check_reads(const char* path, const char* name, const struct contents* file) {
  int seq_file = is_seq_file(path);
  size_t length = file->length;
  int fd = open(name, O_RDONLY | O_CLOEXEC);
  CHECK(fd >= 0, "open %s: %s", name, strerror(errno));
  if (fd < 0) {
    return;
  }

  /* The start of the file in one vectored read, and the size a seek
   * reports. */
  char buffer[256];
  size_t whole_length = length < sizeof(buffer) ? length : sizeof(buffer);
  struct iovec whole = {buffer, sizeof(buffer)};
  ssize_t got = outcome(readv(fd, &whole, 1));
  if (file->stable) {
    CHECK(got == (ssize_t)whole_length && is_file_at(path, file, 0, buffer, whole_length),
          "%s readv gave %zd of %zu bytes", name, got, whole_length);
  } else {
    CHECK(got > 0, "%s readv gave %zd", name, got);
  }
  if (seq_file) {
    CHECK(outcome(lseek(fd, 0, SEEK_END)) == -EINVAL, "%s SEEK_END", name);
  } else {
    CHECK(outcome(lseek(fd, 0, SEEK_END)) == 0, "%s SEEK_END", name);
  }

  /* kiocb_set_rw_flags: an unknown flag first, then APPEND with NOAPPEND,
   * then what these files do not support. An empty vector reads nothing
   * before any flag is looked at. */
  static const struct {
    int flags;
    int error;
  } FLAGS[] = {
      {RWF_HIPRI, 0},
      {RWF_DSYNC, 0},
      {RWF_SYNC, 0},
      {RWF_APPEND, 0},
      {RWF_NOAPPEND, 0},
      {RWF_NOSIGNAL, 0},
      {RWF_NOWAIT, EOPNOTSUPP},
      {RWF_ATOMIC, EOPNOTSUPP},
      {RWF_DONTCACHE, EOPNOTSUPP},
      {RWF_UNKNOWN, EOPNOTSUPP},
      {RWF_APPEND | RWF_NOAPPEND, EINVAL},
      {RWF_NOWAIT | RWF_UNKNOWN, EOPNOTSUPP},
      {RWF_APPEND | RWF_NOAPPEND | RWF_UNKNOWN, EOPNOTSUPP},
      {RWF_ATOMIC | RWF_APPEND | RWF_NOAPPEND, EINVAL},
  };
  struct iovec empty = {buffer, 0};
  int read_iter = has_read_iter(path);
  for (size_t i = 0; i < sizeof(FLAGS) / sizeof(FLAGS[0]); i++) {
    got = outcome(preadv2(fd, &whole, 1, 0, FLAGS[i].flags));
    int error = read_iter ? FLAGS[i].error
                          : FLAGS[i].flags & ~RWF_HIPRI ? EOPNOTSUPP : 0;
    if (error) {
      CHECK(got == -error, "%s preadv2 flags %#x gave %zd, not %d", name, FLAGS[i].flags, got,
            -error);
    } else if (file->stable) {
      CHECK(got == (ssize_t)whole_length, "%s preadv2 flags %#x gave %zd, not %zu", name,
            FLAGS[i].flags, got, whole_length);
    } else {
      CHECK(got >= 0, "%s preadv2 flags %#x gave %zd", name, FLAGS[i].flags, got);
    }
    got = outcome(preadv2(fd, &empty, 1, 0, FLAGS[i].flags));
    CHECK(got == 0, "%s empty preadv2 flags %#x gave %zd", name, FLAGS[i].flags, got);
  }

  /* import_iovec checks every destination before the file is read. */
  struct iovec outside = {(void*)UINTPTR_MAX, 1};
  got = outcome(preadv(fd, &outside, 1, 100000000));
  CHECK(got == -EFAULT, "%s preadv outside the address space at end of file: %zd", name, got);
  struct iovec later[2] = {{buffer, sizeof(buffer)}, {(void*)UINTPTR_MAX, 1}};
  got = outcome(preadv(fd, later, 2, 0));
  CHECK(got == -EFAULT, "%s preadv outside the address space in a later vector: %zd", name,
        got);

  /* A buffer with two writable bytes before an inaccessible page. */
  long page = sysconf(_SC_PAGESIZE);
  char* pages = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  CHECK(pages != MAP_FAILED, "mmap: %s", strerror(errno));
  if (pages != MAP_FAILED) {
    CHECK(mprotect(pages + page, page, PROT_NONE) == 0, "mprotect: %s", strerror(errno));
    char* edge = pages + page - 2;
    ssize_t torn = length <= 2 ? (ssize_t)length : seq_file ? 2 : -EFAULT;
    off_t after = torn < 0 ? 0 : torn;
    lseek(fd, 0, SEEK_SET);
    got = outcome(read(fd, edge, 16));
    CHECK(got == torn, "%s read into a torn buffer gave %zd, not %zd", name, got, torn);
    CHECK(lseek(fd, 0, SEEK_CUR) == after, "%s after a torn read is at %ld", name,
          (long)lseek(fd, 0, SEEK_CUR));
    got = outcome(pread(fd, edge, 16, 0));
    CHECK(got == torn, "%s pread into a torn buffer gave %zd, not %zd", name, got, torn);
    char second[32];
    memset(second, 'Z', sizeof(second));
    struct iovec torn_vectors[2] = {{edge, 16}, {second, sizeof(second)}};
    lseek(fd, 0, SEEK_SET);
    got = outcome(readv(fd, torn_vectors, 2));
    CHECK(got == torn && (length <= 2 || second[0] == 'Z'),
          "%s readv into a torn buffer gave %zd, not %zd", name, got, torn);
    CHECK(lseek(fd, 0, SEEK_CUR) == after, "%s after a torn readv is at %ld", name,
          (long)lseek(fd, 0, SEEK_CUR));
    munmap(pages, 2 * page);
  }
  close(fd);
}

/* The same reads through a symbolic link to `path`. */
static void check_reads_through_a_link(const char* path, const struct contents* file) {
  char directory[] = "/tmp/virtual-kernel-files-XXXXXX";
  CHECK(mkdtemp(directory) != NULL, "mkdtemp: %s", strerror(errno));
  char link[64];
  snprintf(link, sizeof(link), "%s/link", directory);
  CHECK(symlink(path, link) == 0, "symlink: %s", strerror(errno));
  check_reads(path, link, file);
  unlink(link);
  rmdir(directory);
}

/* The program's own path, to exec it again. */
static const char* self_path;

/* In the image exec'd by check_exec_keeps_the_position: descriptor `fd`,
 * opened on `path` through a link, was read for 6 bytes before the exec; it
 * reads on from there. */
static int inherited_main(int fd, const char* path) {
  size_t length;
  char* expected = read_whole(path, &length);
  size_t got_length;
  char* got = NULL;
  {
    size_t capacity = 4096, used = 0;
    got = malloc(capacity + 1);
    for (;;) {
      if (used == capacity) {
        capacity *= 2;
        got = realloc(got, capacity + 1);
      }
      ssize_t n = read(fd, got + used, capacity - used);
      if (n <= 0) {
        break;
      }
      used += n;
    }
    got_length = used;
  }
  if (!expected || length < 6 || got_length != length - 6 ||
      memcmp(got, expected + 6, got_length) != 0) {
    fprintf(stderr, "FAIL: %s inherited across exec read %zu bytes, not the %zu after 6\n",
            path, got_length, length < 6 ? 0 : length - 6);
    return 1;
  }
  return 0;
}

/* A descriptor opened on `path` through a symbolic link and partly read
 * keeps its position in the image exec'd next. */
static void check_exec_keeps_the_position(const char* path) {
  char directory[] = "/tmp/virtual-kernel-files-exec-XXXXXX";
  CHECK(mkdtemp(directory) != NULL, "mkdtemp: %s", strerror(errno));
  char link[64];
  snprintf(link, sizeof(link), "%s/link", directory);
  CHECK(symlink(path, link) == 0, "symlink: %s", strerror(errno));
  int fd = open(link, O_RDONLY);
  CHECK(fd >= 0, "open %s: %s", link, strerror(errno));
  char head[6];
  CHECK(fd >= 0 && read(fd, head, sizeof(head)) == 6, "read 6 bytes of %s", path);
  pid_t child = fork();
  if (child == 0) {
    char fd_text[16];
    snprintf(fd_text, sizeof(fd_text), "%d", fd);
    execl(self_path, self_path, "--inherited", fd_text, path, (char*)NULL);
    _exit(127);
  }
  int status = 0;
  CHECK(child > 0 && waitpid(child, &status, 0) == child && WIFEXITED(status) &&
            WEXITSTATUS(status) == 0,
        "%s: the exec'd image did not read on from the position (status %#x)", path, status);
  close(fd);
  unlink(link);
  rmdir(directory);
}

/* The number of CPUs a cpulist file (`0-3,8`) names, or -1 if unreadable. */
static int cpulist_count(const char* path) {
  size_t length;
  char* list = read_whole(path, &length);
  if (list == NULL) {
    return -1;
  }
  int count = 0;
  for (char* range = strtok(list, ",\n"); range; range = strtok(NULL, ",\n")) {
    int low, high;
    if (sscanf(range, "%d-%d", &low, &high) == 2) {
      count += high - low + 1;
    } else {
      count++;
    }
  }
  free(list);
  return count;
}

/* The number of whitespace-separated words on the first line of `text`. */
static int header_words(const char* text) {
  int words = 0, in_word = 0;
  for (; *text && *text != '\n'; text++) {
    if (*text == ' ' || *text == '\t') {
      in_word = 0;
    } else if (!in_word) {
      in_word = 1;
      words++;
    }
  }
  return words;
}

/* The number of lines of `text` that start with `cpu` and a digit. */
static int cpu_lines(const char* text) {
  int count = 0;
  for (const char* line = text; line && *line; line = strchr(line, '\n'), line = line ? line + 1 : NULL) {
    if (strncmp(line, "cpu", 3) == 0 && line[3] >= '0' && line[3] <= '9') {
      count++;
    }
  }
  return count;
}

static const struct contents* find(const struct contents* files, const char* path) {
  for (size_t i = 0; i < NFILES; i++) {
    if (strcmp(FILES[i], path) == 0) {
      return files[i].bytes ? &files[i] : NULL;
    }
  }
  return NULL;
}

/* The tables agree with each other, as Linux's do. */
static void check_one_machine(const struct contents* files) {
  int online = cpulist_count("/sys/devices/system/cpu/online");
  int possible = cpulist_count("/sys/devices/system/cpu/possible");
  size_t length;
  char* stat = read_whole("/proc/stat", &length);
  const struct contents* interrupts = find(files, "/proc/interrupts");
  const struct contents* softirqs = find(files, "/proc/softirqs");
  const struct contents* schedstat = find(files, "/proc/schedstat");
  CHECK(online > 0 && possible >= online, "online %d, possible %d CPUs", online, possible);
  CHECK(stat && cpu_lines(stat) == online, "/proc/stat has %d CPU lines, %d online",
        stat ? cpu_lines(stat) : -1, online);
  CHECK(!interrupts || header_words(interrupts->bytes) == online,
        "/proc/interrupts has %d CPU columns, %d online",
        interrupts ? header_words(interrupts->bytes) : -1, online);
  CHECK(!softirqs || header_words(softirqs->bytes) == possible,
        "/proc/softirqs has %d CPU columns, %d possible",
        softirqs ? header_words(softirqs->bytes) : -1, possible);
  CHECK(!schedstat || cpu_lines(schedstat->bytes) == online,
        "/proc/schedstat has %d CPU lines, %d online",
        schedstat ? cpu_lines(schedstat->bytes) : -1, online);
  free(stat);

  /* MemTotal is sysinfo's total memory in whole kB. Linux counts both in
   * pages, so natively the two are exactly equal; Hermit's configured size
   * need not be a whole number of kB (a known difference, recorded at
   * https://github.com/rrnewton/hermit/issues/4263). */
  const struct contents* meminfo = find(files, "/proc/meminfo");
  struct sysinfo info;
  unsigned long long total_kb = 0;
  CHECK(sysinfo(&info) == 0, "sysinfo: %s", strerror(errno));
  CHECK(meminfo && sscanf(meminfo->bytes, "MemTotal: %llu kB", &total_kb) == 1 &&
            (host ? total_kb * 1024 == (unsigned long long)info.totalram * info.mem_unit
                  : total_kb == (unsigned long long)info.totalram * info.mem_unit / 1024),
        "MemTotal %llu kB, sysinfo %llu bytes", total_kb,
        (unsigned long long)info.totalram * info.mem_unit);

  /* buddyinfo lists exactly the zones whose page sets zoneinfo shows. */
  const struct contents* buddyinfo = find(files, "/proc/buddyinfo");
  const struct contents* zoneinfo = find(files, "/proc/zoneinfo");
  if (buddyinfo && zoneinfo) {
    int populated = 0, listed = 0;
    const char* zone = zoneinfo->bytes;
    while ((zone = strstr(zone, "\nNode ")) || zone == NULL) {
      if (zone == NULL) {
        break;
      }
      zone++;
      const char* next = strstr(zone, "\nNode ");
      const char* pagesets = strstr(zone, "\n  pagesets\n");
      if (pagesets && (!next || pagesets < next)) {
        populated++;
        char name[64];
        const char* end = strchr(zone, '\n');
        snprintf(name, sizeof(name), "%.*s", (int)(end - zone), zone);
        CHECK(strstr(buddyinfo->bytes, name) != NULL, "buddyinfo lacks \"%s\"", name);
      }
    }
    if (strncmp(zoneinfo->bytes, "Node ", 5) == 0) {
      const char* next = strstr(zoneinfo->bytes, "\nNode ");
      const char* pagesets = strstr(zoneinfo->bytes, "\n  pagesets\n");
      if (pagesets && (!next || pagesets < next)) {
        populated++;
        char name[64];
        const char* end = strchr(zoneinfo->bytes, '\n');
        snprintf(name, sizeof(name), "%.*s", (int)(end - zoneinfo->bytes), zoneinfo->bytes);
        CHECK(strstr(buddyinfo->bytes, name) != NULL, "buddyinfo lacks \"%s\"", name);
      }
    }
    for (const char* line = buddyinfo->bytes; *line; line = strchr(line, '\n') + 1) {
      listed++;
    }
    CHECK(populated == listed, "zoneinfo shows %d populated zones, buddyinfo %d", populated,
          listed);
  }
}

static int starts_with(const char* text, const char* prefix) {
  return strncmp(text, prefix, strlen(prefix)) == 0;
}

static int ends_with(const char* text, const char* suffix) {
  size_t a = strlen(text), b = strlen(suffix);
  return a >= b && strcmp(text + a - b, suffix) == 0;
}

/* `file`'s one line, without its newline, or NULL if it has another number
 * of lines. */
static char* one_line(const struct contents* file) {
  if (file->length == 0 || memchr(file->bytes, '\n', file->length - 1) != NULL) {
    return NULL;
  }
  return strndup(file->bytes, file->length - 1);
}

static uint64_t fnv1a(const char* bytes, size_t length) {
  uint64_t hash = 0xcbf29ce484222325ull;
  for (size_t i = 0; i < length; i++) {
    hash = (hash ^ (unsigned char)bytes[i]) * 0x100000001b3ull;
  }
  return hash;
}

int main(int argc, char** argv) {
  self_path = argv[0];
  if (argc == 4 && strcmp(argv[1], "--inherited") == 0) {
    return inherited_main(atoi(argv[2]), argv[3]);
  }
  int listings = 1;
  for (int i = 1; i < argc; i++) {
    if (strcmp(argv[i], "--kvm-local-procfs") == 0) {
      listings = 0;
    } else if (strcmp(argv[i], "--host") == 0) {
      host = 1;
    } else {
      fprintf(stderr, "usage: %s [--kvm-local-procfs] [--host]\n", argv[0]);
      return 2;
    }
  }
  struct utsname un;
  if (uname(&un) != 0) {
    perror("uname");
    return 1;
  }
  /* A procfs file every backend serves: procfs's device. (KVM's own /proc
   * refuses the /proc root itself until it is removed,
   * https://github.com/rrnewton/hermit/issues/4266.) */
  struct stat proc;
  CHECK(stat("/proc/sys/kernel/random/uuid", &proc) == 0, "stat uuid: %s",
        strerror(errno));
  /* A 0644 tunable is writable only by global root. */
  int global_root = access("/proc/sys/kernel/pid_max", W_OK) == 0;

  struct contents files[NFILES];
  struct stat stats[NFILES];
  for (size_t i = 0; i < NFILES; i++) {
    files[i] = read_file(FILES[i], &stats[i]);
    if (files[i].bytes == NULL) {
      continue;
    }
    CHECK(S_ISREG(stats[i].st_mode), "%s is not a regular file", FILES[i]);
    CHECK(stats[i].st_dev == proc.st_dev, "%s is not on procfs's device", FILES[i]);
    if (!global_root) {
      CHECK(open(FILES[i], O_WRONLY) == -1 && errno == EACCES,
            "%s opened for writing", FILES[i]);
    }
    check_access_paths(FILES[i], &stats[i], &files[i], listings);
    check_reads(FILES[i], FILES[i], &files[i]);
    if (listings) {
      check_reads_through_a_link(FILES[i], &files[i]);
    }
  }
  check_one_machine(files);
  if (listings) {
    check_exec_keeps_the_position("/proc/version");
    /* The host's meminfo changes between the two images' reads. */
    if (!host) {
      check_exec_keeps_the_position("/proc/meminfo");
    }
  }

  char* version = files[0].bytes ? one_line(&files[0]) : NULL;
  char* osrelease = files[2].bytes ? one_line(&files[2]) : NULL;
  char* ostype = files[3].bytes ? one_line(&files[3]) : NULL;
  char* kernel_version = files[4].bytes ? one_line(&files[4]) : NULL;
  CHECK(osrelease && strcmp(osrelease, un.release) == 0, "osrelease %s, uname %s",
        osrelease, un.release);
  CHECK(ostype && strcmp(ostype, un.sysname) == 0, "ostype %s, uname %s", ostype,
        un.sysname);
  CHECK(kernel_version && strcmp(kernel_version, un.version) == 0,
        "kernel/version %s, uname %s", kernel_version, un.version);
  char prefix[256];
  snprintf(prefix, sizeof(prefix), "%s version %s ", un.sysname, un.release);
  CHECK(version && starts_with(version, prefix) && ends_with(version, un.version),
        "/proc/version \"%s\" does not describe uname's kernel", version);

  for (size_t i = 0; i < NFILES; i++) {
    if (files[i].bytes == NULL) {
      printf("%s absent\n", FILES[i]);
      continue;
    }
    printf("%s mode=%04o dev=%#lx ino=%lu ", FILES[i], (unsigned)(stats[i].st_mode & 07777),
           (unsigned long)stats[i].st_dev, (unsigned long)stats[i].st_ino);
    char* line = one_line(&files[i]);
    if (!files[i].stable) {
      printf("volatile\n");
    } else if (line) {
      printf("%s\n", line);
    } else {
      size_t lines = 0;
      for (size_t j = 0; j < files[i].length; j++) {
        lines += files[i].bytes[j] == '\n';
      }
      printf("bytes=%zu lines=%zu fnv=%016llx\n", files[i].length, lines,
             (unsigned long long)fnv1a(files[i].bytes, files[i].length));
    }
    free(line);
  }
  return failures == 0 ? 0 : 1;
}
