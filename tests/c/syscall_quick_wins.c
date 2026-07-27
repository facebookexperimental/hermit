/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <unistd.h>

static void require_zero(long result, const char *name) {
  if (result != 0) {
    fprintf(stderr, "%s failed: %s\n", name, strerror(errno));
    exit(1);
  }
}

int main(void) {
  uid_t ruid = (uid_t)-1;
  uid_t euid = (uid_t)-1;
  uid_t suid = (uid_t)-1;
  gid_t rgid = (gid_t)-1;
  gid_t egid = (gid_t)-1;
  gid_t sgid = (gid_t)-1;

  require_zero(syscall(SYS_getresuid, &ruid, &euid, &suid), "getresuid");
  require_zero(syscall(SYS_getresgid, &rgid, &egid, &sgid), "getresgid");
  if (ruid == (uid_t)-1 || euid == (uid_t)-1 || suid == (uid_t)-1 ||
      rgid == (gid_t)-1 || egid == (gid_t)-1 || sgid == (gid_t)-1) {
    fputs("credential syscall did not initialize every output\n", stderr);
    return 1;
  }

  size_t page_size = (size_t)sysconf(_SC_PAGESIZE);
  void *page = mmap(NULL, page_size, PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (page == MAP_FAILED) {
    perror("mmap");
    return 1;
  }
  require_zero(syscall(SYS_munlock, page, page_size), "munlock");
  require_zero(syscall(SYS_munlockall), "munlockall");
  if (munmap(page, page_size) != 0) {
    perror("munmap");
    return 1;
  }

  char path[128];
  snprintf(path, sizeof(path), "/tmp/hermit-syscall-quick-wins-%ld",
           (long)getpid());
  int fd = open(path, O_CREAT | O_TRUNC | O_RDWR, 0600);
  if (fd < 0 || write(fd, "x", 1) != 1) {
    perror("open/write");
    return 1;
  }
  require_zero(syscall(SYS_fsync, fd), "fsync");
  if (close(fd) != 0 || unlink(path) != 0) {
    perror("close/unlink");
    return 1;
  }

  /* shutdown(2): the socket half-close control op, determinized in #818. Half-
   * close the write end of a connected AF_UNIX stream socketpair; the call must
   * return 0. This exercises shutdown as one of the bundled quick-win syscalls
   * under --panic-on-unsupported-syscalls, complementing the standalone
   * tests/rust/shutdown.rs guest with a C-language witness. */
  int sv[2];
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) {
    perror("socketpair");
    return 1;
  }
  require_zero(syscall(SYS_shutdown, sv[0], SHUT_WR), "shutdown");
  if (close(sv[0]) != 0 || close(sv[1]) != 0) {
    perror("close socketpair");
    return 1;
  }

  printf("syscall-quick-wins-ok uids=%u:%u:%u gids=%u:%u:%u vm=ok fs=ok "
         "net=ok\n",
         ruid, euid, suid, rgid, egid, sgid);
  return 0;
}
