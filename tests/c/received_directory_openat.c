/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * received_directory_openat: a name relative to a directory descriptor this
 * process received over SCM_RIGHTS is looked up below that directory, as
 * Linux looks it up, by openat, fstatat, statx, faccessat and readlinkat.
 * Detcore keeps no record of a received descriptor where its tool does not
 * discover descriptors as they are used (ptrace, KVM), and those calls used
 * to fail with EBADF though the descriptor was open.
 *
 * It makes a directory under /tmp holding a file `f` ("hello\n") and a
 * symbolic link `l` to `f`, and passes that directory, and /proc, to itself
 * over a socketpair. Then, relative to each received descriptor: the file
 * opens and reads its bytes, fstatat and statx describe the file it opened,
 * faccessat(R_OK) succeeds, readlinkat of the link reads `f` and of the file
 * is EINVAL, and a name that is not there is ENOENT.
 *
 * Below a received /proc/self/ns directory, readlinkat of `mnt` reads what
 * readlink of /proc/self/ns/mnt reads; below a received /proc/self/fd
 * directory, readlinkat of a socket's and a pipe's number reads what the
 * absolute spelling reads. Under Hermit those links are Detcore's
 * canonicalized ones, which a run compared with --verify-strict must show
 * alike in both runs.
 *
 * Below a received /proc, mounts and self/mountinfo read exactly what
 * /proc/mounts and /proc/self/mountinfo read.
 *
 * Below a received /proc/self directory, a name the --proc level omits
 * (limits at minimal) is absent too, through openat, fstatat and faccessat.
 *
 * A thread with a descriptor table of its own (a clone without CLONE_FILES)
 * receives /proc at a number that, in the thread group leader's table, is a
 * /tmp directory: below it, version reads Linux's banner and, at minimal,
 * interrupts is absent. Detcore must name the descriptor by the calling
 * thread's table (/proc/thread-self/fd/N), not the leader's
 * (/proc/self/fd/N), which here is another directory.
 *
 * usage: received_directory_openat [--hermit] [--minimal]
 *                                  [--no-process-directories]
 *   --hermit: print what was checked, which must be the same on every
 *   backend; natively print nothing. Exits 0 only if every check passed.
 *   --minimal: the run's /proc is at Hermit's --proc=minimal level, which
 *   omits /proc/interrupts: below a received /proc it is absent too (openat,
 *   fstatat and faccessat ENOENT), as its absolute spelling is.
 *   --no-process-directories: skip the received /proc/self, /proc/self/ns
 *   and /proc/self/fd directories, named on stderr, for a backend whose own
 *   /proc cannot open a process's directory yet (KVM).
 *   --no-private-tables: skip the thread with a descriptor table of its own,
 *   named on stderr, for a backend that cannot give a thread one (KVM
 *   refuses a clone without CLONE_FILES).
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/futex.h>
#include <sched.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

static int failures;
static int hermit;
static int minimal;
static int process_directories = 1;
static int private_tables = 1;

#define CHECK(condition, ...)                              \
  do {                                                     \
    if (!(condition)) {                                    \
      fprintf(stderr, "FAIL %s:%d: ", __FILE__, __LINE__); \
      fprintf(stderr, __VA_ARGS__);                        \
      fprintf(stderr, "\n");                               \
      failures++;                                          \
    }                                                      \
  } while (0)

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

/* `fd`, passed to this process over `socket` and received as a new
 * descriptor. */
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
  CHECK(sendmsg(pair[0], &message, 0) == 1, "sendmsg: %s", strerror(errno));
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

/* Below the received directory `directory`, `name` is a file that reads a
 * line starting with `start`; `link`, if not NULL, is a symbolic link to
 * `name`. */
static void check_below(int directory, const char* what, const char* name, const char* start,
                        const char* link) {
  int fd = openat(directory, name, O_RDONLY);
  CHECK(fd >= 0, "%s: openat(%s): %s", what, name, strerror(errno));
  if (fd >= 0) {
    char buffer[64];
    ssize_t length = read(fd, buffer, sizeof(buffer));
    CHECK(length >= (ssize_t)strlen(start) && memcmp(buffer, start, strlen(start)) == 0,
          "%s: %s read %zd bytes that do not start with %s", what, name, length, start);
    struct stat opened;
    struct stat by_name;
    CHECK(fstat(fd, &opened) == 0, "%s: fstat", what);
    CHECK(fstatat(directory, name, &by_name, 0) == 0, "%s: fstatat(%s): %s", what, name,
          strerror(errno));
    CHECK(opened.st_dev == by_name.st_dev && opened.st_ino == by_name.st_ino,
          "%s: fstatat names another file than openat opened", what);
    struct statx extended;
    CHECK(statx(directory, name, 0, STATX_INO, &extended) == 0 &&
              extended.stx_ino == (unsigned long long)opened.st_ino,
          "%s: statx(%s): %s", what, name, strerror(errno));
    close(fd);
  }
  CHECK(faccessat(directory, name, R_OK, 0) == 0, "%s: faccessat(%s): %s", what, name,
        strerror(errno));
  char target[64];
  errno = 0;
  CHECK(readlinkat(directory, name, target, sizeof(target)) == -1 && errno == EINVAL,
        "%s: readlinkat(%s) is not EINVAL: %s", what, name, strerror(errno));
  if (link != NULL) {
    ssize_t length = readlinkat(directory, link, target, sizeof(target) - 1);
    CHECK(length == (ssize_t)strlen(name) && memcmp(target, name, strlen(name)) == 0,
          "%s: readlinkat(%s) gave %zd bytes", what, link, length);
  }
  errno = 0;
  CHECK(openat(directory, "absent", O_RDONLY) == -1 && errno == ENOENT,
        "%s: openat(absent) is not ENOENT: %s", what, strerror(errno));
}

/* readlinkat of `name` below the received directory `directory` reads
 * what readlink of `absolute` reads. */
static void check_link_below(int directory, const char* name, const char* absolute,
                             const char* what) {
  char below[256];
  char direct[256];
  ssize_t length = readlinkat(directory, name, below, sizeof(below) - 1);
  ssize_t expected = readlink(absolute, direct, sizeof(direct) - 1);
  CHECK(length >= 0 && expected >= 0, "%s: readlinkat(%s) %zd, readlink(%s) %zd: %s", what, name,
        length, absolute, expected, strerror(errno));
  if (length >= 0 && expected >= 0) {
    below[length] = '\0';
    direct[expected] = '\0';
    CHECK(strcmp(below, direct) == 0, "%s: readlinkat(%s) read %s where %s reads %s", what, name,
          below, absolute, direct);
  }
}

/* The whole of `path`, opened relative to `directory` (or absolutely where
 * `directory` is AT_FDCWD), into `buffer`; its length, or -1. */
static ssize_t read_whole(int directory, const char* path, char* buffer, size_t capacity) {
  int fd = openat(directory, path, O_RDONLY);
  if (fd < 0) {
    return -1;
  }
  size_t length = 0;
  for (;;) {
    ssize_t got = read(fd, buffer + length, capacity - length);
    if (got < 0 || (got == 0 && length == capacity)) {
      close(fd);
      return -1;
    }
    if (got == 0) {
      break;
    }
    length += (size_t)got;
    if (length == capacity) {
      close(fd);
      return -1;
    }
  }
  close(fd);
  return (ssize_t)length;
}

/* `name`, opened below the received directory `directory`, reads exactly
 * what its absolute spelling `absolute` reads, and is not empty. */
static void check_contents_below(int directory, const char* name, const char* absolute,
                                 const char* what) {
  enum { CAPACITY = 1 << 20 };
  char* below = malloc(CAPACITY);
  char* direct = malloc(CAPACITY);
  CHECK(below != NULL && direct != NULL, "%s: malloc", what);
  if (below == NULL || direct == NULL) {
    free(below);
    free(direct);
    return;
  }
  ssize_t length = read_whole(directory, name, below, CAPACITY);
  ssize_t expected = read_whole(AT_FDCWD, absolute, direct, CAPACITY);
  CHECK(length > 0 && expected > 0, "%s: %s read %zd bytes, %s %zd: %s", what, name, length,
        absolute, expected, strerror(errno));
  CHECK(length == expected && memcmp(below, direct, (size_t)length) == 0,
        "%s: %s does not read what %s reads", what, name, absolute);
  free(below);
  free(direct);
}

/* Names below `directory` that the --proc=minimal level omits are absent:
 * openat, fstatat and faccessat give ENOENT, as for `absolute`. */
static void check_omitted_below(int directory, const char* name, const char* absolute,
                                const char* what) {
  struct stat ignored;
  errno = 0;
  CHECK(openat(directory, name, O_RDONLY) == -1 && errno == ENOENT,
        "%s: openat(%s) at minimal is not ENOENT: %s", what, name, strerror(errno));
  errno = 0;
  CHECK(fstatat(directory, name, &ignored, 0) == -1 && errno == ENOENT,
        "%s: fstatat(%s) at minimal is not ENOENT: %s", what, name, strerror(errno));
  errno = 0;
  CHECK(faccessat(directory, name, R_OK, 0) == -1 && errno == ENOENT,
        "%s: faccessat(%s) at minimal is not ENOENT: %s", what, name, strerror(errno));
  errno = 0;
  CHECK(open(absolute, O_RDONLY) == -1 && errno == ENOENT, "%s at minimal is not ENOENT: %s",
        absolute, strerror(errno));
}

/* What the thread with a descriptor table of its own needs: the socketpair,
 * and the leader's /tmp directory, whose number the thread frees in its own
 * table so that the received /proc takes it. */
struct private_table {
  int* pair;
  int leader_directory;
  int received;
};

/* The thread, made without CLONE_FILES: its table starts as a copy of the
 * process's. The process waits for it to exit, so the two never run at
 * once. */
static int receive_in_private_table(void* argument) {
  struct private_table* table = argument;
  int proc = open("/proc", O_RDONLY | O_DIRECTORY);
  CHECK(proc >= 0, "the thread: open /proc: %s", strerror(errno));
  /* Only this thread's table loses the number, which the received
   * descriptor, the lowest free one, then takes. */
  close(table->leader_directory);
  int received = pass_to_self(table->pair, proc);
  close(proc);
  table->received = received;
  if (received >= 0) {
    check_below(received, "a received /proc in a private table", "version", "Linux version ",
                NULL);
    if (minimal) {
      check_omitted_below(received, "interrupts", "/proc/interrupts",
                          "a received /proc in a private table");
    }
    close(received);
  }
  return 0;
}

/* A thread with a descriptor table of its own receives /proc at the number
 * that the leader's table gives the directory `directory_path`. */
static void check_private_table(int pair[2], const char* directory_path) {
  struct private_table table = {pair, open(directory_path, O_RDONLY | O_DIRECTORY), -1};
  CHECK(table.leader_directory >= 0, "open %s: %s", directory_path, strerror(errno));
  size_t size = 1 << 18;
  char* stack =
      mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_STACK, -1, 0);
  CHECK(stack != MAP_FAILED, "mmap: %s", strerror(errno));
  if (stack == MAP_FAILED) {
    return;
  }
  static volatile pid_t word;
  int flags = CLONE_VM | CLONE_FS | CLONE_SIGHAND | CLONE_THREAD | CLONE_SYSVSEM |
              CLONE_PARENT_SETTID | CLONE_CHILD_CLEARTID;
  int tid = clone(receive_in_private_table, stack + size, flags, &table, &word, NULL, &word);
  CHECK(tid > 0, "clone without CLONE_FILES: %s", strerror(errno));
  for (pid_t seen; tid > 0 && (seen = word) != 0;) {
    syscall(SYS_futex, &word, FUTEX_WAIT, seen, NULL, NULL, 0);
  }
  munmap(stack, size);
  CHECK(table.received == table.leader_directory,
        "the thread received /proc as %d, not the number %d its table freed", table.received,
        table.leader_directory);
  /* In the leader's table the number is still the /tmp directory. */
  struct stat leader_view;
  struct stat expected_view;
  CHECK(fstat(table.leader_directory, &leader_view) == 0 &&
            stat(directory_path, &expected_view) == 0 && leader_view.st_ino == expected_view.st_ino,
        "the leader's descriptor %d is no longer its /tmp directory", table.leader_directory);
  close(table.leader_directory);
  say("a thread with its own descriptor table: below the /proc it received, at a number the "
      "leader's table gives a /tmp directory, version reads Linux's banner%s",
      minimal ? " and interrupts is absent" : "");
}

int main(int argc, char** argv) {
  for (int arg = 1; arg < argc; arg++) {
    if (strcmp(argv[arg], "--hermit") == 0) {
      hermit = 1;
    } else if (strcmp(argv[arg], "--minimal") == 0) {
      minimal = 1;
    } else if (strcmp(argv[arg], "--no-process-directories") == 0) {
      process_directories = 0;
    } else if (strcmp(argv[arg], "--no-private-tables") == 0) {
      private_tables = 0;
    } else {
      fprintf(stderr,
              "usage: %s [--hermit] [--minimal] [--no-process-directories] "
              "[--no-private-tables]\n",
              argv[0]);
      return 2;
    }
  }
  char directory_path[] = "/tmp/received-directory-XXXXXX";
  CHECK(mkdtemp(directory_path) != NULL, "mkdtemp: %s", strerror(errno));
  char path[128];
  snprintf(path, sizeof(path), "%s/f", directory_path);
  int file = open(path, O_WRONLY | O_CREAT | O_EXCL, 0644);
  CHECK(file >= 0 && write(file, "hello\n", 6) == 6, "create %s: %s", path, strerror(errno));
  close(file);
  snprintf(path, sizeof(path), "%s/l", directory_path);
  CHECK(symlink("f", path) == 0, "symlink %s: %s", path, strerror(errno));

  int pair[2];
  CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0, "socketpair: %s", strerror(errno));
  int directory = open(directory_path, O_RDONLY | O_DIRECTORY);
  CHECK(directory >= 0, "open %s: %s", directory_path, strerror(errno));
  int received = pass_to_self(pair, directory);
  close(directory);
  if (received >= 0) {
    check_below(received, "a received /tmp directory", "f", "hello", "l");
    close(received);
  }
  say("a received /tmp directory: openat, fstatat, statx, faccessat and readlinkat look "
      "names up below it");

  int proc = open("/proc", O_RDONLY | O_DIRECTORY);
  CHECK(proc >= 0, "open /proc: %s", strerror(errno));
  received = pass_to_self(pair, proc);
  close(proc);
  if (received >= 0) {
    check_below(received, "a received /proc", "version", "Linux version ", NULL);
    if (minimal) {
      check_omitted_below(received, "interrupts", "/proc/interrupts", "a received /proc");
    }
    close(received);
  }
  say("a received /proc: openat, fstatat, statx, faccessat and readlinkat look names up "
      "below it");
  /* The mount tables, below a received /proc: Detcore renders them, so the
   * backend is never asked for them (package K, K3b). */
  proc = open("/proc", O_RDONLY | O_DIRECTORY);
  CHECK(proc >= 0, "open /proc: %s", strerror(errno));
  received = pass_to_self(pair, proc);
  close(proc);
  if (received >= 0) {
    check_contents_below(received, "mounts", "/proc/mounts", "a received /proc");
    check_contents_below(received, "self/mountinfo", "/proc/self/mountinfo", "a received /proc");
    close(received);
  }
  say("a received /proc: mounts and self/mountinfo read what their absolute spellings read");
  if (minimal) {
    say("a received /proc at minimal: interrupts is absent below it, as at its absolute "
        "spelling");
  }

  /* A thread with a descriptor table of its own. */
  if (!private_tables) {
    fprintf(stderr, "SKIPPED: a thread with a descriptor table of its own "
                    "(--no-private-tables)\n");
  } else {
    check_private_table(pair, directory_path);
  }

  if (!process_directories) {
    fprintf(stderr, "SKIPPED: received /proc/self, /proc/self/ns and /proc/self/fd "
                    "directories (--no-process-directories)\n");
  } else {
    if (minimal) {
      int self = open("/proc/self", O_RDONLY | O_DIRECTORY);
      CHECK(self >= 0, "open /proc/self: %s", strerror(errno));
      received = pass_to_self(pair, self);
      close(self);
      if (received >= 0) {
        check_omitted_below(received, "limits", "/proc/self/limits", "a received /proc/self");
        close(received);
      }
      say("a received /proc/self at minimal: limits is absent below it, as at its absolute "
          "spelling");
    }
    int ns = open("/proc/self/ns", O_RDONLY | O_DIRECTORY);
    CHECK(ns >= 0, "open /proc/self/ns: %s", strerror(errno));
    received = pass_to_self(pair, ns);
    close(ns);
    if (received >= 0) {
      check_link_below(received, "mnt", "/proc/self/ns/mnt", "a received /proc/self/ns");
      check_link_below(received, "pid", "/proc/self/ns/pid", "a received /proc/self/ns");
      close(received);
    }
    say("a received /proc/self/ns: readlinkat of mnt and pid reads the absolute links");
    int pipe_ends[2];
    CHECK(pipe(pipe_ends) == 0, "pipe: %s", strerror(errno));
    int fds = open("/proc/self/fd", O_RDONLY | O_DIRECTORY);
    CHECK(fds >= 0, "open /proc/self/fd: %s", strerror(errno));
    received = pass_to_self(pair, fds);
    close(fds);
    if (received >= 0) {
      char name[16];
      char absolute[64];
      int linked[] = {pair[0], pipe_ends[0]};
      for (unsigned i = 0; i < 2; i++) {
        snprintf(name, sizeof(name), "%d", linked[i]);
        snprintf(absolute, sizeof(absolute), "/proc/self/fd/%d", linked[i]);
        check_link_below(received, name, absolute, "a received /proc/self/fd");
      }
      close(received);
    }
    close(pipe_ends[0]);
    close(pipe_ends[1]);
    say("a received /proc/self/fd: readlinkat of a socket's and a pipe's number reads the "
        "absolute links");
  }

  snprintf(path, sizeof(path), "%s/f", directory_path);
  unlink(path);
  snprintf(path, sizeof(path), "%s/l", directory_path);
  unlink(path);
  rmdir(directory_path);
  return failures == 0 ? 0 : 1;
}
