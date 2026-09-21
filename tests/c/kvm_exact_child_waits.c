/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile sig_atomic_t sigchld_count;

struct blocked_child {
  pid_t pid;
  int release_fd;
  int exit_marker_fd;
};

static void fail(const char *message) {
  fprintf(stderr, "%s: %s\n", message, strerror(errno));
  exit(1);
}

static void on_sigchld(int signal_number) {
  (void)signal_number;
  ++sigchld_count;
}

static void install_sigchld(void (*handler)(int)) {
  struct sigaction action = {0};
  action.sa_handler = handler;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGCHLD, &action, NULL) != 0)
    fail("sigaction");
}

static struct blocked_child spawn_blocked_child(int exit_code) {
  int release_pipe[2];
  int marker_pipe[2];
  if (pipe(release_pipe) != 0 || pipe(marker_pipe) != 0)
    fail("pipe");

  pid_t child = fork();
  if (child < 0)
    fail("fork");
  if (child == 0) {
    char token;
    close(release_pipe[1]);
    close(marker_pipe[0]);
    if (read(release_pipe[0], &token, sizeof(token)) != 1)
      _exit(126);
    if (write(marker_pipe[1], &token, sizeof(token)) != 1)
      _exit(127);
    if (read(release_pipe[0], &token, sizeof(token)) != 1)
      _exit(125);
    _exit(exit_code);
  }

  close(release_pipe[0]);
  close(marker_pipe[1]);
  return (struct blocked_child){
      .pid = child,
      .release_fd = release_pipe[1],
      .exit_marker_fd = marker_pipe[0],
  };
}

static void release_child(struct blocked_child *child) {
  const char token = 'x';
  char marker;
  if (write(child->release_fd, &token, sizeof(token)) != 1)
    fail("release child");
  if (read(child->exit_marker_fd, &marker, sizeof(marker)) != 1)
    fail("read exit marker");
  if (write(child->release_fd, &token, sizeof(token)) != 1)
    fail("acknowledge child exit");
  close(child->release_fd);
  close(child->exit_marker_fd);
}


static void require_empty_nonblocking_waits(pid_t child) {
  int status = 0;
  if (wait4(child, &status, WNOHANG, NULL) != 0)
    fail("wait4 live-child WNOHANG");

  siginfo_t info;
  memset(&info, 0xa5, sizeof(info));
  if (waitid(P_PID, child, &info, WEXITED | WNOHANG) != 0)
    fail("waitid live-child WNOHANG");
  if (info.si_pid != 0)
    exit(2);
}

int main(void) {
  install_sigchld(on_sigchld);

  struct blocked_child wait4_child = spawn_blocked_child(7);
  require_empty_nonblocking_waits(wait4_child.pid);
  release_child(&wait4_child);

  int status = 0;
  // Do not retry EINTR: a ready exact-child status must win over SIGCHLD.
  pid_t waited = wait4(wait4_child.pid, &status, 0, NULL);
  if (waited != wait4_child.pid || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 7)
    return 3;

  struct blocked_child waitid_child = spawn_blocked_child(9);
  require_empty_nonblocking_waits(waitid_child.pid);
  release_child(&waitid_child);

  siginfo_t info;
  memset(&info, 0, sizeof(info));
  // Do not retry EINTR: a ready exact-child status must win over SIGCHLD.
  if (waitid(P_PID, waitid_child.pid, &info, WEXITED) != 0)
    return 4;
  if (info.si_code != CLD_EXITED || info.si_pid != waitid_child.pid ||
      info.si_status != 9)
    return 5;

  struct blocked_child any_child = spawn_blocked_child(11);
  int any_status = 0;
  if (wait4(-1, &any_status, WNOHANG, NULL) != 0)
    fail("wait4 any live-child WNOHANG");
  release_child(&any_child);

  // Shells use wait4(-1). Keep its ready-child-versus-SIGCHLD path single-shot.
  pid_t any_waited = wait4(-1, &any_status, 0, NULL);
  if (any_waited != any_child.pid || !WIFEXITED(any_status) ||
      WEXITSTATUS(any_status) != 11)
    return 6;

  struct blocked_child waitid_any_child = spawn_blocked_child(13);
  memset(&info, 0xa5, sizeof(info));
  if (waitid(P_ALL, 0, &info, WEXITED | WNOHANG) != 0)
    fail("waitid any live-child WNOHANG");
  if (info.si_pid != 0)
    return 7;
  release_child(&waitid_any_child);

  memset(&info, 0, sizeof(info));
  // waitid(P_ALL) exercises the same any-child route used by shell wait loops.
  if (waitid(P_ALL, 0, &info, WEXITED) != 0)
    return 8;
  if (info.si_code != CLD_EXITED || info.si_pid != waitid_any_child.pid ||
      info.si_status != 13)
    return 9;

  puts("wait4=7 waitid=9 wait4-any=11 waitid-any=13 live-wnohang=empty "
       "child-ready-won");
  return 0;
}
