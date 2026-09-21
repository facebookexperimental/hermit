/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Record/replay regression for Detcore's virtual /proc timer-slack file.
 *
 * Replayer reserves a recorded virtual descriptor with an eventfd. The procfs
 * layer must therefore bind the task incarnation named by the proc path, not
 * the placeholder's anonymous inode. The first open is AT_FDCWD-relative and
 * exercises zero-length read/pread plus lseek before a snapshot exists: none
 * may touch Replayer's placeholder. Reading and writing the same virtual scalar
 * then drives both affected operations; printing the values makes any
 * record/replay control-flow or state mismatch visible to --verify.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <unistd.h>

static int fail(const char* operation) {
  fprintf(stderr, "%s failed: %s\n", operation, strerror(errno));
  return 1;
}

static long read_proc_timer_slack(void) {
  const int fd = open("/proc/self/timerslack_ns", O_RDONLY);
  if (fd < 0) {
    return -1;
  }

  char buffer[64] = {0};
  const ssize_t length = read(fd, buffer, sizeof(buffer) - 1);
  const int saved_errno = errno;
  if (close(fd) != 0 && length >= 0) {
    return -1;
  }
  if (length <= 0) {
    errno = saved_errno;
    return -1;
  }

  char* end = NULL;
  errno = 0;
  const long value = strtol(buffer, &end, 10);
  if (errno != 0 || end == buffer || (*end != '\n' && *end != '\0')) {
    errno = EINVAL;
    return -1;
  }
  return value;
}

static long read_relative_proc_timer_slack(void) {
  if (chdir("/") != 0) {
    return -1;
  }
  const int path_fd =
      open("proc/self/timerslack_ns", O_PATH | O_CLOEXEC);
  if (path_fd < 0) {
    return -1;
  }
  errno = 0;
  if (lseek(path_fd, 0, SEEK_SET) != -1 || errno != EBADF) {
    const int saved_errno = errno == 0 ? EIO : errno;
    close(path_fd);
    errno = saved_errno;
    return -1;
  }
  if (close(path_fd) != 0) {
    return -1;
  }

  const int fd = open("proc/self/timerslack_ns", O_RDONLY);
  if (fd < 0) {
    return -1;
  }

  char untouched = 0;
  if (read(fd, &untouched, 0) != 0 || pread(fd, &untouched, 0, 0) != 0 ||
      lseek(fd, 1, SEEK_SET) != 1 || lseek(fd, 0, SEEK_CUR) != 1) {
    const int saved_errno = errno == 0 ? EIO : errno;
    close(fd);
    errno = saved_errno;
    return -1;
  }
  errno = 0;
  if (lseek(fd, 0, SEEK_END) != -1 || errno != EINVAL ||
      lseek(fd, 0, SEEK_SET) != 0) {
    const int saved_errno = errno == 0 ? EIO : errno;
    close(fd);
    errno = saved_errno;
    return -1;
  }

  char buffer[64] = {0};
  const ssize_t length = read(fd, buffer, sizeof(buffer) - 1);
  const int saved_errno = errno;
  if (close(fd) != 0 && length >= 0) {
    return -1;
  }
  if (length <= 0) {
    errno = saved_errno;
    return -1;
  }

  char* end = NULL;
  errno = 0;
  const long value = strtol(buffer, &end, 10);
  if (errno != 0 || end == buffer || (*end != '\n' && *end != '\0')) {
    errno = EINVAL;
    return -1;
  }
  return value;
}

int main(void) {
  const long before = read_relative_proc_timer_slack();
  if (before < 0) {
    return fail("relative timer-slack zero-read/seek probe");
  }

  const int fd = open("/proc/self/timerslack_ns", O_WRONLY);
  if (fd < 0) {
    return fail("open(/proc/self/timerslack_ns, O_WRONLY)");
  }
  static const char requested[] = "777777\n";
  if (write(fd, requested, sizeof(requested) - 1) !=
      (ssize_t)(sizeof(requested) - 1)) {
    return fail("write(/proc/self/timerslack_ns)");
  }
  if (close(fd) != 0) {
    return fail("close(/proc/self/timerslack_ns)");
  }

  const long after_proc = read_proc_timer_slack();
  if (after_proc < 0) {
    return fail("reread(/proc/self/timerslack_ns)");
  }
  const long after_prctl = prctl(PR_GET_TIMERSLACK, 0, 0, 0, 0);
  if (after_prctl < 0) {
    return fail("prctl(PR_GET_TIMERSLACK)");
  }
  if (after_proc != 777777 || after_prctl != 777777) {
    fprintf(
        stderr,
        "timer slack mismatch: proc=%ld prctl=%ld\n",
        after_proc,
        after_prctl);
    return 2;
  }

  printf(
      "relative_zero=ok relative_seek=ok opath_seek=ok before=%ld "
      "after_proc=%ld after_prctl=%ld\n",
      before,
      after_proc,
      after_prctl);
  return 0;
}
