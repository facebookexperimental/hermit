/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* A program break's life as Linux's brk gives it, for an ET_EXEC image: start,
 * growth, a request within the page, a shrink, a request below the start,
 * unaligned growth, kept bytes, growth refused within a page of another
 * mapping or in a grow-down mapping's guard gap, a shrink with nothing left
 * to unmap, a fork child's own break, then malloc and sbrk. Raw brk comes
 * first, before stdio or malloc touch the heap. Compared with Linux
 * (setarch -R) on every backend that answers brk. */

#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static long brk_(long to) { return syscall(SYS_brk, to); }

int main(int argc, char **argv) {
  int root_only = argc == 2 &&
      (strcmp(argv[1], "--chroot-root-brk") == 0 ||
       strcmp(argv[1], "--vfork-then-chroot-root-brk") == 0);
  if (argc == 2 &&
      (strcmp(argv[1], "--chroot-into-proc") == 0 ||
       strcmp(argv[1], "--vfork-then-chroot-into-proc") == 0 || root_only)) {
    if (strcmp(argv[1], "--vfork-then-chroot-into-proc") == 0 ||
        strcmp(argv[1], "--vfork-then-chroot-root-brk") == 0) {
      pid_t child = vfork();
      if (child == 0)
        _exit(0);
      int status = 0;
      if (child < 0 || waitpid(child, &status, 0) != child ||
          !WIFEXITED(status) || WEXITSTATUS(status) != 0)
        return 2;
    }
    if (chroot("/proc") != 0)
      return 2;
  } else if (argc != 1 && !(argc == 2 && strcmp(argv[1], "-") == 0)) {
    return 2;
  }
  /* Raw brk first, before stdio or malloc touch the heap; printed after. */
  long r[13];
  r[0] = brk_(0);
  long start = r[0];
  r[1] = brk_(start + 3 * 4096);
  memset((void *)start, 0x5a, 3 * 4096);
  r[2] = brk_(start + 3 * 4096 + 100);
  r[3] = brk_(start + 4096);
  r[4] = brk_(start - 4096);
  r[5] = brk_(0);
  r[6] = brk_(start + 5000);
  r[7] = ((volatile char *)start)[4095] == 0x5a;
  r[8] = brk_(start);
  /* Growth that would come within a page of a mapping is refused. */
  void *blocker = mmap((void *)(start + 2 * 4096), 4096, PROT_READ,
                       MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
  r[9] = blocker == (void *)(start + 2 * 4096);
  r[10] = brk_(start + 3 * 4096);
  r[11] = brk_(start + 4096);
  r[12] = brk_(start + 2 * 4096);
  munmap(blocker, 4096);
  brk_(start);
  /* A shrink with nothing left to unmap keeps the old break. */
  long empty[3];
  empty[0] = brk_(start + 3 * 4096);
  munmap((void *)start, 3 * 4096);
  empty[1] = brk_(start + 4096);
  empty[2] = brk_(0);
  /* Growth into a grow-down mapping's guard gap is refused. */
  long gap[2];
  void *down = mmap((void *)(start + 64 * 4096), 4096, PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS | MAP_GROWSDOWN | MAP_FIXED_NOREPLACE, -1, 0);
  gap[0] = down == (void *)(start + 64 * 4096);
  gap[1] = brk_(start + 3 * 4096 + 2 * 4096);
  munmap(down, 4096);
  brk_(start);
  const char *labels[] = {"start", "grow 3 pages", "within the page", "shrink to 1 page",
                          "below the start", "query", "unaligned grow", "kept bytes", "back",
                          "blocker placed", "past the blocker", "a page short of it",
                          "up to it"};
  /* A fork child moves its own break, not its parent's. */
  long before_fork = 0, child_grew = 0, parent_after = 0;
  if (!root_only) {
    before_fork = brk_(0);
    int pipefd[2];
    if (pipe(pipefd) != 0)
      return 2;
    pid_t child = fork();
    if (child == 0) {
      long grew = brk_(before_fork + 3 * 4096);
      if (write(pipefd[1], &grew, sizeof grew) != sizeof grew)
        _exit(3);
      _exit(0);
    }
    if (read(pipefd[0], &child_grew, sizeof child_grew) != sizeof child_grew)
      return 4;
    waitpid(child, NULL, 0);
    parent_after = brk_(0);
  }
  for (int i = 0; i < 13; i++)
    printf("%s: %#lx\n", labels[i], r[i]);
  printf("emptied heap grown to %#lx, shrink answers %#lx, query %#lx\n", empty[0], empty[1],
         empty[2]);
  printf("grow-down mapping placed: %ld, growth into its gap: %#lx\n", gap[0], gap[1]);
  if (!root_only)
    printf("child grew by 3 pages: %s, the parent kept its own: %s\n",
           child_grew == before_fork + 3 * 4096 ? "yes" : "no",
           parent_after == before_fork ? "yes" : "no");
  char *small = malloc(24), *medium = malloc(5000);
  printf("malloc small %p medium %p\n", (void *)small, (void *)medium);
  free(medium);
  printf("sbrk(0) %p\n", sbrk(0));
  return 0;
}
