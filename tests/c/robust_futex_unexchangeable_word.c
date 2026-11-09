/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A robust mutex whose lock word is on a MAP_SHARED mapping of a memfd, held
 * by a child process that then dies or execs. Linux marks the word
 * FUTEX_OWNER_DIED and wakes the next locker, from exit_robust_list in the
 * child's exit, or from futex_exec_release once the child's exec can no
 * longer fail, so the parent's lock returns EOWNERDEAD:
 *
 *   exit  the child, single-threaded, locks the mutex and exits;
 *   exec  the child locks the mutex and execs this program again (argument
 *         after-exec), whose new image exits at once.
 *
 * Prints "<mode>: lock=EOWNERDEAD child=exit 0". Every process here keeps a
 * single thread, which is all a process holding a shared file mapping can
 * have under Hermit's KVM backend.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

static void die(const char* what) {
  fprintf(stderr, "robust-unexchangeable: %s: %s\n", what, strerror(errno));
  _exit(2);
}

int main(int argc, char** argv) {
  if (argc == 2 && strcmp(argv[1], "after-exec") == 0) {
    return 0;
  }
  if (argc != 2 ||
      (strcmp(argv[1], "exit") != 0 && strcmp(argv[1], "exec") != 0)) {
    fprintf(stderr, "usage: %s exit|exec\n", argv[0]);
    return 64;
  }
  long page_size = sysconf(_SC_PAGESIZE);
  int fd = (int)syscall(SYS_memfd_create, "robust-unexchangeable", 0);
  if (fd < 0 || ftruncate(fd, page_size) != 0) {
    die("memfd");
  }
  pthread_mutex_t* mutex = mmap(
      NULL, (size_t)page_size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  if (mutex == MAP_FAILED) {
    die("mmap");
  }
  pthread_mutexattr_t attr;
  if (pthread_mutexattr_init(&attr) != 0 ||
      pthread_mutexattr_setpshared(&attr, PTHREAD_PROCESS_SHARED) != 0 ||
      pthread_mutexattr_setrobust(&attr, PTHREAD_MUTEX_ROBUST) != 0 ||
      pthread_mutex_init(mutex, &attr) != 0 ||
      pthread_mutexattr_destroy(&attr) != 0) {
    die("mutex init");
  }
  pid_t child = fork();
  if (child < 0) {
    die("fork");
  }
  if (child == 0) {
    if (pthread_mutex_lock(mutex) != 0) {
      die("child lock");
    }
    if (strcmp(argv[1], "exec") == 0) {
      execl(argv[0], argv[0], "after-exec", (char*)NULL);
      die("exec");
    }
    _exit(0);
  }
  int status;
  while (waitpid(child, &status, 0) != child) {
    if (errno != EINTR) {
      die("waitpid");
    }
  }
  int lock = pthread_mutex_lock(mutex);
  printf(
      "%s: lock=%s child=%s\n",
      argv[1],
      lock == EOWNERDEAD ? "EOWNERDEAD" : lock == 0 ? "0" : strerror(lock),
      WIFEXITED(status) && WEXITSTATUS(status) == 0 ? "exit 0" : "abnormal");
  return 0;
}
