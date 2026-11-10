/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// clone3 with set_tid asking for a chosen pid
// (https://github.com/rrnewton/hermit/issues/4370). Modes:
//   reaped  the pid of a child just reaped
//   thread  the tid of a thread just joined
//   fresh   a pid no task has had (the first child's plus 50)
//   newpid  a reaped child's pid, for a child in a new PID namespace
//           (CLONE_NEWPID, set_tid = {1, pid})
//   newpidzero the newpid call with set_tid = {0, pid}, which Linux rejects
//   held    the caller's own pid, which a live task holds
//   zombie  the pid of a child that exited and is not yet reaped
//   foreignzombie the pid of another process's exited, unreaped child
//   autoreap the pid of a child reaped automatically (SIGCHLD ignored)
//   orphan  the pid of a grandchild whose parent exited, reaped by init
//   efault  a reaped child's pid, in a two-entry set_tid array whose second
//           entry is on an unmapped page
//   edge    a reaped child's pid, in a one-entry set_tid array in the last
//           four bytes of a page that an unmapped page follows
//   unaligned a reaped child's pid, in a one-entry set_tid array at an
//           address two bytes past a four-byte boundary
//   wronly  a reaped child's pid, in a one-entry set_tid array on a page
//           mapped writable but not readable, which the kernel reads
//   wronlyargs a reaped child's pid, with the struct clone_args itself on
//           such a page
//   size73  a reaped child's pid, in a one-entry set_tid array, with a
//           73-byte struct clone_args (the kernel zero-extends it)
//   nonetail a reaped child's pid, with a struct clone_args whose last 48
//           bytes lie on a PROT_NONE page (EFAULT)
//   deep    a reaped child's pid, followed by a pid for the namespace holding
//           the caller's (set_tid = {pid, 1})
//   nocap   a reaped child's pid, asked for after dropping every effective
//           capability, which Linux answers with EPERM
// Prints "got the requested pid: 1" when the kernel gave the new child that
// pid, or "clone3: <error>" and exits 1 when the call failed.
#define _GNU_SOURCE
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <errno.h>
#include <linux/capability.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

struct clone_args_v2 {
  uint64_t flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size,
      tls, set_tid, set_tid_size, cgroup;
};

static _Atomic pid_t worker_tid;

static void* worker(void* arg) {
  (void)arg;
  atomic_store(&worker_tid, (pid_t)syscall(SYS_gettid));
  return NULL;
}

static pid_t reaped_child(void) {
  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return -1;
  }
  if (child == 0) {
    _exit(0);
  }
  int status = 0;
  if (waitpid(child, &status, 0) != child) {
    perror("waitpid");
    return -1;
  }
  return child;
}

/* Wait, polling, until no task has the id `pid`; reap it ourselves if we
 * are its parent after all (as a PID namespace's init is natively). */
static void wait_until_gone(pid_t pid) {
  struct timespec tick = {0, 1000000};
  while (kill(pid, 0) == 0 || errno != ESRCH) {
    waitpid(pid, NULL, WNOHANG);
    nanosleep(&tick, NULL);
  }
}

static pid_t orphan_grandchild(void) {
  int fds[2];
  if (pipe(fds) != 0) {
    return -1;
  }
  pid_t middle = fork();
  if (middle == 0) {
    pid_t parent = getpid();
    if (fork() == 0) {
      pid_t self = getpid();
      if (write(fds[1], &self, sizeof(self)) != (ssize_t)sizeof(self)) {
        _exit(1);
      }
      struct timespec tick = {0, 1000000};
      while (getppid() == parent) {
        nanosleep(&tick, NULL);
      }
      _exit(0);
    }
    _exit(0);
  }
  pid_t grandchild = -1;
  if (read(fds[0], &grandchild, sizeof(grandchild)) != (ssize_t)sizeof(grandchild)) {
    return -1;
  }
  waitpid(middle, NULL, 0);
  wait_until_gone(grandchild);
  return grandchild;
}

/* A process that keeps an exited child unreaped until `release` is
 * written to; returns the child's pid through `*keeper`'s pipe. */
static pid_t foreign_zombie(pid_t* keeper, int* release) {
  int report[2], hold[2];
  if (pipe(report) != 0 || pipe(hold) != 0) {
    return -1;
  }
  *keeper = fork();
  if (*keeper == 0) {
    pid_t zombie = fork();
    if (zombie == 0) {
      _exit(0);
    }
    siginfo_t info;
    waitid(P_PID, zombie, &info, WEXITED | WNOWAIT);
    if (write(report[1], &zombie, sizeof(zombie)) != (ssize_t)sizeof(zombie)) {
      _exit(1);
    }
    char byte;
    if (read(hold[0], &byte, 1) != 1) {
      _exit(1);
    }
    waitpid(zombie, NULL, 0);
    _exit(0);
  }
  pid_t zombie = -1;
  if (read(report[0], &zombie, sizeof(zombie)) != (ssize_t)sizeof(zombie)) {
    return -1;
  }
  *release = hold[1];
  return zombie;
}

int main(int argc, char** argv) {
  const char* mode = argc > 1 ? argv[1] : "reaped";
  pid_t want;
  pid_t zombie = 0, keeper = 0;
  int release = -1;
  if (!strcmp(mode, "foreignzombie")) {
    want = foreign_zombie(&keeper, &release);
    if (want < 0) {
      return 1;
    }
  } else if (!strcmp(mode, "zombie")) {
    zombie = fork();
    if (zombie == 0) {
      _exit(0);
    }
    siginfo_t info;
    if (waitid(P_PID, zombie, &info, WEXITED | WNOWAIT) != 0) {
      perror("waitid");
      return 1;
    }
    want = zombie;
  } else if (!strcmp(mode, "autoreap")) {
    signal(SIGCHLD, SIG_IGN);
    want = fork();
    if (want == 0) {
      _exit(0);
    }
    wait_until_gone(want);
    signal(SIGCHLD, SIG_DFL);
  } else if (!strcmp(mode, "orphan")) {
    want = orphan_grandchild();
    if (want < 0) {
      return 1;
    }
  } else if (!strcmp(mode, "thread")) {
    pthread_t thread;
    if (pthread_create(&thread, NULL, worker, NULL) != 0) {
      return 1;
    }
    pthread_join(thread, NULL);
    want = atomic_load(&worker_tid);
  } else {
    want = reaped_child();
    if (want < 0) {
      return 1;
    }
    if (!strcmp(mode, "fresh")) {
      want += 50;
    }
  }
  if (!strcmp(mode, "held")) {
    want = getpid();
  }
  pid_t storage[2] = {want, 0};
  pid_t* tids = storage;
  struct clone_args_v2 args;
  memset(&args, 0, sizeof(args));
  args.exit_signal = SIGCHLD;
  args.set_tid_size = 1;
  if (!strcmp(mode, "deep")) {
    tids[1] = 1;
    args.set_tid_size = 2;
  }
  if (!strcmp(mode, "nocap")) {
    struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
    struct __user_cap_data_struct data[2];
    if (syscall(SYS_capget, &header, data) != 0) {
      perror("capget");
      return 1;
    }
    data[0].effective = 0;
    data[1].effective = 0;
    if (syscall(SYS_capset, &header, data) != 0) {
      perror("capset");
      return 1;
    }
  }
  if (!strcmp(mode, "efault") || !strcmp(mode, "edge")) {
    long page = sysconf(_SC_PAGESIZE);
    char* pages = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (pages == MAP_FAILED || munmap(pages + page, page) != 0) {
      return 1;
    }
    tids = (pid_t*)(pages + page - sizeof(pid_t));
    tids[0] = want;
    args.set_tid_size = strcmp(mode, "edge") ? 2 : 1;
  }
  if (!strcmp(mode, "unaligned")) {
    static char buffer[16] __attribute__((aligned(8)));
    tids = (pid_t*)(buffer + 2);
    memcpy(tids, &want, sizeof want);
    args.set_tid_size = 1;
  }
  if (!strcmp(mode, "wronly")) {
    tids = mmap(NULL, sysconf(_SC_PAGESIZE), PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (tids == MAP_FAILED) {
      return 1;
    }
    tids[0] = want;
    args.set_tid_size = 1;
  }
  args.set_tid = (uint64_t)(uintptr_t)tids;
  if (!strcmp(mode, "newpid") || !strcmp(mode, "newpidzero")) {
    tids[0] = strcmp(mode, "newpid") ? 0 : 1;
    tids[1] = want;
    args.flags = CLONE_NEWPID;
    args.set_tid_size = 2;
  }
  struct clone_args_v2* argp = &args;
  size_t size = sizeof(args);
  if (!strcmp(mode, "size73")) {
    size = 73;
  }
  if (!strcmp(mode, "nonetail")) {
    long page = sysconf(_SC_PAGESIZE);
    char* pages = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (pages == MAP_FAILED || mprotect(pages + page, page, PROT_NONE) != 0) {
      return 1;
    }
    argp = (struct clone_args_v2*)(pages + page - 40);
    memcpy(argp, &args, 40);
  }
  if (!strcmp(mode, "wronlyargs")) {
    argp = mmap(NULL, sysconf(_SC_PAGESIZE), PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (argp == MAP_FAILED) {
      return 1;
    }
    *argp = args;
  }
  pid_t child = (pid_t)syscall(SYS_clone3, argp, size);
  if (child < 0) {
    perror("clone3");
    if (zombie > 0) {
      waitpid(zombie, NULL, 0);
    }
    if (keeper > 0) {
      if (write(release, "x", 1) != 1) {
        return 1;
      }
      waitpid(keeper, NULL, 0);
    }
    return 1;
  }
  if (child == 0) {
    _exit(0);
  }
  int status = 0;
  if (waitpid(child, &status, 0) != child) {
    perror("waitpid");
    return 1;
  }
  printf("got the requested pid: %d\n", child == want);
  return 0;
}
