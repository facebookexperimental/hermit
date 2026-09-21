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
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

static void print_identity(const char *label) {
  dprintf(STDOUT_FILENO, "%s pid=%ld ppid=%ld tid=%ld\n", label, (long)getpid(),
          (long)getppid(), syscall(SYS_gettid));
}

static int print_proc_identity(void) {
  long stat_pid = -1;
  long stat_ppid = -1;
  long status_pid = -1;
  long status_ppid = -1;
  long tracer_pid = -1;
  char comm[256];
  char state;
  char line[256];
  FILE *file = fopen("/proc/self/stat", "r");
  if (file == NULL || fscanf(file, "%ld (%255[^)]) %c %ld", &stat_pid, comm,
                             &state, &stat_ppid) != 4) {
    if (file != NULL)
      fclose(file);
    return 1;
  }
  fclose(file);

  file = fopen("/proc/self/status", "r");
  if (file == NULL)
    return 2;
  while (fgets(line, sizeof(line), file) != NULL) {
    (void)sscanf(line, "Pid:\t%ld", &status_pid);
    (void)sscanf(line, "PPid:\t%ld", &status_ppid);
    (void)sscanf(line, "TracerPid:\t%ld", &tracer_pid);
  }
  fclose(file);
  dprintf(STDOUT_FILENO, "exec-proc stat=%ld/%ld status=%ld/%ld tracer=%ld\n",
          stat_pid, stat_ppid, status_pid, status_ppid, tracer_pid);
  return stat_pid == 6 && stat_ppid == 3 && status_pid == 6 &&
                 status_ppid == 3 && tracer_pid == 1
             ? 0
             : 3;
}

// TODO-HUMAN-REVIEW(PR-723): Review guest-visible PID lifecycle expectations.
int main(int argc, char **argv) {
  if (argc == 2 && strcmp(argv[1], "--exec-child") == 0) {
    print_identity("exec-child");
    return print_proc_identity() == 0 ? 8 : 12;
  }
  if (argc == 2 && strcmp(argv[1], "--vfork-exec-child") == 0) {
    print_identity("vfork-exec-child");
    return 10;
  }

  print_identity("root");
  int unknown_status = 0;
  errno = 0;
  if (kill(12345, 0) != -1 || errno != ESRCH)
    return 13;
  errno = 0;
  if (waitpid(12345, &unknown_status, WNOHANG) != -1 || errno != ECHILD)
    return 14;
  pid_t child = fork();
  if (child < 0)
    return 1;
  if (child == 0) {
    pid_t grandchild = fork();
    if (grandchild < 0)
      _exit(2);
    if (grandchild == 0) {
      print_identity("grandchild");
      _exit(5);
    }
    int status = 0;
    pid_t waited = waitpid(grandchild, &status, 0);
    print_identity("child");
    dprintf(STDOUT_FILENO, "child grandchild=%ld waited=%ld exit=%d\n",
            (long)grandchild, (long)waited,
            WIFEXITED(status) ? WEXITSTATUS(status) : -1);
    _exit(6);
  }

  if (kill(child, 0) != 0)
    return 3;
  int child_status = 0;
  pid_t child_waited = waitpid(child, &child_status, 0);
  dprintf(STDOUT_FILENO, "root child=%ld waited=%ld exit=%d\n", (long)child,
          (long)child_waited,
          WIFEXITED(child_status) ? WEXITSTATUS(child_status) : -1);

  pid_t exec_child = fork();
  if (exec_child < 0)
    return 4;
  if (exec_child == 0) {
    execl(argv[0], argv[0], "--exec-child", NULL);
    _exit(127);
  }
  int exec_status = 0;
  pid_t exec_waited = waitpid(exec_child, &exec_status, 0);
  dprintf(STDOUT_FILENO, "root exec=%ld waited=%ld exit=%d\n", (long)exec_child,
          (long)exec_waited,
          WIFEXITED(exec_status) ? WEXITSTATUS(exec_status) : -1);

  pid_t waitid_child = fork();
  if (waitid_child < 0)
    return 5;
  if (waitid_child == 0) {
    print_identity("waitid-child");
    _exit(9);
  }
  siginfo_t info = {0};
  if (waitid(P_PID, waitid_child, &info, WEXITED) != 0)
    return 6;
  dprintf(STDOUT_FILENO, "root waitid=%ld reported=%ld exit=%d\n",
          (long)waitid_child, (long)info.si_pid, info.si_status);

  int null_fd = open("/dev/null", O_WRONLY);
  if (null_fd < 0)
    return 7;
  pid_t vfork_child = vfork();
  if (vfork_child < 0)
    return 8;
  if (vfork_child == 0) {
    long pid = syscall(SYS_getpid);
    long ppid = syscall(SYS_getppid);
    long tid = syscall(SYS_gettid);
    long written = syscall(SYS_write, null_fd, "x", 1);
    _exit((pid == 8 ? 0 : 1) | (ppid == 3 ? 0 : 2) | (tid == 8 ? 0 : 4) |
          (written == 1 ? 0 : 8));
  }
  int vfork_status = 0;
  pid_t vfork_waited = waitpid(vfork_child, &vfork_status, 0);
  dprintf(STDOUT_FILENO, "root vfork=%ld waited=%ld exit=%d pid=%ld tid=%ld\n",
          (long)vfork_child, (long)vfork_waited,
          WIFEXITED(vfork_status) ? WEXITSTATUS(vfork_status) : -1,
          (long)getpid(), syscall(SYS_gettid));
  close(null_fd);

  pid_t vfork_exec_child = vfork();
  if (vfork_exec_child < 0)
    return 8;
  if (vfork_exec_child == 0) {
    execl(argv[0], argv[0], "--vfork-exec-child", NULL);
    _exit(127);
  }
  int vfork_exec_status = 0;
  pid_t vfork_exec_waited = waitpid(vfork_exec_child, &vfork_exec_status, 0);
  dprintf(STDOUT_FILENO,
          "root vfork-exec=%ld waited=%ld exit=%d pid=%ld tid=%ld\n",
          (long)vfork_exec_child, (long)vfork_exec_waited,
          WIFEXITED(vfork_exec_status) ? WEXITSTATUS(vfork_exec_status) : -1,
          (long)getpid(), syscall(SYS_gettid));
  return 0;
}
