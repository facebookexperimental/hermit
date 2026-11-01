/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Spellings with `..` of /proc/locks and of a descriptor's fdinfo, which
 * Linux's path walk resolves through procfs's own directories: `self` and
 * `thread-self` are links it follows first, so /proc/self/.. is /proc, and a
 * component after a file is ENOTDIR (package K slice K8-d of
 * https://github.com/rrnewton/hermit/issues/4213).
 *
 * The program opens DIRECTORY/locked twice and takes an OFD read lock through
 * each descriptor, so /proc/locks holds two rows for the file, which only a
 * reader of every description shows. Then, for each spelling, it opens and
 * reads it and prints whether the bytes are the plain spelling's, or the
 * errno; then the type and size a stat of it reports, or the errno. The bytes
 * are the plain spelling's when they equal a read of it just before or just
 * after. Lines name each spelling by a template: <fd> is the first locked
 * descriptor, <pid> and <tid> the caller's own numbers. Nothing printed
 * depends on the host, the backend or the run.
 *
 * Usage: procfs_dotdot_spellings DIRECTORY [native] [links | init]
 *
 * native: the run is native. Other processes can change the host's
 *   /proc/locks twice within the three reads of a comparison, so a mismatch
 *   is tried again, up to ATTEMPTS times a millisecond apart, before it is
 *   reported. So is an ELOOP, a known divergence
 *   (https://github.com/rrnewton/hermit/issues/5045): Linux 7.1.3 keeps the
 *   link count across its RCU-walk retry. So its lookup can report ELOOP for
 *   a path that follows more than 20 links and no more than its limit of 40,
 *   when its lockless (RCU) walk falls back to a reference walk, as a mount
 *   anywhere on the host can make it do. filename_lookup and do_file_open
 *   count the links in nd->total_link_count, set once by set_nameidata, so
 *   the retry counts again the links the first walk followed. Without it
 *   (under Hermit, whose /proc/locks holds only the guest's own rows) the
 *   first comparison decides: a mismatch, or an error, is reported at once.
 * links: instead of the spellings above, spellings that follow the
 *   /proc/self link 40 and 41 times, around Linux's limit on the links one
 *   lookup follows (MAXSYMLINKS), with a missing task or a file before or
 *   after the 41st: the first link past the limit is ELOOP, and an earlier
 *   missing task (ENOENT) or file (ENOTDIR) decides first. 4194305 is past
 *   any pid Linux can give (PID_MAX_LIMIT).
 * init: open /proc/1/fdinfo/0, a descriptor's fdinfo of the run's pid 1,
 *   and print the result.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#define CHECK(condition)                                                    \
  do {                                                                      \
    if (!(condition)) {                                                     \
      fprintf(stderr, "check failed at line %d: errno %d\n", __LINE__, errno); \
      exit(2);                                                              \
    }                                                                       \
  } while (0)

enum { CAPACITY = 1 << 20, ATTEMPTS = 200 };

/* The attempts one spelling's read, and its stat, may each take: ATTEMPTS
 * natively, else one. */
static int attempts = 1;

/* The whole file at `path`, or NULL with errno set. */
static char *read_all(const char *path, size_t *length) {
  int fd = open(path, O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    return NULL;
  }
  char *buffer = malloc(CAPACITY);
  CHECK(buffer != NULL);
  size_t used = 0;
  for (;;) {
    ssize_t n = read(fd, buffer + used, CAPACITY - used);
    if (n < 0) {
      int saved = errno;
      free(buffer);
      close(fd);
      errno = saved;
      return NULL;
    }
    if (n == 0) {
      break;
    }
    used += (size_t)n;
    CHECK(used < CAPACITY);
  }
  close(fd);
  *length = used;
  return buffer;
}

static const char *errno_name(int number) {
  switch (number) {
    case ENOENT:
      return "ENOENT";
    case ENOTDIR:
      return "ENOTDIR";
    case EACCES:
      return "EACCES";
    case ELOOP:
      return "ELOOP";
    case EISDIR:
      return "EISDIR";
    case EPERM:
      return "EPERM";
    case ENOSYS:
      return "ENOSYS";
    case EOPNOTSUPP:
      return "EOPNOTSUPP";
    default:
      return "other";
  }
}

static int same(const char *bytes, size_t length, const char *reference, size_t reference_length) {
  return reference != NULL && length == reference_length && memcmp(bytes, reference, length) == 0;
}

/* A millisecond's pause before each attempt after the first. */
static void pause_before(int attempt) {
  if (attempt > 0) {
    struct timespec pause = {.tv_sec = 0, .tv_nsec = 1000000};
    nanosleep(&pause, NULL);
  }
}

/* Print the spelling `label` (a template) at `path`: its read, compared with
 * the plain spelling `plain`, and its stat. Natively, each is tried again
 * while it mismatches or fails with ELOOP (see the top of the file). */
static void show(const char *label, const char *path, const char *plain) {
  int error = 0;
  int equal = 0;
  for (int attempt = 0; attempt < attempts; attempt++) {
    pause_before(attempt);
    error = 0;
    equal = 0;
    size_t before_length = 0;
    char *before = read_all(plain, &before_length);
    size_t length = 0;
    char *bytes = read_all(path, &length);
    if (bytes == NULL) {
      error = errno;
    } else {
      size_t after_length = 0;
      char *after = read_all(plain, &after_length);
      equal = same(bytes, length, before, before_length) || same(bytes, length, after, after_length);
      free(after);
      free(bytes);
    }
    free(before);
    /* A native ELOOP is tried again, as it may be Linux's false one
     * (https://github.com/rrnewton/hermit/issues/5045): Linux 7.1.3 keeps
     * the link count across its RCU-walk retry. */
    if (error == 0 ? equal : error != ELOOP) {
      break;
    }
  }
  if (error != 0) {
    printf("%s: open or read %s\n", label, errno_name(error));
  } else {
    printf("%s: read %s\n", label, equal ? "the plain spelling's bytes" : "other bytes than the plain spelling");
  }
  struct stat status;
  int stat_error = 0;
  for (int attempt = 0; attempt < attempts; attempt++) {
    pause_before(attempt);
    stat_error = stat(path, &status) < 0 ? errno : 0;
    /* The same false ELOOP (https://github.com/rrnewton/hermit/issues/5045):
     * Linux 7.1.3 keeps the link count across its RCU-walk retry. */
    if (stat_error != ELOOP) {
      break;
    }
  }
  if (stat_error != 0) {
    printf("%s: stat %s\n", label, errno_name(stat_error));
  } else {
    printf("%s: stat %s size %lld\n", label, S_ISREG(status.st_mode) ? "regular" : "other",
           (long long)status.st_size);
  }
}

/* `template` with <pid>, <tid> and <fd> replaced by those numbers. */
static void expand(const char *template, int pid, int tid, int fd, char *out, size_t size) {
  size_t used = 0;
  out[0] = 0;
  while (*template != 0) {
    int number = -1;
    size_t skip = 0;
    if (strncmp(template, "<pid>", 5) == 0) {
      number = pid;
      skip = 5;
    } else if (strncmp(template, "<tid>", 5) == 0) {
      number = tid;
      skip = 5;
    } else if (strncmp(template, "<fd>", 4) == 0) {
      number = fd;
      skip = 4;
    }
    int written = number >= 0 ? snprintf(out + used, size - used, "%d", number)
                              : snprintf(out + used, size - used, "%c", *template);
    CHECK(written > 0 && (size_t)written < size - used);
    used += (size_t)written;
    template += number >= 0 ? skip : 1;
  }
}

/* HEAD, then COUNT times "self/../", then TAIL, into `out`. */
static void through_self(char *out, size_t size, const char *head, int count, const char *tail) {
  int used = snprintf(out, size, "%s", head);
  CHECK(used > 0 && (size_t)used < size);
  for (int i = 0; i < count; i++) {
    int written = snprintf(out + used, size - (size_t)used, "self/../");
    CHECK(written > 0 && (size_t)written < size - (size_t)used);
    used += written;
  }
  int written = snprintf(out + used, size - (size_t)used, "%s", tail);
  CHECK(written >= 0 && (size_t)written < size - (size_t)used);
}

/* The links mode (see the top of the file). */
static void show_link_limit(int fd, const char *plain_fdinfo) {
  static const struct {
    const char *label, *head;
    int count;
    const char *tail;
    int fdinfo;
  } spellings[] = {
      {"/proc/(self/..)x40/locks", "/proc/", 40, "locks", 0},
      {"/proc/(self/..)x41/locks", "/proc/", 41, "locks", 0},
      {"/proc/(self/..)x39/self/fdinfo/<fd>", "/proc/", 39, "self/fdinfo/", 1},
      {"/proc/(self/..)x40/self/fdinfo/<fd>", "/proc/", 40, "self/fdinfo/", 1},
      {"/proc/thread-self/../../../(self/..)x39/locks", "/proc/thread-self/../../../", 39, "locks", 0},
      {"/proc/thread-self/../../../(self/..)x40/locks", "/proc/thread-self/../../../", 40, "locks", 0},
      {"/proc/4194305/../(self/..)x41/locks", "/proc/4194305/../", 41, "locks", 0},
      {"/proc/(self/..)x41/4194305/../locks", "/proc/", 41, "4194305/../locks", 0},
      {"/proc/(self/..)x40/4194305/../locks", "/proc/", 40, "4194305/../locks", 0},
      {"/proc/locks/../(self/..)x41/locks", "/proc/locks/../", 41, "locks", 0},
  };
  for (size_t i = 0; i < sizeof spellings / sizeof spellings[0]; i++) {
    char tail[64];
    if (spellings[i].fdinfo) {
      CHECK(snprintf(tail, sizeof tail, "%s%d", spellings[i].tail, fd) < (int)sizeof tail);
    } else {
      CHECK(snprintf(tail, sizeof tail, "%s", spellings[i].tail) < (int)sizeof tail);
    }
    char path[1024];
    through_self(path, sizeof path, spellings[i].head, spellings[i].count, tail);
    show(spellings[i].label, path, spellings[i].fdinfo ? plain_fdinfo : "/proc/locks");
  }
}

int main(int argc, char **argv) {
  CHECK(argc >= 2);
  int links = 0, init = 0;
  for (int i = 2; i < argc; i++) {
    if (strcmp(argv[i], "native") == 0) {
      attempts = ATTEMPTS;
    } else if (strcmp(argv[i], "links") == 0) {
      links = 1;
    } else if (strcmp(argv[i], "init") == 0) {
      init = 1;
    } else {
      CHECK(!"an argument is native, links or init");
    }
  }
  if (init) {
    int fd = open("/proc/1/fdinfo/0", O_RDONLY | O_CLOEXEC);
    printf("/proc/1/fdinfo/0: open %s\n", fd >= 0 ? "ok" : errno_name(errno));
    printf("done\n");
    return 0;
  }
  char locked[4096];
  CHECK(snprintf(locked, sizeof locked, "%s/locked", argv[1]) < (int)sizeof locked);
  int first = open(locked, O_RDWR | O_CREAT | O_CLOEXEC, 0600);
  CHECK(first >= 0);
  int second = open(locked, O_RDWR | O_CLOEXEC);
  CHECK(second >= 0);
  struct flock lock = {.l_type = F_RDLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 0};
  CHECK(fcntl(first, F_OFD_SETLK, &lock) == 0);
  CHECK(fcntl(second, F_OFD_SETLK, &lock) == 0);
  int pid = getpid();
  int tid = (int)syscall(SYS_gettid);
  char plain_fdinfo[64];
  snprintf(plain_fdinfo, sizeof plain_fdinfo, "/proc/self/fdinfo/%d", first);
  if (links) {
    show_link_limit(first, plain_fdinfo);
    printf("done\n");
    return 0;
  }

  const char *locks[] = {
      "/proc/self/../locks",
      "/proc/<pid>/../locks",
      "/proc/../proc/locks",
      "/proc/self/task/<tid>/../../../locks",
      "/proc/thread-self/../locks",
      "/proc/locks/..",
      "/proc/self/../locks/",
  };
  const char *fdinfo[] = {
      "/proc/self/fdinfo/../fdinfo/<fd>",
      "/proc/self/fd/../fdinfo/<fd>",
      "/proc/self/../self/fdinfo/<fd>",
      "/proc/<pid>/fdinfo/../fdinfo/<fd>",
      "/proc/thread-self/fdinfo/../fdinfo/<fd>",
      "/proc/self/task/<tid>/fdinfo/../fdinfo/<fd>",
      "/proc/self/fdinfo/<fd>/..",
      "/proc/self/fdinfo/x/../<fd>",
      "/proc/self/fdinfo/99/..",
      "/proc/self/./fdinfo/<fd>",
      "/proc//self/fdinfo/<fd>",
      "/proc/self/fdinfo/99",
  };
  for (size_t i = 0; i < sizeof locks / sizeof locks[0]; i++) {
    char path[256];
    expand(locks[i], pid, tid, first, path, sizeof path);
    show(locks[i], path, "/proc/locks");
  }
  for (size_t i = 0; i < sizeof fdinfo / sizeof fdinfo[0]; i++) {
    char path[256];
    expand(fdinfo[i], pid, tid, first, path, sizeof path);
    show(fdinfo[i], path, plain_fdinfo);
  }
  printf("done\n");
  return 0;
}
