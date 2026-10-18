/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * rendered_mount_table: a process's mountinfo and mounts files, which Detcore
 * renders on every backend from the container mount table hermit-cli builds
 * once before the guest starts (https://github.com/rrnewton/hermit/issues/4213,
 * package K slice K3a), answer as Linux's do:
 * - every spelling of a process's mountinfo (/proc/self, /proc/thread-self,
 *   /proc/<pid>, /proc/self/task/<tid>) reads the same bytes, and so does
 *   every spelling of its mounts, /proc/mounts included;
 * - mounts has a row for each mountinfo row, in the same order, with the same
 *   source, mountpoint and type, and the options show_vfsmnt merges from the
 *   mount's and the superblock's, the whole string;
 * - both files have size 0, and seek only from the start or the position
 *   (SEEK_END is EINVAL);
 * - the mnt_id in a /tmp file's fdinfo, and its STATX_MNT_ID (asked for or
 *   not), name the mountinfo row of the nearest mount holding it, and so do
 *   the program file's;
 * - a rendered file's STATX_MNT_ID is its fdinfo mnt_id, by its path, by its
 *   descriptor and through a symbolic link to its descriptor link;
 * - /proc/mounts is a symbolic link: O_NOFOLLOW is ELOOP;
 * - a zombie's mountinfo cannot be opened (EINVAL: it has no mount
 *   namespace); a process that does not exist has none (ENOENT).
 *
 * With --chroot instead, as root: descriptors opened before a chroot (one by
 * the file's own path, one through a `..` spelling) read the view they were
 * opened with after it, and a chrooted child's mountinfo, read by its parent,
 * is the child's own view: the mounts below its new root, none here. A child
 * created with CLONE_FS shares its creator's root, so its chroot moves the
 * creator's too, and a third process reading the creator's mountinfo sees
 * the creator's new view (none). With --nested-pid, as root: a CLONE_NEWPID
 * child that chroots and opens self/mountinfo through a /proc directory
 * descriptor it kept sees its own view (none).
 *
 * With --chroot --report, the chroot checks print what they read instead of
 * requiring Linux's answer.
 *
 * usage: rendered_mount_table [--hermit] | --chroot [--hermit] [--report]
 *        | --nested-pid
 *   --hermit: under Hermit, it also prints both files, which must be the same
 *   on every backend. Without it (natively) the same relations are checked
 *   against the kernel's files, the link's target is checked too, and only
 *   the checks are printed.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef STATX_MNT_ID
#define STATX_MNT_ID 0x00001000U
#endif

#define MAX_FILE (1 << 20)
#define MAX_ROWS 4096

static void die(const char *what) {
  fprintf(stderr, "rendered_mount_table: %s: %s\n", what, strerror(errno));
  exit(1);
}

static void fail(const char *what) {
  fprintf(stderr, "rendered_mount_table: %s\n", what);
  exit(1);
}

/* The whole file at `path`, read in pieces of `chunk` bytes, NUL-terminated;
 * its length in *length. */
static char *slurp(const char *path, size_t chunk, size_t *length) {
  int fd = open(path, O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    die(path);
  }
  char *bytes = malloc(MAX_FILE + 1);
  if (bytes == NULL) {
    fail("out of memory");
  }
  size_t used = 0;
  for (;;) {
    size_t want = chunk;
    if (want > MAX_FILE - used) {
      want = MAX_FILE - used;
    }
    if (want == 0) {
      fail("a mount table file is larger than 1 MiB");
    }
    ssize_t got = read(fd, bytes + used, want);
    if (got < 0) {
      die(path);
    }
    if (got == 0) {
      break;
    }
    used += (size_t)got;
  }
  close(fd);
  bytes[used] = '\0';
  *length = used;
  return bytes;
}

/* Whether `path` reads `bytes`, read in small pieces: a seq_file reread from
 * a position must continue. */
static int same(const char *path, const char *bytes, size_t length) {
  size_t other_length = 0;
  char *other = slurp(path, 7, &other_length);
  int equal = other_length == length && memcmp(other, bytes, length) == 0;
  if (!equal) {
    fprintf(stderr, "rendered_mount_table: %s differs from the first spelling\n",
            path);
  }
  free(other);
  return equal;
}

/* Count the lines of `text`. */
static size_t count_lines(const char *text) {
  size_t count = 0;
  for (; *text != '\0'; text++) {
    count += *text == '\n';
  }
  return count;
}

/* Read the mountinfo and mounts files through every spelling, and require
 * they agree. Natively the host's mounts may change between two reads, so a
 * disagreement is retried there, a few times; under Hermit the table is fixed
 * for the run, and a disagreement fails at once. */
static void read_both(int hermit, char **mountinfo, size_t *mountinfo_length,
                      char **mounts, size_t *mounts_length) {
  char path[128];
  for (int attempt = 0; attempt < (hermit ? 1 : 5); attempt++) {
    *mountinfo = slurp("/proc/self/mountinfo", 4096, mountinfo_length);
    int agree = same("/proc/thread-self/mountinfo", *mountinfo,
                     *mountinfo_length);
    snprintf(path, sizeof(path), "/proc/%d/mountinfo", getpid());
    agree = agree && same(path, *mountinfo, *mountinfo_length);
    snprintf(path, sizeof(path), "/proc/self/task/%ld/mountinfo",
             (long)syscall(SYS_gettid));
    agree = agree && same(path, *mountinfo, *mountinfo_length);
    *mounts = slurp("/proc/self/mounts", 4096, mounts_length);
    agree = agree && same("/proc/mounts", *mounts, *mounts_length);
    agree = agree && same("/proc/thread-self/mounts", *mounts, *mounts_length);
    snprintf(path, sizeof(path), "/proc/%d/mounts", getpid());
    agree = agree && same(path, *mounts, *mounts_length);
    agree = agree && count_lines(*mountinfo) == count_lines(*mounts);
    if (agree) {
      return;
    }
    free(*mountinfo);
    free(*mounts);
  }
  fail("the spellings of mountinfo and mounts disagree");
}

/* Split `text` in place into lines; returns how many. */
static size_t lines(char *text, char **rows) {
  size_t count = 0;
  for (char *line = strtok(text, "\n"); line != NULL;
       line = strtok(NULL, "\n")) {
    if (count == MAX_ROWS) {
      fail("too many rows");
    }
    rows[count++] = line;
  }
  return count;
}

/* Field `index` (0-based, space-separated) of `line`, copied to `out`. */
static void field(const char *line, int index, char *out, size_t size) {
  const char *start = line;
  for (int i = 0; i < index; i++) {
    start = strchr(start, ' ');
    if (start == NULL) {
      fail("a row has too few fields");
    }
    start++;
  }
  size_t length = strcspn(start, " ");
  if (length >= size) {
    fail("a field is too long");
  }
  memcpy(out, start, length);
  out[length] = '\0';
}

/* The fields of a mountinfo row after its " - " separator: type, source and
 * superblock options. */
static const char *after_separator(const char *line) {
  const char *separator = strstr(line, " - ");
  if (separator == NULL) {
    fail("a mountinfo row has no separator");
  }
  return separator + 3;
}

static void check_size_zero(const char *path) {
  struct stat by_path;
  if (stat(path, &by_path) != 0) {
    die(path);
  }
  int fd = open(path, O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    die(path);
  }
  struct stat by_fd;
  if (fstat(fd, &by_fd) != 0) {
    die("fstat");
  }
  if (by_path.st_size != 0 || by_fd.st_size != 0 || !S_ISREG(by_fd.st_mode)) {
    fprintf(stderr, "rendered_mount_table: %s: size %lld/%lld, mode %o\n", path,
            (long long)by_path.st_size, (long long)by_fd.st_size,
            (unsigned)by_fd.st_mode);
    exit(1);
  }
  errno = 0;
  if (lseek(fd, 0, SEEK_END) != -1 || errno != EINVAL) {
    fprintf(stderr, "rendered_mount_table: %s: SEEK_END gave errno %d\n", path,
            errno);
    exit(1);
  }
  close(fd);
}

/* The mnt_id line of descriptor `fd`'s fdinfo. */
static unsigned long long fdinfo_mount_id(int fd) {
  char path[64];
  snprintf(path, sizeof(path), "/proc/self/fdinfo/%d", fd);
  size_t length = 0;
  char *text = slurp(path, 4096, &length);
  char *line = strstr(text, "mnt_id:");
  if (line == NULL) {
    fail("fdinfo has no mnt_id");
  }
  unsigned long long id = strtoull(line + strlen("mnt_id:"), NULL, 10);
  free(text);
  return id;
}

static unsigned long long statx_mount_id_at(int dirfd, const char *path,
                                            int flags, unsigned mask) {
  struct statx result;
  memset(&result, 0, sizeof(result));
  if (syscall(SYS_statx, dirfd, path, flags, mask, &result) != 0) {
    die("statx");
  }
  if ((result.stx_mask & STATX_MNT_ID) == 0) {
    fail("statx reported no STATX_MNT_ID");
  }
  return result.stx_mnt_id;
}

/* A descriptor's STATX_MNT_ID, asked for, and not asked for: Linux fills it
 * either way, and the two must agree. */
static unsigned long long statx_mount_id(int fd) {
  unsigned long long asked =
      statx_mount_id_at(fd, "", AT_EMPTY_PATH, STATX_MNT_ID);
  unsigned long long unasked =
      statx_mount_id_at(fd, "", AT_EMPTY_PATH, STATX_BASIC_STATS);
  if (asked != unasked) {
    fprintf(stderr, "rendered_mount_table: STATX_MNT_ID %llu asked, %llu not\n",
            asked, unasked);
    exit(1);
  }
  return asked;
}

/* Whether mountpoint `mountpoint` holds path `path`. */
static int holds(const char *mountpoint, const char *path) {
  size_t length = strlen(mountpoint);
  if (strcmp(mountpoint, "/") == 0) {
    return 1;
  }
  return strncmp(path, mountpoint, length) == 0 &&
         (path[length] == '/' || path[length] == '\0');
}

/* The mount ID of the nearest row holding `path`: the deepest mountpoint
 * that holds it, and of the rows stacked there, the last (on top). Copies its
 * mountpoint to `mountpoint`. */
static unsigned long long nearest_holder(char **rows, size_t count,
                                         const char *path, char *mountpoint,
                                         size_t size) {
  unsigned long long best = 0;
  size_t best_length = 0;
  int found = 0;
  for (size_t row = 0; row < count; row++) {
    char id[32], candidate[PATH_MAX];
    field(rows[row], 0, id, sizeof(id));
    field(rows[row], 4, candidate, sizeof(candidate));
    size_t length = strlen(candidate);
    if (holds(candidate, path) && (!found || length >= best_length)) {
      found = 1;
      best = strtoull(id, NULL, 10);
      best_length = length;
      snprintf(mountpoint, size, "%s", candidate);
    }
  }
  if (!found) {
    fail("no row holds the path");
  }
  return best;
}

static int is_superblock_flag(const char *option) {
  return strcmp(option, "sync") == 0 || strcmp(option, "dirsync") == 0 ||
         strcmp(option, "mand") == 0 || strcmp(option, "lazytime") == 0;
}

/* The options show_vfsmnt writes for a mount whose mountinfo row has
 * `mount_options` and `super_options`: ro if either is read-only, then the
 * superblock flags (show_sb_opts), the mount's flags (show_mnt_opts) and the
 * filesystem's options (show_options). */
static void merged_options(const char *mount_options, const char *super_options,
                           char *out, size_t size) {
  char mount[4096], super[8192];
  snprintf(mount, sizeof(mount), "%s", mount_options);
  snprintf(super, sizeof(super), "%s", super_options);
  char *mount_rest = strchr(mount, ',');
  char *super_rest = strchr(super, ',');
  if (mount_rest != NULL) {
    *mount_rest++ = '\0';
  }
  if (super_rest != NULL) {
    *super_rest++ = '\0';
  }
  int read_only = strcmp(mount, "ro") == 0 || strcmp(super, "ro") == 0;
  char flags[8192] = "", own[8192] = "";
  for (char *option = super_rest ? strtok(super_rest, ",") : NULL;
       option != NULL; option = strtok(NULL, ",")) {
    char *target = (own[0] == '\0' && is_superblock_flag(option)) ? flags : own;
    if (target[0] != '\0') {
      strncat(target, ",", 8191 - strlen(target));
    }
    strncat(target, option, 8191 - strlen(target));
  }
  snprintf(out, size, "%s%s%s%s%s%s%s", read_only ? "ro" : "rw",
           flags[0] ? "," : "", flags, mount_rest && mount_rest[0] ? "," : "",
           mount_rest ? mount_rest : "", own[0] ? "," : "", own);
}

/* The rows of `text` (a mountinfo) that name mount `id`. */
static int names_mount(char **rows, size_t count, unsigned long long id) {
  for (size_t row = 0; row < count; row++) {
    char field_id[32];
    field(rows[row], 0, field_id, sizeof(field_id));
    if (strtoull(field_id, NULL, 10) == id) {
      return 1;
    }
  }
  return 0;
}

/* Read `fd` from the start to its end. */
static char *read_all(int fd, size_t *length) {
  char *bytes = malloc(MAX_FILE + 1);
  if (bytes == NULL) {
    fail("out of memory");
  }
  size_t used = 0;
  for (;;) {
    ssize_t got = read(fd, bytes + used, MAX_FILE - used);
    if (got < 0) {
      die("read");
    }
    if (got == 0) {
      break;
    }
    used += (size_t)got;
  }
  bytes[used] = '\0';
  *length = used;
  return bytes;
}

/* Set by --kvm-local-procfs: KVM's own /proc, which still answers every /proc
 * spelling Detcore does not resolve (https://github.com/rrnewton/hermit/issues/4282),
 * refuses statx through a symbolic link to a descriptor link (ELOOP), so that
 * one spelling is skipped there, and the skip is reported on stderr. */
static int kvm_local_procfs = 0;

/* The child of clone_fs_checks: change the root it shares. */
static int chroot_into(void *dir) { return chroot((const char *)dir) == 0 ? 0 : 2; }

/* An empty directory to chroot into, which the caller removes. */
static void make_empty_dir(const char *dir) {
  if (mkdir(dir, 0755) != 0 && errno != EEXIST) {
    die("mkdir");
  }
}

#define CLONE_FS_ROOT "/tmp/rendered-mount-table-clone-fs"
#define NESTED_ROOT "/tmp/rendered-mount-table-nested"

/* A CLONE_FS|SIGCHLD child's chroot is its creator's: a third process,
 * forked first with a root of its own, reads the creator's mountinfo after
 * it and must find the creator's new view, the mounts below the empty
 * directory: none. Runs in a process of its own, which it changes; exits 0
 * when the view was empty. */
static void clone_fs_checks(void) {
  char root[] = CLONE_FS_ROOT;
  int go[2], report[2];
  if (pipe(go) != 0 || pipe(report) != 0) {
    die("pipe");
  }
  pid_t creator = getpid();
  pid_t observer = fork();
  if (observer < 0) {
    die("fork");
  }
  if (observer == 0) {
    char byte = 0;
    if (read(go[0], &byte, 1) != 1) {
      _exit(3);
    }
    char path[64];
    snprintf(path, sizeof(path), "/proc/%d/mountinfo", creator);
    size_t length = 0;
    slurp(path, 4096, &length);
    _exit(write(report[1], &length, sizeof(length)) == sizeof(length) ? 0 : 4);
  }
  static char stack[1 << 16];
  pid_t child = clone(chroot_into, stack + sizeof(stack), CLONE_FS | SIGCHLD, root);
  int status = 0;
  if (child < 0 || waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    _exit(5);
  }
  size_t length = 1;
  if (write(go[1], "x", 1) != 1 ||
      read(report[0], &length, sizeof(length)) != sizeof(length) ||
      waitpid(observer, &status, 0) != observer) {
    _exit(6);
  }
  _exit(length == 0 ? 0 : 7);
}

/* The child of nested_pid_checks: chroot, then open self/mountinfo through
 * the /proc directory descriptor its creator opened. */
static int read_self_through_kept_proc(void *arg) {
  int proc_dir = *(int *)arg;
  if (chroot(NESTED_ROOT) != 0) {
    return 2;
  }
  int fd = openat(proc_dir, "self/mountinfo", O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    return 3;
  }
  size_t length = 0;
  read_all(fd, &length);
  return length == 0 ? 0 : 4;
}

/* A CLONE_NEWPID child, whose own PID namespace's numbers are not the
 * guest's, chroots and reads self/mountinfo through a kept /proc directory
 * descriptor: its own view, none. Exits with the child's status. */
static void nested_pid_checks(void) {
  int proc_dir = open("/proc", O_DIRECTORY | O_RDONLY | O_CLOEXEC);
  if (proc_dir < 0) {
    die("open /proc");
  }
  static char stack[1 << 16];
  pid_t child = clone(read_self_through_kept_proc, stack + sizeof(stack),
                      CLONE_NEWPID | SIGCHLD, &proc_dir);
  int status = 0;
  if (child < 0 || waitpid(child, &status, 0) != child || !WIFEXITED(status)) {
    _exit(10);
  }
  _exit(WEXITSTATUS(status));
}

/* Set by --report (with --chroot): print what each chroot check read
 * instead of requiring Linux's answer, for runs that compare one execution
 * with another (record and replay) rather than with Linux. */
static int report_only = 0;

/* Run `check` in a forked process and return its exit status, which must be
 * 0 unless --report. */
static int in_child(void (*check)(void), const char *what) {
  pid_t pid = fork();
  if (pid < 0) {
    die("fork");
  }
  if (pid == 0) {
    check();
  }
  int status = 0;
  if (waitpid(pid, &status, 0) != pid || !WIFEXITED(status) ||
      (WEXITSTATUS(status) != 0 && !report_only)) {
    fprintf(stderr, "rendered_mount_table: %s (status %d)\n", what, status);
    exit(1);
  }
  return WEXITSTATUS(status);
}

static int chroot_checks(int hermit) {
  char root[] = "/tmp/rendered-mount-table-root-XXXXXX";
  if (mkdtemp(root) == NULL) {
    die("mkdtemp");
  }
  size_t before_length = 0;
  char *before = slurp("/proc/self/mountinfo", 4096, &before_length);
  int direct = open("/proc/self/mountinfo", O_RDONLY | O_CLOEXEC);
  int alias = open("/proc/self/../self/mountinfo", O_RDONLY | O_CLOEXEC);
  if (direct < 0 || alias < 0) {
    die("open mountinfo before the chroot");
  }
  int ready[2], go[2];
  if (pipe(ready) != 0 || pipe(go) != 0) {
    die("pipe");
  }
  pid_t child = fork();
  if (child < 0) {
    die("fork");
  }
  if (child == 0) {
    char byte = 0;
    if (chroot(root) != 0) {
      if (errno == EPERM && !hermit) {
        _exit(77);
      }
      _exit(2);
    }
    if (write(ready[1], "x", 1) != 1 || read(go[0], &byte, 1) != 1) {
      _exit(3);
    }
    size_t length = 0;
    char *bytes = read_all(direct, &length);
    if (length != before_length || memcmp(bytes, before, length) != 0) {
      _exit(4);
    }
    bytes = read_all(alias, &length);
    if (length != before_length || memcmp(bytes, before, length) != 0) {
      _exit(5);
    }
    _exit(0);
  }
  close(ready[1]);
  char byte = 0;
  int status = 0;
  if (read(ready[0], &byte, 1) != 1) {
    if (waitpid(child, &status, 0) == child && WIFEXITED(status) &&
        WEXITSTATUS(status) == 77) {
      printf("chroot: not run (no CAP_SYS_CHROOT)\n");
      rmdir(root);
      return 0;
    }
    fail("the child did not chroot");
  }
  char path[64];
  snprintf(path, sizeof(path), "/proc/%d/mountinfo", child);
  size_t child_length = 0;
  char *child_view = slurp(path, 4096, &child_length);
  if (write(go[1], "x", 1) != 1) {
    die("write");
  }
  if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    fprintf(stderr,
            "rendered_mount_table: a descriptor opened before the chroot "
            "changed its view (child status %d)\n",
            status);
    exit(1);
  }
  if (report_only) {
    rmdir(root);
    printf("chroot report: descriptors opened before a chroot keep their "
           "view; a chrooted child's mountinfo, read by its parent, has %zu "
           "bytes\n",
           child_length);
    make_empty_dir(CLONE_FS_ROOT);
    int status = in_child(clone_fs_checks, "the CLONE_FS check failed");
    rmdir(CLONE_FS_ROOT);
    printf("chroot report: a CLONE_FS child's creator's mountinfo, read by a "
           "third process, is %s\n",
           status == 0 ? "empty" : "not empty");
    return 0;
  }
  if (child_length != 0) {
    fprintf(stderr,
            "rendered_mount_table: the chrooted child's mountinfo, read by "
            "its parent:\n%s",
            child_view);
    exit(1);
  }
  rmdir(root);
  printf("chroot: descriptors opened before a chroot keep their view; a "
         "chrooted child's mountinfo, read by its parent, is its own (0 "
         "rows)\n");
  make_empty_dir(CLONE_FS_ROOT);
  in_child(clone_fs_checks,
           "a CLONE_FS child's chroot did not move its creator's view");
  rmdir(CLONE_FS_ROOT);
  printf("chroot: a CLONE_FS child's chroot moves its creator's root, as a "
         "third process reading the creator's mountinfo sees (0 rows)\n");
  return 0;
}

/* --nested-pid: a separate mode, because a chrooted task that opens a procfs
 * file has no procfs mount below its root through which Detcore could name
 * that file's inode, which Detcore records as a determinism loss, so this
 * case cannot be verified at bitwise parity. */
static int nested_pid_mode(void) {
  make_empty_dir(NESTED_ROOT);
  in_child(nested_pid_checks,
           "a CLONE_NEWPID child's self/mountinfo was not its own view");
  rmdir(NESTED_ROOT);
  printf("nested pid: a chrooted CLONE_NEWPID child reading self/mountinfo "
         "through a kept /proc descriptor sees its own view (0 rows)\n");
  return 0;
}

int main(int argc, char **argv) {
  int chroot_mode = argc > 1 && strcmp(argv[1], "--chroot") == 0;
  int hermit = 0;
  for (int arg = 1; arg < argc; arg++) {
    hermit |= strcmp(argv[arg], "--hermit") == 0;
    kvm_local_procfs |= strcmp(argv[arg], "--kvm-local-procfs") == 0;
    report_only |= strcmp(argv[arg], "--report") == 0;
  }
  if (chroot_mode) {
    return chroot_checks(hermit);
  }
  if (argc > 1 && strcmp(argv[1], "--nested-pid") == 0) {
    return nested_pid_mode();
  }
  char path[128];

  size_t mountinfo_length = 0, mounts_length = 0;
  char *mountinfo = NULL, *mounts = NULL;
  read_both(hermit, &mountinfo, &mountinfo_length, &mounts, &mounts_length);

  if (hermit) {
    printf("--- mountinfo\n%s--- mounts\n%s---\n", mountinfo, mounts);
  }

  static char *info_rows[MAX_ROWS];
  static char *mounts_rows[MAX_ROWS];
  char *info_copy = strdup(mountinfo);
  char *mounts_copy = strdup(mounts);
  size_t rows = lines(info_copy, info_rows);
  if (rows == 0 || lines(mounts_copy, mounts_rows) != rows) {
    fail("mounts and mountinfo have different row counts");
  }
  for (size_t row = 0; row < rows; row++) {
    char info_mountpoint[PATH_MAX], mounts_mountpoint[PATH_MAX];
    char info_type[256], mounts_type[256], info_source[PATH_MAX],
        mounts_source[PATH_MAX], mount_options[4096], super_options[8192],
        options[8192], expected[16384];
    field(info_rows[row], 4, info_mountpoint, sizeof(info_mountpoint));
    field(info_rows[row], 5, mount_options, sizeof(mount_options));
    const char *tail = after_separator(info_rows[row]);
    field(tail, 0, info_type, sizeof(info_type));
    field(tail, 1, info_source, sizeof(info_source));
    field(tail, 2, super_options, sizeof(super_options));
    field(mounts_rows[row], 0, mounts_source, sizeof(mounts_source));
    field(mounts_rows[row], 1, mounts_mountpoint, sizeof(mounts_mountpoint));
    field(mounts_rows[row], 2, mounts_type, sizeof(mounts_type));
    field(mounts_rows[row], 3, options, sizeof(options));
    merged_options(mount_options, super_options, expected, sizeof(expected));
    if (strcmp(info_mountpoint, mounts_mountpoint) != 0 ||
        strcmp(info_type, mounts_type) != 0 ||
        strcmp(info_source, mounts_source) != 0 ||
        strcmp(options, expected) != 0) {
      fprintf(stderr, "rendered_mount_table: row %zu: %s vs %s (expected %s)\n",
              row, info_rows[row], mounts_rows[row], expected);
      exit(1);
    }
  }
  printf("mountinfo and mounts: every spelling reads the same bytes; %s\n",
         "mounts has mountinfo's rows, in order, with show_vfsmnt's options");

  check_size_zero("/proc/self/mountinfo");
  check_size_zero("/proc/self/mounts");
  check_size_zero("/proc/mounts");
  printf("size: 0 for both files, and SEEK_END is EINVAL\n");

  /* The nearest mount holding a /tmp file names its mountinfo row. */
  char probe[] = "/tmp/rendered-mount-table-XXXXXX";
  int fd = mkstemp(probe);
  if (fd < 0) {
    die("mkstemp under /tmp");
  }
  unsigned long long from_fdinfo = fdinfo_mount_id(fd);
  unsigned long long from_statx = statx_mount_id(fd);
  close(fd);
  char holder[PATH_MAX];
  unsigned long long nearest =
      nearest_holder(info_rows, rows, probe, holder, sizeof(holder));
  unlink(probe);
  if (from_fdinfo != from_statx || from_fdinfo != nearest) {
    fprintf(stderr,
            "rendered_mount_table: /tmp file: fdinfo mnt_id %llu, statx %llu, "
            "nearest row %llu at %s\n",
            from_fdinfo, from_statx, nearest, holder);
    exit(1);
  }
  printf("fdinfo: a /tmp file's mnt_id and STATX_MNT_ID (asked for or not) "
         "name the row of the nearest mount holding it\n");

  /* The program's own file: wherever it lives (a host mount the table may
   * hide, or a backend's own mount over it), its mnt_id names the nearest row
   * holding it. */
  char program[PATH_MAX];
  ssize_t program_length =
      readlink("/proc/self/exe", program, sizeof(program) - 1);
  if (program_length < 0) {
    die("readlink /proc/self/exe");
  }
  program[program_length] = '\0';
  int exe = open(program, O_RDONLY | O_CLOEXEC);
  if (exe < 0) {
    die(program);
  }
  unsigned long long exe_fdinfo = fdinfo_mount_id(exe);
  unsigned long long exe_statx = statx_mount_id(exe);
  close(exe);
  char program_mount[PATH_MAX];
  unsigned long long exe_nearest = nearest_holder(
      info_rows, rows, program, program_mount, sizeof(program_mount));
  if (exe_fdinfo != exe_statx || exe_fdinfo != exe_nearest) {
    fprintf(stderr,
            "rendered_mount_table: program: fdinfo mnt_id %llu, statx %llu, "
            "nearest row %llu at %s\n",
            exe_fdinfo, exe_statx, exe_nearest, program_mount);
    exit(1);
  }
  if (hermit) {
    printf("program: its mnt_id and STATX_MNT_ID name the nearest row holding "
           "it, at %s\n",
           program_mount);
  } else {
    printf("program: its mnt_id and STATX_MNT_ID name the nearest row holding "
           "it\n");
  }

  /* A rendered file's mount, by every spelling: its fdinfo, its path, its
   * descriptor, and a symbolic link to its descriptor link. */
  int rendered = open("/proc/self/mountinfo", O_RDONLY | O_CLOEXEC);
  if (rendered < 0) {
    die("open /proc/self/mountinfo");
  }
  char link[] = "/tmp/rendered-mount-table-link-XXXXXX";
  int placeholder = mkstemp(link);
  if (placeholder < 0) {
    die("mkstemp");
  }
  close(placeholder);
  unlink(link);
  char target[64];
  snprintf(target, sizeof(target), "/proc/self/fd/%d", rendered);
  if (symlink(target, link) != 0) {
    die("symlink");
  }
  unsigned long long by_fdinfo = fdinfo_mount_id(rendered);
  unsigned long long by_descriptor = statx_mount_id(rendered);
  unsigned long long by_path =
      statx_mount_id_at(AT_FDCWD, "/proc/self/mountinfo", 0, STATX_MNT_ID);
  if (by_fdinfo != by_descriptor || by_fdinfo != by_path) {
    fprintf(stderr,
            "rendered_mount_table: mountinfo's mount: fdinfo %llu, descriptor "
            "%llu, path %llu\n",
            by_fdinfo, by_descriptor, by_path);
    exit(1);
  }
  if (!names_mount(info_rows, rows, by_fdinfo)) {
    fail("mountinfo's own mount names no row");
  }
  printf("statx: a rendered file's STATX_MNT_ID is its fdinfo mnt_id by its "
         "path and its descriptor\n");
  if (kvm_local_procfs) {
    fprintf(stderr, "rendered_mount_table: skipped: statx through a symbolic "
                    "link to a descriptor link (--kvm-local-procfs)\n");
  } else {
    unsigned long long by_link =
        statx_mount_id_at(AT_FDCWD, link, 0, STATX_MNT_ID);
    if (by_link != by_fdinfo) {
      fprintf(stderr,
              "rendered_mount_table: mountinfo's mount: fdinfo %llu, link "
              "%llu\n",
              by_fdinfo, by_link);
      exit(1);
    }
    printf("statx: and through a symbolic link to its descriptor link\n");
  }
  unlink(link);
  close(rendered);

  errno = 0;
  if (open("/proc/mounts", O_RDONLY | O_NOFOLLOW | O_CLOEXEC) != -1 ||
      errno != ELOOP) {
    fprintf(stderr, "rendered_mount_table: /proc/mounts O_NOFOLLOW: errno %d\n",
            errno);
    exit(1);
  }
  if (!hermit) {
    char link_target[64];
    ssize_t length =
        readlink("/proc/mounts", link_target, sizeof(link_target) - 1);
    if (length < 0) {
      die("readlink /proc/mounts");
    }
    link_target[length] = '\0';
    if (strcmp(link_target, "self/mounts") != 0) {
      fprintf(stderr, "rendered_mount_table: /proc/mounts -> %s\n", link_target);
      exit(1);
    }
  }
  printf("link: /proc/mounts with O_NOFOLLOW is ELOOP\n");

  /* A zombie has no mount namespace; a reaped process has no directory. */
  pid_t child = fork();
  if (child < 0) {
    die("fork");
  }
  if (child == 0) {
    _exit(0);
  }
  siginfo_t info;
  memset(&info, 0, sizeof(info));
  if (waitid(P_PID, (id_t)child, &info, WEXITED | WNOWAIT) != 0) {
    die("waitid WNOWAIT");
  }
  snprintf(path, sizeof(path), "/proc/%d/mountinfo", child);
  errno = 0;
  int zombie = open(path, O_RDONLY | O_CLOEXEC);
  int zombie_errno = errno;
  if (waitpid(child, NULL, 0) != child) {
    die("waitpid");
  }
  if (zombie != -1 || zombie_errno != EINVAL) {
    fprintf(stderr, "rendered_mount_table: zombie mountinfo: fd %d errno %d\n",
            zombie, zombie_errno);
    exit(1);
  }
  errno = 0;
  if (open(path, O_RDONLY | O_CLOEXEC) != -1 || errno != ENOENT) {
    fprintf(stderr, "rendered_mount_table: reaped mountinfo: errno %d\n",
            errno);
    exit(1);
  }
  printf("exit: a zombie's mountinfo is EINVAL, a reaped process's ENOENT\n");

  free(info_copy);
  free(mounts_copy);
  free(mountinfo);
  free(mounts);
  return 0;
}
