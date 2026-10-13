/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Guest for hermit-cli/tests/cli.rs: the procfs files Detcore renders without
 * the backend opening them (https://github.com/rrnewton/hermit/issues/4213),
 * /proc/sys/kernel/random/uuid and /proc/sys/kernel/pty/nr.
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
 * usage: rendered_procfs_files [--no-foreign-fd-links] [--kvm-local-procfs]
 *                              [--no-maps-or-chroot] [--chroot-only]
 *                              [--require-chroot]
 *        rendered_procfs_files --inherited <fd> <ino> <offset>
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
 *   --chroot-only runs only the process chroot checks, for a run that
 *     cannot verify them (see the previous option); not a thread's chroot,
 *     which hangs under DBT (https://github.com/rrnewton/hermit/issues/4254).
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
#include <unistd.h>

#define PROC_SUPER_MAGIC 0x9fa0

static int failures;
static int kvm_local_procfs;
static int no_foreign_fd_links;
static int no_maps_or_chroot;
static int chroot_only;
static int require_chroot;
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

static void check_descriptor_refusals(const char* path, int fd) {
  REFUSED(write(fd, "1\n", 2), EBADF, path);
  REFUSED(pwrite(fd, "1\n", 2, 0), EBADF, path);
  REFUSED(ftruncate(fd, 0), EINVAL, path);
  REFUSED(fallocate(fd, 0, 0, 1), EBADF, path);
  struct flock lock = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
  REFUSED(fcntl(fd, F_SETLK, &lock), EBADF, path);
  int source = open(program, O_RDONLY);
  REFUSED(sendfile(fd, source, NULL, 1), EBADF, path);
  close(source);
  REFUSED(mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, fd, 0), ENODEV, path);
  REFUSED(mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0), EACCES, path);
  REFUSED(mmap(NULL, 0, PROT_READ, MAP_PRIVATE, fd, 0), EINVAL, path);
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
  struct stat ignored;
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

/* The calls a 0444 procfs sysctl file answers by path, through `spelling`. */
static void check_path_calls(const char* path, const char* spelling) {
  char value[16];
  CHECK(access(spelling, F_OK) == 0, "%s access(F_OK): %s", spelling, strerror(errno));
  CHECK(access(spelling, R_OK) == 0, "%s access(R_OK): %s", spelling, strerror(errno));
  REFUSED(access(spelling, W_OK), EACCES, spelling);
  REFUSED(access(spelling, X_OK), EACCES, spelling);
  REFUSED(faccessat(AT_FDCWD, spelling, W_OK, AT_EACCESS), EACCES, spelling);
  REFUSED(getxattr(spelling, "user.x", value, sizeof(value)), EOPNOTSUPP, spelling);
  REFUSED(getxattr(spelling, "trusted.x", value, sizeof(value)), ENODATA, spelling);
  REFUSED(setxattr(spelling, "foo.x", "1", 1, 0), EACCES, spelling);
  CHECK(listxattr(spelling, value, sizeof(value)) == 0, "%s listxattr", spelling);
  REFUSED(setxattr(spelling, "user.x", "1", 1, 0), EACCES, spelling);
  REFUSED(removexattr(spelling, "user.x"), EACCES, spelling);
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

static void check_file(const char* path, int uuid) {
  struct stat by_path;
  CHECK(stat(path, &by_path) == 0, "stat(%s): %s", path, strerror(errno));
  CHECK(S_ISREG(by_path.st_mode) && (by_path.st_mode & 07777) == 0444, "%s mode %o", path,
        by_path.st_mode);
  CHECK(by_path.st_size == 0 && by_path.st_nlink == 1, "%s size %lld nlink %lld", path,
        (long long)by_path.st_size, (long long)by_path.st_nlink);
  check_open_errors(path);
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
  if (!kvm_local_procfs) {
    /* procfs names descriptors without leading zeros (KVM's own procfs
     * resolves this one). */
    char zero_link[80];
    snprintf(zero_link, sizeof(zero_link), "/proc/self/fd/0%d", fd);
    REFUSED(open(zero_link, O_RDONLY), ENOENT, zero_link);
  }
  struct statx extended;
  REFUSED(syscall(SYS_statx, AT_FDCWD, path, 0, 0x80000000u, &extended), EINVAL, path);
  REFUSED(syscall(SYS_openat, AT_FDCWD, self_link, 020000000 | O_RDWR), EINVAL, self_link);
  char link_dir[80];
  struct stat link_ignored;
  snprintf(link_dir, sizeof(link_dir), "%s/", self_link);
  REFUSED(open(link_dir, O_RDONLY), ENOTDIR, link_dir);
  REFUSED(stat(link_dir, &link_ignored), ENOTDIR, link_dir);
  snprintf(link_dir, sizeof(link_dir), "%s/.", self_link);
  REFUSED(open(link_dir, O_RDONLY), ENOTDIR, link_dir);
  /* This thread's spelling of the link: the same file, and the same link
   * even into a buffer shorter than anything but the path. (Under DBT the
   * thread ID Detcore gives the guest does not name a /proc task entry.) */
  if (!no_foreign_fd_links) {
    char task_link[96];
    snprintf(task_link, sizeof(task_link), "/proc/self/task/%d/fd/%d", (int)gettid(), fd);
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

  char contents[64];
  ssize_t length = read(fd, contents, sizeof(contents));
  CHECK(length > 0 && (uuid ? is_uuid(contents, length) : is_count(contents, length)),
        "%s contents %.*s", path, (int)(length > 0 ? length : 0), contents);
  char rest[64];
  CHECK(read(fd, rest, sizeof(rest)) == 0, "%s second read is not at end", path);
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
  ssize_t preread = pread(fd, rest, sizeof(rest), 0);
  CHECK(preread > 0 && (uuid ? is_uuid(rest, preread) : is_count(rest, preread)),
        "%s pread contents %.*s", path, (int)(preread > 0 ? preread : 0), rest);

  check_descriptor_refusals(path, fd);
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
    ssize_t reread = read(reopened, rest, sizeof(rest));
    CHECK(reread > 0 && (uuid ? is_uuid(rest, reread) && memcmp(rest, contents, 36) != 0
                              : is_count(rest, reread)),
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
    REFUSED(read(located, rest, sizeof(rest)), EBADF, path);
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
    char aliased[64];
    ssize_t aliased_length = through_alias >= 0 ? read(through_alias, aliased, sizeof(aliased)) : -1;
    CHECK(aliased_length > 0 && (uuid ? is_uuid(aliased, aliased_length)
                                      : is_count(aliased, aliased_length)),
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
      char text[64];
      ssize_t again_length = again >= 0 ? pread(again, text, sizeof(text), 0) : -1;
      struct stat by_stdin;
      struct statfs stdin_filesystem;
      int ok = again_length > 0 &&
               (uuid ? is_uuid(text, again_length) : is_count(text, again_length)) &&
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
    /* Another process's descriptor link names the file, and reopens it. */
    pid_t parent = getpid();
    pid_t child = fork();
    if (child == 0) {
      char other_link[64];
      snprintf(other_link, sizeof(other_link), "/proc/%d/fd/%d", (int)parent, fd);
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

/* After exec: the inherited descriptor is still the file, at its position. */
static int inherited(int fd, unsigned long long ino, long offset) {
  struct stat by_fd;
  CHECK(fstat(fd, &by_fd) == 0 && by_fd.st_ino == ino && S_ISREG(by_fd.st_mode) &&
            (by_fd.st_mode & 07777) == 0444,
        "inherited descriptor identity %llu mode %o", (unsigned long long)by_fd.st_ino,
        by_fd.st_mode);
  CHECK(lseek(fd, 0, SEEK_CUR) == offset, "inherited position %ld",
        (long)lseek(fd, 0, SEEK_CUR));
  char rest[64];
  ssize_t length = read(fd, rest, sizeof(rest));
  CHECK(length == 37 - offset && is_uuid_text(rest, length), "inherited read %zd", length);
  REFUSED(write(fd, "1\n", 2), EBADF, "inherited descriptor");
  printf("inherited descriptor read %zd bytes\n", length);
  return failures == 0 ? 0 : 1;
}

int main(int argc, char** argv) {
  program = argv[0];
  if (argc == 5 && strcmp(argv[1], "--inherited") == 0) {
    return inherited(atoi(argv[2]), strtoull(argv[3], NULL, 10), atol(argv[4]));
  }
  for (int i = 1; i < argc; i++) {
    if (strcmp(argv[i], "--kvm-local-procfs") == 0) {
      kvm_local_procfs = 1;
      no_foreign_fd_links = 1;
    } else if (strcmp(argv[i], "--no-foreign-fd-links") == 0) {
      no_foreign_fd_links = 1;
    } else if (strcmp(argv[i], "--no-maps-or-chroot") == 0) {
      no_maps_or_chroot = 1;
    } else if (strcmp(argv[i], "--chroot-only") == 0) {
      chroot_only = 1;
    } else if (strcmp(argv[i], "--require-chroot") == 0) {
      require_chroot = 1;
    } else {
      program = NULL;
    }
  }
  if (program == NULL || program[0] != '/') {
    fprintf(stderr,
            "usage: /path/to/rendered_procfs_files [--no-foreign-fd-links] "
            "[--kvm-local-procfs] [--no-maps-or-chroot] [--chroot-only] "
            "[--require-chroot]\n");
    return 2;
  }
  if (chroot_only) {
    /* Not the thread chroot, which hangs under DBT
     * (https://github.com/rrnewton/hermit/issues/4254). */
    check_chroot();
    printf("chroot checks %s\n", failures == 0 ? "passed" : "failed");
    return failures == 0 ? 0 : 1;
  }
  check_file("/proc/sys/kernel/random/uuid", 1);
  check_file("/proc/sys/kernel/pty/nr", 0);
  if (!kvm_local_procfs && !no_maps_or_chroot) {
    check_chroot();
    check_thread_chroot();
  }
  if (failures != 0) {
    return 1;
  }
  /* The descriptor survives exec, as it is, at its position. */
  int fd = open("/proc/sys/kernel/random/uuid", O_RDONLY);
  struct stat by_fd;
  char head[10];
  if (fd < 0 || fstat(fd, &by_fd) != 0 || read(fd, head, sizeof(head)) != sizeof(head)) {
    fprintf(stderr, "FAIL: cannot prepare the inherited descriptor: %s\n", strerror(errno));
    return 1;
  }
  fflush(stdout);
  char fd_text[16];
  char ino_text[32];
  snprintf(fd_text, sizeof(fd_text), "%d", fd);
  snprintf(ino_text, sizeof(ino_text), "%llu", (unsigned long long)by_fd.st_ino);
  char* const arguments[] = {(char*)program, "--inherited", fd_text, ino_text, "10", NULL};
  execv(program, arguments);
  fprintf(stderr, "FAIL: exec: %s\n", strerror(errno));
  return 1;
}
