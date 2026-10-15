/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A seek in a thread whose descriptor table is its own: the worker is made
// with CLONE_THREAD but without CLONE_FILES, so descriptor 60 names one file
// in the leader and another in the worker (Codex review of
// https://github.com/rrnewton/hermit/pull/4406).
//
// Usage: thread_table_seek MODE DIR FILE OUT
//   file-in-worker  the leader's 60 is DIR; the worker moves FILE onto its 60,
//                   seeks it to 2 and copies 2 bytes from it with sendfile
//                   into OUT, which the leader then prints.
//   dir-in-worker   the leader's 60 is FILE; the worker moves DIR onto its 60
//                   and seeks it to SEEK_END.
// The leader opens both before making the worker, which inherits a copy of
// its table, so the worker only moves a descriptor it has (dup2). The leader
// creates FILE and writes "abcdef" through that descriptor, so its offset is 6
// when the worker gets it. The worker makes raw system calls only and leaves
// its results in globals, which the leader prints once it has exited.
#define _GNU_SOURCE
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#define TARGET_FD 60
#define OUT_FD 61
#define OTHER_FD 62

static char worker_stack[64 * 1024] __attribute__((aligned(16)));
static const char* mode;
static volatile long seek_result = -1;
static volatile long copy_result = -1;
static volatile int worker_done;

static long raw(long number, long a, long b, long c, long d) {
  return syscall(number, a, b, c, d);
}

static int worker(void* unused) {
  (void)unused;
  int worker_file = !strcmp(mode, "file-in-worker");
  if (raw(SYS_dup2, OTHER_FD, TARGET_FD, 0, 0) == TARGET_FD) {
    if (worker_file) {
      seek_result = raw(SYS_lseek, TARGET_FD, 2, SEEK_SET, 0);
      copy_result = raw(SYS_sendfile, OUT_FD, TARGET_FD, 0, 2);
    } else {
      seek_result = raw(SYS_lseek, TARGET_FD, 0, SEEK_END, 0);
    }
  }
  worker_done = 1;
  raw(SYS_exit, 0, 0, 0, 0);
  return 0;
}

int main(int argc, char** argv) {
  if (argc != 5 || (strcmp(argv[1], "file-in-worker") && strcmp(argv[1], "dir-in-worker"))) {
    fprintf(stderr, "usage: %s file-in-worker|dir-in-worker DIR FILE OUT\n", argv[0]);
    return 64;
  }
  mode = argv[1];
  const char* dir = argv[2];
  const char* file = argv[3];
  setvbuf(stdout, NULL, _IONBF, 0);
  int leader_file = !strcmp(mode, "dir-in-worker");
  int dirfd = open(dir, O_RDONLY | O_DIRECTORY);
  int filefd = open(file, O_RDWR | O_CREAT | O_TRUNC, 0644);
  if (dirfd < 0 || filefd < 0 || write(filefd, "abcdef", 6) != 6 || dup2(leader_file ? filefd : dirfd, TARGET_FD) != TARGET_FD ||
      dup2(leader_file ? dirfd : filefd, OTHER_FD) != OTHER_FD) {
    perror("leader descriptors");
    return 1;
  }
  close(dirfd);
  close(filefd);
  int out = open(argv[4], O_RDWR | O_CREAT | O_TRUNC, 0644);
  if (out < 0 || dup2(out, OUT_FD) != OUT_FD) {
    perror(argv[4]);
    return 1;
  }
  close(out);
  int flags = CLONE_VM | CLONE_FS | CLONE_SIGHAND | CLONE_THREAD | CLONE_SYSVSEM;
  if (clone(worker, worker_stack + sizeof(worker_stack), flags, NULL) < 0) {
    perror("clone");
    return 1;
  }
  while (!worker_done) {
    sched_yield();
  }
  if (leader_file) {
    printf("%s seek end %s\n", mode, seek_result >= 0 ? "ok" : "failed");
  } else {
    char copied[8] = {0};
    if (pread(OUT_FD, copied, sizeof(copied) - 1, 0) < 0) {
      perror("pread");
      return 1;
    }
    printf("%s seek %ld copied %ld \"%s\"\n", mode, seek_result, copy_result, copied);
  }
  return 0;
}
