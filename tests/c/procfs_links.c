/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * procfs_links: the /proc links whose targets Detcore decides itself
 * (https://github.com/rrnewton/hermit/issues/4266, slice B3a-2) read the
 * same by every spelling: `self` names getpid(), `thread-self` names
 * `<pid>/task/<tid>`, the `mounts` and `net` links name `self/mounts` and
 * `self/net`, each namespace link reads the same through `self`, the pid,
 * `thread-self` and the thread's directory, and a pipe's or socket's
 * descriptor link reads the same through `self`, the pid, the thread's
 * directory, `thread-self` and `/dev/fd`. Linux's answers to a short
 * buffer, a zero-length one, a trailing slash or `.`, and an empty
 * pathname on an `O_PATH` descriptor are checked too, and so are a second
 * thread's own names and the links of a thread made without CLONE_FILES,
 * which read its own descriptor table.
 *
 * usage: procfs_links [--host] [--no-private-table] [--no-link-descriptor]
 *   --host: natively, check the same relations and print nothing (the
 *   targets are the host's).
 *   --no-private-table: skip the thread made without CLONE_FILES (a
 *   backend without such threads, KVM).
 *   --no-link-descriptor: skip the empty pathname on an `O_PATH` descriptor
 *   of the /proc/self link itself, which the kernel answers: KVM's own /proc
 *   refuses the open, DBT's kernel names its own pid, and the recorder does
 *   not record readlinkat.
 *
 * Under Hermit it prints the targets, which must be the same on every
 * backend.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <linux/futex.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <unistd.h>

static int failures;
static int host;
static int private_table = 1;
static int link_descriptor = 1;

#define CHECK(condition, ...)                \
  do {                                       \
    if (!(condition)) {                      \
      fprintf(stderr, "FAIL: " __VA_ARGS__); \
      fprintf(stderr, "\n");                 \
      failures++;                            \
    }                                        \
  } while (0)

/* The link at `path`, into `target`; an empty string where it fails. */
static void link_of(const char* path, char* target, size_t size) {
  ssize_t length = readlink(path, target, size - 1);
  CHECK(length >= 0, "readlink %s: %s", path, strerror(errno));
  target[length >= 0 ? length : 0] = '\0';
}

/* Every spelling in `paths` reads the first one's target, printed as
 * `label target` under Hermit. */
static void same_target(const char* label, const char* const* paths, int count) {
  char first[256];
  link_of(paths[0], first, sizeof first);
  for (int i = 1; i < count; i++) {
    char other[256];
    link_of(paths[i], other, sizeof other);
    CHECK(strcmp(first, other) == 0, "%s reads %s, %s reads %s", paths[0],
        first, paths[i], other);
  }
  if (!host) {
    printf("%s %s\n", label, first);
  }
}

static void* second_thread(void* unused) {
  (void)unused;
  int pid = getpid();
  long tid = syscall(SYS_gettid);
  char expected[64], target[256], paths[3][128];
  CHECK(tid != pid, "the second thread's tid is its pid");
  snprintf(expected, sizeof expected, "%d/task/%ld", pid, tid);
  link_of("/proc/thread-self", target, sizeof target);
  CHECK(strcmp(target, expected) == 0,
      "a second thread's /proc/thread-self reads %s, not %s", target, expected);
  snprintf(paths[0], sizeof paths[0], "/proc/thread-self/ns/pid");
  snprintf(paths[1], sizeof paths[1], "/proc/self/task/%ld/ns/pid", tid);
  const char* namespace_spellings[] = {paths[0], paths[1]};
  same_target("second thread ns/pid", namespace_spellings, 2);
  if (!host) {
    printf("a second thread names itself\n");
  }
  return NULL;
}

/* In a thread made without CLONE_FILES, `*number`, which is /dev/null in
 * the process's table, becomes /dev/zero in the thread's own: its links by
 * the thread's spellings read /dev/zero. (Not a pipe: Detcore names a
 * pipe's link from the descriptor itself, so a link read from the wrong
 * table would still look right.) */
static int private_table_thread(void* arg) {
  int number = *(int*)arg;
  int pid = getpid();
  long tid = syscall(SYS_gettid);
  int zero = open("/dev/zero", O_RDONLY);
  CHECK(zero >= 0, "open /dev/zero: %s", strerror(errno));
  CHECK(dup2(zero, number) == number, "dup2: %s", strerror(errno));
  char paths[3][128], target[256];
  snprintf(paths[0], sizeof paths[0], "/proc/thread-self/fd/%d", number);
  snprintf(paths[1], sizeof paths[1], "/proc/self/task/%ld/fd/%d", tid, number);
  snprintf(paths[2], sizeof paths[2], "/proc/%d/task/%ld/fd/%d", pid, tid, number);
  const char* spellings[] = {paths[0], paths[1], paths[2]};
  same_target("own-table descriptor", spellings, 3);
  link_of(paths[1], target, sizeof target);
  CHECK(strcmp(target, "/dev/zero") == 0,
      "%s reads %s, not the thread's own /dev/zero", paths[1], target);
  return 0;
}

static void check_private_descriptor_table(void) {
  size_t size = 1 << 18;
  char* stack = mmap(NULL, size, PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS | MAP_STACK, -1, 0);
  CHECK(stack != MAP_FAILED, "mmap: %s", strerror(errno));
  if (stack == MAP_FAILED) {
    return;
  }
  int number = open("/dev/null", O_RDONLY);
  CHECK(number >= 0, "open /dev/null: %s", strerror(errno));
  static volatile pid_t word;
  int flags = CLONE_VM | CLONE_FS | CLONE_SIGHAND | CLONE_THREAD |
      CLONE_SYSVSEM | CLONE_PARENT_SETTID | CLONE_CHILD_CLEARTID;
  int tid = clone(private_table_thread, stack + size, flags, &number, &word,
      NULL, &word);
  CHECK(tid > 0, "clone without CLONE_FILES: %s", strerror(errno));
  for (pid_t seen; tid > 0 && (seen = word) != 0;) {
    syscall(SYS_futex, &word, FUTEX_WAIT, seen, NULL, NULL, 0);
  }
  char path[64], target[256];
  snprintf(path, sizeof path, "/proc/self/fd/%d", number);
  link_of(path, target, sizeof target);
  CHECK(strcmp(target, "/dev/null") == 0,
      "%s reads %s after the thread's own table changed", path, target);
  close(number);
  munmap(stack, size);
  if (!host) {
    printf("a thread with its own descriptor table names that table\n");
  }
}

int main(int argc, char** argv) {
  for (int i = 1; i < argc; i++) {
    if (strcmp(argv[i], "--host") == 0) {
      host = 1;
    } else if (strcmp(argv[i], "--no-private-table") == 0) {
      private_table = 0;
    } else if (strcmp(argv[i], "--no-link-descriptor") == 0) {
      link_descriptor = 0;
    } else {
      fprintf(stderr,
          "usage: %s [--host] [--no-private-table] [--no-link-descriptor]\n",
          argv[0]);
      return 2;
    }
  }
  int pid = getpid();
  long tid = syscall(SYS_gettid);
  char expected[64], expected_pid[32], target[256], path[128], paths[5][128];

  snprintf(expected, sizeof expected, "%d", pid);
  snprintf(expected_pid, sizeof expected_pid, "%d", pid);
  link_of("/proc/self", target, sizeof target);
  CHECK(strcmp(target, expected) == 0, "/proc/self reads %s, getpid() %s",
      target, expected);
  link_of("/proc/./self", target, sizeof target);
  CHECK(strcmp(target, expected) == 0, "/proc/./self reads %s", target);
  int proc = open("/proc", O_RDONLY | O_DIRECTORY);
  CHECK(proc >= 0, "open /proc: %s", strerror(errno));
  ssize_t length = readlinkat(proc, "self", target, sizeof target - 1);
  target[length >= 0 ? length : 0] = '\0';
  CHECK(strcmp(target, expected) == 0, "readlinkat /proc self reads %s", target);
  close(proc);
  if (!host) {
    printf("self names getpid()\n");
  }

  snprintf(expected, sizeof expected, "%d/task/%ld", pid, tid);
  link_of("/proc/thread-self", target, sizeof target);
  CHECK(strcmp(target, expected) == 0, "/proc/thread-self reads %s, not %s",
      target, expected);
  if (!host) {
    printf("thread-self names <pid>/task/<tid>\n");
  }

  link_of("/proc/mounts", target, sizeof target);
  CHECK(strcmp(target, "self/mounts") == 0, "/proc/mounts reads %s", target);
  link_of("/proc/net", target, sizeof target);
  CHECK(strcmp(target, "self/net") == 0, "/proc/net reads %s", target);
  if (!host) {
    printf("mounts and net name self/mounts and self/net\n");
  }

  /* A buffer of one byte takes the first byte; none at all, EINVAL; a
   * trailing slash follows the link to a directory, which is no link. */
  char one = 0;
  errno = 0;
  CHECK(readlink("/proc/self", &one, 1) == 1 && one == expected_pid[0],
      "readlink /proc/self into one byte: %c (%s)", one, strerror(errno));
  errno = 0;
  CHECK(syscall(SYS_readlink, "/proc/self", target, 0) == -1 && errno == EINVAL,
      "readlink /proc/self into no bytes: %s", strerror(errno));
  errno = 0;
  CHECK(readlink("/proc/self/", target, sizeof target) == -1 && errno == EINVAL,
      "readlink /proc/self/: %s", strerror(errno));
  errno = 0;
  CHECK(readlink("/proc/thread-self/.", target, sizeof target) == -1 &&
          errno == EINVAL,
      "readlink /proc/thread-self/.: %s", strerror(errno));
  /* ...and a trailing slash after a link to a file, ENOTDIR. */
  errno = 0;
  CHECK(readlink("/proc/mounts/", target, sizeof target) == -1 &&
          errno == ENOTDIR,
      "readlink /proc/mounts/: %s", strerror(errno));
  errno = 0;
  CHECK(readlink("/proc/self/ns/pid/", target, sizeof target) == -1 &&
          errno == ENOTDIR,
      "readlink /proc/self/ns/pid/: %s", strerror(errno));
  /* A buffer Linux cannot write is EFAULT, here for a target that fills it
   * exactly (`self/net`, eight bytes). */
  char* volatile unmapped = (char*)1;
  errno = 0;
  CHECK(readlink("/proc/net", unmapped, 8) == -1 && errno == EFAULT,
      "readlink /proc/net into an unmapped buffer: %s", strerror(errno));
  /* The size is refused before the path is looked up. */
  errno = 0;
  CHECK(syscall(SYS_readlink, "/proc/mounts/", target, 0) == -1 &&
          errno == EINVAL,
      "readlink /proc/mounts/ into no bytes: %s", strerror(errno));
  /* An empty pathname reads the link a descriptor is. */
  if (link_descriptor) {
    int link = open("/proc/self", O_PATH | O_NOFOLLOW);
    CHECK(link >= 0, "open /proc/self O_PATH|O_NOFOLLOW: %s", strerror(errno));
    length = readlinkat(link, "", target, sizeof target - 1);
    target[length >= 0 ? length : 0] = '\0';
    CHECK(strcmp(target, expected_pid) == 0,
        "readlinkat of an O_PATH /proc/self descriptor reads %s (%s)", target,
        strerror(errno));
    close(link);
  }
  if (!host) {
    printf("short, empty and trailing-slash reads as Linux\n");
  }

  const char* namespaces[] = {"cgroup", "ipc", "mnt", "net", "pid",
      "pid_for_children", "time", "time_for_children", "user", "uts"};
  for (unsigned n = 0; n < sizeof namespaces / sizeof namespaces[0]; n++) {
    const char* name = namespaces[n];
    snprintf(paths[0], sizeof paths[0], "/proc/self/ns/%s", name);
    snprintf(paths[1], sizeof paths[1], "/proc/%d/ns/%s", pid, name);
    snprintf(paths[2], sizeof paths[2], "/proc/thread-self/ns/%s", name);
    snprintf(paths[3], sizeof paths[3], "/proc/self/task/%ld/ns/%s", tid, name);
    const char* spellings[] = {paths[0], paths[1], paths[2], paths[3]};
    snprintf(path, sizeof path, "ns/%s", name);
    same_target(path, spellings, 4);
  }

  int pipes[2], sockets[2];
  CHECK(pipe(pipes) == 0, "pipe: %s", strerror(errno));
  CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sockets) == 0, "socketpair: %s",
      strerror(errno));
  struct {
    const char* label;
    int fd;
  } descriptors[] = {{"pipe", pipes[0]}, {"socket", sockets[0]}};
  for (unsigned d = 0; d < sizeof descriptors / sizeof descriptors[0]; d++) {
    int fd = descriptors[d].fd;
    snprintf(paths[0], sizeof paths[0], "/proc/self/fd/%d", fd);
    snprintf(paths[1], sizeof paths[1], "/proc/%d/fd/%d", pid, fd);
    snprintf(paths[2], sizeof paths[2], "/proc/self/task/%ld/fd/%d", tid, fd);
    snprintf(paths[3], sizeof paths[3], "/proc/thread-self/fd/%d", fd);
    snprintf(paths[4], sizeof paths[4], "/dev/fd/%d", fd);
    const char* spellings[] = {paths[0], paths[1], paths[2], paths[3], paths[4]};
    same_target(descriptors[d].label, spellings, 5);
  }

  /* A second thread, whose tid is not the pid, names itself; after it
   * takes a descriptor table of its own, its descriptor links read that
   * table by its thread's spellings. */
  pthread_t thread;
  CHECK(pthread_create(&thread, NULL, second_thread, NULL) == 0,
      "pthread_create");
  pthread_join(thread, NULL);
  if (private_table) {
    check_private_descriptor_table();
  }

  if (failures) {
    fprintf(stderr, "%d checks failed\n", failures);
    return 1;
  }
  return 0;
}
