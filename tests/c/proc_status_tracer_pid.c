/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// The TracerPid that each procfs path to a task's status shows. A program run
// outside a debugger reads 0 on every one: Linux's task_state
// (fs/proc/array.c) shows a tracer's pid only for a traced task. Paths: the
// task's own (/proc/self, /proc/thread-self), by number (/proc/<pid>,
// /proc/<pid>/task/<tid>), a second thread's /proc/thread-self, and a forked
// child's /proc/self. The numbers vary, so only the labels are printed.

#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static void print_tracer_pid(const char* label, const char* path) {
  FILE* file = fopen(path, "r");
  char line[256];
  long tracer_pid = -1;
  while (file != NULL && fgets(line, sizeof(line), file) != NULL) {
    if (sscanf(line, "TracerPid:\t%ld", &tracer_pid) == 1) {
      break;
    }
  }
  if (file != NULL) {
    fclose(file);
  }
  printf("%s %ld\n", label, tracer_pid);
  fflush(stdout);
}

static void* second_thread(void* unused) {
  (void)unused;
  print_tracer_pid("second thread's thread-self", "/proc/thread-self/status");
  return NULL;
}

int main(void) {
  char path[64];
  print_tracer_pid("self", "/proc/self/status");
  print_tracer_pid("thread-self", "/proc/thread-self/status");
  snprintf(path, sizeof(path), "/proc/%d/status", getpid());
  print_tracer_pid("by pid", path);
  snprintf(path, sizeof(path), "/proc/%d/task/%d/status", getpid(), gettid());
  print_tracer_pid("by pid and tid", path);

  pthread_t thread;
  if (pthread_create(&thread, NULL, second_thread, NULL) != 0 ||
      pthread_join(thread, NULL) != 0) {
    return 1;
  }

  pid_t child = fork();
  if (child < 0) {
    return 1;
  }
  if (child == 0) {
    print_tracer_pid("forked child's self", "/proc/self/status");
    _exit(0);
  }
  int status = 0;
  if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    return 1;
  }
  return 0;
}
