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
 * getcwd fails with ENOENT), by seven spellings. Paths below the scratch
 * directory are printed from "<dir>". A child's failed checks fail its
 * parent. It removes what it made in DIR before it ends, so that a second
 * run in DIR (--verify) starts from the same directory.
 *
 * usage: proc_link_targets DIR
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
static ssize_t read_link(const char *path, char *out) {
  ssize_t n = readlink(path, out, PATH_MAX - 1);
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

static void check_child(int inherited) {
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

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s DIR\n", argv[0]);
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

  thread_fd = file;
  pthread_t thread;
  CHECK(
      pthread_create(&thread, NULL, thread_main, NULL) == 0,
      "pthread_create failed");
  CHECK(pthread_join(thread, NULL) == 0, "pthread_join failed");

  check_child(file);
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
