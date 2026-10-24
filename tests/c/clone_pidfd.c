/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A pidfd from clone(CLONE_PIDFD) or clone3's pidfd field
// (https://github.com/rrnewton/hermit/issues/4430). Modes:
//   clone   clone(CLONE_PIDFD): pidfd_send_signal(SIGKILL) through it, then
//           waitpid reports the kill
//   clone3  the same through clone3's pidfd field
//   clone3v0 the same with a 64-byte clone_args (its first version) that ends
//           at the end of a mapping, followed by an unmapped page
//   waitid  waitid(P_PIDFD, WNOHANG), polled, on a clone pidfd reaps the
//           child's exit status (a blocking P_PIDFD wait is refused by
//           Hermit, for any pidfd)
//   flags   the pidfd is close-on-exec, and the child, told its number over
//           a pipe, does not have it
//   vfork   clone(CLONE_PIDFD | CLONE_VM | CLONE_VFORK), as glibc's pidfd_spawn
//           makes its child: the clone succeeds and the child's exit status 4
//           is reaped (Hermit does not track this pidfd yet,
//           https://github.com/rrnewton/hermit/issues/4443)
//   rewrite the vfork child overwrites the parent's pidfd word with 1
//           (stdout): pidfd_send_signal through 1 is EBADF, as stdout is no
//           pidfd, and the child's status 4 is reaped
//   files   clone(CLONE_PIDFD | CLONE_FILES): the clone succeeds and the
//           child's exit status 6 is reaped
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef P_PIDFD
#define P_PIDFD 3
#endif

struct clone_args_v2 {
  uint64_t flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size,
      tls, set_tid, set_tid_size, cgroup;
};

static int vfork_body(void* arg) {
  if (arg != NULL) {
    *(volatile int*)arg = 1;
  }
  syscall(SYS_exit, 4);
  return 0;
}

/* Runs `flags` through glibc's clone with a child running vfork_body(arg),
 * then reaps it; returns its exit status. */
static int spawn(int flags, void* arg, int* pidfd) {
  size_t size = 1 << 16;
  char* stack = malloc(size);
  pid_t pid = clone(vfork_body, stack + size, flags, arg, pidfd);
  if (pid < 0) {
    perror("clone");
    return -1;
  }
  int status = 0;
  if (waitpid(pid, &status, 0) != pid) {
    perror("waitpid");
    return -1;
  }
  return WEXITSTATUS(status);
}

/* Polls waitid(P_PIDFD, WNOHANG) until the child's exit is reported (a
 * blocking P_PIDFD wait is refused by Hermit, for any pidfd). */
static int wait_pidfd(int pidfd, siginfo_t* info) {
  struct timespec tick = {0, 1000 * 1000};
  for (;;) {
    memset(info, 0, sizeof(*info));
    if (syscall(SYS_waitid, P_PIDFD, pidfd, info, WEXITED | WNOHANG, NULL) != 0) {
      perror("waitid");
      return -1;
    }
    if (info->si_pid != 0) {
      return 0;
    }
    nanosleep(&tick, NULL);
  }
}

int main(int argc, char** argv) {
  const char* mode = argc > 1 ? argv[1] : "clone";
  int pidfd = -1;
  long pid;
  if (!strcmp(mode, "vfork")) {
    int status = spawn(CLONE_PIDFD | CLONE_VM | CLONE_VFORK | SIGCHLD, NULL, &pidfd);
    printf("vfork: pidfd %d, status %d\n", pidfd >= 0, status);
    return 0;
  }
  if (!strcmp(mode, "rewrite")) {
    int status = spawn(CLONE_PIDFD | CLONE_VM | CLONE_VFORK | SIGCHLD, &pidfd, &pidfd);
    long sent = syscall(SYS_pidfd_send_signal, pidfd, 0, NULL, 0);
    printf("rewrite: word %d, send %ld (%s), status %d\n", pidfd, sent,
           sent == 0 ? "ok" : strerror(errno), status);
    return 0;
  }
  int report[2];
  if (pipe(report) != 0) {
    return 1;
  }
  if (!strcmp(mode, "files")) {
    pid = syscall(SYS_clone, CLONE_PIDFD | CLONE_FILES | SIGCHLD, 0, &pidfd, 0, 0);
    if (pid == 0) {
      _exit(6);
    }
    int status = 0;
    if (pid < 0 || waitpid(pid, &status, 0) != pid) {
      perror("clone");
      return 1;
    }
    printf("files: pidfd %d, status %d\n", pidfd >= 0, WEXITSTATUS(status));
    return 0;
  }
  if (!strcmp(mode, "clone3")) {
    struct clone_args_v2 args;
    memset(&args, 0, sizeof(args));
    args.flags = CLONE_PIDFD;
    args.pidfd = (uint64_t)(uintptr_t)&pidfd;
    args.exit_signal = SIGCHLD;
    pid = syscall(SYS_clone3, &args, sizeof(args));
  } else if (!strcmp(mode, "clone3v0")) {
    long page = sysconf(_SC_PAGESIZE);
    char* pages = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (pages == MAP_FAILED || munmap(pages + page, page) != 0) {
      return 1;
    }
    const size_t v0 = 64;
    struct clone_args_v2* args = (struct clone_args_v2*)(pages + page - v0);
    memset(args, 0, v0);
    args->flags = CLONE_PIDFD;
    args->pidfd = (uint64_t)(uintptr_t)&pidfd;
    args->exit_signal = SIGCHLD;
    pid = syscall(SYS_clone3, args, v0);
  } else {
    pid = syscall(SYS_clone, CLONE_PIDFD | SIGCHLD, 0, &pidfd, 0, 0);
  }
  if (pid < 0) {
    perror("clone");
    return 1;
  }
  if (pid == 0) {
    if (!strcmp(mode, "flags")) {
      // The parent sends the pidfd's number, which is not open here.
      int number = -1;
      if (read(report[0], &number, sizeof(number)) != (ssize_t)sizeof(number) || number < 0) {
        _exit(9);
      }
      _exit(fcntl(number, F_GETFD) == -1 && errno == EBADF ? 7 : 8);
    }
    if (!strcmp(mode, "waitid")) {
      _exit(5);
    }
    for (;;) {
      pause();
    }
  }
  if (!strcmp(mode, "waitid") || !strcmp(mode, "flags")) {
    siginfo_t info;
    int flags = fcntl(pidfd, F_GETFD);
    if (write(report[1], &pidfd, sizeof(pidfd)) != (ssize_t)sizeof(pidfd)) {
      return 1;
    }
    if (wait_pidfd(pidfd, &info) != 0) {
      return 1;
    }
    if (!strcmp(mode, "flags")) {
      printf("cloexec %d, child status %d\n", flags == FD_CLOEXEC, info.si_status);
    } else {
      printf("waitid: pid matches %d, code %d, status %d\n", info.si_pid == pid,
             info.si_code == CLD_EXITED, info.si_status);
    }
    return 0;
  }
  long sent = syscall(SYS_pidfd_send_signal, pidfd, SIGKILL, NULL, 0);
  if (sent != 0) {
    perror("pidfd_send_signal");
  }
  int status = 0;
  waitpid(pid, &status, 0);
  printf("%s: send %ld, killed by %d\n", mode, sent, WIFSIGNALED(status) ? WTERMSIG(status) : -1);
  return close(pidfd) == 0 ? 0 : 1;
}
