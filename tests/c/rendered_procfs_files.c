/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Guest for hermit-cli/tests/cli.rs: the procfs files Detcore renders without
 * the backend opening them (https://github.com/rrnewton/hermit/issues/4213),
 * /proc/sys/kernel/random/uuid, /proc/sys/kernel/pty/nr, /proc/uptime and
 * /proc/stat. The first two are sysctl files and the others seq_files, which
 * Linux seeks, maps and writes at a position differently (check_seeks,
 * check_descriptor_refusals).
 *
 * Every check is one Linux gives for the real procfs file; most also fail
 * for a memfd standing in for it, which is how a missed spelling shows: the
 * open-flag errors, the metadata and its agreement across spellings
 * (descriptor links of this process, its threads and another process,
 * /dev/stdin, a symbolic link to a descriptor link, a directory listing) and
 * with every other procfs file's device, contents, reopening through those
 * spellings, fdinfo, status flags, filesystem type, the calls a 0444 procfs
 * sysctl file refuses by path and by descriptor (access, writes, write
 * locks, leases, O_NOATIME and O_DIRECT, sendfile into it, mappings, fsync,
 * chmod, truncate, times, extended attributes, file ioctls, seals), the
 * descriptor's survival across exec, and nonexistence inside a chroot, made
 * by the process or by one of its threads. The program prints one line per
 * file with the identity the guest sees, which must be the same on every
 * backend, and exits 0 only if every check passed.
 *
 * usage: rendered_procfs_files [--no-foreign-fd-links] [--noreplace-overlap-eperm]
 *                              [--kvm-local-procfs]
 *                              [--no-maps-or-chroot] [--chroot-only]
 *                              [--require-chroot] [--xattr-arguments-first]
 *                              [--detcore-xattr-order]
 *        rendered_procfs_files --inherited <fd> <duplicate> <ino> <offset> <kvm>
 *                              <stat-fd> <stat-head> <uptime-fd>
 *
 * The program must be started by its absolute path, which it execs again.
 * Two options skip checks that need procfs files other than the two
 * rendered ones, which a backend does not yet serve faithfully; both gaps
 * close when Detcore renders the per-process files too (a later step of the
 * issue above):
 *   --no-foreign-fd-links skips another process's /proc/<pid>/fd/N and a
 *     thread's /proc/self/task/<tid>/fd/N. Under DBT the IDs Detcore gives
 *     the guest do not name the container's /proc entries.
 *   --kvm-local-procfs also skips the /proc/self/fd listing and the mode of
 *     a descriptor link, a /proc directory listing, and a symbolic link to
 *     the file, which the KVM backend's own procfs refuses or answers
 *     differently, and chroot, which KVM does not support. It also skips
 *     the device agreement with other procfs files, which KVM's procfs
 *     numbers itself.
 *   --no-maps-or-chroot skips /proc/self/maps and chroot, whose strict
 *     comparison DBT cannot yet make for reasons that predate rendered files
 *     (https://github.com/rrnewton/hermit/issues/4232 and
 *     https://github.com/rrnewton/hermit/issues/4233).
 *   --require-chroot fails a chroot check that could not chroot (EPERM),
 *     which a run without privilege otherwise skips; Hermit's container is
 *     privileged.
 *   --xattr-arguments-first checks that an extended-attribute call's name,
 *     size and value are refused before its path is looked up, as Linux 6.13
 *     and later do; an older kernel looks the path up first.
 *   --detcore-xattr-order checks the same order only for the spellings
 *     Detcore resolves itself (a rendered file's name, or a descriptor
 *     number), which Detcore answers in that order on any host.
 *   --chroot-only runs only the process chroot checks, for a run that
 *     cannot verify them (see the previous option); not a thread's chroot,
 *     which hangs under DBT (https://github.com/rrnewton/hermit/issues/4254).
 *   --noreplace-overlap-eperm expects EPERM rather than EEXIST from
 *     MAP_FIXED_NOREPLACE over an existing mapping, as DBT answers it for
 *     every mapping (https://github.com/rrnewton/hermit/issues/4429); the
 *     call is still made and the mapping must still survive.
 * --inherited is the program after it execs itself.
 */

#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/fiemap.h>
#include <linux/fs.h>
#include <linux/stat.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/sendfile.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <sys/xattr.h>
#include <time.h>
#include <unistd.h>

#ifndef SYS_faccessat2
#define SYS_faccessat2 439
#endif

#define PROC_SUPER_MAGIC 0x9fa0

/* Room for a whole /proc/stat in one read, whose size grows with the host's
 * CPUs and interrupt numbers (some 40 KiB on a host of 128 CPUs). */
#define READ_CAPACITY (1 << 20)
static char contents_buffer[READ_CAPACITY];
static char rest_buffer[READ_CAPACITY];

/* How many bytes of /proc/stat the program reads before it execs itself. */
#define STAT_HEAD 8

static int failures;
static int kvm_local_procfs;
static int no_foreign_fd_links;
static int noreplace_overlap_eperm;
/* The checks assume none of the capabilities that bypass the refusals they
 * expect; with --no-bypass-capabilities the guest checks that first. */
static int no_bypass_capabilities;
static int no_maps_or_chroot;
static int chroot_only;
static int require_chroot;
static int xattr_arguments_first;
static int detcore_xattr_order;
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
#define REFUSED(call, expected, what)                                     \
  do {                                                                    \
    errno = 0;                                                            \
    long refused_result = (long)(call);                                   \
    int refused_errno = errno;                                            \
    CHECK(refused_result == -1 && refused_errno == (expected),            \
          "%s: %s gave %ld (%s), not %s", (what), #call, refused_result,  \
          strerror(refused_errno), strerror(expected));                   \
  } while (0)

/* Whether this process lacks every capability that would bypass a refusal
 * the checks expect: CAP_DAC_OVERRIDE (1), CAP_DAC_READ_SEARCH (2),
 * CAP_FOWNER (3), CAP_SYS_RAWIO (17), CAP_SYS_ADMIN (21) and CAP_LEASE (28),
 * as /proc/self/status shows them. */
static int lacks_bypass_capabilities(void) {
  FILE* status = fopen("/proc/self/status", "r");
  char line[256];
  unsigned long long effective = ~0ULL;
  while (status && fgets(line, sizeof(line), status)) {
    if (sscanf(line, "CapEff: %llx", &effective) == 1) {
      break;
    }
  }
  if (status) {
    fclose(status);
  }
  const unsigned long long bypass =
      1ULL << 1 | 1ULL << 2 | 1ULL << 3 | 1ULL << 17 | 1ULL << 21 | 1ULL << 28;
  if (effective & bypass) {
    fprintf(stderr, "FAIL: capabilities %#llx that bypass the checked refusals are effective\n",
            effective & bypass);
    return 0;
  }
  return 1;
}

static int is_uuid_text(const char* text, size_t length) {
  for (size_t i = 0; i < length; i++) {
    if (!strchr("0123456789abcdef-\n", text[i])) {
      return 0;
    }
  }
  return 1;
}

static int is_uuid(const char* text, size_t length) {
  if (length != 37 || text[36] != '\n' || text[14] != '4') {
    return 0;
  }
  for (size_t i = 0; i < 36; i++) {
    int hyphen = i == 8 || i == 13 || i == 18 || i == 23;
    if (hyphen ? text[i] != '-' : !strchr("0123456789abcdef", text[i])) {
      return 0;
    }
  }
  return strchr("89ab", text[19]) != NULL;
}

static int is_count(const char* text, size_t length) {
  if (length < 2 || text[length - 1] != '\n') {
    return 0;
  }
  for (size_t i = 0; i + 1 < length; i++) {
    if (text[i] < '0' || text[i] > '9') {
      return 0;
    }
  }
  return 1;
}

/* "<seconds>.<cc> <idle>.<cc>\n", as /proc/uptime reads. */
static int is_uptime(const char* text, size_t length) {
  if (length < 10 || text[length - 1] != '\n') {
    return 0;
  }
  int fields = 0;
  size_t i = 0;
  while (i + 1 < length) {
    size_t start = i;
    while (i + 1 < length && text[i] >= '0' && text[i] <= '9') {
      i++;
    }
    if (i == start || i + 3 >= length || text[i] != '.' || text[i + 1] < '0' || text[i + 1] > '9' ||
        text[i + 2] < '0' || text[i + 2] > '9') {
      return 0;
    }
    i += 3;
    fields++;
    if (fields == 1) {
      if (text[i] != ' ') {
        return 0;
      }
      i++;
    }
  }
  return fields == 2;
}

/* Whether the line at `*at` is `prefix` and then decimal numbers, each after
 * one space, at least `minimum` and, unless `maximum` is 0, at most `maximum`
 * of them, as show_stat prints its lines. Moves `*at` past it. */
static int numbers_line(const char* text, size_t length, size_t* at, const char* prefix,
                        int minimum, int maximum) {
  size_t prefix_length = strlen(prefix);
  size_t i = *at;
  if (length - i < prefix_length || memcmp(text + i, prefix, prefix_length) != 0) {
    return 0;
  }
  i += prefix_length;
  int count = 0;
  for (;;) {
    size_t start = i;
    while (i < length && text[i] >= '0' && text[i] <= '9') {
      i++;
    }
    if (i == start) {
      return 0;
    }
    count++;
    if (i < length && text[i] == ' ') {
      i++;
      continue;
    }
    break;
  }
  if (i >= length || text[i] != '\n' || count < minimum || (maximum != 0 && count > maximum)) {
    return 0;
  }
  *at = i + 1;
  return 1;
}

/* /proc/stat in show_stat's layout: the aggregate line ("cpu" and two
 * spaces), a "cpuN" line per CPU, "intr" with its total and a count per
 * interrupt number, ctxt, btime, processes, procs_running, procs_blocked,
 * and "softirq" with its total and a count for each of the ten softirqs. */
static int is_stat(const char* text, size_t length) {
  size_t at = 0;
  if (!numbers_line(text, length, &at, "cpu  ", 4, 0)) {
    return 0;
  }
  int cpus = 0;
  while (length - at > 3 && memcmp(text + at, "cpu", 3) == 0 && text[at + 3] >= '0' &&
         text[at + 3] <= '9') {
    char prefix[32];
    size_t name = 3;
    while (at + name < length && name < sizeof(prefix) - 2 && text[at + name] >= '0' &&
           text[at + name] <= '9') {
      name++;
    }
    if (at + name >= length || text[at + name] != ' ') {
      return 0;
    }
    memcpy(prefix, text + at, name + 1);
    prefix[name + 1] = '\0';
    if (!numbers_line(text, length, &at, prefix, 4, 0)) {
      return 0;
    }
    cpus++;
  }
  return cpus > 0 && numbers_line(text, length, &at, "intr ", 2, 0) &&
         numbers_line(text, length, &at, "ctxt ", 1, 1) &&
         numbers_line(text, length, &at, "btime ", 1, 1) &&
         numbers_line(text, length, &at, "processes ", 1, 1) &&
         numbers_line(text, length, &at, "procs_running ", 1, 1) &&
         numbers_line(text, length, &at, "procs_blocked ", 1, 1) &&
         numbers_line(text, length, &at, "softirq ", 11, 11) && at == length;
}

/* The kinds of file this program checks. */
enum kind { COUNT, UUID, UPTIME, STAT };

static int has_contents(enum kind kind, const char* text, size_t length) {
  switch (kind) {
    case UUID:
      return is_uuid(text, length);
    case UPTIME:
      return is_uptime(text, length);
    case STAT:
      return is_stat(text, length);
    default:
      return is_count(text, length);
  }
}

/* Whether the kind of file is a seq_file (every procfs file outside
 * /proc/sys) rather than a sysctl file. */
static int is_seq_file(enum kind kind) {
  return kind == UPTIME || kind == STAT;
}

/* Prints `label`, then `text` with each newline but its last shown as \n, so
 * that a file of several lines prints as one. */
static void print_one_line(const char* label, const char* text, ssize_t length) {
  printf("%s", label);
  for (ssize_t i = 0; i < length; i++) {
    if (text[i] == '\n' && i + 1 < length) {
      printf("\\n");
    } else {
      putchar(text[i]);
    }
  }
}

/* Whether two timestamps are the same. */
static int same_time(struct timespec a, struct timespec b) {
  return a.tv_sec == b.tv_sec && a.tv_nsec == b.tv_nsec;
}

/* The times of the open file `fd` of `path`, a seq_file whose first read
 * Detcore may have recorded on the memfd standing in for it, are the file's
 * own through every view, fstat, statx with AT_EMPTY_PATH and a stat of the
 * descriptor link, as a stat of the path shows them, and none is before 1970:
 * nothing of the stand-in shows. `where` names the process for a failure. */
static void check_hidden_times(int fd, const char* path, const char* where) {
  struct stat by_path, by_fd, by_link;
  struct statx by_statx;
  char link[64];
  snprintf(link, sizeof(link), "/proc/self/fd/%d", fd);
  int ok = stat(path, &by_path) == 0 && fstat(fd, &by_fd) == 0 && stat(link, &by_link) == 0 &&
           statx(fd, "", AT_EMPTY_PATH, STATX_BASIC_STATS, &by_statx) == 0;
  CHECK(ok, "%s times %s: %s", path, where, strerror(errno));
  if (!ok) {
    return;
  }
  CHECK(by_path.st_atim.tv_sec >= 0 && by_path.st_mtim.tv_sec >= 0 && by_path.st_ctim.tv_sec >= 0,
        "%s times %s: before 1970 by path", path, where);
  const struct stat* views[] = {&by_fd, &by_link};
  for (size_t i = 0; i < sizeof(views) / sizeof(views[0]); i++) {
    CHECK(same_time(views[i]->st_atim, by_path.st_atim) &&
              same_time(views[i]->st_mtim, by_path.st_mtim) &&
              same_time(views[i]->st_ctim, by_path.st_ctim),
          "%s times %s: %s shows mtime %lld.%09ld, the path %lld.%09ld", path, where,
          i == 0 ? "fstat" : "the descriptor link", (long long)views[i]->st_mtim.tv_sec,
          views[i]->st_mtim.tv_nsec, (long long)by_path.st_mtim.tv_sec, by_path.st_mtim.tv_nsec);
  }
  struct timespec statx_mtime = {by_statx.stx_mtime.tv_sec, by_statx.stx_mtime.tv_nsec};
  struct timespec statx_atime = {by_statx.stx_atime.tv_sec, by_statx.stx_atime.tv_nsec};
  CHECK(same_time(statx_mtime, by_path.st_mtim) && same_time(statx_atime, by_path.st_atim),
        "%s times %s: statx shows mtime %lld.%09ld", path, where, (long long)statx_mtime.tv_sec,
        statx_mtime.tv_nsec);
}

/* A time before 1970 set on the open file `fd` is refused, as for any procfs
 * file the caller does not own, and changes none of its times. */
static void check_times_refused(int fd, const char* path, const char* where) {
  struct timespec before_1970[2] = {{.tv_sec = -5}, {.tv_sec = -5}};
  REFUSED(futimens(fd, before_1970), EPERM, path);
  check_hidden_times(fd, path, where);
}

/* The value of `label` in this process's fdinfo for `fd`, or -1. */
static long long fdinfo_value(int fd, const char* label) {
  char path[64];
  snprintf(path, sizeof(path), "/proc/self/fdinfo/%d", fd);
  FILE* file = fopen(path, "r");
  if (file == NULL) {
    return -1;
  }
  char line[256];
  long long value = -1;
  size_t label_length = strlen(label);
  while (fgets(line, sizeof(line), file) != NULL) {
    if (strncmp(line, label, label_length) == 0 && line[label_length] == ':') {
      value = strtoll(line + label_length + 1, NULL, label[0] == 'f' ? 8 : 10);
    }
  }
  fclose(file);
  return value;
}

static int maps_name_placeholder(void) {
  FILE* file = fopen("/proc/self/maps", "r");
  if (file == NULL) {
    return 1;
  }
  char line[512];
  int found = 0;
  while (fgets(line, sizeof(line), file) != NULL) {
    found |= strstr(line, "hermit-procfs") != NULL;
  }
  fclose(file);
  return found;
}

/* The d_type and d_ino of `name` in directory `path`, or 0 and 0. */
static void listed(const char* path, const char* name, unsigned char* type,
                   unsigned long long* ino) {
  *type = 0;
  *ino = 0;
  DIR* directory = opendir(path);
  if (directory == NULL) {
    return;
  }
  struct dirent* entry;
  while ((entry = readdir(directory)) != NULL) {
    if (strcmp(entry->d_name, name) == 0) {
      *type = entry->d_type;
      *ino = entry->d_ino;
    }
  }
  closedir(directory);
}

static int link_names(const char* link, const char* path) {
  char target[256];
  ssize_t length = readlink(link, target, sizeof(target));
  return length == (ssize_t)strlen(path) && memcmp(target, path, length) == 0;
}

static void check_open_errors(const char* path) {
  char spelling[128];
  struct stat ignored;
  REFUSED(open(path, O_WRONLY), EACCES, path);
  REFUSED(open(path, O_RDWR), EACCES, path);
  REFUSED(open(path, O_RDONLY | O_TRUNC), EACCES, path);
  REFUSED(open(path, O_RDONLY | O_DIRECTORY), ENOTDIR, path);
  REFUSED(open(path, O_RDONLY | O_CREAT | O_EXCL, 0444), EEXIST, path);
  REFUSED(open(path, O_RDONLY | O_CREAT | O_DIRECTORY, 0444), EINVAL, path);
  REFUSED(open(path, O_TMPFILE | O_RDONLY, 0444), EINVAL, path);
  REFUSED(open(path, O_RDONLY | O_NOATIME), EPERM, path);
  snprintf(spelling, sizeof(spelling), "%s/", path);
  REFUSED(open(spelling, O_RDONLY), ENOTDIR, spelling);
  REFUSED(stat(spelling, &ignored), ENOTDIR, spelling);
}

static void check_descriptor_refusals(const char* path, int fd, enum kind kind) {
  /* A seq_file (every procfs file outside /proc/sys) is opened without
   * FMODE_PWRITE and maps through proc_reg_mmap, which has no method behind
   * it; a sysctl file has no mmap method at all. */
  int seq_file = is_seq_file(kind);
  REFUSED(write(fd, "1\n", 2), EBADF, path);
  REFUSED(pwrite(fd, "1\n", 2, 0), seq_file ? ESPIPE : EBADF, path);
  REFUSED(pwrite(fd, "1\n", 2, -1), EINVAL, path);
  REFUSED(ftruncate(fd, 0), EINVAL, path);
  REFUSED(fallocate(fd, 0, 0, 1), EBADF, path);
  struct flock lock = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
  REFUSED(fcntl(fd, F_SETLK, &lock), EBADF, path);
  int source = open(program, O_RDONLY);
  REFUSED(sendfile(fd, source, NULL, 1), EBADF, path);
  close(source);
  REFUSED(mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, fd, 0), seq_file ? EIO : ENODEV, path);
  REFUSED(mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0), EACCES, path);
  REFUSED(mmap(NULL, 0, PROT_READ, MAP_PRIVATE, fd, 0), EINVAL, path);
  /* The checks before the file's own, in do_mmap's order, and what the
   * mapping leaves: MAP_FIXED_NOREPLACE over a mapping is EEXIST and keeps
   * it; a seq_file refuses MAP_GROWSDOWN, and its MAP_FIXED mapping unmaps
   * the range before proc_reg_mmap fails (EIO), where a sysctl file's fails
   * first (ENODEV) and keeps it; a vacant range stays vacant. */
  char* page = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  CHECK(page != MAP_FAILED, "%s anonymous page: %s", path, strerror(errno));
  if (page != MAP_FAILED) {
    page[0] = 'p';
    REFUSED(mmap(page, 4096, PROT_READ, MAP_PRIVATE | MAP_FIXED_NOREPLACE, fd, 0),
            noreplace_overlap_eperm ? EPERM : EEXIST, path);
    CHECK(msync(page, 4096, MS_ASYNC) == 0 && page[0] == 'p',
          "%s MAP_FIXED_NOREPLACE disturbed the mapping it refused", path);
    REFUSED(mmap(NULL, 4096, PROT_READ, MAP_PRIVATE | MAP_GROWSDOWN, fd, 0),
            seq_file ? EINVAL : ENODEV, path);
    /* MAP_SHARED_VALIDATE refuses a flag procfs does not support before any
     * MAP_FIXED unmapping. */
    REFUSED(mmap(page, 4096, PROT_READ, 0x03 /* MAP_SHARED_VALIDATE */ | MAP_FIXED | (int)0x80000000,
                 fd, 0),
            EOPNOTSUPP, path);
    CHECK(msync(page, 4096, MS_ASYNC) == 0 && page[0] == 'p',
          "%s a refused MAP_SHARED_VALIDATE disturbed the mapping", path);
    REFUSED(mmap(page, 4096, PROT_READ, MAP_PRIVATE | MAP_FIXED, fd, 0), seq_file ? EIO : ENODEV,
            path);
    errno = 0;
    int kept = msync(page, 4096, MS_ASYNC) == 0;
    CHECK(seq_file ? !kept && errno == ENOMEM : kept && page[0] == 'p',
          "%s MAP_FIXED left the range %s", path, kept ? "mapped" : "unmapped");
    if (kept) {
      munmap(page, 4096);
    }
    /* The range is free now: a NOREPLACE mapping fails and maps nothing. */
    REFUSED(mmap(page, 4096, PROT_READ, MAP_PRIVATE | MAP_FIXED_NOREPLACE, fd, 0),
            seq_file ? EIO : ENODEV, path);
    errno = 0;
    CHECK(msync(page, 4096, MS_ASYNC) == -1 && errno == ENOMEM,
          "%s MAP_FIXED_NOREPLACE left a mapping", path);
  }
  REFUSED(fsync(fd), EINVAL, path);
  REFUSED(fdatasync(fd), EINVAL, path);
  REFUSED(fchmod(fd, 0644), EPERM, path);
  char value[16];
  REFUSED(fgetxattr(fd, "user.x", value, sizeof(value)), EOPNOTSUPP, path);
  REFUSED(fsetxattr(fd, "user.x", "1", 1, 0), EACCES, path);
  REFUSED(fremovexattr(fd, "user.x"), EACCES, path);
  CHECK(flistxattr(fd, value, sizeof(value)) == 0, "%s flistxattr", path);
  int block_size = 0;
  CHECK(ioctl(fd, FIGETBSZ, &block_size) == 0 && block_size == 1024, "%s FIGETBSZ %d",
        path, block_size);
  long flags;
  REFUSED(ioctl(fd, FS_IOC_GETFLAGS, &flags), ENOTTY, path);
  REFUSED(fcntl(fd, F_GET_SEALS), EINVAL, path);
  REFUSED(fcntl(fd, F_ADD_SEALS, F_SEAL_WRITE), EPERM, path);
  REFUSED(fcntl(fd, F_SETLEASE, F_RDLCK), EACCES, path);
  CHECK(fcntl(fd, F_GETLEASE) == F_UNLCK, "%s F_GETLEASE", path);
  int status = fcntl(fd, F_GETFL);
  REFUSED(fcntl(fd, F_SETFL, status | O_NOATIME), EPERM, path);
  REFUSED(fcntl(fd, F_SETFL, status | O_DIRECT), EINVAL, path);
  CHECK(fcntl(fd, F_GETFL) == status, "%s F_GETFL changed to %#o", path, fcntl(fd, F_GETFL));
  REFUSED(futimens(fd, NULL), EACCES, path);
  struct timespec times[2] = {{.tv_sec = 1}, {.tv_sec = 1}};
  REFUSED(futimens(fd, times), EPERM, path);
  struct timespec omitted[2] = {{.tv_nsec = UTIME_OMIT}, {.tv_nsec = UTIME_OMIT}};
  CHECK(futimens(fd, omitted) == 0, "%s futimens(UTIME_OMIT): %s", path, strerror(errno));
  REFUSED(syscall(SYS_fchmodat2, fd, "", 0644, AT_EMPTY_PATH), EPERM, path);
  REFUSED(faccessat(fd, "", W_OK, AT_EMPTY_PATH), EACCES, path);
  REFUSED(syscall(SYS_utimensat, fd, NULL, NULL, AT_SYMLINK_NOFOLLOW), EINVAL, path);
  REFUSED(fgetxattr(fd, "", value, sizeof(value)), ERANGE, path);
  REFUSED(fgetxattr(fd, "trusted.x", value, sizeof(value)), ENODATA, path);
  REFUSED(fsetxattr(fd, "foo.x", "1", 1, 0), EACCES, path);
  REFUSED(fsetxattr(fd, "system.posix_acl_access", "1", 1, 0), EINVAL, path);
  static const unsigned char acl[28] = {2, 0, 0, 0,   1,   0,   6,   0,   255, 255,
                                        255, 255, 4, 0, 4, 0, 255, 255, 255, 255,
                                        0x20, 0, 4, 0, 255, 255, 255, 255};
  REFUSED(fsetxattr(fd, "system.posix_acl_access", acl, sizeof(acl), 0), EOPNOTSUPP, path);
  REFUSED(fsetxattr(fd, "security.x", "1", 1, 0), EPERM, path);
  REFUSED(fremovexattr(fd, "security.x"), EPERM, path);
  REFUSED(fsetxattr(fd, "system.x", "1", 1, 0), EOPNOTSUPP, path);
  /* The raw call: the C library's prototype forbids the null value its
   * size describes, which is the case under test. */
  REFUSED(syscall(SYS_fsetxattr, fd, "user.x", NULL, 1, 0), EFAULT, path);
  REFUSED(mmap(NULL, 4096, PROT_READ, MAP_PRIVATE | MAP_HUGETLB, fd, 0), EINVAL, path);
  REFUSED(mmap(NULL, 4096, PROT_READ, MAP_SHARED_VALIDATE | MAP_SYNC, fd, 0), EOPNOTSUPP, path);
  int clone_source = open(program, O_RDONLY);
  REFUSED(ioctl(fd, FICLONE, clone_source), EXDEV, path);
  close(clone_source);
  REFUSED(ioctl(fd, FICLONE, 999), EBADF, path);
  struct timespec mixed[2] = {{.tv_nsec = UTIME_NOW}, {.tv_nsec = UTIME_OMIT}};
  REFUSED(futimens(fd, mixed), EPERM, path);
  REFUSED(openat(fd, "", O_RDONLY), ENOENT, path);
  REFUSED(mmap((void*)0x10001, 4096, PROT_READ, MAP_PRIVATE | MAP_FIXED, fd, 0), EINVAL, path);
  REFUSED(mmap(NULL, 1UL << 62, PROT_READ, MAP_PRIVATE, fd, 0), ENOMEM, path);
  static const unsigned char named_acl[44] = {
      2, 0, 0, 0, 1,   0, 6, 0, 255, 255, 255, 255, 2,   0,   4,   0,   255, 255, 255, 255, 4, 0,
      4, 0, 255, 255, 255, 255, 0x10, 0, 4, 0, 255, 255, 255, 255, 0x20, 0, 4, 0, 255, 255, 255, 255};
  REFUSED(fsetxattr(fd, "system.posix_acl_access", named_acl, sizeof(named_acl), 0), EINVAL, path);
  /* Only the parser refuses a list (a partial entry); procfs then cannot keep
   * any list, even an empty one or one without a group entry. */
  REFUSED(fsetxattr(fd, "system.posix_acl_access", acl, 0, 0), EOPNOTSUPP, path);
  REFUSED(fsetxattr(fd, "system.posix_acl_access", acl, 12, 0), EOPNOTSUPP, path);
  REFUSED(fsetxattr(fd, "system.posix_acl_access", acl, 6, 0), EINVAL, path);
  /* file_mmap_ok: an offset past the largest file offset. */
  REFUSED(mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, fd, -4096), EOVERFLOW, path);
  /* A fixed address below vm.mmap_min_addr needs CAP_SYS_RAWIO. */
  REFUSED(mmap((void*)0x1000, 4096, PROT_READ, MAP_PRIVATE | MAP_FIXED, fd, 0), EPERM, path);
  /* With AT_FDCWD a NULL path is looked up, whatever the flags. */
  REFUSED(syscall(SYS_utimensat, AT_FDCWD, NULL, NULL, AT_SYMLINK_NOFOLLOW), EFAULT, path);
  /* Both times omitted is success before the flags are looked at. */
  CHECK(syscall(SYS_utimensat, fd, NULL, omitted, AT_SYMLINK_NOFOLLOW) == 0,
        "%s utimensat(UTIME_OMIT, AT_SYMLINK_NOFOLLOW): %s", path, strerror(errno));
  /* The size less the position, and the bytes used, of a procfs file whose
   * size is 0. */
  int unread = 12345;
  CHECK(ioctl(fd, FIONREAD, &unread) == 0 && unread == -(int)lseek(fd, 0, SEEK_CUR),
        "%s FIONREAD %d at %ld", path, unread, (long)lseek(fd, 0, SEEK_CUR));
  long long used = 777;
  CHECK(ioctl(fd, FIOQSIZE, &used) == 0 && used == 0, "%s FIOQSIZE %lld", path, used);
  struct stat ignored;
  /* Flags newfstatat refuses before it looks up the file. */
  REFUSED(syscall(SYS_newfstatat, AT_FDCWD, path, &ignored, 0x80000000), EINVAL, path);
  /* ...but takes the synchronization flags, and on the descriptor itself
   * looks at no flag (measured on Linux 7.1). */
  CHECK(syscall(SYS_newfstatat, AT_FDCWD, path, &ignored, AT_STATX_SYNC_TYPE) == 0,
        "%s newfstatat(AT_STATX_SYNC_TYPE): %s", path, strerror(errno));
  CHECK(syscall(SYS_newfstatat, fd, "", &ignored, AT_EMPTY_PATH | 0x80000000) == 0,
        "%s newfstatat(\"\", AT_EMPTY_PATH|unknown): %s", path, strerror(errno));
  struct statx by_descriptor;
  CHECK(syscall(SYS_statx, fd, "", AT_EMPTY_PATH | 0x80000000, STATX_BASIC_STATS,
                &by_descriptor) == 0,
        "%s statx(\"\", AT_EMPTY_PATH|unknown): %s", path, strerror(errno));
  /* utimensat: both times omitted is success before the path is looked up;
   * unknown flags are refused before it; out-of-range times after it. */
  char slashed[128];
  snprintf(slashed, sizeof(slashed), "%s/", path);
  CHECK(syscall(SYS_utimensat, AT_FDCWD, slashed, omitted, 0) == 0,
        "%s utimensat(UTIME_OMIT): %s", slashed, strerror(errno));
  REFUSED(syscall(SYS_utimensat, AT_FDCWD, slashed, NULL, 0x8000), EINVAL, slashed);
  struct timespec bad_nsec[2] = {{.tv_nsec = -5}, {.tv_nsec = 0}};
  REFUSED(syscall(SYS_utimensat, AT_FDCWD, slashed, bad_nsec, 0), ENOTDIR, slashed);
  REFUSED(fstatat(fd, "", &ignored, 0), ENOENT, path);
  int block = 0;
  REFUSED(ioctl(fd, FIBMAP, &block), EPERM, path);
  struct {
    struct fiemap map;
    struct fiemap_extent extent;
  } extents = {.map = {.fm_length = ~0ULL, .fm_extent_count = 1}};
  REFUSED(ioctl(fd, FS_IOC_FIEMAP, &extents), EOPNOTSUPP, path);
  REFUSED(ioctl(fd, FIFREEZE, 0), EPERM, path);
  uint64_t hint = 1;
  REFUSED(fcntl(fd, 1036 /* F_SET_RW_HINT */, &hint), EPERM, path);
}

/* Whether Detcore resolves `spelling` itself: its last component is the
 * name of a rendered file this program checks, or a descriptor number
 * (Detcore's `could_name_rendered`). */
static int detcore_resolves(const char* spelling) {
  const char* last = strrchr(spelling, '/');
  last = last ? last + 1 : spelling;
  if (strcmp(last, "uuid") == 0 || strcmp(last, "nr") == 0) {
    return 1;
  }
  if (*last == 0) {
    return 0;
  }
  for (const char* c = last; *c; c++) {
    if (*c < '0' || *c > '9') {
      return 0;
    }
  }
  return 1;
}

/* The calls a 0444 procfs sysctl file answers by path, through `spelling`. */
static void check_path_calls(const char* path, const char* spelling) {
  char value[16];
  CHECK(access(spelling, F_OK) == 0, "%s access(F_OK): %s", spelling, strerror(errno));
  CHECK(access(spelling, R_OK) == 0, "%s access(R_OK): %s", spelling, strerror(errno));
  REFUSED(access(spelling, W_OK), EACCES, spelling);
  REFUSED(access(spelling, X_OK), EACCES, spelling);
  REFUSED(faccessat(AT_FDCWD, spelling, W_OK, AT_EACCESS), EACCES, spelling);
  /* Linux reads mode and flags as int, ignoring the upper bits of the raw
     registers, and refuses invalid ones before it looks at the path or the
     descriptor. */
  CHECK(syscall(SYS_access, spelling, 0x100000004UL) == 0, "%s raw access(R_OK): %s", spelling,
        strerror(errno));
  CHECK(syscall(SYS_faccessat, AT_FDCWD, spelling, 0x100000004UL, 0) == 0,
        "%s raw faccessat(R_OK): %s", spelling, strerror(errno));
  CHECK(syscall(SYS_faccessat2, AT_FDCWD, spelling, 0x100000004UL, 0x100000000UL) == 0,
        "%s raw faccessat2(R_OK): %s", spelling, strerror(errno));
  const char* base = strrchr(path, '/') + 1;
  REFUSED(syscall(SYS_faccessat, -1, base, 8, 0), EINVAL, spelling);
  REFUSED(syscall(SYS_faccessat2, -1, base, R_OK, 0x10000), EINVAL, spelling);
  REFUSED(getxattr(spelling, "user.x", value, sizeof(value)), EOPNOTSUPP, spelling);
  REFUSED(getxattr(spelling, "trusted.x", value, sizeof(value)), ENODATA, spelling);
  REFUSED(setxattr(spelling, "foo.x", "1", 1, 0), EACCES, spelling);
  CHECK(listxattr(spelling, value, sizeof(value)) == 0, "%s listxattr", spelling);
  REFUSED(setxattr(spelling, "user.x", "1", 1, 0), EACCES, spelling);
  REFUSED(removexattr(spelling, "user.x"), EACCES, spelling);
  /* setxattr flags are an int: an upper register bit is ignored. Invalid
     flags are refused before the name is read; a name is read for at most
     XATTR_NAME_MAX + 1 bytes, so 256 bytes with no NUL before an unmapped
     page are too long, not a fault. */
  /* The deliberately invalid pointer, read through a volatile so the
     compiler's access analysis cannot see it (gcc 15 -Werror rejects a
     constant (void*)1 passed with a nonzero size). The kernel still
     receives exactly address 1. */
  char* volatile bad_ptr = (char*)1;
  REFUSED(syscall(SYS_setxattr, spelling, "user.x", "1", 1, 0x100000000UL), EACCES, spelling);
  REFUSED(syscall(SYS_setxattr, spelling, bad_ptr, "1", 1, 4), EINVAL, spelling);
  if (xattr_arguments_first || (detcore_xattr_order && detcore_resolves(spelling))) {
    /* The name, the size and the value are checked before the path is
       looked up, so a path error (a trailing slash: ENOTDIR) cannot hide
       them. */
    char slashed_spelling[512];
    snprintf(slashed_spelling, sizeof(slashed_spelling), "%s/", spelling);
    REFUSED(setxattr(slashed_spelling, "user.x", bad_ptr, 65537, 0), E2BIG, spelling);
    REFUSED(setxattr(slashed_spelling, "user.x", bad_ptr, 1, 0), EFAULT, spelling);
    REFUSED(getxattr(slashed_spelling, bad_ptr, value, sizeof(value)), EFAULT, spelling);
    REFUSED(removexattr(slashed_spelling, ""), ERANGE, spelling);
  }
  {
    long page = sysconf(_SC_PAGESIZE);
    char* pages = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(pages != MAP_FAILED, "mmap name pages: %s", strerror(errno));
    CHECK(munmap(pages + page, page) == 0, "munmap name page: %s", strerror(errno));
    char* long_name = pages + page - 256;
    memset(long_name, 'u', 256);
    REFUSED(getxattr(spelling, long_name, value, sizeof(value)), ERANGE, spelling);
    REFUSED(setxattr(spelling, long_name, "1", 1, 0), ERANGE, spelling);
    REFUSED(removexattr(spelling, long_name), ERANGE, spelling);
    /* A short value ending just before the unmapped page is readable: the
       refusal is the file's (EACCES), not a fault. */
    char* short_value = pages + page - 6;
    memcpy(short_value, "abc", 3);
    REFUSED(setxattr(spelling, "user.x", short_value, 3, 0), EACCES, spelling);
    /* A whole name, or a short value, on a page the guest may not read is a
       fault, even though the page is mapped. */
    char* protected_bytes = pages + 128;
    memcpy(protected_bytes, "user.x", sizeof("user.x"));
    CHECK(mprotect(pages, page, PROT_NONE) == 0, "mprotect name page: %s", strerror(errno));
    REFUSED(getxattr(spelling, protected_bytes, value, sizeof(value)), EFAULT, spelling);
    REFUSED(setxattr(spelling, "user.x", protected_bytes, 3, 0), EFAULT, spelling);
    CHECK(munmap(pages, page) == 0, "munmap name pages: %s", strerror(errno));
  }
  REFUSED(chmod(spelling, 0644), EPERM, spelling);
  struct timeval bad_times[2] = {{.tv_usec = 2000000}, {0}};
  REFUSED(syscall(SYS_utimes, spelling, bad_times), EINVAL, spelling);
  REFUSED(truncate(spelling, 100), EACCES, spelling);
  REFUSED(utimensat(AT_FDCWD, spelling, NULL, 0), EACCES, spelling);
  struct stat by_spelling;
  struct stat by_path;
  CHECK(stat(spelling, &by_spelling) == 0 && stat(path, &by_path) == 0 &&
            by_spelling.st_ino == by_path.st_ino && by_spelling.st_dev == by_path.st_dev &&
            by_spelling.st_size == 0,
        "%s identity or size changed", spelling);
}

/* Seeking a sysctl file: a file of size 0 (`default_llseek`), so SEEK_END
 * is the offset alone and SEEK_DATA finds nothing; an O_PATH descriptor has
 * no position. */
static void check_seeks(const char* path, enum kind kind) {
  int fd = open(path, O_RDONLY);
  char contents[64];
  if (is_seq_file(kind)) {
    /* seq_lseek seeks only from the start or the position. */
    CHECK(fd >= 0, "open(%s): %s", path, strerror(errno));
    REFUSED(lseek(fd, 0, SEEK_END), EINVAL, path);
    REFUSED(lseek(fd, 0, SEEK_DATA), EINVAL, path);
    REFUSED(lseek(fd, 0, SEEK_HOLE), EINVAL, path);
    CHECK(read(fd, contents, sizeof(contents)) > 0, "%s read after the refused seeks", path);
    REFUSED(lseek(fd, 0, SEEK_END), EINVAL, path);
  } else {
    CHECK(fd >= 0 && lseek(fd, 0, SEEK_END) == 0, "%s SEEK_END %ld", path,
          (long)lseek(fd, 0, SEEK_CUR));
    CHECK(read(fd, contents, sizeof(contents)) > 0, "%s read after SEEK_END", path);
    REFUSED(lseek(fd, 0, SEEK_DATA), ENXIO, path);
    REFUSED(lseek(fd, 0, SEEK_HOLE), ENXIO, path);
  }
  REFUSED(lseek(fd, -1, SEEK_SET), EINVAL, path);
  close(fd);
  int located = open(path, O_PATH);
  REFUSED(lseek(located, -1, SEEK_SET), EBADF, path);
  REFUSED(lseek(located, 0, SEEK_SET), EBADF, path);
  close(located);
}

static void check_file(const char* path, enum kind kind) {
  struct stat by_path;
  CHECK(stat(path, &by_path) == 0, "stat(%s): %s", path, strerror(errno));
  CHECK(S_ISREG(by_path.st_mode) && (by_path.st_mode & 07777) == 0444, "%s mode %o", path,
        by_path.st_mode);
  CHECK(by_path.st_size == 0 && by_path.st_nlink == 1, "%s size %lld nlink %lld", path,
        (long long)by_path.st_size, (long long)by_path.st_nlink);
  check_open_errors(path);
  check_seeks(path, kind);
  check_path_calls(path, path);
  if (!kvm_local_procfs) {
    /* One procfs device, Detcore's, for every procfs file. */
    struct stat other;
    CHECK(stat("/proc", &other) == 0 && other.st_dev == by_path.st_dev,
          "%s device differs from /proc's", path);
    CHECK(stat("/proc/sys/kernel/hostname", &other) == 0 && other.st_dev == by_path.st_dev,
          "%s device differs from /proc/sys/kernel/hostname's", path);
  }

  int fd = open(path, O_RDONLY | O_APPEND);
  CHECK(fd >= 0, "open(%s): %s", path, strerror(errno));
  if (fd < 0) {
    return;
  }
  struct stat by_fd;
  CHECK(fstat(fd, &by_fd) == 0, "fstat(%s): %s", path, strerror(errno));
  CHECK(by_fd.st_dev == by_path.st_dev && by_fd.st_ino == by_path.st_ino &&
            by_fd.st_uid == by_path.st_uid,
        "%s fstat identity differs from stat", path);
  int status = fcntl(fd, F_GETFL);
  CHECK((status & O_ACCMODE) == O_RDONLY && (status & O_APPEND), "%s F_GETFL %#o", path,
        status);

  char self_link[64];
  snprintf(self_link, sizeof(self_link), "/proc/self/fd/%d", fd);
  CHECK(link_names(self_link, path), "%s descriptor link", path);
  check_path_calls(path, self_link);
  {
    /* procfs names descriptors without leading zeros. */
    char zero_link[80];
    snprintf(zero_link, sizeof(zero_link), "/proc/self/fd/0%d", fd);
    REFUSED(open(zero_link, O_RDONLY), ENOENT, zero_link);
    snprintf(zero_link, sizeof(zero_link), "/dev/fd/0%d", fd);
    REFUSED(open(zero_link, O_RDONLY), ENOENT, zero_link);
    /* Not even a creating open finds it (measured on Linux 7.1). */
    snprintf(zero_link, sizeof(zero_link), "/proc/self/fd/0%d", fd);
    REFUSED(open(zero_link, O_RDONLY | O_CREAT, 0444), ENOENT, zero_link);
  }
  if (!kvm_local_procfs) {
    /* The link read through a symbolic link to the descriptor directory, into
     * a buffer shorter than the link: the start of the file's path. (KVM's
     * own /proc refuses symbolic links into /proc until it is removed,
     * https://github.com/rrnewton/hermit/issues/4282.) */
    char directory[] = "/tmp/rendered-procfs-fd-XXXXXX";
    CHECK(mkdtemp(directory) != NULL, "mkdtemp: %s", strerror(errno));
    char directory_link[96], through[128], target[20];
    snprintf(directory_link, sizeof(directory_link), "%s/fd", directory);
    CHECK(symlink("/proc/self/fd", directory_link) == 0, "symlink: %s", strerror(errno));
    snprintf(through, sizeof(through), "%s/%d", directory_link, fd);
    ssize_t short_length = readlink(through, target, sizeof(target));
    /* A path shorter than the buffer, as /proc/uptime, is read whole. */
    size_t expected = strlen(path) < sizeof(target) ? strlen(path) : sizeof(target);
    CHECK(short_length == (ssize_t)expected && memcmp(target, path, expected) == 0,
          "%s read into %zu bytes: %.*s", through, sizeof(target),
          (int)(short_length > 0 ? short_length : 0), target);
    unlink(directory_link);
    rmdir(directory);
  }
  struct statx extended;
  REFUSED(syscall(SYS_statx, AT_FDCWD, path, 0, 0x80000000u, &extended), EINVAL, path);
  /* `O_CREAT|O_EXCL` follows no final link: the link exists. */
  REFUSED(open(self_link, O_RDONLY | O_CREAT | O_EXCL | O_NOFOLLOW, 0444), EEXIST, self_link);
  REFUSED(syscall(SYS_openat, AT_FDCWD, self_link, 020000000 | O_RDWR), EINVAL, self_link);
  char link_dir[80];
  struct stat link_ignored;
  snprintf(link_dir, sizeof(link_dir), "%s/", self_link);
  REFUSED(open(link_dir, O_RDONLY), ENOTDIR, link_dir);
  REFUSED(stat(link_dir, &link_ignored), ENOTDIR, link_dir);
  /* statx refuses its arguments before it looks up the path. */
  REFUSED(syscall(SYS_statx, AT_FDCWD, link_dir, 0, 0x80000000u, &extended), EINVAL, link_dir);
  snprintf(link_dir, sizeof(link_dir), "%s/.", self_link);
  REFUSED(open(link_dir, O_RDONLY), ENOTDIR, link_dir);
  /* This thread's spelling of the link: the same file, and the same link
   * even into a buffer shorter than anything but the path. The thread is
   * named as procfs names it (/proc/thread-self), which is what a /proc
   * reader sees. */
  if (!no_foreign_fd_links) {
    char thread_self[64];
    ssize_t named = readlink("/proc/thread-self", thread_self, sizeof(thread_self) - 1);
    thread_self[named < 0 ? 0 : named] = '\0';
    char task_link[160];
    snprintf(task_link, sizeof(task_link), "/proc/%s/fd/%d", thread_self, fd);
    struct stat by_task;
    CHECK(stat(task_link, &by_task) == 0 && by_task.st_ino == by_path.st_ino &&
              by_task.st_dev == by_path.st_dev,
          "%s task link identity", path);
    char short_target[40];
    memset(short_target, 'x', sizeof(short_target));
    ssize_t short_length = readlink(task_link, short_target, sizeof(short_target));
    CHECK(short_length == (ssize_t)strlen(path) && memcmp(short_target, path, short_length) == 0 &&
              short_target[short_length] == 'x',
          "%s task link read into %zu bytes: %zd", path, sizeof(short_target), short_length);
  }
  if (!kvm_local_procfs) {
    struct stat link_stat;
    CHECK(lstat(self_link, &link_stat) == 0 && (link_stat.st_mode & 0777) == 0500,
          "%s descriptor link mode %o", path, link_stat.st_mode);
    unsigned char type;
    unsigned long long ino;
    listed("/proc/self/fd", strrchr(self_link, '/') + 1, &type, &ino);
    CHECK(type == DT_LNK, "%s descriptor is not listed as a link", path);
  }

  char* contents = contents_buffer;
  ssize_t length = read(fd, contents, READ_CAPACITY);
  CHECK(length > 0 && has_contents(kind, contents, length),
        "%s contents %.*s", path, (int)(length > 0 ? length : 0), contents);
  char* rest = rest_buffer;
  CHECK(read(fd, rest, READ_CAPACITY) == 0, "%s second read is not at end", path);
  if (is_seq_file(kind)) {
    /* Its first read took the snapshot; the times are still the file's, in
     * this process and in a child before it reads. */
    check_hidden_times(fd, path, "after the first read");
    fflush(stdout);
    pid_t viewer = fork();
    if (viewer == 0) {
      int inherited_failures = failures;
      check_times_refused(fd, path, "in a child before it reads");
      _exit(failures == inherited_failures ? 0 : 1);
    }
    int viewer_status = 0;
    waitpid(viewer, &viewer_status, 0);
    CHECK(WIFEXITED(viewer_status) && WEXITSTATUS(viewer_status) == 0,
          "%s times in a child before it reads", path);
  }
  CHECK(fdinfo_value(fd, "pos") == length, "%s fdinfo pos %lld", path,
        fdinfo_value(fd, "pos"));
  long long fdinfo_flags = fdinfo_value(fd, "flags");
  CHECK((fdinfo_flags & O_ACCMODE) == O_RDONLY && (fdinfo_flags & O_APPEND),
        "%s fdinfo flags %llo", path, fdinfo_flags);
  CHECK(fdinfo_value(fd, "ino") == (long long)by_fd.st_ino, "%s fdinfo ino %lld", path,
        fdinfo_value(fd, "ino"));
  CHECK(fdinfo_value(fd, "mnt_id") > 0, "%s fdinfo mnt_id %lld", path,
        fdinfo_value(fd, "mnt_id"));

  /* Linux renders the file again for every read from its start. */
  ssize_t preread = pread(fd, rest, READ_CAPACITY, 0);
  CHECK(preread > 0 && has_contents(kind, rest, preread),
        "%s pread contents %.*s", path, (int)(preread > 0 ? preread : 0), rest);

  check_descriptor_refusals(path, fd, kind);
  if (!no_maps_or_chroot) {
    CHECK(!maps_name_placeholder(), "/proc/self/maps names a placeholder");
  }

  struct statfs filesystem;
  CHECK(fstatfs(fd, &filesystem) == 0 && filesystem.f_type == PROC_SUPER_MAGIC,
        "%s fstatfs type %#lx", path, (long)filesystem.f_type);
  CHECK(statfs(path, &filesystem) == 0 && filesystem.f_type == PROC_SUPER_MAGIC,
        "%s statfs type %#lx", path, (long)filesystem.f_type);

  /* Reopening through the descriptor link opens the file again. */
  REFUSED(open(self_link, O_RDONLY | O_NOFOLLOW), ELOOP, self_link);
  int reopened = open(self_link, O_RDONLY);
  CHECK(reopened >= 0, "reopen %s: %s", self_link, strerror(errno));
  if (reopened >= 0) {
    struct stat by_reopened;
    CHECK(fstat(reopened, &by_reopened) == 0 && by_reopened.st_ino == by_path.st_ino,
          "%s reopened identity differs", path);
    ssize_t reread = read(reopened, rest, READ_CAPACITY);
    CHECK(reread > 0 && (has_contents(kind, rest, reread) &&
                              (kind != UUID || memcmp(rest, contents, 36) != 0)),
          "%s reopened contents %.*s", path, (int)(reread > 0 ? reread : 0), rest);
    close(reopened);
  }

  /* An O_PATH descriptor names the file and reads nothing. */
  int located = open(path, O_PATH);
  CHECK(located >= 0, "O_PATH open %s: %s", path, strerror(errno));
  if (located >= 0) {
    struct stat by_located;
    CHECK(fstat(located, &by_located) == 0 && by_located.st_ino == by_path.st_ino &&
              by_located.st_dev == by_path.st_dev,
          "%s O_PATH identity differs", path);
    REFUSED(read(located, rest, READ_CAPACITY), EBADF, path);
    char located_link[64];
    snprintf(located_link, sizeof(located_link), "/proc/self/fd/%d", located);
    CHECK(link_names(located_link, path), "%s O_PATH descriptor link", path);
    close(located);
  }

  if (!kvm_local_procfs) {
    /* The directory listing and a symbolic link name the same inode. */
    char directory[128];
    snprintf(directory, sizeof(directory), "%s", path);
    *strrchr(directory, '/') = '\0';
    unsigned char type;
    unsigned long long ino;
    listed(directory, strrchr(path, '/') + 1, &type, &ino);
    CHECK(ino == (unsigned long long)by_path.st_ino, "%s listed with inode %llu", path, ino);
    char alias[] = "/tmp/rendered-procfs-alias-XXXXXX";
    CHECK(mkdtemp(alias) != NULL, "mkdtemp: %s", strerror(errno));
    char alias_link[160];
    snprintf(alias_link, sizeof(alias_link), "%s/link", alias);
    CHECK(symlink(path, alias_link) == 0, "symlink: %s", strerror(errno));
    struct stat by_alias;
    CHECK(stat(alias_link, &by_alias) == 0 && by_alias.st_ino == by_path.st_ino &&
              by_alias.st_dev == by_path.st_dev,
          "%s symbolic link identity %llu", path, (unsigned long long)by_alias.st_ino);
    /* A ".." is resolved by walking the components before it. */
    struct stat ignored;
    REFUSED(stat("/proc/thread-self/../sys/kernel/random/uuid", &ignored), ENOENT, path);
    char through_file[160];
    snprintf(through_file, sizeof(through_file), "%s/../%s", path, strrchr(path, '/') + 1);
    REFUSED(stat(through_file, &ignored), ENOTDIR, through_file);
    REFUSED(open(through_file, O_RDONLY), ENOTDIR, through_file);

    /* A symbolic link to the descriptor link opens the file again. */
    unlink(alias_link);
    CHECK(symlink(self_link, alias_link) == 0, "symlink: %s", strerror(errno));
    int through_alias = open(alias_link, O_RDONLY);
    char* aliased = rest;
    ssize_t aliased_length = through_alias >= 0 ? read(through_alias, aliased, READ_CAPACITY) : -1;
    CHECK(aliased_length > 0 && has_contents(kind, aliased, aliased_length),
          "%s through a link to its descriptor link: %zd", path, aliased_length);
    if (through_alias >= 0) {
      close(through_alias);
    }
    REFUSED(open(alias_link, O_RDWR), EACCES, alias_link);
    check_path_calls(path, alias_link);
    int located_alias = open(alias_link, O_PATH);
    CHECK(located_alias >= 0, "O_PATH open %s: %s", alias_link, strerror(errno));
    if (located_alias >= 0) {
      REFUSED(faccessat(located_alias, "", W_OK, AT_EMPTY_PATH), EACCES, alias_link);
      REFUSED(syscall(SYS_fchmodat2, located_alias, "", 0644, AT_EMPTY_PATH), EPERM, alias_link);
      REFUSED(syscall(SYS_utimensat, located_alias, NULL, NULL, 0), EBADF, alias_link);
      /* The flags are checked before the descriptor, the times after it;
       * both omitted is success before anything. */
      struct timespec bad_times[2] = {{.tv_nsec = -5}, {.tv_nsec = 0}};
      REFUSED(syscall(SYS_utimensat, located_alias, NULL, bad_times, 0), EBADF, alias_link);
      REFUSED(syscall(SYS_utimensat, located_alias, NULL, bad_times, AT_SYMLINK_NOFOLLOW), EINVAL,
              alias_link);
      struct timespec both_omitted[2] = {{.tv_nsec = UTIME_OMIT}, {.tv_nsec = UTIME_OMIT}};
      CHECK(syscall(SYS_utimensat, located_alias, NULL, both_omitted, 0) == 0,
            "%s O_PATH utimensat(UTIME_OMIT): %s", alias_link, strerror(errno));
      close(located_alias);
    }
    CHECK(stat(alias_link, &by_alias) == 0 && by_alias.st_ino == by_path.st_ino &&
              by_alias.st_dev == by_path.st_dev,
          "%s link to its descriptor link identity %llu", path,
          (unsigned long long)by_alias.st_ino);
    unlink(alias_link);
    rmdir(alias);

    /* /dev/stdin, when the file is standard input, opens it again. */
    pid_t reader = fork();
    if (reader == 0) {
      dup2(fd, 0);
      int again = open("/dev/stdin", O_RDONLY);
      char* text = rest;
      ssize_t again_length = again >= 0 ? pread(again, text, READ_CAPACITY, 0) : -1;
      struct stat by_stdin;
      struct statfs stdin_filesystem;
      int ok = again_length > 0 &&
               has_contents(kind, text, again_length) &&
               stat("/dev/stdin", &by_stdin) == 0 && by_stdin.st_ino == by_path.st_ino &&
               statfs("/dev/stdin", &stdin_filesystem) == 0 &&
               stdin_filesystem.f_type == PROC_SUPER_MAGIC &&
               open("/dev/stdin", O_RDWR) == -1 && errno == EACCES;
      _exit(ok ? 0 : 1);
    }
    int reader_status = 0;
    waitpid(reader, &reader_status, 0);
    CHECK(WIFEXITED(reader_status) && WEXITSTATUS(reader_status) == 0,
          "%s reopened as /dev/stdin", path);
  }
  if (!no_foreign_fd_links) {
    /* Another process's descriptor link names the file, and reopens it. The
     * process is named as procfs names it (/proc/self). */
    char parent[32];
    ssize_t named = readlink("/proc/self", parent, sizeof(parent) - 1);
    parent[named < 0 ? 0 : named] = '\0';
    pid_t child = fork();
    if (child == 0) {
      char other_link[96];
      snprintf(other_link, sizeof(other_link), "/proc/%s/fd/%d", parent, fd);
      /* Without a rendered descriptor of its own. */
      close(fd);
      errno = 0;
      int ok = access(other_link, W_OK) == -1 && errno == EACCES;
      ok = ok && link_names(other_link, path);
      int other = open(other_link, O_RDONLY);
      struct stat by_other;
      ok = ok && other >= 0 && fstat(other, &by_other) == 0 &&
           by_other.st_ino == by_path.st_ino;
      _exit(ok ? 0 : 1);
    }
    int child_status = 0;
    waitpid(child, &child_status, 0);
    CHECK(WIFEXITED(child_status) && WEXITSTATUS(child_status) == 0,
          "%s another process's descriptor link", path);
  }
  close(fd);
  printf("%s dev=%#llx ino=%llu uid=%u\n", path, (unsigned long long)by_path.st_dev,
         (unsigned long long)by_path.st_ino, (unsigned)by_path.st_uid);
}

/* Inside a chroot without /proc, neither file exists. Only a privileged
 * process can chroot; an unprivileged native run skips this. */
static void check_chroot(void) {
  char root[] = "/tmp/rendered-procfs-root-XXXXXX";
  CHECK(mkdtemp(root) != NULL, "mkdtemp: %s", strerror(errno));
  pid_t child = fork();
  if (child == 0) {
    int before = open("/proc/sys/kernel/random/uuid", O_RDONLY);
    if (chroot(root) != 0) {
      _exit(errno == EPERM ? 2 : 3);
    }
    /* A descriptor opened before is still the file. */
    struct stat by_before;
    struct statfs filesystem;
    char text[64];
    if (before < 0 || fstat(before, &by_before) != 0 || !S_ISREG(by_before.st_mode) ||
        fstatfs(before, &filesystem) != 0 || filesystem.f_type != PROC_SUPER_MAGIC ||
        read(before, text, sizeof(text)) != 37) {
      _exit(4);
    }
    errno = 0;
    if (faccessat(before, "", W_OK, AT_EMPTY_PATH) != -1 || errno != EACCES ||
        futimens(before, NULL) != -1 || errno != EACCES ||
        syscall(SYS_fchmodat2, before, "", 0644, AT_EMPTY_PATH) != -1 || errno != EPERM) {
      _exit(5);
    }
    struct stat ignored;
    int ok = open("/proc/sys/kernel/random/uuid", O_RDONLY) == -1 && errno == ENOENT;
    ok = ok && stat("/proc/sys/kernel/pty/nr", &ignored) == -1 && errno == ENOENT;
    _exit(ok ? 0 : 1);
  }
  int status = 0;
  waitpid(child, &status, 0);
  CHECK(WIFEXITED(status) &&
            (WEXITSTATUS(status) == 0 || (WEXITSTATUS(status) == 2 && !require_chroot)),
        "a chroot check failed (status %d)", status);
  rmdir(root);
}

static void* chroot_thread(void* root) {
  return (void*)(long)(chroot((const char*)root) == 0 ? 0 : errno);
}

/* A thread's chroot changes the root of every thread of its process, which
 * shares it: afterwards, neither file exists for the thread that did not
 * call chroot either. */
static void check_thread_chroot(void) {
  char root[] = "/tmp/rendered-procfs-thread-root-XXXXXX";
  CHECK(mkdtemp(root) != NULL, "mkdtemp: %s", strerror(errno));
  pid_t child = fork();
  if (child == 0) {
    pthread_t thread;
    void* result = NULL;
    if (pthread_create(&thread, NULL, chroot_thread, root) != 0 ||
        pthread_join(thread, &result) != 0) {
      _exit(3);
    }
    if (result != NULL) {
      _exit((long)result == EPERM ? 2 : 3);
    }
    struct stat ignored;
    int ok = open("/proc/sys/kernel/random/uuid", O_RDONLY) == -1 && errno == ENOENT;
    ok = ok && stat("/proc/sys/kernel/pty/nr", &ignored) == -1 && errno == ENOENT;
    ok = ok && access("/proc/sys/kernel/random/uuid", R_OK) == -1 && errno == ENOENT;
    _exit(ok ? 0 : 1);
  }
  int status = 0;
  waitpid(child, &status, 0);
  CHECK(WIFEXITED(status) &&
            (WEXITSTATUS(status) == 0 || (WEXITSTATUS(status) == 2 && !require_chroot)),
        "the rendered files exist after a thread's chroot (status %d)", status);
  rmdir(root);
}

/* A process that shares the open file across fork moves the one position:
 * the parent continues where its child stopped. Prints both parts. */
static void check_fork_shares_the_position(void) {
  int fd = open("/proc/sys/kernel/random/uuid", O_RDONLY);
  CHECK(fd >= 0, "open for fork: %s", strerror(errno));
  fflush(stdout);
  pid_t child = fork();
  if (child == 0) {
    char head[10];
    ssize_t length = read(fd, head, sizeof(head));
    printf("fork child: %.*s\n", (int)(length > 0 ? length : 0), head);
    fflush(stdout);
    _exit(length == (ssize_t)sizeof(head) ? 0 : 1);
  }
  int status = 0;
  waitpid(child, &status, 0);
  CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0, "fork child read");
  /* What the parent sees of the shared position before it reads or seeks:
   * FIONREAD (the size, 0, less the position) and its fdinfo. */
  int unread = 12345;
  CHECK(ioctl(fd, FIONREAD, &unread) == 0 && unread == -10, "FIONREAD after the child %d",
        unread);
  char fdinfo_path[64];
  snprintf(fdinfo_path, sizeof(fdinfo_path), "/proc/self/fdinfo/%d", fd);
  FILE* fdinfo = fopen(fdinfo_path, "r");
  long position = -1;
  CHECK(fdinfo != NULL && fscanf(fdinfo, "pos: %ld", &position) == 1 && position == 10,
        "fdinfo position after the child %ld", position);
  if (fdinfo) {
    fclose(fdinfo);
  }
  CHECK(lseek(fd, 0, SEEK_CUR) == 10, "position after the child's read %ld",
        (long)lseek(fd, 0, SEEK_CUR));
  char rest[64];
  ssize_t length = read(fd, rest, sizeof(rest));
  CHECK(length == 27 && is_uuid_text(rest, length), "parent read after fork %zd", length);
  /* The rest ends with the file's newline. */
  printf("fork parent: %.*s", (int)(length > 0 ? length : 0), rest);
  char whole[64];
  ssize_t whole_length = pread(fd, whole, sizeof(whole), 0);
  printf("fork whole: %.*s", (int)(whole_length > 0 ? whole_length : 0), whole);
  close(fd);
}

/* Print the file's contents, as one read returns them, on one line. */
static void print_contents(const char* path) {
  char* contents = contents_buffer;
  int fd = open(path, O_RDONLY);
  ssize_t length = fd < 0 ? -1 : read(fd, contents, READ_CAPACITY);
  CHECK(length > 0 && contents[length - 1] == '\n', "%s contents", path);
  if (fd >= 0) {
    close(fd);
  }
  char label[96];
  snprintf(label, sizeof(label), "%s: ", path);
  print_one_line(label, contents, length > 0 ? length : 0);
}

/* After exec: the inherited descriptor and its duplicate are still the file,
 * one open file at its position. Prints what they read, which every backend
 * must agree on. */
static int inherited(int fd, int duplicate, unsigned long long ino, long offset, int stat_fd,
                     const char* stat_head, int uptime_fd) {
  /* Before this image reads them, /proc/stat and /proc/uptime, whose first
   * reads took their snapshots before exec, show the files' own times, and a
   * time set on them is refused. */
  check_times_refused(stat_fd, "/proc/stat", "after exec before it reads");
  check_hidden_times(uptime_fd, "/proc/uptime", "after exec");
  /* Through a symbolic link to its descriptor link first, before anything
   * looks at the descriptor; then by the descriptor link, and through an
   * O_PATH reopen of it. */
  char self_link[64];
  snprintf(self_link, sizeof(self_link), "/proc/self/fd/%d", fd);
  /* (Not on KVM, whose own /proc refuses symbolic links into /proc until it
   * is removed, https://github.com/rrnewton/hermit/issues/4282.) */
  if (!kvm_local_procfs) {
    char alias[] = "/tmp/rendered-procfs-exec-XXXXXX";
    CHECK(mkdtemp(alias) != NULL, "mkdtemp: %s", strerror(errno));
    char alias_link[96];
    snprintf(alias_link, sizeof(alias_link), "%s/link", alias);
    CHECK(symlink(self_link, alias_link) == 0, "symlink: %s", strerror(errno));
    REFUSED(access(alias_link, W_OK), EACCES, alias_link);
    struct stat by_alias;
    CHECK(stat(alias_link, &by_alias) == 0 && by_alias.st_ino == ino && by_alias.st_size == 0,
          "inherited descriptor through a symbolic link %llu",
          (unsigned long long)by_alias.st_ino);
    unlink(alias_link);
    rmdir(alias);
  }
  struct stat by_link;
  CHECK(stat(self_link, &by_link) == 0 && by_link.st_ino == ino && by_link.st_size == 0,
        "inherited descriptor link identity %llu", (unsigned long long)by_link.st_ino);
  struct statx extended;
  CHECK(statx(AT_FDCWD, self_link, 0, STATX_BASIC_STATS, &extended) == 0 &&
            extended.stx_ino == ino,
        "inherited descriptor link statx %llu", (unsigned long long)extended.stx_ino);
  int located = open(self_link, O_PATH | O_CLOEXEC);
  struct stat by_located;
  CHECK(located >= 0 && fstat(located, &by_located) == 0 && by_located.st_ino == ino,
        "inherited descriptor's O_PATH reopen identity %llu",
        (unsigned long long)by_located.st_ino);
  if (located >= 0) {
    close(located);
  }
  struct stat by_fd;
  CHECK(fstat(fd, &by_fd) == 0 && by_fd.st_ino == ino && S_ISREG(by_fd.st_mode) &&
            (by_fd.st_mode & 07777) == 0444,
        "inherited descriptor identity %llu mode %o", (unsigned long long)by_fd.st_ino,
        by_fd.st_mode);
  CHECK(lseek(fd, 0, SEEK_CUR) == offset, "inherited position %ld",
        (long)lseek(fd, 0, SEEK_CUR));
  /* The duplicate shares the position. */
  char next[5];
  CHECK(read(duplicate, next, sizeof(next)) == sizeof(next), "inherited duplicate read");
  CHECK(lseek(fd, 0, SEEK_CUR) == offset + (long)sizeof(next), "shared position %ld",
        (long)lseek(fd, 0, SEEK_CUR));
  char rest[64];
  ssize_t length = read(fd, rest, sizeof(rest));
  CHECK(length == 37 - offset - (long)sizeof(next) && is_uuid_text(next, sizeof(next)) &&
            is_uuid_text(rest, length),
        "inherited read %zd", length);
  REFUSED(write(fd, "1\n", 2), EBADF, "inherited descriptor");
  /* The rest ends with the file's newline. */
  printf("after exec: %.*s%.*s", (int)sizeof(next), next, (int)(length > 0 ? length : 0), rest);
  /* The whole file, read from its start, as Hermit keeps it: one snapshot
   * (Linux formats a new UUID for each read). */
  char whole[64];
  ssize_t whole_length = pread(fd, whole, sizeof(whole), 0);
  printf("exec whole: %.*s", (int)(whole_length > 0 ? whole_length : 0), whole);
  /* /proc/stat, read in part before exec, continues from the snapshot that
   * read took (Linux keeps the seq_file buffer with the open file), even a
   * second later, when a new snapshot would show more time. */
  struct timespec second = {1, 0};
  CHECK(nanosleep(&second, NULL) == 0, "nanosleep: %s", strerror(errno));
  CHECK(lseek(stat_fd, 0, SEEK_CUR) == STAT_HEAD, "inherited /proc/stat position %ld",
        (long)lseek(stat_fd, 0, SEEK_CUR));
  size_t head_length = strlen(stat_head);
  memcpy(contents_buffer, stat_head, head_length);
  ssize_t stat_rest = read(stat_fd, contents_buffer + head_length, READ_CAPACITY - head_length);
  CHECK(head_length == STAT_HEAD && stat_rest > 0 &&
            is_stat(contents_buffer, head_length + (size_t)stat_rest),
        "inherited /proc/stat read %zd: %.*s", stat_rest,
        (int)(stat_rest > 0 ? head_length + (size_t)stat_rest : 0), contents_buffer);
  print_one_line("stat after exec: ", contents_buffer + head_length, stat_rest > 0 ? stat_rest : 0);
  return failures == 0 ? 0 : 1;
}

int main(int argc, char** argv) {
  program = argv[0];
  if (argc == 10 && strcmp(argv[1], "--inherited") == 0) {
    kvm_local_procfs = atoi(argv[6]);
    return inherited(atoi(argv[2]), atoi(argv[3]), strtoull(argv[4], NULL, 10), atol(argv[5]),
                     atoi(argv[7]), argv[8], atoi(argv[9]));
  }
  for (int i = 1; i < argc; i++) {
    if (strcmp(argv[i], "--kvm-local-procfs") == 0) {
      kvm_local_procfs = 1;
      no_foreign_fd_links = 1;
    } else if (strcmp(argv[i], "--noreplace-overlap-eperm") == 0) {
      noreplace_overlap_eperm = 1;
    } else if (strcmp(argv[i], "--no-foreign-fd-links") == 0) {
      no_foreign_fd_links = 1;
    } else if (strcmp(argv[i], "--no-maps-or-chroot") == 0) {
      no_maps_or_chroot = 1;
    } else if (strcmp(argv[i], "--chroot-only") == 0) {
      chroot_only = 1;
    } else if (strcmp(argv[i], "--require-chroot") == 0) {
      require_chroot = 1;
    } else if (strcmp(argv[i], "--xattr-arguments-first") == 0) {
      xattr_arguments_first = 1;
    } else if (strcmp(argv[i], "--detcore-xattr-order") == 0) {
      detcore_xattr_order = 1;
    } else if (strcmp(argv[i], "--no-bypass-capabilities") == 0) {
      no_bypass_capabilities = 1;
    } else {
      program = NULL;
    }
  }
  if (program == NULL || program[0] != '/') {
    fprintf(stderr,
            "usage: /path/to/rendered_procfs_files [--no-foreign-fd-links] "
            "[--kvm-local-procfs] [--no-maps-or-chroot] [--chroot-only] "
            "[--require-chroot] [--no-bypass-capabilities] [--xattr-arguments-first] "
            "[--detcore-xattr-order]\n");
    return 2;
  }
  if (no_bypass_capabilities && !lacks_bypass_capabilities()) {
    return 1;
  }
  if (chroot_only) {
    /* Not the thread chroot, which hangs under DBT
     * (https://github.com/rrnewton/hermit/issues/4254). */
    check_chroot();
    printf("chroot checks %s\n", failures == 0 ? "passed" : "failed");
    return failures == 0 ? 0 : 1;
  }
  /* The workload every backend runs alike, first, so that each draws the
   * same UUIDs: each file's contents, a read shared across fork, and the
   * descriptor that survives exec. Its output must be the same on every
   * backend. */
  print_contents("/proc/sys/kernel/random/uuid");
  print_contents("/proc/sys/kernel/pty/nr");
  print_contents("/proc/stat");
  /* The start of /proc/stat, whose first read takes its snapshot, in the
   * same second as the read above; the rest is read after exec. */
  int stat_fd = open("/proc/stat", O_RDONLY);
  char stat_head[STAT_HEAD + 1] = {0};
  if (stat_fd < 0 || read(stat_fd, stat_head, STAT_HEAD) != STAT_HEAD ||
      memchr(stat_head, '\0', STAT_HEAD) != NULL) {
    fprintf(stderr, "FAIL: cannot prepare the inherited /proc/stat: %s\n", strerror(errno));
    return 1;
  }
  printf("stat before exec: %s\n", stat_head);
  check_hidden_times(stat_fd, "/proc/stat", "after the first read");
  {
    pid_t viewer = fork();
    if (viewer == 0) {
      /* Before this process reads the shared open file: a time set on it is
       * refused, and the read after exec still continues the snapshot. */
      int inherited_failures = failures;
      check_times_refused(stat_fd, "/proc/stat", "in a child before it reads");
      _exit(failures == inherited_failures ? 0 : 1);
    }
    int viewer_status = 0;
    waitpid(viewer, &viewer_status, 0);
    CHECK(WIFEXITED(viewer_status) && WEXITSTATUS(viewer_status) == 0,
          "/proc/stat times in a child before it reads");
  }
  /* /proc/uptime read whole, whose times the image after exec checks. */
  int uptime_fd = open("/proc/uptime", O_RDONLY);
  char uptime_text[64];
  if (uptime_fd < 0 || read(uptime_fd, uptime_text, sizeof(uptime_text)) <= 0) {
    fprintf(stderr, "FAIL: cannot prepare the inherited /proc/uptime: %s\n", strerror(errno));
    return 1;
  }
  check_fork_shares_the_position();
  int fd = open("/proc/sys/kernel/random/uuid", O_RDONLY);
  int duplicate = dup(fd);
  struct stat by_fd;
  char head[10];
  if (fd < 0 || duplicate < 0 || fstat(fd, &by_fd) != 0 ||
      read(fd, head, sizeof(head)) != sizeof(head)) {
    fprintf(stderr, "FAIL: cannot prepare the inherited descriptor: %s\n", strerror(errno));
    return 1;
  }
  printf("before exec: %.*s\n", (int)sizeof(head), head);

  check_file("/proc/sys/kernel/random/uuid", UUID);
  check_file("/proc/sys/kernel/pty/nr", COUNT);
  check_file("/proc/uptime", UPTIME);
  check_file("/proc/stat", STAT);
  if (!kvm_local_procfs && !no_maps_or_chroot) {
    check_chroot();
    check_thread_chroot();
  }
  if (failures != 0) {
    return 1;
  }
  /* The descriptor survives exec, as it is, at its position, with the
   * snapshot it took. */
  fflush(stdout);
  char fd_text[16];
  char duplicate_text[16];
  char ino_text[32];
  char stat_fd_text[16];
  snprintf(stat_fd_text, sizeof(stat_fd_text), "%d", stat_fd);
  char uptime_fd_text[16];
  snprintf(uptime_fd_text, sizeof(uptime_fd_text), "%d", uptime_fd);
  snprintf(fd_text, sizeof(fd_text), "%d", fd);
  snprintf(duplicate_text, sizeof(duplicate_text), "%d", duplicate);
  snprintf(ino_text, sizeof(ino_text), "%llu", (unsigned long long)by_fd.st_ino);
  char* const arguments[] = {(char*)program,
                             "--inherited",
                             fd_text,
                             duplicate_text,
                             ino_text,
                             "10",
                             kvm_local_procfs ? "1" : "0",
                             stat_fd_text,
                             stat_head,
                             uptime_fd_text,
                             NULL};
  execv(program, arguments);
  fprintf(stderr, "FAIL: exec: %s\n", strerror(errno));
  return 1;
}
