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
 * after. Natively, other processes can change /proc/locks twice within those
 * three reads, so a mismatch is tried again, up to ATTEMPTS times a
 * millisecond apart, before it is reported; under Hermit, whose /proc/locks
 * holds only the guest's own rows, the first attempt decides. Lines name each
 * spelling by a template: <fd> is the first locked descriptor, <pid> and
 * <tid> the caller's own numbers. Nothing printed depends on the host, the
 * backend or the run.
 *
 * Usage: procfs_dotdot_spellings DIRECTORY
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

/* Print the spelling `label` (a template) at `path`: its read, compared with
 * the plain spelling `plain` (see the top of the file), and its stat. */
static void show(const char *label, const char *path, const char *plain) {
  int error = 0;
  int equal = 0;
  for (int attempt = 0; attempt < ATTEMPTS && !equal && error == 0; attempt++) {
    if (attempt > 0) {
      struct timespec pause = {.tv_sec = 0, .tv_nsec = 1000000};
      nanosleep(&pause, NULL);
    }
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
  }
  if (error != 0) {
    printf("%s: open or read %s\n", label, errno_name(error));
  } else {
    printf("%s: read %s\n", label, equal ? "the plain spelling's bytes" : "other bytes than the plain spelling");
  }
  struct stat status;
  if (stat(path, &status) < 0) {
    printf("%s: stat %s\n", label, errno_name(errno));
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

int main(int argc, char **argv) {
  CHECK(argc == 2);
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
