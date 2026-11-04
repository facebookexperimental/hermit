/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * proc_link_targets: readlink of the caller's own /proc/<pid>/fd/<n>, exe
 * and cwd links for each kind of object a link names, as Linux answers them
 * (proc_fd_link, fs/proc/fd.c; proc_exe_link and proc_cwd_link,
 * fs/proc/base.c; each rendered by d_path, fs/d_path.c, in
 * do_proc_readlink). Under Hermit on KVM, Detcore answers them from the
 * backend's link_path fact (package K slice F2-h).
 *
 * For each descriptor it prints the target read through /proc/self/fd/<n>
 * and how many of six spellings read the same target: /proc/self/fd/<n>,
 * /proc/<pid>/fd/<n>, /proc/thread-self/fd/<n>,
 * /proc/<pid>/task/<tid>/fd/<n>, /dev/fd/<n>, and readlinkat of "<n>" from
 * a descriptor of /proc/self/fd. The kinds: a regular file; one renamed
 * after its open; one unlinked after its open ("<path> (deleted)"); a
 * directory, and one removed after its open; an O_PATH descriptor; /dev/null;
 * both ends of a pipe and a socket of a socket pair ("pipe:[<inode>]" and
 * "socket:[<inode>]", whose inode is the one fstat reports); an eventfd, an
 * epoll instance, a timerfd and a signalfd ("anon_inode:[eventfd]",
 * "anon_inode:[eventpoll]", "anon_inode:[timerfd]",
 * "anon_inode:[signalfd]"); a memfd ("/memfd:<name> (deleted)"); and a
 * named FIFO opened by its path (the path, not "pipe:[<inode>]"). Then:
 * a closed descriptor (ENOENT); a 3-byte buffer (3 bytes, no NUL, the rest
 * untouched), also for a pipe's; a zero size (EINVAL); an unmapped buffer
 * (EFAULT); the links of a file and a pipe followed by a final slash
 * (ENOTDIR) and of a directory and a removed one (EINVAL); a
 * second thread's reads of the process's descriptors; a fork's child's
 * reads of an inherited descriptor and of one it opened (after the
 * signalfd is closed: KVM creates no process while one is open); the exe link
 * (argv[0]'s realpath); and the working directory's link after the
 * directory is renamed, and after it is removed ("<path> (deleted)", while
 * getcwd fails with ENOENT), by seven spellings. Then a "." after a link,
 * which Linux's walk looks up in what the link leads to (link_path_walk,
 * fs/namei.c): ENOTDIR for a file, a pipe and the image, EINVAL for a
 * directory (searched, then no link), ENOENT for a closed descriptor, by
 * the fd, cwd, root and exe links; and the links of descriptors open on the
 * process's own /proc/self/status and /proc/self/maps ("/proc/<pid>/status",
 * "/proc/<pid>/maps", printed as whether they name the caller's pid), also
 * as a fork's child reads the inherited status descriptor (its opener's
 * pid). Paths below the scratch directory are printed from "<dir>". A
 * child's failed checks fail its parent. It removes what it made in DIR
 * before it ends, so that a second run in DIR (--verify) starts from the
 * same directory.
 *
 * With --sibling-directories it instead reads another live thread's cwd
 * and root links (/proc/self/task/<tid>/cwd and root): the working and root
 * directories the threads share (CLONE_FS), "<dir>" and "/".
 *
 * With --directory-links it instead reads the caller's own descriptor links
 * through a link to one of its own descriptor directories: Linux follows
 * the descriptor link D of a descriptor open on /proc/self/fd (a link to
 * the directory /proc/<pid>/fd, proc_fd_link) and then reads the link N in
 * that directory, so /proc/self/fd/D/N reads descriptor N's target. It
 * prints, for a regular file in DIR and for /dev/null, that target by the
 * five absolute spellings of D's link (/proc/self/fd/D, /proc/<pid>/fd/D,
 * /proc/thread-self/fd/D, /proc/<pid>/task/<tid>/fd/D, /dev/fd/D) and by
 * readlinkat(D, "N"); then through descriptors open on /proc/thread-self/fd
 * and on /dev/fd, through one open on /proc (/proc/self/fd/P/self/fd/N and
 * readlinkat(P, "self/fd/N")), through D twice (/proc/self/fd/D/D/N), and
 * relative to D and to P (readlinkat(D, "D/N"), readlinkat(P,
 * "self/fd/D/N")); the link of D itself read through D ("/proc/<pid>/fd",
 * printed with "<pid>" where it names the caller's pid); a closed
 * descriptor through D (ENOENT); the file's link through D followed by a
 * final slash and by "/." (ENOTDIR); the file's link through D and a ".."
 * that climbs out of the directory D leads to (/proc/self/fd/D/../fd/N
 * and /proc/self/fd/D/../../self/fd/N); and each side of Linux's 40-link
 * budget (MAXSYMLINKS) for /dev/null's link: 19 and 20 "P/self/fd/"
 * segments after /proc/self/fd/, then "N" (39 and 41 links: the target,
 * then ELOOP), and 19 then "N/" (40 links, N's own link followed:
 * ENOTDIR); 39 and 40 "D/" segments then "N" (40 and 41 links); and 38
 * and 39 "D/" segments then "N/" or "N/." (40 and 41 links, N's own link
 * followed: ENOTDIR, then ELOOP); 19 and 20 "P/self/fd/" segments then
 * D's own link (39 links: "/proc/<pid>/fd"; then 41: ELOOP); and, after 39
 * "D/" segments (40 links), a descriptor proved closed (fcntl F_GETFD)
 * followed by "/" and by "/.", and a name that is no number followed by
 * "/.": ENOENT, since Linux charges a link only where one exists to follow;
 * and 39 "D/" segments then "../fd/N" (40 links: a ".." follows none).
 *
 * With --directory-links --short-kernel-lookups it leaves out the
 * link-budget lines that the kernel answers on ptrace by a lookup of 21 to
 * 40 links (long_kernel_lookups).
 *
 * With --directory-links --native-oracle (the native run the tests compare
 * with) a link-budget probe retries an ELOOP (link_attempts); under Hermit
 * each observation is one lookup.
 *
 * usage: proc_link_targets DIR [--sibling-directories | --directory-links
 *        [--short-kernel-lookups | --native-oracle]]
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/mman.h>
#include <sys/signalfd.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/timerfd.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

static int failures;
static const char *dir;

/* Whether --directory-links probes the link-budget lines whose answer, on
 * ptrace, is the host kernel's lookup of 21 to 40 links (the
 * show_kernel_budget calls, the one list of them): 0 with
 * --short-kernel-lookups, which the ptrace test passes, and which prints
 * each such line as "known-gap #5045: <label>" without looking it up. Such
 * a lookup can fail with ELOOP by host timing
 * (https://github.com/rrnewton/hermit/issues/5045: when something aborts
 * the RCU walk, filename_lookup retries it in ref-walk mode with the links
 * already counted kept in total_link_count, fs/namei.c#n671 and
 * #n2682-n2685), and ptrace passes the kernel's answer through; looked up
 * at all, the retry would add events to the run's log. Probed in that
 * mode as always: the lines Detcore answers itself (D's own link through
 * P), the 41-link lines (ELOOP on every walk) and lines of 20 links or
 * fewer. Remove the mode, and its use in hermit-cli/tests/cli.rs, once
 * #5045 is fixed. */
static int long_kernel_lookups = 1;
static size_t dir_len;

#define CHECK(cond, ...)                                   \
  do {                                                     \
    if (!(cond)) {                                         \
      failures++;                                          \
      fprintf(stderr, "FAIL %s:%d: ", __FILE__, __LINE__); \
      fprintf(stderr, __VA_ARGS__);                        \
      fprintf(stderr, "\n");                               \
    }                                                      \
  } while (0)

static const char *errno_name(int error) {
  switch (error) {
    case 0:
      return "0";
    case ENOENT:
      return "ENOENT";
    case EINVAL:
      return "EINVAL";
    case EFAULT:
      return "EFAULT";
    case ENOTDIR:
      return "ENOTDIR";
    case EACCES:
      return "EACCES";
    case ELOOP:
      return "ELOOP";
    case EBADF:
      return "EBADF";
    case ENAMETOOLONG:
      return "ENAMETOOLONG";
    default: {
      static char number[16];
      snprintf(number, sizeof(number), "%d", error);
      return number;
    }
  }
}

static pid_t gettid_raw(void) {
  return (pid_t)syscall(SYS_gettid);
}

/* `text` with the scratch directory's prefix printed as "<dir>". */
static void print_target(const char *label, const char *text) {
  if (strncmp(text, dir, dir_len) == 0 &&
      (text[dir_len] == '/' || text[dir_len] == ' ' || text[dir_len] == 0)) {
    printf("%s=<dir>%s\n", label, text + dir_len);
  } else {
    printf("%s=%s\n", label, text);
  }
}

/* readlink of `path` into `out` (PATH_MAX bytes), NUL-terminated; the
 * result, or -1 with errno. */
/* How many times read_link asks before it takes ELOOP for an answer: 1,
 * except for the link-budget probes (show_budget_spelled) of the native
 * oracle (--native-oracle, which the tests pass only to the native run;
 * under Hermit every observation is one lookup). Linux v6.17
 * keeps the links an aborted RCU walk counted when it retries the lookup in
 * ref-walk mode (filename_lookup, fs/namei.c#n2682-n2685, with
 * total_link_count carried from set_nameidata, #n671), so a lookup of
 * exactly MAXSYMLINKS links can fail with ELOOP when another task changes
 * the dentries it walked (https://github.com/rrnewton/hermit/issues/5045,
 * which a lookup of 21 to 40 links can meet); measured on a loaded host,
 * 200 native runs of --directory-links: 10 to 17 ELOOPs on each 40-link
 * line, none on a 39- or 41-link line. The retries keep the native
 * transcript stable; each budget line's attempts are printed to standard
 * error ("attempts: <label> <n>"), which the tests log and do not compare. */
static int link_attempts = 1;
/* Whether this is the native oracle's run (--native-oracle). */
static int native_oracle = 0;
/* How many lookups read_link's last call made. */
static int last_link_attempts = 0;

static ssize_t read_link(const char *path, char *out) {
  ssize_t n;
  for (int attempt = 1;; attempt++) {
    n = readlink(path, out, PATH_MAX - 1);
    last_link_attempts = attempt;
    if (n >= 0 || errno != ELOOP || attempt >= link_attempts) {
      break;
    }
  }
  if (n >= 0) {
    out[n] = 0;
  }
  return n;
}

static int proc_fd_directory = -1;

/* How many of the six spellings of descriptor `fd`'s link read `expected`;
 * each that does not is reported on standard error. */
static int same_spellings(int fd, const char *expected) {
  char paths[5][96];
  pid_t pid = getpid();
  pid_t tid = gettid_raw();
  snprintf(paths[0], sizeof(paths[0]), "/proc/self/fd/%d", fd);
  snprintf(paths[1], sizeof(paths[1]), "/proc/%d/fd/%d", (int)pid, fd);
  snprintf(paths[2], sizeof(paths[2]), "/proc/thread-self/fd/%d", fd);
  snprintf(
      paths[3],
      sizeof(paths[3]),
      "/proc/%d/task/%d/fd/%d",
      (int)pid,
      (int)tid,
      fd);
  snprintf(paths[4], sizeof(paths[4]), "/dev/fd/%d", fd);
  int same = 0;
  char got[PATH_MAX];
  for (int i = 0; i < 5; i++) {
    ssize_t n = read_link(paths[i], got);
    if (n >= 0 && strcmp(got, expected) == 0) {
      same++;
    } else {
      fprintf(
          stderr,
          "spelling %s: %s (errno %s), expected %s\n",
          paths[i],
          n >= 0 ? got : "-1",
          errno_name(n >= 0 ? 0 : errno),
          expected);
    }
  }
  char name[16];
  snprintf(name, sizeof(name), "%d", fd);
  ssize_t n = readlinkat(proc_fd_directory, name, got, PATH_MAX - 1);
  if (n >= 0) {
    got[n] = 0;
  }
  if (n >= 0 && strcmp(got, expected) == 0) {
    same++;
  } else {
    fprintf(
        stderr,
        "spelling readlinkat(/proc/self/fd, %s): %s (errno %s), expected %s\n",
        name,
        n >= 0 ? got : "-1",
        errno_name(n >= 0 ? 0 : errno),
        expected);
  }
  return same;
}

/* Print descriptor `fd`'s link target, as `label`, and how many spellings
 * agree. */
static void show(const char *label, int fd) {
  char path[64];
  char target[PATH_MAX];
  snprintf(path, sizeof(path), "/proc/self/fd/%d", fd);
  ssize_t n = read_link(path, target);
  if (n < 0) {
    printf("%s=-1 errno=%s\n", label, errno_name(errno));
    failures++;
    return;
  }
  print_target(label, target);
  printf("%s: spellings=6 same=%d\n", label, same_spellings(fd, target));
}

/* Print whether descriptor `fd`'s link is "<kind>:[<inode>]" with the inode
 * its fstat reports, which differs from run to run natively. */
static void show_anonymous(const char *label, const char *kind, int fd) {
  char path[64];
  char target[PATH_MAX];
  char expected[96];
  struct stat st;
  snprintf(path, sizeof(path), "/proc/self/fd/%d", fd);
  ssize_t n = read_link(path, target);
  CHECK(fstat(fd, &st) == 0, "fstat %s: %s", label, strerror(errno));
  snprintf(expected, sizeof(expected), "%s:[%lu]", kind, (unsigned long)st.st_ino);
  if (n < 0) {
    printf("%s=-1 errno=%s\n", label, errno_name(errno));
    failures++;
    return;
  }
  printf(
      "%s: names %s:[<its fstat inode>]=%d\n",
      label,
      kind,
      strcmp(target, expected) == 0);
  printf("%s: spellings=6 same=%d\n", label, same_spellings(fd, target));
}

static void check_buffers(int fd) {
  char path[64];
  char full[PATH_MAX];
  snprintf(path, sizeof(path), "/proc/self/fd/%d", fd);
  CHECK(read_link(path, full) > 3, "the file's link is short");

  char small[8];
  memset(small, '#', sizeof(small));
  errno = 0;
  ssize_t n = readlink(path, small, 3);
  int untouched = 1;
  for (size_t i = 3; i < sizeof(small); i++) {
    untouched &= small[i] == '#';
  }
  printf(
      "fd: 3-byte buffer n=%zd errno=%s written=%d untouched=%d\n",
      n,
      errno_name(n < 0 ? errno : 0),
      n == 3 && memcmp(small, full, 3) == 0,
      untouched);

  errno = 0;
  n = readlink(path, small, 0);
  printf("fd: size 0=%zd errno=%s\n", n, errno_name(n < 0 ? errno : 0));

  void *unmapped =
      mmap(NULL, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  CHECK(unmapped != MAP_FAILED, "mmap: %s", strerror(errno));
  CHECK(munmap(unmapped, 4096) == 0, "munmap: %s", strerror(errno));
  errno = 0;
  n = readlink(path, (char *)unmapped, 64);
  printf(
      "fd: unwritable buffer=%zd errno=%s\n", n, errno_name(n < 0 ? errno : 0));
}

/* readlink of descriptor `fd`'s link followed by a final slash. */
static void show_followed(const char *label, int fd) {
  char path[64];
  char got[PATH_MAX];
  snprintf(path, sizeof(path), "/proc/self/fd/%d/", fd);
  errno = 0;
  ssize_t n = read_link(path, got);
  printf("followed: %s fd/=%zd errno=%s\n", label, n, errno_name(n < 0 ? errno : 0));
}

/* An anonymous object's link cut to a 3-byte buffer. */
static void check_anonymous_buffer(int fd) {
  char path[64];
  char small[8];
  snprintf(path, sizeof(path), "/proc/self/fd/%d", fd);
  memset(small, '#', sizeof(small));
  errno = 0;
  ssize_t n = readlink(path, small, 3);
  int untouched = 1;
  for (size_t i = 3; i < sizeof(small); i++) {
    untouched &= small[i] == '#';
  }
  printf(
      "pipe: 3-byte buffer n=%zd errno=%s written=%d untouched=%d\n",
      n,
      errno_name(n < 0 ? errno : 0),
      n == 3 && memcmp(small, "pip", 3) == 0,
      untouched);
}

/* readlink of `path`, printed as `label` with the target or the errno. */
static void show_path(const char *label, const char *path) {
  char got[PATH_MAX];
  errno = 0;
  ssize_t n = read_link(path, got);
  if (n >= 0) {
    print_target(label, got);
  } else {
    printf("%s=-1 errno=%s\n", label, errno_name(errno));
  }
}

/* readlink of descriptor `fd`'s link with `tail` after it ("/." and the
 * like): a lookup in what the link leads to. */
static void show_walked(const char *label, const char *spelling, int fd, const char *tail) {
  char path[96];
  char shown[160];
  snprintf(path, sizeof(path), spelling, fd);
  strncat(path, tail, sizeof(path) - strlen(path) - 1);
  snprintf(shown, sizeof(shown), "walked: %s fd%s", label, tail);
  show_path(shown, path);
}

/* Print whether descriptor `fd`, open on the caller's own /proc/self/<name>,
 * links to "/proc/<the caller's pid>/<name>", and how many spellings agree. */
static void show_own_proc_file(const char *label, int fd, const char *name) {
  char path[64];
  char target[PATH_MAX];
  char expected[96];
  snprintf(path, sizeof(path), "/proc/self/fd/%d", fd);
  snprintf(expected, sizeof(expected), "/proc/%d/%s", (int)getpid(), name);
  ssize_t n = read_link(path, target);
  if (n < 0) {
    printf("%s=-1 errno=%s\n", label, errno_name(errno));
    failures++;
    return;
  }
  printf("%s: names /proc/<pid>/%s=%d\n", label, name, strcmp(target, expected) == 0);
  printf("%s: spellings=6 same=%d\n", label, same_spellings(fd, target));
}

static int thread_fd;

static void *thread_main(void *argument) {
  (void)argument;
  show("thread: the file", thread_fd);
  return NULL;
}

/* The seven spellings of the caller's own working-directory link. */
static void show_cwd(const char *label) {
  char paths[7][96];
  pid_t pid = getpid();
  pid_t tid = gettid_raw();
  snprintf(paths[0], sizeof(paths[0]), "/proc/self/cwd");
  snprintf(paths[1], sizeof(paths[1]), "/proc/%d/cwd", (int)pid);
  snprintf(paths[2], sizeof(paths[2]), "/proc/thread-self/cwd");
  snprintf(paths[3], sizeof(paths[3]), "/proc/self/task/%d/cwd", (int)tid);
  snprintf(
      paths[4], sizeof(paths[4]), "/proc/%d/task/%d/cwd", (int)pid, (int)tid);
  snprintf(paths[5], sizeof(paths[5]), "//proc/./self/cwd");
  char target[PATH_MAX];
  ssize_t n = read_link(paths[0], target);
  if (n < 0) {
    printf("%s: cwd=-1 errno=%s\n", label, errno_name(errno));
    failures++;
    return;
  }
  char cwd_label[96];
  snprintf(cwd_label, sizeof(cwd_label), "%s: cwd", label);
  print_target(cwd_label, target);
  int same = 0;
  char got[PATH_MAX];
  for (int i = 0; i < 6; i++) {
    if (read_link(paths[i], got) >= 0 && strcmp(got, target) == 0) {
      same++;
    } else {
      fprintf(stderr, "cwd spelling %s differs\n", paths[i]);
    }
  }
  int proc = open("/proc", O_RDONLY | O_DIRECTORY);
  CHECK(proc >= 0, "open /proc: %s", strerror(errno));
  n = readlinkat(proc, "self/cwd", got, PATH_MAX - 1);
  if (n >= 0) {
    got[n] = 0;
  }
  if (n >= 0 && strcmp(got, target) == 0) {
    same++;
  } else {
    fprintf(stderr, "cwd spelling readlinkat(/proc, self/cwd) differs\n");
  }
  close(proc);
  printf("%s: cwd spellings=7 same=%d\n", label, same);
}

static void check_cwd(void) {
  char path[PATH_MAX];
  snprintf(path, sizeof(path), "%s/wd", dir);
  CHECK(mkdir(path, 0755) == 0, "mkdir %s: %s", path, strerror(errno));
  CHECK(chdir(path) == 0, "chdir %s: %s", path, strerror(errno));
  show_cwd("entered");
  char moved[PATH_MAX];
  snprintf(moved, sizeof(moved), "%s/moved", dir);
  CHECK(rename(path, moved) == 0, "rename: %s", strerror(errno));
  show_cwd("renamed");
  CHECK(rmdir(moved) == 0, "rmdir: %s", strerror(errno));
  show_cwd("removed");
  char buffer[PATH_MAX];
  errno = 0;
  char *got = getcwd(buffer, sizeof(buffer));
  printf(
      "removed: getcwd=%s errno=%s\n",
      got ? "non-NULL" : "NULL",
      errno_name(got ? 0 : errno));
  CHECK(chdir(dir) == 0, "chdir back: %s", strerror(errno));
}

static void check_exe(const char *argv0) {
  char real[PATH_MAX];
  char target[PATH_MAX];
  CHECK(realpath(argv0, real) != NULL, "realpath: %s", strerror(errno));
  ssize_t n = read_link("/proc/self/exe", target);
  printf("exe: is argv[0]'s realpath=%d\n", n >= 0 && strcmp(real, target) == 0);
}

static void check_child(int inherited, int status_fd) {
  pid_t child = fork();
  CHECK(child >= 0, "fork: %s", strerror(errno));
  if (child == 0) {
    failures = 0;
    /* The parent's /proc/self/fd descriptor stays bound to the parent. */
    close(proc_fd_directory);
    proc_fd_directory =
        open("/proc/self/fd", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    CHECK(proc_fd_directory >= 0, "open /proc/self/fd: %s", strerror(errno));
    show("child: the inherited file", inherited);
    /* The inherited descriptor names its opener's file, the parent's. */
    char status_path[64];
    char status_target[PATH_MAX];
    char status_expected[96];
    snprintf(status_path, sizeof(status_path), "/proc/self/fd/%d", status_fd);
    snprintf(status_expected, sizeof(status_expected), "/proc/%d/status", (int)getppid());
    ssize_t status_n = read_link(status_path, status_target);
    printf(
        "child: the inherited status names its opener's /proc/<pid>/status=%d\n",
        status_n >= 0 && strcmp(status_target, status_expected) == 0);
    char path[PATH_MAX];
    snprintf(path, sizeof(path), "%s/child-file", dir);
    int fd = open(path, O_RDWR | O_CREAT | O_CLOEXEC, 0644);
    CHECK(fd >= 0, "open %s: %s", path, strerror(errno));
    show("child: its own file", fd);
    fflush(stdout);
    _exit(failures == 0 ? 0 : 1);
  }
  int status = 0;
  CHECK(waitpid(child, &status, 0) == child, "waitpid: %s", strerror(errno));
  printf(
      "child exited=%d\n", WIFEXITED(status) ? WEXITSTATUS(status) : 128);
  if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    failures++;
  }
}

static int sibling_tid;
static int sibling_ready[2];
static int sibling_release[2];

static void *sibling_main(void *argument) {
  (void)argument;
  sibling_tid = (int)gettid_raw();
  char byte = 1;
  CHECK(write(sibling_ready[1], &byte, 1) == 1, "write: %s", strerror(errno));
  CHECK(read(sibling_release[0], &byte, 1) == 1, "read: %s", strerror(errno));
  return NULL;
}

/* Another live thread's working and root directory links, which it shares
 * with the caller (CLONE_FS). */
static int check_sibling_directories(void) {
  CHECK(pipe2(sibling_ready, O_CLOEXEC) == 0, "pipe2: %s", strerror(errno));
  CHECK(pipe2(sibling_release, O_CLOEXEC) == 0, "pipe2: %s", strerror(errno));
  pthread_t thread;
  CHECK(
      pthread_create(&thread, NULL, sibling_main, NULL) == 0,
      "pthread_create failed");
  char byte;
  CHECK(read(sibling_ready[0], &byte, 1) == 1, "read: %s", strerror(errno));
  char path[96];
  snprintf(path, sizeof(path), "/proc/self/task/%d/cwd", sibling_tid);
  show_path("sibling: self/task/<tid>/cwd", path);
  snprintf(path, sizeof(path), "/proc/self/task/%d/root", sibling_tid);
  show_path("sibling: self/task/<tid>/root", path);
  byte = 1;
  CHECK(write(sibling_release[1], &byte, 1) == 1, "write: %s", strerror(errno));
  CHECK(pthread_join(thread, NULL) == 0, "pthread_join failed");
  printf("failures=%d\n", failures);
  return failures == 0 ? 0 : 1;
}

/* readlink of `path`, printed as `label`, with "/proc/<the caller's
 * pid>/" printed as "/proc/<pid>/". */
static void show_path_own_pid(const char *label, const char *path) {
  char got[PATH_MAX];
  char own[32];
  errno = 0;
  ssize_t n = read_link(path, got);
  if (n < 0) {
    printf("%s=-1 errno=%s\n", label, errno_name(errno));
    return;
  }
  int own_len = snprintf(own, sizeof(own), "/proc/%d/", (int)getpid());
  if (strncmp(got, own, own_len) == 0) {
    printf("%s=/proc/<pid>/%s\n", label, got + own_len);
  } else {
    print_target(label, got);
  }
}

/* readlinkat of `name` from `dirfd`, printed as `label`. */
static void show_at(const char *label, int dirfd, const char *name) {
  char got[PATH_MAX];
  errno = 0;
  ssize_t n = readlinkat(dirfd, name, got, PATH_MAX - 1);
  if (n >= 0) {
    got[n] = 0;
    print_target(label, got);
  } else {
    printf("%s=-1 errno=%s\n", label, errno_name(errno));
  }
}

/* Descriptor `fd`'s link read through a link to the caller's own
 * descriptor directory, by each spelling --directory-links names. */
static void show_through_directories(
    const char *label,
    int fd,
    int self_fd,
    int thread_fd,
    int dev_fd,
    int proc) {
  char path[160];
  char shown[160];
  int pid = (int)getpid();
  int tid = (int)gettid_raw();
  const char *spellings[] = {
      "/proc/self/fd/%d/%d",
      "/proc/%d/fd/%d/%d",
      "/proc/thread-self/fd/%d/%d",
      "/proc/%d/task/%d/fd/%d/%d",
      "/dev/fd/%d/%d",
  };
  const char *names[] = {
      "self/fd/D/N", "<pid>/fd/D/N", "thread-self/fd/D/N",
      "<pid>/task/<tid>/fd/D/N", "/dev/fd/D/N"};
  for (int i = 0; i < 5; i++) {
    switch (i) {
      case 1:
        snprintf(path, sizeof(path), spellings[i], pid, self_fd, fd);
        break;
      case 3:
        snprintf(path, sizeof(path), spellings[i], pid, tid, self_fd, fd);
        break;
      default:
        snprintf(path, sizeof(path), spellings[i], self_fd, fd);
    }
    snprintf(shown, sizeof(shown), "through: %s %s", label, names[i]);
    show_path(shown, path);
  }
  char name[32];
  snprintf(name, sizeof(name), "%d", fd);
  snprintf(shown, sizeof(shown), "through: %s readlinkat(D, N)", label);
  show_at(shown, self_fd, name);
  snprintf(path, sizeof(path), "/proc/self/fd/%d/%d", thread_fd, fd);
  snprintf(shown, sizeof(shown), "through: %s self/fd/<thread-self/fd>/N", label);
  show_path(shown, path);
  snprintf(path, sizeof(path), "/proc/self/fd/%d/%d", dev_fd, fd);
  snprintf(shown, sizeof(shown), "through: %s self/fd/</dev/fd>/N", label);
  show_path(shown, path);
  snprintf(path, sizeof(path), "/proc/self/fd/%d/self/fd/%d", proc, fd);
  snprintf(shown, sizeof(shown), "through: %s self/fd/</proc>/self/fd/N", label);
  show_path(shown, path);
  snprintf(name, sizeof(name), "self/fd/%d", fd);
  snprintf(shown, sizeof(shown), "through: %s readlinkat(</proc>, self/fd/N)", label);
  show_at(shown, proc, name);
  snprintf(path, sizeof(path), "/proc/self/fd/%d/%d/%d", self_fd, self_fd, fd);
  snprintf(shown, sizeof(shown), "through: %s self/fd/D/D/N", label);
  show_path(shown, path);
  snprintf(name, sizeof(name), "%d/%d", self_fd, fd);
  snprintf(shown, sizeof(shown), "through: %s readlinkat(D, D/N)", label);
  show_at(shown, self_fd, name);
  snprintf(name, sizeof(name), "self/fd/%d/%d", self_fd, fd);
  snprintf(shown, sizeof(shown), "through: %s readlinkat(</proc>, self/fd/D/N)", label);
  show_at(shown, proc, name);
}

/* readlink of "/proc/self/fd/" followed by `count` segments of descriptor
 * `dir` (each "D/", or "P/self/fd/" where `below_proc`), then descriptor
 * `target` (named `name` in the label) and `tail`, printed by `show`: a
 * lookup that follows 1 + count links (self, then each segment's
 * descriptor link; 2 per P segment, whose self is a link again), plus 1
 * where `tail` makes it follow `target`'s link, if that link exists. Linux
 * allows 40 (MAXSYMLINKS). */
static void show_budget_spelled(
    const char *label,
    int dir,
    int count,
    int below_proc,
    const char *target,
    const char *name,
    const char *tail,
    void (*show)(const char *, const char *)) {
  char path[4096];
  char shown[128];
  int used = snprintf(path, sizeof(path), "/proc/self/fd/");
  for (int i = 0; i < count; i++) {
    used += snprintf(
        path + used, sizeof(path) - (size_t)used, "%d/%s", dir, below_proc ? "self/fd/" : "");
  }
  snprintf(path + used, sizeof(path) - (size_t)used, "%s%s", target, tail);
  snprintf(shown, sizeof(shown), "budget: %d %s segments %s%s", count, label, name, tail);
  link_attempts = native_oracle ? 8 : 1;
  show(shown, path);
  link_attempts = 1;
  if (native_oracle) {
    fprintf(stderr, "attempts: %s %d\n", shown, last_link_attempts);
  }
}

/* show_budget_spelled for the descriptor `target`. */
static void show_budget(
    const char *label,
    int dir,
    int count,
    int below_proc,
    int target,
    const char *name,
    const char *tail,
    void (*show)(const char *, const char *)) {
  char spelled[32];
  snprintf(spelled, sizeof(spelled), "%d", target);
  show_budget_spelled(label, dir, count, below_proc, spelled, name, tail, show);
}

/* The known-gap line of a link-budget line ptrace leaves out
 * (long_kernel_lookups). */
static void show_known_gap(int count, const char *label, const char *name, const char *tail) {
  printf("known-gap #5045: budget: %d %s segments %s%s\n", count, label, name, tail);
}

/* show_budget for a line the kernel answers on ptrace by a lookup of 21 to
 * 40 links, or its known-gap line, without a lookup, with
 * --short-kernel-lookups (long_kernel_lookups). */
static void show_kernel_budget(
    const char *label,
    int dir,
    int count,
    int below_proc,
    int target,
    const char *name,
    const char *tail) {
  if (long_kernel_lookups) {
    show_budget(label, dir, count, below_proc, target, name, tail, show_path);
  } else {
    show_known_gap(count, label, name, tail);
  }
}

/* show_kernel_budget for the spelled target `target`. */
static void show_kernel_budget_spelled(
    const char *label,
    int dir,
    int count,
    const char *target,
    const char *name,
    const char *tail) {
  if (long_kernel_lookups) {
    show_budget_spelled(label, dir, count, 0, target, name, tail, show_path);
  } else {
    show_known_gap(count, label, name, tail);
  }
}

/* The caller's own descriptor links read through a link to one of its own
 * descriptor directories (--directory-links). */
static int check_directory_links(void) {
  char path[PATH_MAX];
  snprintf(path, sizeof(path), "%s/through", dir);
  int file = open(path, O_RDWR | O_CREAT | O_TRUNC | O_CLOEXEC, 0600);
  CHECK(file >= 0, "open %s: %s", path, strerror(errno));
  int null = open("/dev/null", O_RDONLY | O_CLOEXEC);
  CHECK(null >= 0, "open /dev/null: %s", strerror(errno));
  int self_fd = open("/proc/self/fd", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  CHECK(self_fd >= 0, "open /proc/self/fd: %s", strerror(errno));
  int thread_fd = open("/proc/thread-self/fd", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  CHECK(thread_fd >= 0, "open /proc/thread-self/fd: %s", strerror(errno));
  int dev_fd = open("/dev/fd", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  CHECK(dev_fd >= 0, "open /dev/fd: %s", strerror(errno));
  int proc = open("/proc", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  CHECK(proc >= 0, "open /proc: %s", strerror(errno));
  show_through_directories("file", file, self_fd, thread_fd, dev_fd, proc);
  show_through_directories("null", null, self_fd, thread_fd, dev_fd, proc);
  snprintf(path, sizeof(path), "/proc/self/fd/%d/%d", self_fd, self_fd);
  show_path_own_pid("through: D's own link self/fd/D/D", path);
  snprintf(path, sizeof(path), "/proc/self/fd/%d/%d", self_fd, 999);
  show_path("through: closed self/fd/D/999", path);
  snprintf(path, sizeof(path), "/proc/self/fd/%d/%d/", self_fd, file);
  show_path("through: followed file self/fd/D/N/", path);
  snprintf(path, sizeof(path), "/proc/self/fd/%d/%d/.", self_fd, file);
  show_path("through: walked file self/fd/D/N/.", path);
  snprintf(path, sizeof(path), "/proc/self/fd/%d/../fd/%d", self_fd, file);
  show_path("through: climbed file self/fd/D/../fd/N", path);
  snprintf(path, sizeof(path), "/proc/self/fd/%d/../../self/fd/%d", self_fd, file);
  show_path("through: climbed file self/fd/D/../../self/fd/N", path);
  /* Each side of the 40-link budget. */
  /* show_kernel_budget: a line the kernel answers on ptrace by a lookup of
   * 21 to 40 links, a known gap in ptrace's L2 run (long_kernel_lookups),
   * https://github.com/rrnewton/hermit/issues/5045. The list is exactly
   * these nine calls (PTRACE_KNOWN_GAPS in hermit-cli/tests/cli.rs) and
   * must not grow; its lines are removed when the kernel flake is fixed.
   * ptrace's separate non-L2 run still executes each, once. */
  show_kernel_budget("P", proc, 19, 1, null, "N", "");
  show_budget("P", proc, 20, 1, null, "N", "", show_path);
  show_kernel_budget("P", proc, 19, 1, null, "N", "/");
  show_kernel_budget("D", self_fd, 39, 0, null, "N", "");
  show_budget("D", self_fd, 40, 0, null, "N", "", show_path);
  show_kernel_budget("D", self_fd, 38, 0, null, "N", "/");
  show_budget("D", self_fd, 39, 0, null, "N", "/", show_path);
  show_kernel_budget("D", self_fd, 38, 0, null, "N", "/.");
  show_budget("D", self_fd, 39, 0, null, "N", "/.", show_path);
  show_budget("P", proc, 19, 1, self_fd, "D", "", show_path_own_pid);
  show_budget("P", proc, 20, 1, self_fd, "D", "", show_path_own_pid);
  /* An entry that does not exist past exactly 40 links: no link to follow. */
  CHECK(fcntl(999, F_GETFD) == -1 && errno == EBADF, "descriptor 999 is open");
  show_kernel_budget_spelled("D", self_fd, 39, "999", "closed", "/");
  show_kernel_budget_spelled("D", self_fd, 39, "999", "closed", "/.");
  show_kernel_budget_spelled("D", self_fd, 39, "absent", "absent", "/.");
  /* A ".." climbs out of the directory the 40th link led to: no link. */
  char climbed[32];
  snprintf(climbed, sizeof(climbed), "../fd/%d", null);
  show_kernel_budget_spelled("D", self_fd, 39, climbed, "../fd/N", "");
  int fds[] = {file, null, self_fd, thread_fd, dev_fd, proc};
  for (size_t i = 0; i < sizeof(fds) / sizeof(fds[0]); i++) {
    CHECK(close(fds[i]) == 0, "close: %s", strerror(errno));
  }
  snprintf(path, sizeof(path), "%s/through", dir);
  CHECK(unlink(path) == 0, "unlink %s: %s", path, strerror(errno));
  printf("failures=%d\n", failures);
  return failures == 0 ? 0 : 1;
}

int main(int argc, char **argv) {
  int sibling = argc == 3 && strcmp(argv[2], "--sibling-directories") == 0;
  int directory_links = (argc == 3 || argc == 4) && strcmp(argv[2], "--directory-links") == 0;
  if (directory_links && argc == 4) {
    if (strcmp(argv[3], "--short-kernel-lookups") == 0) {
      long_kernel_lookups = 0;
    } else if (strcmp(argv[3], "--native-oracle") == 0) {
      native_oracle = 1;
    } else {
      directory_links = 0;
    }
  }
  if (argc != 2 && !sibling && !directory_links) {
    fprintf(
        stderr,
        "usage: %s DIR [--sibling-directories | --directory-links "
        "[--short-kernel-lookups | --native-oracle]]\n",
        argv[0]);
    return 2;
  }
  setvbuf(stdout, NULL, _IOLBF, 0);
  char real_dir[PATH_MAX];
  if (realpath(argv[1], real_dir) == NULL) {
    perror("realpath DIR");
    return 2;
  }
  dir = real_dir;
  dir_len = strlen(dir);
  if (chdir(dir) != 0) {
    perror("chdir DIR");
    return 2;
  }
  if (sibling) {
    return check_sibling_directories();
  }
  if (directory_links) {
    return check_directory_links();
  }
  proc_fd_directory = open("/proc/self/fd", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  CHECK(proc_fd_directory >= 0, "open /proc/self/fd: %s", strerror(errno));

  char path[PATH_MAX];
  snprintf(path, sizeof(path), "%s/file", dir);
  int file = open(path, O_RDWR | O_CREAT | O_CLOEXEC, 0644);
  CHECK(file >= 0, "open %s: %s", path, strerror(errno));
  show("file", file);

  snprintf(path, sizeof(path), "%s/before", dir);
  int renamed = open(path, O_RDWR | O_CREAT | O_CLOEXEC, 0644);
  CHECK(renamed >= 0, "open %s: %s", path, strerror(errno));
  char after[PATH_MAX];
  snprintf(after, sizeof(after), "%s/after", dir);
  CHECK(rename(path, after) == 0, "rename: %s", strerror(errno));
  show("renamed file", renamed);

  snprintf(path, sizeof(path), "%s/doomed", dir);
  int deleted = open(path, O_RDWR | O_CREAT | O_CLOEXEC, 0644);
  CHECK(deleted >= 0, "open %s: %s", path, strerror(errno));
  CHECK(unlink(path) == 0, "unlink: %s", strerror(errno));
  show("unlinked file", deleted);

  snprintf(path, sizeof(path), "%s/sub", dir);
  CHECK(mkdir(path, 0755) == 0, "mkdir: %s", strerror(errno));
  int directory = open(path, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  CHECK(directory >= 0, "open %s: %s", path, strerror(errno));
  show("directory", directory);

  snprintf(path, sizeof(path), "%s/gone", dir);
  CHECK(mkdir(path, 0755) == 0, "mkdir: %s", strerror(errno));
  int removed = open(path, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  CHECK(removed >= 0, "open %s: %s", path, strerror(errno));
  CHECK(rmdir(path) == 0, "rmdir: %s", strerror(errno));
  show("removed directory", removed);

  snprintf(path, sizeof(path), "%s/file", dir);
  int opath = open(path, O_PATH | O_CLOEXEC);
  CHECK(opath >= 0, "open O_PATH: %s", strerror(errno));
  show("O_PATH file", opath);

  int null = open("/dev/null", O_RDWR | O_CLOEXEC);
  CHECK(null >= 0, "open /dev/null: %s", strerror(errno));
  show("/dev/null", null);

  int pipes[2];
  CHECK(pipe2(pipes, O_CLOEXEC) == 0, "pipe2: %s", strerror(errno));
  show_anonymous("pipe read end", "pipe", pipes[0]);
  show_anonymous("pipe write end", "pipe", pipes[1]);

  int sockets[2];
  CHECK(
      socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, sockets) == 0,
      "socketpair: %s",
      strerror(errno));
  show_anonymous("socket", "socket", sockets[0]);

  int event = eventfd(0, EFD_CLOEXEC);
  CHECK(event >= 0, "eventfd: %s", strerror(errno));
  show("eventfd", event);

  int epoll = epoll_create1(EPOLL_CLOEXEC);
  CHECK(epoll >= 0, "epoll_create1: %s", strerror(errno));
  show("epoll", epoll);

  int timer = timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC);
  CHECK(timer >= 0, "timerfd_create: %s", strerror(errno));
  show("timerfd", timer);

  sigset_t mask;
  sigemptyset(&mask);
  sigaddset(&mask, SIGUSR1);
  int signals = signalfd(-1, &mask, SFD_CLOEXEC);
  CHECK(signals >= 0, "signalfd: %s", strerror(errno));
  show("signalfd", signals);
  /* KVM creates no process while a signalfd is open. */
  CHECK(close(signals) == 0, "close signalfd: %s", strerror(errno));

  int memory = memfd_create("f2h-link", MFD_CLOEXEC);
  CHECK(memory >= 0, "memfd_create: %s", strerror(errno));
  show("memfd", memory);

  snprintf(path, sizeof(path), "%s/fifo", dir);
  CHECK(mkfifo(path, 0644) == 0, "mkfifo: %s", strerror(errno));
  int fifo = open(path, O_RDWR | O_CLOEXEC);
  CHECK(fifo >= 0, "open %s: %s", path, strerror(errno));
  show("named fifo", fifo);

  int closed = dup(file);
  CHECK(closed >= 0, "dup: %s", strerror(errno));
  CHECK(close(closed) == 0, "close: %s", strerror(errno));
  char target[PATH_MAX];
  snprintf(path, sizeof(path), "/proc/self/fd/%d", closed);
  errno = 0;
  ssize_t n = read_link(path, target);
  printf("closed fd=%zd errno=%s\n", n, errno_name(n < 0 ? errno : 0));

  check_buffers(file);
  check_anonymous_buffer(pipes[0]);
  show_followed("file", file);
  show_followed("directory", directory);
  show_followed("removed directory", removed);
  show_followed("pipe", pipes[0]);

  /* A "." after a link: a lookup in what it leads to. */
  show_walked("file", "/proc/self/fd/%d", file, "/.");
  show_walked("directory", "/proc/self/fd/%d", directory, "/.");
  show_walked("directory", "/proc/self/fd/%d", directory, "/./");
  show_walked("removed directory", "/proc/self/fd/%d", removed, "/.");
  show_walked("pipe", "/proc/self/fd/%d", pipes[0], "/.");
  show_walked("closed", "/proc/self/fd/%d", closed, "/.");
  show_walked("thread-self file", "/proc/thread-self/fd/%d", file, "//.");
  show_walked("dev/fd directory", "/dev/fd/%d", directory, "/.");
  show_path("walked: self/cwd/.", "/proc/self/cwd/.");
  show_path("walked: thread-self/cwd/.", "/proc/thread-self/cwd/.");
  show_path("walked: self/root/.", "/proc/self/root/.");
  show_path("walked: self/exe/.", "/proc/self/exe/.");
  show_path("walked: self/exe/./", "/proc/self/exe/./");

  /* The process's own procfs files, which a backend may keep itself. */
  int status = open("/proc/self/status", O_RDONLY | O_CLOEXEC);
  CHECK(status >= 0, "open /proc/self/status: %s", strerror(errno));
  show_own_proc_file("status", status, "status");
  show_followed("status", status);
  show_walked("status", "/proc/self/fd/%d", status, "/.");
  int maps = open("/proc/self/maps", O_RDONLY | O_CLOEXEC);
  CHECK(maps >= 0, "open /proc/self/maps: %s", strerror(errno));
  show_own_proc_file("maps", maps, "maps");
  CHECK(close(maps) == 0, "close maps: %s", strerror(errno));

  thread_fd = file;
  pthread_t thread;
  CHECK(
      pthread_create(&thread, NULL, thread_main, NULL) == 0,
      "pthread_create failed");
  CHECK(pthread_join(thread, NULL) == 0, "pthread_join failed");

  check_child(file, status);
  CHECK(close(status) == 0, "close status: %s", strerror(errno));
  check_exe(argv[0]);
  check_cwd();

  /* Leave DIR as it was found, for the next run in it. */
  const char *names[] = {"file", "after", "child-file", "fifo"};
  for (size_t i = 0; i < sizeof(names) / sizeof(names[0]); i++) {
    snprintf(path, sizeof(path), "%s/%s", dir, names[i]);
    CHECK(unlink(path) == 0, "unlink %s: %s", path, strerror(errno));
  }
  snprintf(path, sizeof(path), "%s/sub", dir);
  CHECK(rmdir(path) == 0, "rmdir %s: %s", path, strerror(errno));

  printf("failures=%d\n", failures);
  return failures == 0 ? 0 : 1;
}
