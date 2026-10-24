/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// gettimeofday with its timeval in a page the process maps but cannot read
// (PROT_NONE). Linux fails the call with EFAULT. Before Hermit lets the guest
// see that result, Detcore must confirm that the failed call stored no host
// time in the page (detcore/src/syscalls/time.rs).
//
// - ptrace reads the word through the tracer, which ignores page protections,
//   so it confirms the store and the call returns -1 with errno EFAULT.
// - DBT reads the word in-process, cannot, and refuses with a tool error
//   (StoppedWordUnreadable), which ends the process with exit 101.
//
// With the argument `fork`, a forked child makes the call before any exec,
// and the parent waits for it, ignores its status, and exits 0.

#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static int call_into_unreadable_page(void) {
  void* page = mmap(NULL, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (page == MAP_FAILED) {
    perror("mmap");
    return 2;
  }
  errno = 0;
  long result = syscall(SYS_gettimeofday, page, NULL);
  int error = errno;
  printf(
      "gettimeofday returned %ld errno=%s\n",
      result,
      error == EFAULT ? "EFAULT" : strerror(error));
  fflush(stdout);
  return 0;
}

int main(int argc, char** argv) {
  if (argc == 1) {
    return call_into_unreadable_page();
  }
  if (argc == 2 && strcmp(argv[1], "fork") == 0) {
    fflush(stdout);
    pid_t child = fork();
    if (child < 0) {
      perror("fork");
      return 3;
    }
    if (child == 0) {
      _exit(call_into_unreadable_page());
    }
    int status = 0;
    if (waitpid(child, &status, 0) != child) {
      perror("waitpid");
      return 4;
    }
    printf("parent ignored the child's status\n");
    return 0;
  }
  return 64;
}
