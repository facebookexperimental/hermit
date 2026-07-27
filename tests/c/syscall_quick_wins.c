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
#include <linux/seccomp.h>
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

  char sendfile_path[128];
  snprintf(sendfile_path, sizeof(sendfile_path),
           "/tmp/hermit-syscall-sendfile-%ld", (long)getpid());
  int sendfile_fd = open(sendfile_path, O_CREAT | O_TRUNC | O_RDWR, 0600);
  if (sendfile_fd < 0 || lseek(fd, 0, SEEK_SET) != 0) {
    perror("open/lseek sendfile");
    return 1;
  }
  off_t offset = 0;
  if (syscall(SYS_sendfile, sendfile_fd, fd, &offset, 1) != 1 || offset != 1) {
    perror("sendfile");
    return 1;
  }
  char copied = '\0';
  if (lseek(sendfile_fd, 0, SEEK_SET) != 0 ||
      read(sendfile_fd, &copied, 1) != 1 || copied != 'x') {
    fputs("sendfile did not copy the expected byte\n", stderr);
    return 1;
  }
  if (close(fd) != 0 || close(sendfile_fd) != 0 || unlink(path) != 0 ||
      unlink(sendfile_path) != 0) {
    perror("close/unlink sendfile files");
    return 1;
  }

  int range_fd = open("/dev/null", O_RDONLY);
  if (range_fd < 0) {
    perror("open close_range fixture");
    return 1;
  }
  int high_fd = fcntl(range_fd, F_DUPFD_CLOEXEC, 100);
  if (high_fd < 100 || close(range_fd) != 0) {
    perror("prepare close_range");
    return 1;
  }
  require_zero(syscall(SYS_close_range, (unsigned int)high_fd,
                       (unsigned int)high_fd, 0),
               "close_range");
  errno = 0;
  if (fcntl(high_fd, F_GETFD) != -1 || errno != EBADF) {
    fputs("close_range left its descriptor open\n", stderr);
    return 1;
  }

  struct seccomp_notif_sizes sizes;
  errno = 0;
  if (syscall(SYS_seccomp, SECCOMP_GET_NOTIF_SIZES, 0, &sizes) != -1 ||
      errno != ENOSYS) {
    fputs("seccomp capability probe did not return ENOSYS\n", stderr);
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
         "net=ok fd=ok security=ok\n",
         ruid, euid, suid, rgid, egid, sgid);
  return 0;
}
