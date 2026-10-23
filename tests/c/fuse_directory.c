/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A FUSE file system whose root is one directory with 1024-byte blocks, an
// anonymous device and a size that changes, as procfs's stat fields look but
// with a changing size (Codex re-check of
// https://github.com/rrnewton/hermit/pull/4369).
//
// Usage: fuse_directory MODE MOUNT CONTROL COMMAND [ARGS...]
//   Mounts the file system on MOUNT (inside a user and mount namespace the
//   caller made), runs COMMAND, then unmounts and exits with COMMAND's status.
//   The directory's size is the number in CONTROL (1111 when CONTROL is
//   missing or unreadable). MODE `fixed` always reports 1024-byte blocks;
//   `changing` reports 1024-byte blocks at size 1111 and 4096-byte blocks
//   otherwise, so one open directory's block size changes.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/fuse.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static const char* control;
static int changing;

static unsigned long long directory_size(void) {
  unsigned long long size = 1111;
  FILE* file = fopen(control, "r");
  if (file) {
    if (fscanf(file, "%llu", &size) != 1) {
      size = 1111;
    }
    fclose(file);
  }
  return size;
}

static void fill_attr(struct fuse_attr* attr) {
  unsigned long long size = directory_size();
  memset(attr, 0, sizeof(*attr));
  attr->ino = FUSE_ROOT_ID;
  attr->mode = S_IFDIR | 0755;
  attr->nlink = 2;
  attr->size = size;
  attr->blocks = 8;
  attr->blksize = changing && size != 1111 ? 4096 : 1024;
}

static int reply(int fd, uint64_t unique, int error, const void* data, size_t len) {
  unsigned char buf[4096];
  struct fuse_out_header header = {
      .len = sizeof(header) + len, .error = -error, .unique = unique};
  if (sizeof(header) + len > sizeof(buf)) {
    return -1;
  }
  memcpy(buf, &header, sizeof(header));
  if (len) {
    memcpy(buf + sizeof(header), data, len);
  }
  return write(fd, buf, sizeof(header) + len) == (ssize_t)(sizeof(header) + len) ? 0 : -1;
}

static void serve(int fd) {
  static unsigned char buf[FUSE_MIN_READ_BUFFER * 2];
  for (;;) {
    ssize_t n = read(fd, buf, sizeof(buf));
    if (n < 0 && errno == EINTR) {
      continue;
    }
    if (n < (ssize_t)sizeof(struct fuse_in_header)) {
      _exit(0);
    }
    struct fuse_in_header* in = (struct fuse_in_header*)buf;
    int rc = 0;
    switch (in->opcode) {
      case FUSE_INIT: {
        struct fuse_init_in* init = (struct fuse_init_in*)(buf + sizeof(*in));
        struct fuse_init_out out = {0};
        out.major = FUSE_KERNEL_VERSION;
        out.minor = init->minor < 31 ? init->minor : 31;
        out.max_write = 4096;
        out.max_background = 8;
        out.congestion_threshold = 6;
        out.time_gran = 1;
        rc = reply(fd, in->unique, 0, &out, sizeof(out));
        break;
      }
      case FUSE_GETATTR: {
        // attr_valid 0: the kernel asks again on every stat.
        struct fuse_attr_out out = {0};
        fill_attr(&out.attr);
        rc = reply(fd, in->unique, 0, &out, sizeof(out));
        break;
      }
      case FUSE_OPENDIR: {
        struct fuse_open_out out = {.fh = 1};
        rc = reply(fd, in->unique, 0, &out, sizeof(out));
        break;
      }
      case FUSE_STATFS: {
        struct fuse_statfs_out out = {0};
        out.st.bsize = 1024;
        out.st.frsize = 1024;
        out.st.namelen = 255;
        rc = reply(fd, in->unique, 0, &out, sizeof(out));
        break;
      }
      case FUSE_READDIR:
      case FUSE_READDIRPLUS:
      case FUSE_RELEASEDIR:
      case FUSE_ACCESS:
      case FUSE_FLUSH:
        rc = reply(fd, in->unique, 0, NULL, 0);
        break;
      case FUSE_FORGET:
      case FUSE_BATCH_FORGET:
        continue;
      case FUSE_DESTROY:
        reply(fd, in->unique, 0, NULL, 0);
        _exit(0);
      default:
        rc = reply(fd, in->unique, in->opcode == FUSE_LOOKUP ? ENOENT : ENOSYS, NULL, 0);
        break;
    }
    if (rc != 0) {
      _exit(0);
    }
  }
}

int main(int argc, char** argv) {
  if (argc < 5 || (strcmp(argv[1], "fixed") && strcmp(argv[1], "changing"))) {
    fprintf(stderr, "usage: %s fixed|changing MOUNT CONTROL COMMAND [ARGS...]\n", argv[0]);
    return 64;
  }
  changing = !strcmp(argv[1], "changing");
  const char* mount_point = argv[2];
  control = argv[3];
  int fd = open("/dev/fuse", O_RDWR | O_CLOEXEC);
  if (fd < 0) {
    perror("/dev/fuse");
    return 3;
  }
  char options[128];
  snprintf(
      options,
      sizeof(options),
      "fd=%d,rootmode=40000,user_id=%u,group_id=%u",
      fd,
      getuid(),
      getgid());
  if (mount("fuse_directory", mount_point, "fuse", MS_NOSUID | MS_NODEV, options) != 0) {
    perror("mount fuse");
    return 4;
  }
  pid_t parent = getpid();
  pid_t server = fork();
  if (server < 0) {
    perror("fork");
    return 5;
  }
  if (server == 0) {
    if (prctl(PR_SET_PDEATHSIG, SIGKILL) != 0 || getppid() != parent) {
      _exit(1);
    }
    serve(fd);
  }
  close(fd);
  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return 5;
  }
  if (child == 0) {
    execvp(argv[4], argv + 4);
    perror(argv[4]);
    _exit(127);
  }
  int status = 0;
  while (waitpid(child, &status, 0) < 0) {
    if (errno != EINTR) {
      perror("waitpid");
      return 6;
    }
  }
  umount2(mount_point, MNT_DETACH);
  kill(server, SIGKILL);
  waitpid(server, NULL, 0);
  return WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status);
}
