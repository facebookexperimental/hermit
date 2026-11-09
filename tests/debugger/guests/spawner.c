/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Debugger guest: a multithreaded process that spawns /bin/true twice with
 * posix_spawn(), which glibc runs as a vfork (CLONE_VM | CLONE_VFORK), then
 * calls after_spawns(). A second thread sleeps throughout, so the vforks come
 * from a process with more than one thread.
 */

#include <pthread.h>
#include <spawn.h>
#include <stdio.h>
#include <sys/wait.h>
#include <unistd.h>

extern char** environ;

static void* idle(void* arg) {
  (void)arg;
  for (;;) {
    sleep(1);
  }
  return NULL;
}

/* 0 when /bin/true was spawned and exited 0. */
static int spawn_true(void) {
  pid_t pid;
  char* argv[] = {"/bin/true", NULL};
  if (posix_spawn(&pid, "/bin/true", NULL, NULL, argv, environ) != 0) {
    return 1;
  }
  int status = 0;
  if (waitpid(pid, &status, 0) != pid) {
    return 2;
  }
  return WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : 3;
}

__attribute__((noinline)) int after_spawns(int first, int second) {
  return first + second;
}

int main(void) {
  pthread_t thread;
  pthread_create(&thread, NULL, idle, NULL);
  int first = spawn_true();
  int second = spawn_true();
  int failures = after_spawns(first, second);
  printf("spawns %d %d\n", first, second);
  return failures == 0 ? 0 : 1;
}
