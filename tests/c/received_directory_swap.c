/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * received_directory_swap: a directory opened below a received /proc
 * descriptor that another thread replaces during the open behaves as the
 * object the open returned, whatever was named first.
 *
 * The process receives /proc over SCM_RIGHTS as descriptor N, so Detcore has
 * no record of it, and makes an ordinary directory D holding self/marker but
 * no self/mountinfo. The main thread opens "self" below N. A second thread,
 * started just before, busy-waits about SPIN iterations and then replaces N
 * with D (dup2). Depending on when the replacement lands, the returned
 * descriptor R holds /proc/self or D/self. Linux allows both, but R must then
 * behave as what it holds: on procfs, mountinfo opens below it; on D/self,
 * marker opens below it and mountinfo is ENOENT. A tool that marked R as
 * /proc/self from the name it saw before the replacement would open a
 * mountinfo below an ordinary directory that has none.
 *
 * It prints which object R held ("procfs" or "replacement") and exits 0 when
 * R behaved as that object. Under Hermit the test widens the window with the
 * procfs_spelling_resolved race probe, so the replacement lands between
 * Detcore naming N and the kernel's open.
 *
 * usage: received_directory_swap [SPIN]
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/magic.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/vfs.h>
#include <unistd.h>

static atomic_int go;
static int received = -1;
static int replacement = -1;
static unsigned long long spin = 400000000ULL;

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
  if (sendmsg(pair[0], &message, 0) != 1) {
    return -1;
  }
  memset(control, 0, sizeof(control));
  message.msg_controllen = sizeof(control);
  int got = -1;
  if (recvmsg(pair[1], &message, 0) == 1) {
    header = CMSG_FIRSTHDR(&message);
    if (header != NULL && header->cmsg_type == SCM_RIGHTS) {
      memcpy(&got, CMSG_DATA(header), sizeof(int));
    }
  }
  return got;
}

static void* replace(void* unused) {
  (void)unused;
  while (!atomic_load(&go)) {
  }
  for (volatile unsigned long long i = 0; i < spin; i++) {
  }
  if (dup2(replacement, received) != received) {
    perror("dup2");
    exit(2);
  }
  return NULL;
}

int main(int argc, char** argv) {
  if (argc > 1) {
    spin = strtoull(argv[1], NULL, 10);
  }
  char directory[] = "/tmp/received-directory-swap-XXXXXX";
  if (mkdtemp(directory) == NULL) {
    perror("mkdtemp");
    return 2;
  }
  char path[128];
  snprintf(path, sizeof(path), "%s/self", directory);
  if (mkdir(path, 0755) != 0) {
    perror("mkdir self");
    return 2;
  }
  snprintf(path, sizeof(path), "%s/self/marker", directory);
  int marker = open(path, O_WRONLY | O_CREAT | O_EXCL, 0644);
  if (marker < 0) {
    perror("create marker");
    return 2;
  }
  close(marker);
  replacement = open(directory, O_RDONLY | O_DIRECTORY);
  int pair[2];
  if (replacement < 0 || socketpair(AF_UNIX, SOCK_STREAM, 0, pair) != 0) {
    perror("setup");
    return 2;
  }
  int proc = open("/proc", O_RDONLY | O_DIRECTORY);
  received = proc < 0 ? -1 : pass_to_self(pair, proc);
  close(proc);
  if (received < 0) {
    fprintf(stderr, "no /proc descriptor was received\n");
    return 2;
  }

  pthread_t thread;
  if (pthread_create(&thread, NULL, replace, NULL) != 0) {
    perror("pthread_create");
    return 2;
  }
  atomic_store(&go, 1);
  int opened = openat(received, "self", O_RDONLY | O_DIRECTORY);
  pthread_join(thread, NULL);
  if (opened < 0) {
    perror("openat(self)");
    return 1;
  }

  int failures = 0;
  struct statfs filesystem;
  if (fstatfs(opened, &filesystem) != 0) {
    perror("fstatfs");
    return 1;
  }
  if (filesystem.f_type == PROC_SUPER_MAGIC) {
    printf("procfs\n");
    int mountinfo = openat(opened, "mountinfo", O_RDONLY);
    if (mountinfo < 0) {
      fprintf(stderr, "FAIL: /proc/self holds no mountinfo: %s\n", strerror(errno));
      failures++;
    }
  } else {
    printf("replacement\n");
    int found = openat(opened, "marker", O_RDONLY);
    if (found < 0) {
      fprintf(stderr, "FAIL: the replacement's marker does not open: %s\n", strerror(errno));
      failures++;
    }
    errno = 0;
    int mountinfo = openat(opened, "mountinfo", O_RDONLY);
    if (mountinfo >= 0 || errno != ENOENT) {
      fprintf(stderr,
              "FAIL: below an ordinary directory, mountinfo opened (%d) or failed with %s, "
              "not ENOENT\n",
              mountinfo, strerror(errno));
      failures++;
    }
  }

  snprintf(path, sizeof(path), "%s/self/marker", directory);
  unlink(path);
  snprintf(path, sizeof(path), "%s/self", directory);
  rmdir(path);
  rmdir(directory);
  return failures == 0 ? 0 : 1;
}
