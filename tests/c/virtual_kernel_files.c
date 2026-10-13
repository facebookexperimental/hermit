/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * The procfs files that describe the kernel and its fixed limits
 * (https://github.com/rrnewton/hermit/issues/4266).
 *
 * Checks what Linux guarantees, natively and under Hermit:
 * - kernel/osrelease is uname's release, kernel/ostype its sysname and
 *   kernel/version its version;
 * - /proc/version starts "<sysname> version <release> " and ends with
 *   uname's version;
 * - none of the files can be opened for writing by a process that is not
 *   global root, and each is a regular file on the procfs device (that of
 *   kernel/random/uuid);
 * - stat by path, statx and fstat agree, also when stat's result overwrites
 *   the path it was given; access(R_OK) succeeds; the descriptor's link in
 *   /proc/self/fd names the file; and its directory lists it under the same
 *   inode (not with --kvm-local-procfs: KVM's own /proc lists nothing until
 *   it is removed, https://github.com/rrnewton/hermit/issues/4266);
 * - an integer sysctl ends after a partial read, while a string sysctl and a
 *   seq_file file serve the rest;
 * - preadv2 accepts and refuses the flags Linux 7.1 does for these files; a
 *   vectored read refuses a destination outside the address space, also at
 *   end of file or in a later vector; and a read whose buffer faults partway
 *   copies up to the fault from a seq_file but fails with EFAULT from a
 *   sysctl, which then keeps its position;
 * - all of that also holds for the file opened through a symbolic link, which
 *   the kernel resolves (not with --kvm-local-procfs, whose own /proc cannot
 *   be reached through a link, https://github.com/rrnewton/hermit/issues/4282).
 *
 * Then prints, for every file, its permission bits, device and inode and
 * its contents, so that a caller can require the same lines from every
 * backend. A file the host does not have prints "absent".
 */

#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdint.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/sysmacros.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/utsname.h>
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
};

static int failures;

#define CHECK(condition, ...)              \
  do {                                     \
    if (!(condition)) {                    \
      fprintf(stderr, "FAIL: " __VA_ARGS__); \
      fprintf(stderr, "\n");               \
      failures++;                          \
    }                                      \
  } while (0)

/* The file's contents, without its final newline, or NULL if absent. */
static char* read_file(const char* path, struct stat* stat) {
  int fd = open(path, O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    CHECK(errno == ENOENT, "open %s: %s", path, strerror(errno));
    return NULL;
  }
  CHECK(fstat(fd, stat) == 0, "fstat %s: %s", path, strerror(errno));
  static char buffer[4096];
  size_t length = 0;
  for (;;) {
    ssize_t n = read(fd, buffer + length, sizeof(buffer) - 1 - length);
    if (n < 0) {
      CHECK(0, "read %s: %s", path, strerror(errno));
      break;
    }
    if (n == 0 || length + n == sizeof(buffer) - 1) {
      length += n;
      break;
    }
    length += n;
  }
  close(fd);
  buffer[length] = '\0';
  CHECK(length > 0 && buffer[length - 1] == '\n', "%s does not end in a newline", path);
  if (length > 0 && buffer[length - 1] == '\n') {
    buffer[length - 1] = '\0';
  }
  return strdup(buffer);
}

/* Every other way to reach the file agrees with `fstat`'s `st`. */
static void check_access_paths(const char* path, const struct stat* st, const char* contents,
                               int listings) {
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

  int fd = open(path, O_RDONLY | O_CLOEXEC);
  CHECK(fd >= 0, "open %s: %s", path, strerror(errno));
  if (fd >= 0) {
    char link[64], target[PATH_MAX];
    snprintf(link, sizeof(link), "/proc/self/fd/%d", fd);
    ssize_t n = readlink(link, target, sizeof(target) - 1);
    target[n < 0 ? 0 : n] = '\0';
    CHECK(strcmp(target, path) == 0, "%s links to \"%s\"", path, target);

    /* An integer sysctl formats its value only for a read from position 0. */
    char head[2], rest[4096];
    size_t length = strlen(contents) + 1;
    int integer = strspn(contents, "0123456789") == strlen(contents);
    CHECK(read(fd, head, sizeof(head)) == 2, "partial read of %s", path);
    ssize_t more = read(fd, rest, sizeof(rest));
    if (integer) {
      CHECK(more == 0, "%s served %zd bytes after a partial read", path, more);
    } else {
      CHECK(more == (ssize_t)length - 2, "%s served %zd of %zu remaining bytes", path, more,
            length - 2);
    }
    close(fd);
  }

  /* A vectored read is one read of the file, over both buffers; a positioned
   * one starts where it says. */
  fd = open(path, O_RDONLY | O_CLOEXEC);
  if (fd >= 0) {
    /* The file as read, with its newline. */
    size_t length = strlen(contents) + 1;
    char expected[4096];
    snprintf(expected, sizeof(expected), "%s\n", contents);
    char first[3], second[256];
    struct iovec parts[2] = {{first, sizeof(first)}, {second, sizeof(second)}};
    ssize_t total = readv(fd, parts, 2);
    size_t wanted = length < sizeof(first) + sizeof(second) ? length : sizeof(first) + sizeof(second);
    CHECK(total == (ssize_t)wanted && memcmp(first, expected, total < 3 ? total : 3) == 0 &&
              (total <= 3 || memcmp(second, expected + 3, total - 3) == 0),
          "%s readv gave %zd of %zu bytes", path, total, wanted);
    char at_one[256];
    struct iovec whole = {at_one, sizeof(at_one)};
    ssize_t positioned = preadv(fd, &whole, 1, 1);
    int integer = strspn(contents, "0123456789") == strlen(contents);
    size_t rest = length - 1 < sizeof(at_one) ? length - 1 : sizeof(at_one);
    /* An integer sysctl formats nothing for a read past position 0. */
    CHECK(positioned == (integer ? 0 : (ssize_t)rest) &&
              (integer || memcmp(at_one, expected + 1, rest) == 0),
          "%s preadv at 1 gave %zd", path, positioned);
    /* A seq file seeks only from its start or position; a sysctl file is a
     * file of size 0. */
    int seq_file = strncmp(path, "/proc/sys/", 10) != 0;
    if (seq_file) {
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

/* A call's result, or the negated errno it failed with. */
static ssize_t outcome(ssize_t result) {
  return result < 0 ? -errno : result;
}

/* Reads of `path`, opened as `name`, whose contents (without the newline)
 * are `contents`. */
static void check_reads(const char* path, const char* name, const char* contents) {
  int seq_file = strncmp(path, "/proc/sys/", 10) != 0;
  size_t length = strlen(contents) + 1;
  char expected[4096];
  snprintf(expected, sizeof(expected), "%s\n", contents);
  int fd = open(name, O_RDONLY | O_CLOEXEC);
  CHECK(fd >= 0, "open %s: %s", name, strerror(errno));
  if (fd < 0) {
    return;
  }

  /* The whole file in one vectored read, and the size a seek reports. */
  char buffer[256];
  size_t whole_length = length < sizeof(buffer) ? length : sizeof(buffer);
  struct iovec whole = {buffer, sizeof(buffer)};
  ssize_t got = outcome(readv(fd, &whole, 1));
  CHECK(got == (ssize_t)whole_length && memcmp(buffer, expected, whole_length) == 0,
        "%s readv gave %zd of %zu bytes", name, got, whole_length);
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
  for (size_t i = 0; i < sizeof(FLAGS) / sizeof(FLAGS[0]); i++) {
    ssize_t wanted = FLAGS[i].error ? -FLAGS[i].error : (ssize_t)whole_length;
    got = outcome(preadv2(fd, &whole, 1, 0, FLAGS[i].flags));
    CHECK(got == wanted, "%s preadv2 flags %#x gave %zd, not %zd", name, FLAGS[i].flags, got,
          wanted);
    got = outcome(preadv2(fd, &empty, 1, 0, FLAGS[i].flags));
    CHECK(got == 0, "%s empty preadv2 flags %#x gave %zd", name, FLAGS[i].flags, got);
  }

  /* import_iovec checks every destination before the file is read. */
  struct iovec outside = {(void*)UINTPTR_MAX, 1};
  got = outcome(preadv(fd, &outside, 1, 100000));
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
static void check_reads_through_a_link(const char* path, const char* contents) {
  char directory[] = "/tmp/virtual-kernel-files-XXXXXX";
  CHECK(mkdtemp(directory) != NULL, "mkdtemp: %s", strerror(errno));
  char link[64];
  snprintf(link, sizeof(link), "%s/link", directory);
  CHECK(symlink(path, link) == 0, "symlink: %s", strerror(errno));
  check_reads(path, link, contents);
  unlink(link);
  rmdir(directory);
}

static int starts_with(const char* text, const char* prefix) {
  return strncmp(text, prefix, strlen(prefix)) == 0;
}

static int ends_with(const char* text, const char* suffix) {
  size_t a = strlen(text), b = strlen(suffix);
  return a >= b && strcmp(text + a - b, suffix) == 0;
}

int main(int argc, char** argv) {
  int listings = 1;
  for (int i = 1; i < argc; i++) {
    if (strcmp(argv[i], "--kvm-local-procfs") == 0) {
      listings = 0;
    } else {
      fprintf(stderr, "usage: %s [--kvm-local-procfs]\n", argv[0]);
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

  char* contents[sizeof(FILES) / sizeof(FILES[0])];
  struct stat stats[sizeof(FILES) / sizeof(FILES[0])];
  for (size_t i = 0; i < sizeof(FILES) / sizeof(FILES[0]); i++) {
    contents[i] = read_file(FILES[i], &stats[i]);
    if (contents[i] == NULL) {
      continue;
    }
    CHECK(S_ISREG(stats[i].st_mode), "%s is not a regular file", FILES[i]);
    CHECK(stats[i].st_dev == proc.st_dev, "%s is not on procfs's device", FILES[i]);
    if (!global_root) {
      CHECK(open(FILES[i], O_WRONLY) == -1 && errno == EACCES,
            "%s opened for writing", FILES[i]);
    }
    check_access_paths(FILES[i], &stats[i], contents[i], listings);
    check_reads(FILES[i], FILES[i], contents[i]);
    if (listings) {
      check_reads_through_a_link(FILES[i], contents[i]);
    }
  }

  const char* version = contents[0];
  const char* osrelease = contents[2];
  const char* ostype = contents[3];
  const char* kernel_version = contents[4];
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

  for (size_t i = 0; i < sizeof(FILES) / sizeof(FILES[0]); i++) {
    if (contents[i] == NULL) {
      printf("%s absent\n", FILES[i]);
    } else {
      printf("%s mode=%04o dev=%#lx ino=%lu %s\n", FILES[i],
             (unsigned)(stats[i].st_mode & 07777), (unsigned long)stats[i].st_dev,
             (unsigned long)stats[i].st_ino, contents[i]);
    }
  }
  return failures == 0 ? 0 : 1;
}
