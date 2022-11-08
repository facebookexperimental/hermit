/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define __ARCH_WANT_SYS_CLONE

#include <errno.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

int child_set_text(void* arg);

// Tests clone syscall. Parents waits for child to set shared memory and prints
// the text set by the child.
int main(void) {
  const int STACK_SIZE = 0x1000;
  void* child_stack = malloc(STACK_SIZE);
  if (child_stack == NULL) {
    fprintf(stderr, "Malloc failed, reason:\n%s\n", strerror(errno));
    exit(1);
  }

  char* text = "Hello from parent!";
  pid_t pid = clone(
      child_set_text, child_stack + STACK_SIZE, CLONE_VM | SIGCHLD, &text);
  if (pid == -1) {
    fprintf(stderr, "Clone failed, reason:\n%s\n", strerror(errno));
    exit(1);
  }

  int status;
  if (wait(&status) == -1) {
    fprintf(stderr, "Wait failed, reason %s\n", strerror(errno));
    exit(1);
  }

  printf("Child exited with status %d, set text to \"%s\"\n", status, text);
  return 0;
}

int child_set_text(void* arg) {
  *(char**)arg = "Hello from child!";
  return 0;
}
