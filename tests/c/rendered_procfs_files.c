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
 * usage: rendered_procfs_files [--no-foreign-fd-links] [--no-foreign-fd-access]
 *                              [--noreplace-overlap-eperm]
 *                              [--no-clone-files-process] [--no-fifo-link-open]
 *                              [--kvm-local-procfs]
 *                              [--no-maps-or-chroot] [--chroot-only]
 *                              [--require-chroot] [--xattr-arguments-first]
 *                              [--detcore-xattr-order] [--alias-bytes]
 *        rendered_procfs_files --inherited <fd> <duplicate> <ino> <offset> <kvm>
 *                              <stat-fd> <stat-head> <uptime-fd>
 *        rendered_procfs_files --aliases-in-a-thread <alias-bytes>
 *
 * The program must be started by its absolute path, which it execs again.
 * Every check an option skips, or leaves out by its absence, is named on
 * stderr in a line starting "SKIPPED:", and every answer an option accepts
 * in place of Linux's in a line starting "ALLOWED:", each with the option
 * and the reason; stdout, which must be the same on every backend, is
 * unchanged. Per option, what it skips or accepts:
 *   --no-foreign-fd-links (a backend whose procfs does not yet serve the
 *     per-process files faithfully; closes when Detcore renders them too):
 *     - for each file, this thread's descriptor link /proc/<tid>/fd/N;
 *     - for each file, another process's descriptor link /proc/<pid>/fd/N;
 *     - check_selector_aliases_from_a_fork (a parent's /proc/self reopened
 *       by its fork's child) and _from_a_thread (a thread's
 *       /proc/thread-self reopened by the leader);
 *     - accepts, in check_numeric_own_pid_aliases, ENOENT from a fork's
 *       child's /proc/<pid>/fd/N or /proc/<pid>/task/<tid>/fd/N, or that
 *       spelling opening its parent's descriptor if names below it then do
 *       not exist.
 *   --no-foreign-fd-access: for each file, access(W_OK) of another process's
 *     descriptor link, which under DBT reaches the placeholder's memfd that
 *     Detcore there, in one guest process, does not know as that process's
 *     placeholder.
 *   --no-clone-files-process: check_aliases_in_a_clone_files_process (the
 *     directory aliases from a thread of a process created with CLONE_FILES
 *     and without CLONE_THREAD), a clone DBT refuses, ending the run, and
 *     KVM refuses with EOPNOTSUPP.
 *   --no-fifo-link-open: check_alias_source_replaced_during_a_blocked_open (a
 *     blocking open of a FIFO through /proc/<pid>/fd/N whose descriptor
 *     another thread replaces meanwhile): under DBT and KVM the open waits
 *     in the kernel holding the scheduler turn, so the writer never runs.
 *   --kvm-local-procfs implies the three options before the previous one
 *     (--no-foreign-fd-links, --no-clone-files-process, --no-fifo-link-open)
 *     and skips what the KVM backend's own procfs refuses or answers
 *     differently (https://github.com/rrnewton/hermit/issues/4282):
 *     - for each file, the device agreement with /proc and
 *       /proc/sys/kernel/hostname, which KVM's procfs numbers itself;
 *     - for each file, the descriptor link read through a symbolic link to
 *       /proc/self/fd;
 *     - for each file, the descriptor link's mode and its /proc/self/fd
 *       listing;
 *     - for each file, its directory listing, a symbolic link to it, the
 *       ".." walks through it and /proc/thread-self, a symbolic link to its
 *       descriptor link, and its reopening as /dev/stdin;
 *     - after exec, the inherited descriptor through a symbolic link to its
 *       descriptor link;
 *     - for each directory alias, /proc/self/fd/N/ opened with
 *       O_PATH|O_NOFOLLOW (KVM answers ELOOP where Linux follows the link
 *       for the trailing slash);
 *     - check_split_table_aliases (KVM refuses clone without CLONE_FILES with
 *       EINVAL);
 *     - check_aliases_after_a_thread_exec (KVM refuses an exec from a thread
 *       that does not lead its process);
 *     - the process and thread chroot checks (KVM does not support chroot).
 *   --no-maps-or-chroot: for each file, /proc/self/maps names no
 *     placeholder (https://github.com/rrnewton/hermit/issues/4232), and the
 *     process and thread chroot checks
 *     (https://github.com/rrnewton/hermit/issues/4233), whose strict
 *     comparison DBT cannot yet make for reasons that predate rendered files.
 *   --chroot-only runs only the process chroot checks, for a run that
 *     cannot verify them (see the previous option), and skips every other
 *     check; not a thread's chroot, which hangs under DBT
 *     (https://github.com/rrnewton/hermit/issues/4254).
 *   --noreplace-overlap-eperm accepts EPERM rather than EEXIST from
 *     MAP_FIXED_NOREPLACE over an existing mapping, as DBT answers it for
 *     every mapping (https://github.com/rrnewton/hermit/issues/4429); the
 *     call is still made and the mapping must still survive.
 * Options that add checks, whose absence skips them:
 *   --require-chroot fails a chroot check that could not chroot (EPERM);
 *     without it such a check is skipped. Hermit's container is privileged.
 *   --xattr-arguments-first checks that an extended-attribute call's name,
 *     size and value are refused before its path is looked up, as Linux 6.13
 *     and later do; an older kernel looks the path up first.
 *   --detcore-xattr-order checks the same order only for the spellings
 *     Detcore resolves itself (a rendered file's name, or a descriptor
 *     number), which Detcore answers in that order on any host. Without
 *     either option, the order is skipped for each spelling.
 *   --alias-bytes requires a file opened relative to a /proc descriptor that
 *     was itself opened through a descriptor link to read the bytes /proc's
 *     own name reads at the same moment (check_directory_aliases), which
 *     holds where Detcore renders the files; natively the host's files change
 *     between reads, so only their identity and form are checked. It also
 *     requires the blocked open of check_alias_source_replaced_during_a_
 *     blocked_open to have kept its FIFO, as Detcore's deterministic
 *     schedule ensures.
 * --inherited is the program after it execs itself.
 */

#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/fiemap.h>
#include <linux/fs.h>
#include <linux/stat.h>
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
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
static int no_foreign_fd_access;
static int no_clone_files_process;
static int no_fifo_link_open;
static int noreplace_overlap_eperm;
/* The checks assume none of the capabilities that bypass the refusals they
 * expect; with --no-bypass-capabilities the guest checks that first. */
static int no_bypass_capabilities;
static int no_maps_or_chroot;
static int chroot_only;
static int require_chroot;
static int xattr_arguments_first;
static int detcore_xattr_order;
static int alias_bytes;
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

/* Names a check that `flag` skips, and why, on stderr: stdout is compared
 * across backends, and a skipped check must not pass silently. */
static void skipped(const char* check, const char* flag, const char* reason) {
  fprintf(stderr, "SKIPPED: %s (%s: %s)\n", check, flag, reason);
}

/* skipped(), for a check of one file or spelling. */
static void skipped_for(const char* subject, const char* check, const char* flag,
                        const char* reason) {
  fprintf(stderr, "SKIPPED: %s for %s (%s: %s)\n", check, subject, flag, reason);
}

/* Names a result that `flag` accepts in place of Linux's, and why, on
 * stderr: the call is still made and checked against what it accepts. */
static void allowed_for(const char* subject, const char* what, const char* flag,
                        const char* reason) {
  fprintf(stderr, "ALLOWED: %s for %s (%s: %s)\n", what, subject, flag, reason);
}

/* Whether two stat results name the same file. */
static int same_file(const struct stat* a, const struct stat* b) {
  return a->st_dev == b->st_dev && a->st_ino == b->st_ino;
}

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

/* "<a>.<cc> <b>.<cc> <c>.<cc> <running>/<total> <last pid>\n", as
 * /proc/loadavg reads. */
static int is_loadavg(const char* text, size_t length) {
  size_t i = 0;
  for (int field = 0; field < 5; field++) {
    size_t start = i;
    while (i < length && text[i] >= '0' && text[i] <= '9') {
      i++;
    }
    if (i == start || i >= length) {
      return 0;
    }
    if (field < 3) {
      if (length - i < 4 || text[i] != '.' || text[i + 1] < '0' || text[i + 1] > '9' ||
          text[i + 2] < '0' || text[i + 2] > '9' || text[i + 3] != ' ') {
        return 0;
      }
      i += 4;
    } else if (field == 3) {
      if (text[i] != '/') {
        return 0;
      }
      i++;
      start = i;
      while (i < length && text[i] >= '0' && text[i] <= '9') {
        i++;
      }
      if (i == start || i >= length || text[i] != ' ') {
        return 0;
      }
      i++;
    }
  }
  return i + 1 == length && text[i] == '\n';
}

/* The kinds of file this program checks. */
enum kind { COUNT, UUID, UPTIME, STAT, LOADAVG };

static int has_contents(enum kind kind, const char* text, size_t length) {
  switch (kind) {
    case UUID:
      return is_uuid(text, length);
    case UPTIME:
      return is_uptime(text, length);
    case STAT:
      return is_stat(text, length);
    case LOADAVG:
      return is_loadavg(text, length);
    default:
      return is_count(text, length);
  }
}

/* Whether the kind of file is a seq_file (every procfs file outside
 * /proc/sys) rather than a sysctl file. */
static int is_seq_file(enum kind kind) {
  return kind == UPTIME || kind == STAT || kind == LOADAVG;
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
    if (noreplace_overlap_eperm) {
      allowed_for(path, "EPERM, not EEXIST, from MAP_FIXED_NOREPLACE over a mapping",
                  "--noreplace-overlap-eperm",
                  "DBT answers it for every mapping, https://github.com/rrnewton/hermit/issues/4429");
    }
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
  } else {
    skipped_for(spelling,
                "an extended-attribute call's name, size and value refused before its path",
                "no --xattr-arguments-first",
                "a kernel before Linux 6.13 looks the path up first");
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
  } else {
    skipped_for(path, "the device agreement with /proc and /proc/sys/kernel/hostname",
                "--kvm-local-procfs", "KVM's own procfs numbers its devices itself, "
                "https://github.com/rrnewton/hermit/issues/4282");
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
  } else {
    skipped_for(path, "the descriptor link read through a symbolic link to /proc/self/fd",
                "--kvm-local-procfs", "KVM's own procfs refuses symbolic links into /proc, "
                "https://github.com/rrnewton/hermit/issues/4282");
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
  } else {
    skipped_for(path, "this thread's descriptor link /proc/<tid>/fd/N", "--no-foreign-fd-links",
                "the backend's procfs does not serve it, https://github.com/rrnewton/hermit/issues/4282");
  }
  if (!kvm_local_procfs) {
    struct stat link_stat;
    CHECK(lstat(self_link, &link_stat) == 0 && (link_stat.st_mode & 0777) == 0500,
          "%s descriptor link mode %o", path, link_stat.st_mode);
    unsigned char type;
    unsigned long long ino;
    listed("/proc/self/fd", strrchr(self_link, '/') + 1, &type, &ino);
    CHECK(type == DT_LNK, "%s descriptor is not listed as a link", path);
  } else {
    skipped_for(path, "the descriptor link's mode and its /proc/self/fd listing",
                "--kvm-local-procfs", "KVM's own procfs answers them differently, "
                "https://github.com/rrnewton/hermit/issues/4282");
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
  } else {
    skipped_for(path, "/proc/self/maps names no placeholder", "--no-maps-or-chroot",
                "DBT cannot yet compare a read of /proc/self/maps strictly, "
                "https://github.com/rrnewton/hermit/issues/4232");
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
  } else {
    const char* reason = "KVM's own procfs refuses or answers it differently, "
                         "https://github.com/rrnewton/hermit/issues/4282";
    skipped_for(path, "its directory listing", "--kvm-local-procfs", reason);
    skipped_for(path, "a symbolic link to the file", "--kvm-local-procfs", reason);
    skipped_for(path, "the \"..\" walks through it and /proc/thread-self", "--kvm-local-procfs",
                reason);
    skipped_for(path, "a symbolic link to its descriptor link", "--kvm-local-procfs", reason);
    skipped_for(path, "its reopening as /dev/stdin", "--kvm-local-procfs", reason);
  }
  if (!no_foreign_fd_links) {
    /* Another process's descriptor link names the file, and reopens it. The
     * process is named as procfs names it (/proc/self). */
    if (no_foreign_fd_access) {
      skipped_for(path, "access(W_OK) of another process's descriptor link",
                  "--no-foreign-fd-access",
                  "under DBT it reaches the placeholder's memfd, which Detcore there does not "
                  "know as that process's placeholder");
    }
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
      int ok = no_foreign_fd_access || (access(other_link, W_OK) == -1 && errno == EACCES);
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
  } else {
    skipped_for(path, "another process's descriptor link /proc/<pid>/fd/N",
                "--no-foreign-fd-links",
                "the backend's procfs does not serve it, https://github.com/rrnewton/hermit/issues/4282");
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
  if (WIFEXITED(status) && WEXITSTATUS(status) == 2 && !require_chroot) {
    skipped("the process chroot checks", "no --require-chroot", "chroot failed with EPERM");
  }
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
  if (WIFEXITED(status) && WEXITSTATUS(status) == 2 && !require_chroot) {
    skipped("the thread chroot checks", "no --require-chroot", "chroot failed with EPERM");
  }
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

/* The whole file at `path` as one read returns it into `buffer`, or -1. */
static ssize_t read_whole(const char* path, char* buffer) {
  int fd = open(path, O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    return -1;
  }
  ssize_t length = read(fd, buffer, READ_CAPACITY);
  close(fd);
  return length;
}

/* The checks of one directory alias of /proc for check_directory_aliases:
 * the descriptor `spelling` opens with `flags` is /proc, and `name` opened
 * relative to it is /proc/<name> (`canonical`, whose identity is
 * `by_path`). Returns the directory descriptor, or -1. */
static int check_directory_alias(const char* spelling, int flags, const char* name,
                                 const char* canonical, const struct stat* by_path,
                                 enum kind kind) {
  int directory = open(spelling, flags);
  CHECK(directory >= 0, "open(%s) for %s: %s", spelling, canonical, strerror(errno));
  if (directory < 0) {
    return -1;
  }
  char* before = rest_buffer;
  ssize_t before_length = alias_bytes ? read_whole(canonical, before) : 0;
  if (!alias_bytes && kind != UUID) {
    skipped_for(spelling, "the bytes below it against the canonical name's", "no --alias-bytes",
                "natively the host's files change between reads");
  }
  int fd = openat(directory, name, O_RDONLY | O_CLOEXEC);
  CHECK(fd >= 0, "openat(%s, \"%s\"): %s", spelling, name, strerror(errno));
  if (fd >= 0) {
    struct stat by_fd;
    CHECK(fstat(fd, &by_fd) == 0 && by_fd.st_dev == by_path->st_dev &&
              by_fd.st_ino == by_path->st_ino,
          "openat(%s, \"%s\") identity %#llx:%llu, %s's %#llx:%llu", spelling, name,
          (unsigned long long)by_fd.st_dev, (unsigned long long)by_fd.st_ino, canonical,
          (unsigned long long)by_path->st_dev, (unsigned long long)by_path->st_ino);
    char* aliased = contents_buffer;
    ssize_t length = read(fd, aliased, READ_CAPACITY);
    CHECK(length > 0 && has_contents(kind, aliased, length), "openat(%s, \"%s\") contents %.*s",
          spelling, name, (int)(length > 0 ? length : 0), aliased);
    close(fd);
    if (alias_bytes && kind != UUID && length > 0) {
      int same = before_length == length && memcmp(before, aliased, length) == 0;
      if (!same) {
        char* after = rest_buffer;
        ssize_t after_length = read_whole(canonical, after);
        same = after_length == length && memcmp(after, aliased, length) == 0;
      }
      CHECK(same, "openat(%s, \"%s\") read other bytes than %s: %.*s", spelling, name,
            canonical, (int)length, aliased);
    }
  }
  return directory;
}

/* A file below /proc opened relative to a descriptor for /proc that was
 * itself opened through a descriptor link (/proc/self/fd/N or /dev/fd/N of a
 * /proc descriptor, or such a link of such a descriptor) is the file below
 * /proc: Linux follows the link to the directory, and the name is looked up
 * there. So it has the identity and the contents of /proc/<name>, on every
 * backend. With --alias-bytes, where Detcore renders the file as a function
 * of virtual time, the bytes are also those an open of /proc/<name> reads
 * just before or just after it (at most one tick of the file's clock can
 * pass between three adjacent reads); natively the host's files change
 * between reads. A UUID is new at every open, so only its form is
 * compared. O_PATH|O_NOFOLLOW opens a link itself, but not where a trailing
 * slash follows it: /proc/self/fd/N/ so opened is the directory too. */
static void check_directory_aliases(const char* name, enum kind kind) {
  char canonical[128];
  snprintf(canonical, sizeof(canonical), "/proc/%s", name);
  struct stat by_path;
  CHECK(stat(canonical, &by_path) == 0, "stat(%s): %s", canonical, strerror(errno));
  int proc = open("/proc", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  CHECK(proc >= 0, "open(/proc): %s", strerror(errno));
  if (proc < 0) {
    return;
  }
  const int directory_flags = O_RDONLY | O_DIRECTORY | O_CLOEXEC;
  char spelling[64];
  snprintf(spelling, sizeof(spelling), "/proc/self/fd/%d", proc);
  int first_alias = check_directory_alias(spelling, directory_flags, name, canonical, &by_path,
                                          kind);
  snprintf(spelling, sizeof(spelling), "/dev/fd/%d", proc);
  int directory = check_directory_alias(spelling, directory_flags, name, canonical, &by_path,
                                        kind);
  if (directory >= 0) {
    close(directory);
  }
  if (!kvm_local_procfs) {
    snprintf(spelling, sizeof(spelling), "/proc/self/fd/%d/", proc);
    directory = check_directory_alias(spelling, O_PATH | O_NOFOLLOW | O_CLOEXEC, name, canonical,
                                      &by_path, kind);
    if (directory >= 0) {
      close(directory);
    }
  } else {
    /* KVM answers ELOOP where Linux follows the link for the trailing
     * slash:
     * https://github.com/rrnewton/hermit/issues/4485 (KVM: ELOOP on a trailing-slash link)
     */
    char check[160];
    snprintf(check, sizeof(check), "/proc/self/fd/N/ opened O_PATH|O_NOFOLLOW, then \"%s\"",
             name);
    skipped(check, "--kvm-local-procfs",
            "KVM answers ELOOP where Linux follows the link for the trailing slash, "
            "https://github.com/rrnewton/hermit/issues/4485 (KVM trailing-slash ELOOP)");
  }
  if (first_alias >= 0) {
    /* A descriptor link of the descriptor opened through the first. */
    snprintf(spelling, sizeof(spelling), "/dev/fd/%d", first_alias);
    directory = check_directory_alias(spelling, directory_flags, name, canonical, &by_path,
                                      kind);
    if (directory >= 0) {
      close(directory);
    }
    close(first_alias);
  }
  close(proc);
}

/* The same aliases from a second thread, which shares the descriptor table
 * of the thread-group leader (pthread_create shares it): /proc/self/fd/N
 * is the leader's descriptor N, and so the caller's. */
static void* directory_aliases_thread(void* unused) {
  (void)unused;
  check_directory_aliases("stat", STAT);
  return NULL;
}

static void check_directory_aliases_in_a_thread(void) {
  pthread_t thread;
  CHECK(pthread_create(&thread, NULL, directory_aliases_thread, NULL) == 0 &&
            pthread_join(thread, NULL) == 0,
        "directory aliases in a thread");
}

/* The process's descriptor links (/proc/self/fd/N, /dev/fd/N) name the
 * thread-group leader's descriptor table, which a thread cloned without
 * CLONE_FILES does not share; /proc/thread-self/fd/N names the thread's
 * own. In such a thread, where the leader's descriptor N is an empty
 * directory and the thread's is /proc, /proc/self/fd/N opens the empty
 * directory, where "stat" does not exist (to open, or to stat), and
 * /proc/thread-self/fd/N opens /proc. (Not a rendered file below the
 * thread's /proc: Detcore reopens its placeholder through the leader's
 * table there, so the open fails with ENOENT on ptrace, a separate defect:
 * https://github.com/rrnewton/hermit/issues/4485 (Detcore reopens a rendered placeholder through the
 * leader's /proc/self/fd table)) Skipped under --kvm-local-procfs: KVM refuses clone without
 * CLONE_FILES with EINVAL:
 * https://github.com/rrnewton/hermit/issues/4485 (KVM: EINVAL on a thread clone without CLONE_FILES)
 */
struct split_table {
  int fd;
  char process_link[64];
  char thread_link[64];
  int process_errno;
  struct stat process_identity;
  int process_stat_errno;
  int process_fstatat_errno;
  int thread_errno;
  struct stat thread_identity;
  volatile uint32_t running;
};

static int split_table_thread(void* argument) {
  struct split_table* split = argument;
  int proc = open("/proc", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  if (proc < 0 || dup3(proc, split->fd, O_CLOEXEC) < 0) {
    split->process_errno = split->thread_errno = errno;
    return 0;
  }
  close(proc);
  int directory = open(split->process_link, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  split->process_errno = directory < 0 ? errno : 0;
  if (directory >= 0) {
    fstat(directory, &split->process_identity);
    int fd = openat(directory, "stat", O_RDONLY | O_CLOEXEC);
    split->process_stat_errno = fd < 0 ? errno : 0;
    if (fd >= 0) {
      close(fd);
    }
    struct stat ignored;
    split->process_fstatat_errno = fstatat(directory, "stat", &ignored, 0) == 0 ? 0 : errno;
    close(directory);
  }
  directory = open(split->thread_link, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  split->thread_errno = directory < 0 ? errno : 0;
  if (directory >= 0) {
    fstat(directory, &split->thread_identity);
    close(directory);
  }
  close(split->fd);
  return 0;
}

static void check_split_table_aliases(void) {
  static char stack[1 << 16] __attribute__((aligned(16)));
  static struct split_table split;
  char empty[] = "/tmp/rendered-procfs-split-XXXXXX";
  CHECK(mkdtemp(empty) != NULL, "mkdtemp: %s", strerror(errno));
  struct stat empty_identity;
  struct stat proc_identity;
  CHECK(stat(empty, &empty_identity) == 0 && stat("/proc", &proc_identity) == 0,
        "stat for the split table: %s", strerror(errno));
  split.fd = open(empty, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  CHECK(split.fd >= 0, "open(%s): %s", empty, strerror(errno));
  if (split.fd < 0) {
    rmdir(empty);
    return;
  }
  snprintf(split.process_link, sizeof(split.process_link), "/proc/self/fd/%d", split.fd);
  snprintf(split.thread_link, sizeof(split.thread_link), "/proc/thread-self/fd/%d", split.fd);
  split.running = 1;
  pid_t tid = clone(split_table_thread, stack + sizeof(stack),
                    CLONE_VM | CLONE_THREAD | CLONE_SIGHAND | CLONE_CHILD_CLEARTID, &split, NULL,
                    NULL, &split.running);
  CHECK(tid > 0, "clone without CLONE_FILES: %s", strerror(errno));
  while (tid > 0 && __atomic_load_n(&split.running, __ATOMIC_ACQUIRE) != 0) {
    syscall(SYS_futex, &split.running, FUTEX_WAIT, 1, NULL, NULL, 0);
  }
  if (tid > 0) {
    CHECK(split.process_errno == 0 && split.process_identity.st_dev == empty_identity.st_dev &&
              split.process_identity.st_ino == empty_identity.st_ino,
          "%s in a thread with its own table opened %#llx:%llu (%s), not the leader's %s",
          split.process_link, (unsigned long long)split.process_identity.st_dev,
          (unsigned long long)split.process_identity.st_ino, strerror(split.process_errno),
          empty);
    CHECK(split.process_errno == 0 && split.process_stat_errno == ENOENT,
          "openat(%s, \"stat\") in a thread with its own table: %s, not ENOENT",
          split.process_link, strerror(split.process_stat_errno));
    CHECK(split.process_errno == 0 && split.process_fstatat_errno == ENOENT,
          "fstatat(%s, \"stat\") in a thread with its own table: %s, not ENOENT",
          split.process_link, strerror(split.process_fstatat_errno));
    CHECK(split.thread_errno == 0 && split.thread_identity.st_dev == proc_identity.st_dev &&
              split.thread_identity.st_ino == proc_identity.st_ino,
          "%s in a thread with its own table opened %#llx:%llu (%s), not /proc",
          split.thread_link, (unsigned long long)split.thread_identity.st_dev,
          (unsigned long long)split.thread_identity.st_ino, strerror(split.thread_errno));
  }
  close(split.fd);
  rmdir(empty);
}

/* The process (or task) whose /proc/<pid>/stat a "stat" opened relative to
 * `directory` describes: its first field. -1 if it cannot be read. */
static long stat_subject(int directory) {
  char text[128];
  int fd = openat(directory, "stat", O_RDONLY | O_CLOEXEC);
  ssize_t length = fd < 0 ? -1 : read(fd, text, sizeof(text) - 1);
  if (fd >= 0) {
    close(fd);
  }
  if (length <= 0) {
    return -1;
  }
  text[length] = '\0';
  return strtol(text, NULL, 10);
}

/* A descriptor for /proc/self or /proc/thread-self is the directory of the
 * process or thread that opened it, which another process that inherited
 * it, or another thread, reaches through its descriptor link: a fork's
 * child reopening its parent's /proc/self through /proc/self/fd/N finds
 * the parent's "stat", and the leader reopening a thread's
 * /proc/thread-self finds the thread's. */
static void check_selector_aliases_from_a_fork(void) {
  int self = open("/proc/self", O_RDONLY | O_DIRECTORY);
  CHECK(self >= 0, "open(/proc/self): %s", strerror(errno));
  if (self < 0) {
    return;
  }
  fflush(stdout);
  fflush(stderr);
  pid_t child = fork();
  if (child == 0) {
    char spelling[64];
    snprintf(spelling, sizeof(spelling), "/proc/self/fd/%d", self);
    int directory = open(spelling, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    long subject = directory < 0 ? -2 : stat_subject(directory);
    if (subject != (long)getppid()) {
      fprintf(stderr, "FAIL: openat(%s, \"stat\") in a fork's child names %ld, not its parent %ld\n",
              spelling, subject, (long)getppid());
      _exit(1);
    }
    _exit(0);
  }
  int status = 0;
  CHECK(child > 0 && waitpid(child, &status, 0) == child && WIFEXITED(status) &&
            WEXITSTATUS(status) == 0,
        "the parent's /proc/self reopened by its child (status %d)", status);
  close(self);
}

struct thread_self {
  int fd;
  long tid;
  pthread_barrier_t opened;
  pthread_barrier_t checked;
};

static void* thread_self_opener(void* argument) {
  struct thread_self* shared = argument;
  shared->fd = open("/proc/thread-self", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  shared->tid = (long)gettid();
  pthread_barrier_wait(&shared->opened);
  /* Alive until the leader has looked: an exited thread's directory is
   * empty. */
  pthread_barrier_wait(&shared->checked);
  return NULL;
}

static void check_selector_aliases_from_a_thread(void) {
  struct thread_self shared = {.fd = -1, .tid = -1};
  pthread_barrier_init(&shared.opened, NULL, 2);
  pthread_barrier_init(&shared.checked, NULL, 2);
  pthread_t thread;
  int created = pthread_create(&thread, NULL, thread_self_opener, &shared) == 0;
  CHECK(created, "pthread_create for /proc/thread-self");
  if (!created) {
    return;
  }
  pthread_barrier_wait(&shared.opened);
  CHECK(shared.fd >= 0, "open(/proc/thread-self) in a thread");
  if (shared.fd >= 0) {
    char spelling[64];
    snprintf(spelling, sizeof(spelling), "/proc/self/fd/%d", shared.fd);
    int directory = open(spelling, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    long subject = directory < 0 ? -2 : stat_subject(directory);
    CHECK(subject == shared.tid, "openat(%s, \"stat\") names %ld, not the thread %ld", spelling,
          subject, shared.tid);
    if (directory >= 0) {
      close(directory);
    }
  }
  pthread_barrier_wait(&shared.checked);
  pthread_join(thread, NULL);
  if (shared.fd >= 0) {
    close(shared.fd);
  }
  pthread_barrier_destroy(&shared.opened);
  pthread_barrier_destroy(&shared.checked);
}

/* An open of a descriptor link looks the link up once, as it starts. A
 * thread that blocks opening /proc/<pid>/fd/N for reading, where N is an
 * O_PATH descriptor of a FIFO, keeps that FIFO when another thread then
 * replaces descriptor N with /proc, and a name below the descriptor it gets
 * does not exist there (ENOTDIR), as below any FIFO. Where the replacement
 * came first, the open reached /proc and "stat" below it is /proc/stat;
 * natively either order may happen. The writer opens without waiting
 * (O_NONBLOCK): a reader waiting in the open lets it open, and ENXIO for
 * half a second, no reader, means the replacement came first, so neither
 * order leaves a thread waiting. With --alias-bytes (where Detcore
 * schedules the threads deterministically, and the reader's open has
 * blocked before the other thread runs on) the reader must have the FIFO. */
struct blocked_alias {
  char link[64];
  int ready[2];
  int fd;
  int open_errno;
  int got_fifo;
  int stat_errno;
};

static void* blocked_alias_reader(void* argument) {
  struct blocked_alias* blocked = argument;
  char ready = 'r';
  if (write(blocked->ready[1], &ready, 1) != 1) {
    blocked->open_errno = errno;
    return NULL;
  }
  blocked->fd = open(blocked->link, O_RDONLY | O_CLOEXEC);
  blocked->open_errno = blocked->fd < 0 ? errno : 0;
  if (blocked->fd >= 0) {
    struct stat opened;
    blocked->got_fifo = fstat(blocked->fd, &opened) == 0 && S_ISFIFO(opened.st_mode);
    int fd = openat(blocked->fd, "stat", O_RDONLY | O_CLOEXEC);
    blocked->stat_errno = fd < 0 ? errno : 0;
    if (fd >= 0) {
      close(fd);
    }
  }
  return NULL;
}

static void check_alias_source_replaced_during_a_blocked_open(void) {
  /* A blocking FIFO open through a descriptor link hangs the run where
   * Detcore does not find the guest's FIFO (/proc/self/fd/N on every
   * backend, any spelling on DBT and KVM, and under record and replay):
   * https://github.com/rrnewton/hermit/issues/4485 (a blocking FIFO open through a descriptor link hangs)
   */
  if (no_fifo_link_open) {
    skipped("a blocking open of a FIFO through /proc/<pid>/fd/N whose descriptor another thread "
            "replaces (check_alias_source_replaced_during_a_blocked_open)",
            "--no-fifo-link-open",
            "under DBT and KVM that open waits in the kernel holding the scheduler turn, so the "
            "writer never runs and the run hangs, "
            "https://github.com/rrnewton/hermit/issues/4485 (FIFO-link open hangs)");
    return;
  }
  char directory[] = "/tmp/rendered-procfs-fifo-XXXXXX";
  if (mkdtemp(directory) == NULL) {
    CHECK(0, "mkdtemp: %s", strerror(errno));
    return;
  }
  char fifo[96];
  snprintf(fifo, sizeof(fifo), "%s/fifo", directory);
  static struct blocked_alias blocked;
  blocked.fd = -1;
  blocked.open_errno = blocked.stat_errno = blocked.got_fifo = 0;
  int source = -1;
  int made = mkfifo(fifo, 0600) == 0 && (source = open(fifo, O_PATH | O_CLOEXEC)) >= 0 &&
             pipe2(blocked.ready, O_CLOEXEC) == 0;
  CHECK(made, "prepare the FIFO for the blocked alias open: %s", strerror(errno));
  /* By number: Detcore finds a guest's FIFO through the guest's
   * /proc/<pid>, where /proc/self would name its own process. */
  snprintf(blocked.link, sizeof(blocked.link), "/proc/%ld/fd/%d", (long)getpid(), source);
  pthread_t reader;
  if (made && pthread_create(&reader, NULL, blocked_alias_reader, &blocked) == 0) {
    char ready;
    struct timespec pause = {.tv_sec = 0, .tv_nsec = 100 * 1000 * 1000};
    int proc = -1;
    int writer = -1;
    int writer_errno = 0;
    if (read(blocked.ready[0], &ready, 1) == 1) {
      nanosleep(&pause, NULL);
      proc = open("/proc", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
      CHECK(proc >= 0 && dup3(proc, source, O_CLOEXEC) == source,
            "replace descriptor %d with /proc during the blocked open: %s", source,
            strerror(errno));
      /* Natively a reader that has followed the link may not yet count as
       * the FIFO's reader: ENXIO is final only after half a second. */
      struct timespec retry = {.tv_sec = 0, .tv_nsec = 10 * 1000 * 1000};
      for (int attempt = 0; attempt < 50; attempt++) {
        writer = open(fifo, O_WRONLY | O_NONBLOCK | O_CLOEXEC);
        writer_errno = writer < 0 ? errno : 0;
        if (writer >= 0 || writer_errno != ENXIO) {
          break;
        }
        nanosleep(&retry, NULL);
      }
      CHECK(writer >= 0 || writer_errno == ENXIO, "open %s for writing: %s", fifo,
            strerror(writer_errno));
    }
    pthread_join(reader, NULL);
    CHECK(blocked.open_errno == 0, "the blocked open of %s: %s", blocked.link,
          strerror(blocked.open_errno));
    if (blocked.fd >= 0 && blocked.got_fifo) {
      CHECK(blocked.stat_errno == ENOTDIR,
            "openat(<the FIFO a blocked open of %s returned>, \"stat\"): %s, not ENOTDIR",
            blocked.link, blocked.stat_errno == 0 ? "opened" : strerror(blocked.stat_errno));
      CHECK(writer >= 0, "the blocked open of %s returned the FIFO, which had no reader for "
            "the writer (%s)", blocked.link, strerror(writer_errno));
    } else if (blocked.fd >= 0) {
      CHECK(!alias_bytes, "the blocked open of %s returned the descriptor that replaced %d, not the FIFO",
            blocked.link, source);
      CHECK(blocked.stat_errno == 0, "openat(<the /proc a blocked open of %s returned>, \"stat\"): %s",
            blocked.link, strerror(blocked.stat_errno));
    }
    if (blocked.fd >= 0) {
      close(blocked.fd);
    }
    if (writer >= 0) {
      close(writer);
    }
    if (proc >= 0) {
      close(proc);
    }
  } else if (made) {
    CHECK(0, "pthread_create for the blocked alias open");
  }
  if (made) {
    close(blocked.ready[0]);
    close(blocked.ready[1]);
  }
  if (source >= 0) {
    close(source);
  }
  unlink(fifo);
  rmdir(directory);
}

/* The identities check_numeric_alias tells apart. */
struct numeric_alias_identities {
  struct stat empty;
  struct stat proc;
  struct stat stat_file;
};

/* In a fork's child whose descriptor N is /proc while its parent's is an
 * empty directory, the directory `spelling` (a numeric spelling of the
 * child's own descriptor link N) opens must be the one names are then
 * looked up below: "stat" below /proc is /proc/stat, and below the
 * parent's empty directory it does not exist, to open or to stat. getpid()
 * and gettid() name the child itself, so the directory must be /proc.
 * Under --no-foreign-fd-links a backend that serves no numeric /proc
 * subject may refuse the open with ENOENT, which is announced. */
static void check_numeric_alias(const char* spelling, const struct numeric_alias_identities* ids) {
  int directory = open(spelling, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  if (directory < 0) {
    int error = errno;
    if (error == ENOENT && no_foreign_fd_links) {
      char check[160];
      snprintf(check, sizeof(check), "%s in a fork's child (ENOENT)", spelling);
      skipped(check, "--no-foreign-fd-links", "the backend serves no numeric /proc subject");
      return;
    }
    CHECK(0, "open(%s) in a fork's child: %s", spelling, strerror(error));
    return;
  }
  struct stat opened;
  int described = fstat(directory, &opened) == 0;
  CHECK(described, "fstat(%s): %s", spelling, strerror(errno));
  if (described && same_file(&opened, &ids->proc)) {
    int fd = openat(directory, "stat", O_RDONLY | O_CLOEXEC);
    int error = fd < 0 ? errno : 0;
    struct stat by_fd;
    int is_stat_file = fd >= 0 && fstat(fd, &by_fd) == 0 && same_file(&by_fd, &ids->stat_file);
    CHECK(is_stat_file, "openat(%s, \"stat\") in a fork's child is not /proc/stat (%s)", spelling,
          fd < 0 ? strerror(error) : "another file");
    if (fd >= 0) {
      close(fd);
    }
  } else if (described && same_file(&opened, &ids->empty)) {
    CHECK(no_foreign_fd_links,
          "%s in a fork's child opened its parent's descriptor, not the child's /proc", spelling);
    if (no_foreign_fd_links) {
      allowed_for(spelling, "a fork's child opening its parent's descriptor N, not its own",
                  "--no-foreign-fd-links",
                  "the backend's procfs may number processes otherwise; names below it must "
                  "still not exist");
    }
    int fd = openat(directory, "stat", O_RDONLY | O_CLOEXEC);
    int open_errno = fd < 0 ? errno : 0;
    if (fd >= 0) {
      close(fd);
    }
    CHECK(fd < 0 && open_errno == ENOENT,
          "openat(%s, \"stat\") below the parent's empty directory: %s, not ENOENT", spelling,
          fd < 0 ? strerror(open_errno) : "opened");
    struct stat ignored;
    int stat_result = fstatat(directory, "stat", &ignored, 0);
    int stat_errno = stat_result == 0 ? 0 : errno;
    CHECK(stat_result == -1 && stat_errno == ENOENT,
          "fstatat(%s, \"stat\") below the parent's empty directory: %s, not ENOENT", spelling,
          stat_result == 0 ? "found" : strerror(stat_errno));
  } else if (described) {
    CHECK(0, "%s in a fork's child opened %#llx:%llu, neither /proc nor its parent's directory",
          spelling, (unsigned long long)opened.st_dev, (unsigned long long)opened.st_ino);
  }
  close(directory);
}

/* A numeric spelling of the caller's own descriptor link,
 * /proc/<getpid()>/fd/N or /proc/<getpid()>/task/<gettid()>/fd/N, names
 * the caller's descriptor N, as /proc/self/fd/N does. The parent's
 * descriptor N is an empty directory, and its fork's child replaces its own
 * N with /proc (check_numeric_alias). Run as the root process's first
 * fork, before anything else forks, so that the child's pid is the one a
 * backend whose kernel numbers processes otherwise than Detcore (DBT) gives
 * the root process: a spelling resolved by Detcore's numbers there reaches
 * the parent's table, where the kernel's reaches the child's. */
static void check_numeric_own_pid_aliases(void) {
  struct numeric_alias_identities ids;
  /* Named by the pid, not by mkdtemp: this runs before the workload, whose
   * printed UUIDs come from the random stream mkdtemp may draw from (it did
   * on KVM for a guest at one path and not at another). */
  char empty[64];
  snprintf(empty, sizeof(empty), "/tmp/rendered-procfs-numeric-%ld", (long)getpid());
  rmdir(empty);
  if (mkdir(empty, 0700) != 0) {
    CHECK(0, "mkdir(%s): %s", empty, strerror(errno));
    return;
  }
  /* The identities of what is opened, as the checks then open them (KVM's
   * own procfs refuses a stat of /proc itself). */
  int fd = open(empty, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  int proc = open("/proc", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  int stat_file = open("/proc/stat", O_RDONLY | O_CLOEXEC);
  int known = fd >= 0 && proc >= 0 && stat_file >= 0 && fstat(fd, &ids.empty) == 0 &&
              fstat(proc, &ids.proc) == 0 && fstat(stat_file, &ids.stat_file) == 0;
  CHECK(known, "identities for the numeric aliases: %s", strerror(errno));
  if (proc >= 0) {
    close(proc);
  }
  if (stat_file >= 0) {
    close(stat_file);
  }
  if (!known) {
    if (fd >= 0) {
      close(fd);
    }
    rmdir(empty);
    return;
  }
  fflush(stdout);
  fflush(stderr);
  pid_t child = fork();
  if (child == 0) {
    int inherited_failures = failures;
    int proc = open("/proc", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    int replaced = proc >= 0 && dup3(proc, fd, O_CLOEXEC) == fd;
    CHECK(replaced, "replace descriptor %d with /proc in a fork's child: %s", fd,
          strerror(errno));
    if (proc >= 0) {
      close(proc);
    }
    if (replaced) {
      char spelling[96];
      snprintf(spelling, sizeof(spelling), "/proc/%ld/fd/%d", (long)getpid(), fd);
      check_numeric_alias(spelling, &ids);
      snprintf(spelling, sizeof(spelling), "/proc/%ld/task/%ld/fd/%d", (long)getpid(),
               (long)gettid(), fd);
      check_numeric_alias(spelling, &ids);
    }
    _exit(failures == inherited_failures ? 0 : 1);
  }
  int status = 0;
  CHECK(child > 0 && waitpid(child, &status, 0) == child && WIFEXITED(status) &&
            WEXITSTATUS(status) == 0,
        "a numeric spelling of a fork's child's own descriptor link (status %#x)", status);
  close(fd);
  rmdir(empty);
}

/* The directory aliases (check_directory_aliases) from a thread created
 * now, which shares this process's descriptor table. Returns 0 if every
 * check passed. */
static void* alias_checks_thread(void* unused) {
  (void)unused;
  check_directory_aliases("stat", STAT);
  check_directory_aliases("uptime", UPTIME);
  return NULL;
}

static int aliases_from_a_new_thread(void) {
  int before = failures;
  pthread_t thread;
  int created = pthread_create(&thread, NULL, alias_checks_thread, NULL) == 0;
  CHECK(created && pthread_join(thread, NULL) == 0, "directory aliases in a new thread");
  return failures == before ? 0 : 1;
}

/* A process created with CLONE_FILES and without CLONE_THREAD shares its
 * creator's descriptor table and leads a thread group of its own, so a
 * thread it creates reads that table, its own, through /proc/self/fd/N:
 * the directory aliases hold there as in the root process. The creator is
 * a fork's child, so the table is not the root process's. */
static void check_aliases_in_a_clone_files_process(void) {
  if (no_clone_files_process) {
    /* DBT refuses such a clone and stops the run ("copied child cannot share
     * descriptor-table provenance"); KVM refuses it with EOPNOTSUPP:
     * https://github.com/rrnewton/hermit/issues/4485 (DBT ends the run; KVM: EOPNOTSUPP)
     */
    skipped("the directory aliases from a thread of a process created with CLONE_FILES",
            "--no-clone-files-process",
            "DBT refuses such a clone and stops the run; KVM refuses it with EOPNOTSUPP, "
            "https://github.com/rrnewton/hermit/issues/4485 (CLONE_FILES process refused)");
    return;
  }
  fflush(stdout);
  fflush(stderr);
  pid_t child = fork();
  if (child == 0) {
    int inherited_failures = failures;
    long process = syscall(SYS_clone, CLONE_FILES | SIGCHLD, 0, 0, 0, 0);
    if (process == 0) {
      _exit(aliases_from_a_new_thread());
    }
    int error = errno;
    int status = 0;
    CHECK(process > 0 && waitpid((pid_t)process, &status, 0) == (pid_t)process &&
              WIFEXITED(status) && WEXITSTATUS(status) == 0,
          "directory aliases from a thread of a process created with CLONE_FILES (%s, status "
          "%#x)",
          process < 0 ? strerror(error) : "created", status);
    _exit(failures == inherited_failures ? 0 : 1);
  }
  int status = 0;
  CHECK(child > 0 && waitpid(child, &status, 0) == child && WIFEXITED(status) &&
            WEXITSTATUS(status) == 0,
        "directory aliases in a process created with CLONE_FILES (status %#x)", status);
}

/* The new image of a thread that is not its process's leader runs this
 * (--aliases-in-a-thread): exec gave the exec'ing thread a descriptor
 * table of its own, and the thread leads the process now, so a thread it
 * creates shares the process's table and reads it through
 * /proc/self/fd/N. */
static void* exec_from_a_thread(void* unused) {
  (void)unused;
  char* const arguments[] = {(char*)program, "--aliases-in-a-thread", alias_bytes ? "1" : "0",
                             NULL};
  execv(program, arguments);
  fprintf(stderr, "FAIL: exec from a thread: %s\n", strerror(errno));
  _exit(1);
}

static void check_aliases_after_a_thread_exec(void) {
  if (kvm_local_procfs) {
    skipped("the directory aliases from a thread of an image a non-leader thread executed",
            "--kvm-local-procfs", "KVM refuses an exec from a thread that does not lead its "
            "process (NonLeaderExec)");
    return;
  }
  fflush(stdout);
  fflush(stderr);
  pid_t child = fork();
  if (child == 0) {
    pthread_t thread;
    if (pthread_create(&thread, NULL, exec_from_a_thread, NULL) == 0) {
      /* The exec ends this thread. */
      pthread_join(thread, NULL);
    }
    _exit(1);
  }
  int status = 0;
  CHECK(child > 0 && waitpid(child, &status, 0) == child && WIFEXITED(status) &&
            WEXITSTATUS(status) == 0,
        "directory aliases from a thread of an image a non-leader thread executed (status %#x)",
        status);
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
  } else {
    skipped("the inherited descriptor through a symbolic link to its descriptor link",
            "--kvm-local-procfs", "KVM's own procfs refuses symbolic links into /proc, "
            "https://github.com/rrnewton/hermit/issues/4282");
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
  if (argc == 3 && strcmp(argv[1], "--aliases-in-a-thread") == 0) {
    alias_bytes = atoi(argv[2]);
    return aliases_from_a_new_thread();
  }
  if (argc == 10 && strcmp(argv[1], "--inherited") == 0) {
    kvm_local_procfs = atoi(argv[6]);
    return inherited(atoi(argv[2]), atoi(argv[3]), strtoull(argv[4], NULL, 10), atol(argv[5]),
                     atoi(argv[7]), argv[8], atoi(argv[9]));
  }
  for (int i = 1; i < argc; i++) {
    if (strcmp(argv[i], "--kvm-local-procfs") == 0) {
      kvm_local_procfs = 1;
      no_foreign_fd_links = 1;
      no_clone_files_process = 1;
      no_fifo_link_open = 1;
    } else if (strcmp(argv[i], "--no-fifo-link-open") == 0) {
      no_fifo_link_open = 1;
    } else if (strcmp(argv[i], "--no-clone-files-process") == 0) {
      no_clone_files_process = 1;
    } else if (strcmp(argv[i], "--noreplace-overlap-eperm") == 0) {
      noreplace_overlap_eperm = 1;
    } else if (strcmp(argv[i], "--no-foreign-fd-links") == 0) {
      no_foreign_fd_links = 1;
    } else if (strcmp(argv[i], "--no-foreign-fd-access") == 0) {
      no_foreign_fd_access = 1;
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
    } else if (strcmp(argv[i], "--alias-bytes") == 0) {
      alias_bytes = 1;
    } else {
      program = NULL;
    }
  }
  if (program == NULL || program[0] != '/') {
    fprintf(stderr,
            "usage: /path/to/rendered_procfs_files [--no-foreign-fd-links] "
            "[--no-clone-files-process] [--no-fifo-link-open] "
            "[--kvm-local-procfs] [--no-maps-or-chroot] [--chroot-only] "
            "[--require-chroot] [--no-bypass-capabilities] [--xattr-arguments-first] "
            "[--detcore-xattr-order] [--alias-bytes]\n");
    return 2;
  }
  if (no_bypass_capabilities && !lacks_bypass_capabilities()) {
    return 1;
  }
  if (chroot_only) {
    skipped("every check but the process chroot checks", "--chroot-only",
            "a run that verifies the other checks runs them");
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
  check_numeric_own_pid_aliases();
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
  check_directory_aliases("stat", STAT);
  check_directory_aliases("uptime", UPTIME);
  check_directory_aliases("loadavg", LOADAVG);
  check_directory_aliases("sys/kernel/pty/nr", COUNT);
  check_directory_aliases("sys/kernel/random/uuid", UUID);
  check_directory_aliases_in_a_thread();
  check_aliases_in_a_clone_files_process();
  check_aliases_after_a_thread_exec();
  check_alias_source_replaced_during_a_blocked_open();
  if (!kvm_local_procfs) {
    check_split_table_aliases();
  } else {
    /* KVM refuses clone without CLONE_FILES with EINVAL:
     * https://github.com/rrnewton/hermit/issues/4485 (KVM: EINVAL on a thread clone without CLONE_FILES)
     */
    skipped("the descriptor links of a thread cloned without CLONE_FILES "
            "(check_split_table_aliases)",
            "--kvm-local-procfs",
            "KVM refuses clone without CLONE_FILES with EINVAL, "
            "https://github.com/rrnewton/hermit/issues/4485 (KVM thread clone EINVAL)");
  }
  if (!no_foreign_fd_links) {
    check_selector_aliases_from_a_fork();
    check_selector_aliases_from_a_thread();
  } else {
    skipped("a parent's /proc/self reopened by its fork's child through /proc/self/fd/N "
            "(check_selector_aliases_from_a_fork)",
            "--no-foreign-fd-links",
            "KVM's own procfs refuses /proc/self, "
            "https://github.com/rrnewton/hermit/issues/4282");
    skipped("a thread's /proc/thread-self reopened by the leader through /proc/self/fd/N "
            "(check_selector_aliases_from_a_thread)",
            "--no-foreign-fd-links",
            "KVM's own procfs refuses /proc/thread-self, "
            "https://github.com/rrnewton/hermit/issues/4282");
  }
  if (!kvm_local_procfs && !no_maps_or_chroot) {
    check_chroot();
    check_thread_chroot();
  } else {
    skipped("the process and thread chroot checks",
            kvm_local_procfs ? "--kvm-local-procfs" : "--no-maps-or-chroot",
            kvm_local_procfs ? "KVM does not support chroot"
                             : "DBT cannot yet compare a chroot strictly, "
                               "https://github.com/rrnewton/hermit/issues/4233; the cli test "
                               "runs the process chroot checks separately");
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
