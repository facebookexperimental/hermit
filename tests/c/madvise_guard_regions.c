/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Guard regions (madvise MADV_GUARD_INSTALL and MADV_GUARD_REMOVE, Linux
 * 6.13), observed only through syscalls: a kernel copy to or from a guard page
 * faults (EFAULT). The program never touches a guard page itself, so the
 * output is the same on every backend that gives Linux's answers, KVM
 * included, whose page tables do not enforce a guard against a direct access
 * (https://github.com/rrnewton/hermit/issues/5177). Each line evaluates one
 * check, in order.
 *
 * Covered: install and remove, EFAULT for a copy from and into a guard page,
 * a repeated install, an unaligned start (EINVAL), the guard surviving
 * MADV_DONTNEED, mprotect and fork (a child's removal stays its own), a
 * removed private page reading as zeros, a range with a hole (ENOMEM after the
 * mapped part is done), munmap and MAP_FIXED ending a guard, mremap moving it,
 * the mapping left whole in /proc/self/maps (an anonymous mapping and the
 * stack), and a thread whose stack glibc guards (glibc 2.42 installs its
 * stack guard with MADV_GUARD_INSTALL).
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <errno.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef MADV_GUARD_INSTALL
#define MADV_GUARD_INSTALL 102
#endif
#ifndef MADV_GUARD_REMOVE
#define MADV_GUARD_REMOVE 103
#endif

static int pipe_fds[2];
static long page;

/* "ok" when a kernel copy can read the byte at `address`, else the errno. */
static const char *probe(char *address) {
  static char error[32];
  if (write(pipe_fds[1], address, 1) == 1) {
    char byte;
    if (read(pipe_fds[0], &byte, 1) != 1) {
      abort();
    }
    return "ok";
  }
  snprintf(error, sizeof error, "%s", strerror(errno));
  return error;
}

static const char *result(int rc) {
  static char text[32];
  if (rc == 0) {
    return "0";
  }
  snprintf(text, sizeof text, "%s", strerror(errno));
  return text;
}

static int guard(char *address, long pages, int advice) {
  return madvise(address, pages * page, advice);
}

/* The number of /proc/self/maps lines that overlap [start, end). */
static int maps_lines(char *start, char *end) {
  FILE *maps = fopen("/proc/self/maps", "r");
  if (maps == NULL) {
    return -1;
  }
  int count = 0;
  char line[512];
  while (fgets(line, sizeof line, maps) != NULL) {
    uintptr_t low, high;
    if (sscanf(line, "%lx-%lx", &low, &high) == 2 &&
        low < (uintptr_t)end && high > (uintptr_t)start) {
      count++;
    }
  }
  fclose(maps);
  return count;
}

/* The bounds of the [stack] mapping in /proc/self/maps. */
static int stack_mapping(char **start, char **end) {
  FILE *maps = fopen("/proc/self/maps", "r");
  if (maps == NULL) {
    return -1;
  }
  char line[512];
  int found = -1;
  while (fgets(line, sizeof line, maps) != NULL) {
    uintptr_t low, high;
    if (strstr(line, "[stack]") != NULL &&
        sscanf(line, "%lx-%lx", &low, &high) == 2) {
      *start = (char *)low;
      *end = (char *)high;
      found = 0;
    }
  }
  fclose(maps);
  return found;
}

static void *thread_main(void *argument) {
  *(int *)argument = 1;
  return NULL;
}

int main(void) {
  page = sysconf(_SC_PAGESIZE);
  if (pipe(pipe_fds) != 0) {
    return 2;
  }
  char *p = mmap(NULL, 4 * page, PROT_READ | PROT_WRITE,
                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (p == MAP_FAILED) {
    return 3;
  }
  memset(p, 'a', 4 * page);
  int rc = guard(p + page, 1, MADV_GUARD_INSTALL);
  if (rc != 0 && errno == EINVAL) {
    printf("guard: unsupported\n");
    return 0;
  }
  printf("install: %s\n", result(rc));
  printf("guard page: %s\n", probe(p + page));
  printf("neighbours: %s\n", probe(p));
  printf("            %s\n", probe(p + 2 * page));
  if (write(pipe_fds[1], "x", 1) != 1) {
    return 4;
  }
  printf("read into guard: %s\n",
         read(pipe_fds[0], p + page, 1) < 0 ? strerror(errno) : "copied");
  char drain;
  if (read(pipe_fds[0], &drain, 1) != 1) {
    return 4;
  }
  printf("neighbour kept: %c\n", p[0]);
  printf("again: %s\n", result(guard(p + page, 1, MADV_GUARD_INSTALL)));
  printf("guard page: %s\n", probe(p + page));
  printf("unaligned: %s\n", result(madvise(p + 1, page, MADV_GUARD_INSTALL)));

  printf("dontneed: %s\n", result(madvise(p, 4 * page, MADV_DONTNEED)));
  printf("guard page: %s\n", probe(p + page));
  printf("mprotect: %s\n", result(mprotect(p, 4 * page, PROT_READ)));
  printf("guard page: %s\n", probe(p + page));
  mprotect(p, 4 * page, PROT_READ | PROT_WRITE);

  fflush(stdout);
  pid_t child = fork();
  if (child == 0) {
    printf("child guard page: %s\n", probe(p + page));
    printf("child remove: %s\n", result(guard(p + page, 1, MADV_GUARD_REMOVE)));
    printf("child page: %s\n", probe(p + page));
    fflush(stdout);
    _exit(0);
  }
  int status;
  waitpid(child, &status, 0);
  printf("child exit: %d\n", WEXITSTATUS(status));
  printf("parent guard page: %s\n", probe(p + page));

  printf("remove: %s\n", result(guard(p + page, 1, MADV_GUARD_REMOVE)));
  printf("page: %s\n", probe(p + page));
  printf("contents: %d\n", p[page]);

  /* A hole: the mapped part takes the guard, and the call reports ENOMEM. */
  munmap(p + 3 * page, page);
  printf("install over hole: %s\n",
         result(guard(p + 2 * page, 2, MADV_GUARD_INSTALL)));
  printf("mapped part: %s\n", probe(p + 2 * page));
  printf("remove over hole: %s\n",
         result(guard(p + 2 * page, 2, MADV_GUARD_REMOVE)));
  printf("mapped part: %s\n", probe(p + 2 * page));

  /* munmap and a new mapping end a guard. */
  guard(p + 2 * page, 1, MADV_GUARD_INSTALL);
  munmap(p + 2 * page, page);
  char *again = mmap(p + 2 * page, page, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
  printf("remapped: %s\n", again == p + 2 * page ? "same" : "moved");
  printf("page: %s\n", probe(again));
  guard(p, 1, MADV_GUARD_INSTALL);
  char *fixed = mmap(p, page, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
  printf("mapped over guard: %s\n", probe(fixed));

  /* mremap moves a guard with its page; the pages a grow adds have none. */
  char *two = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  guard(two + page, 1, MADV_GUARD_INSTALL);
  char *moved = mremap(two, 2 * page, 3 * page, MREMAP_MAYMOVE);
  printf("moved first: %s\n", probe(moved));
  printf("moved guard: %s\n", probe(moved + page));
  printf("grown: %s\n", probe(moved + 2 * page));

  /* A guard splits no mapping: /proc/self/maps lists a mapping as before.
   * (KVM's /proc/self/maps lists only the stack mapping, which the second
   * check covers.) */
  char *whole = mmap(NULL, 3 * page, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  int before = maps_lines(whole, whole + 3 * page);
  guard(whole + page, 1, MADV_GUARD_INSTALL);
  printf("maps lines over guarded mapping unchanged: %s\n",
         maps_lines(whole, whole + 3 * page) == before ? "yes" : "no");
  char *stack_start = NULL, *stack_end = NULL;
  if (stack_mapping(&stack_start, &stack_end) != 0) {
    return 6;
  }
  printf("stack guard: %s\n",
         result(guard(stack_start, 1, MADV_GUARD_INSTALL)));
  printf("stack guard page: %s\n", probe(stack_start));
  printf("maps lines over guarded stack: %d\n",
         maps_lines(stack_start, stack_end));
  printf("stack guard removed: %s\n",
         result(guard(stack_start, 1, MADV_GUARD_REMOVE)));

  pthread_t thread;
  int value = 0;
  if (pthread_create(&thread, NULL, thread_main, &value) != 0 ||
      pthread_join(thread, NULL) != 0) {
    return 5;
  }
  printf("thread ran: %d\n", value);
  return 0;
}
