/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef __WNOTHREAD
#define __WNOTHREAD 0x20000000
#endif

struct blocked_child {
  pid_t pid;
  int release_fd;
};

static const char *wait_stage = "unset";

static void fail(const char *message) {
  fprintf(stderr, "%s: %s\n", message, strerror(errno));
  exit(1);
}

static struct blocked_child spawn_blocked(int exit_code) {
  int release_pipe[2];
  if (pipe(release_pipe) != 0)
    fail("pipe");
  pid_t child = fork();
  if (child < 0)
    fail("fork");
  if (child == 0) {
    char token;
    close(release_pipe[1]);
    if (read(release_pipe[0], &token, 1) != 1)
      _exit(126);
    _exit(exit_code);
  }
  close(release_pipe[0]);
  return (struct blocked_child){.pid = child, .release_fd = release_pipe[1]};
}

static void release_child(struct blocked_child child) {
  const char token = 'x';
  if (write(child.release_fd, &token, 1) != 1)
    fail("release child");
  close(child.release_fd);
}

static void require_exit(pid_t waited, pid_t expected, int status, int code) {
  if (waited != expected || !WIFEXITED(status) || WEXITSTATUS(status) != code) {
    fprintf(stderr,
            "%s: waited=%d expected=%d status=%#x exited=%d exit_status=%d "
            "errno=%d\n",
            wait_stage, waited, expected, status, WIFEXITED(status),
            WIFEXITED(status) ? WEXITSTATUS(status) : -1, errno);
    exit(2);
  }
}

static void waitpid_exit(pid_t selector, pid_t expected, int options, int code) {
  int status = 0;
  pid_t waited = waitpid(selector, &status, options);
  require_exit(waited, expected, status, code);
}

struct thread_child {
  int pid_write;
  int release_read;
  int done_read;
};

static void *spawn_from_worker(void *opaque) {
  struct thread_child *state = opaque;
  pid_t child = fork();
  if (child < 0)
    return (void *)(intptr_t)1;
  if (child == 0) {
    char token;
    if (read(state->release_read, &token, 1) != 1)
      _exit(126);
    _exit(19);
  }
  if (write(state->pid_write, &child, sizeof(child)) != sizeof(child))
    return (void *)(intptr_t)2;
  char token;
  if (read(state->done_read, &token, 1) != 1)
    return (void *)(intptr_t)3;
  return NULL;
}

static pid_t raw_clone(unsigned long flags) {
  return (pid_t)syscall(SYS_clone, flags, NULL, NULL, NULL, 0);
}

int main(int argc, char **argv) {
  int same_group_only = argc == 2 && strcmp(argv[1], "--same-group-only") == 0;
  int groups_only = argc == 2 && strcmp(argv[1], "--groups-only") == 0;
  int nothread_only = argc == 2 && strcmp(argv[1], "--nothread-only") == 0;
  if (argc > 2 || (argc == 2 && !same_group_only && !groups_only &&
                              !nothread_only)) {
    fprintf(stderr,
            "usage: %s [--same-group-only|--groups-only|--nothread-only]\n",
            argv[0]);
    return 64;
  }
  int full = argc == 1;

  if (full) {
    struct blocked_child negative_group = spawn_blocked(7);
    if (setpgid(negative_group.pid, negative_group.pid) != 0)
      fail("setpgid child");
    release_child(negative_group);
    wait_stage = "wait4 negative pgid";
    waitpid_exit(-negative_group.pid, negative_group.pid, 0, 7);
  }

  if (full || same_group_only || groups_only) {
    struct blocked_child zero_group = spawn_blocked(9);
    release_child(zero_group);
    wait_stage = "wait4 pgid zero";
    waitpid_exit(0, zero_group.pid, 0, 9);
  }

  siginfo_t info = {0};
  if (full) {
    struct blocked_child waitid_group = spawn_blocked(11);
    if (setpgid(waitid_group.pid, waitid_group.pid) != 0)
      fail("setpgid waitid child");
    release_child(waitid_group);
    if (waitid(P_PGID, waitid_group.pid, &info, WEXITED) != 0)
      fail("waitid explicit P_PGID");
    if (info.si_pid != waitid_group.pid || info.si_code != CLD_EXITED ||
        info.si_status != 11)
      return 3;
  }

  if (full || same_group_only || groups_only) {
    struct blocked_child waitid_zero_group = spawn_blocked(13);
    release_child(waitid_zero_group);
    memset(&info, 0, sizeof(info));
    if (waitid(P_PGID, 0, &info, WEXITED) != 0)
      fail("waitid zero P_PGID");
    if (info.si_pid != waitid_zero_group.pid || info.si_code != CLD_EXITED ||
        info.si_status != 13)
      return 4;
  }

  const char token = 'x';
  if (full || same_group_only || nothread_only) {
    int pid_pipe[2];
    int release_pipe[2];
    int done_pipe[2];
    if (pipe(pid_pipe) != 0 || pipe(release_pipe) != 0 || pipe(done_pipe) != 0)
      fail("thread pipes");
    struct thread_child thread_state = {
        .pid_write = pid_pipe[1],
        .release_read = release_pipe[0],
        .done_read = done_pipe[0],
    };
    pthread_t worker;
    if (pthread_create(&worker, NULL, spawn_from_worker, &thread_state) != 0)
      fail("pthread_create");
    pid_t thread_child;
    if (read(pid_pipe[0], &thread_child, sizeof(thread_child)) !=
        sizeof(thread_child))
      fail("read worker child pid");
    errno = 0;
    int status = 0;
    if (waitpid(thread_child, &status, WNOHANG | __WNOTHREAD) != -1 ||
        errno != ECHILD)
      return 5;
    if (write(release_pipe[1], &token, 1) != 1)
      fail("release worker child");
    wait_stage = "wait4 cross-thread";
    waitpid_exit(thread_child, thread_child, 0, 19);
    if (write(done_pipe[1], &token, 1) != 1)
      fail("release worker");
    void *thread_result = NULL;
    if (pthread_join(worker, &thread_result) != 0 || thread_result != NULL)
      return 6;
  }

  if (full) {
    int clone_parent_pid[2];
    int clone_parent_done[2];
    if (pipe(clone_parent_pid) != 0 || pipe(clone_parent_done) != 0)
      fail("clone parent pipes");
    pid_t intermediate = fork();
    if (intermediate < 0)
      fail("fork clone parent");
    if (intermediate == 0) {
      pid_t sibling = raw_clone(CLONE_PARENT | SIGCHLD);
      if (sibling < 0)
        _exit(125);
      if (sibling == 0)
        _exit(23);
      if (write(clone_parent_pid[1], &sibling, sizeof(sibling)) !=
          sizeof(sibling))
        _exit(124);
      char done;
      if (read(clone_parent_done[0], &done, 1) != 1)
        _exit(123);
      _exit(33);
    }
    pid_t sibling;
    if (read(clone_parent_pid[0], &sibling, sizeof(sibling)) != sizeof(sibling))
      fail("read clone parent child");
    wait_stage = "wait4 clone parent sibling";
    waitpid_exit(sibling, sibling, __WNOTHREAD, 23);
    if (write(clone_parent_done[1], &token, 1) != 1)
      fail("release clone parent");
    wait_stage = "wait4 intermediate";
    waitpid_exit(intermediate, intermediate, 0, 33);
  }

  if (same_group_only)
    puts("wait4-pgid0=ok waitid-pgid0=ok nothread=ok");
  else if (groups_only)
    puts("wait4-pgid0=ok waitid-pgid0=ok");
  else if (nothread_only)
    puts("nothread=ok");
  else
    puts("wait4-groups=ok waitid-groups=ok nothread=ok clone-parent=ok");
  return 0;
}
