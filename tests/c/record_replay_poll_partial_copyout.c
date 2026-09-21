/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static int close_pipe(int pipefd[2]) {
  int result = 0;
  if (close(pipefd[0]) != 0) {
    result = -1;
  }
  if (close(pipefd[1]) != 0) {
    result = -1;
  }
  return result;
}

static int partial_copyout(const char* mode, int use_ppoll) {
  int first_pipe[2];
  int second_pipe[2];
  if (pipe(first_pipe) != 0 || pipe(second_pipe) != 0 ||
      write(first_pipe[1], "a", 1) != 1 ||
      write(second_pipe[1], "b", 1) != 1) {
    perror("prepare ready pipes");
    return 1;
  }

  long page_size = sysconf(_SC_PAGESIZE);
  if (page_size <= 0 || (size_t)page_size < sizeof(struct pollfd)) {
    fprintf(stderr, "invalid page size: %ld\n", page_size);
    return 1;
  }

  void* pages = mmap(NULL, (size_t)page_size * 2, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (pages == MAP_FAILED) {
    perror("mmap pollfd pages");
    return 1;
  }

  struct pollfd* fds =
      (struct pollfd*)((unsigned char*)pages + page_size - sizeof(*fds));
  fds[0] = (struct pollfd){
      .fd = first_pipe[0], .events = POLLIN, .revents = 0x1234};
  fds[1] = (struct pollfd){
      .fd = second_pipe[0], .events = POLLIN, .revents = 0x5678};

  /*
   * Both entries remain readable inputs, but only fds[0].revents is writable.
   * Linux writes that first result, faults on the second, and returns EFAULT.
   */
  if (mprotect((unsigned char*)pages + page_size, (size_t)page_size,
               PROT_READ) != 0) {
    perror("mprotect second pollfd page");
    return 1;
  }

  struct timespec timeout = {.tv_sec = 3, .tv_nsec = 456789123};
  errno = 0;
  long result;
  if (use_ppoll) {
    result = syscall(SYS_ppoll, fds, 2, &timeout, NULL, sizeof(uint64_t));
  } else {
    result = syscall(SYS_poll, fds, 2, 3456);
  }
  int observed_errno = errno;
  int first_revents = fds[0].revents;
  int second_revents = fds[1].revents;
  struct timespec observed_timeout = timeout;

  if (mprotect((unsigned char*)pages + page_size, (size_t)page_size,
               PROT_READ | PROT_WRITE) != 0) {
    perror("restore second pollfd page");
    return 1;
  }
  if (munmap(pages, (size_t)page_size * 2) != 0) {
    perror("munmap pollfd pages");
    return 1;
  }
  if (close_pipe(first_pipe) != 0 || close_pipe(second_pipe) != 0) {
    perror("close ready pipes");
    return 1;
  }

  /* Fixed widths keep the write syscall shape identical if a value diverges. */
  printf("mode=%s result=%011ld errno=%011d revents0=%011d "
         "revents1=%011d timeout=%011ld.%09ld\n",
         mode, result, observed_errno, first_revents, second_revents,
         (long)observed_timeout.tv_sec, observed_timeout.tv_nsec);

  if (result != -1 || observed_errno != EFAULT ||
      first_revents != POLLIN || second_revents != 0x5678) {
    fprintf(stderr,
            "%s partial copyout mismatch: result=%ld errno=%d "
            "revents0=%d revents1=%d\n",
            mode, result, observed_errno, first_revents, second_revents);
    return 1;
  }
  if (use_ppoll &&
      (observed_timeout.tv_sec < 0 || observed_timeout.tv_sec > 3 ||
       observed_timeout.tv_nsec < 0 || observed_timeout.tv_nsec >= 1000000000 ||
       (observed_timeout.tv_sec == 3 &&
        observed_timeout.tv_nsec > 456789123))) {
    fprintf(stderr, "invalid ppoll remaining timeout: %ld.%09ld\n",
            observed_timeout.tv_sec, observed_timeout.tv_nsec);
    return 1;
  }
  return 0;
}

static int invalid_nfds(void) {
  struct rlimit limit;
  if (getrlimit(RLIMIT_NOFILE, &limit) != 0) {
    perror("getrlimit RLIMIT_NOFILE");
    return 1;
  }
  if (limit.rlim_cur == RLIM_INFINITY || limit.rlim_cur == (rlim_t)-1) {
    fprintf(stderr, "RLIMIT_NOFILE must be finite for this regression test\n");
    return 1;
  }

  nfds_t nfds = (nfds_t)limit.rlim_cur + 1;
  errno = 0;
  long poll_result = syscall(SYS_poll, (void*)(uintptr_t)1, nfds, 0);
  int poll_errno = errno;

  struct timespec timeout = {.tv_sec = 3, .tv_nsec = 456789123};
  errno = 0;
  long ppoll_result =
      syscall(SYS_ppoll, (void*)(uintptr_t)1, nfds, &timeout, NULL,
              sizeof(uint64_t));
  int ppoll_errno = errno;

  printf("mode=invalid-nfds nfds=%020llu poll=%011ld errno=%011d "
         "ppoll=%011ld errno=%011d timeout=%011ld.%09ld\n",
         (unsigned long long)nfds, poll_result, poll_errno, ppoll_result,
         ppoll_errno, (long)timeout.tv_sec, timeout.tv_nsec);

  if (poll_result != -1 || poll_errno != EINVAL || ppoll_result != -1 ||
      ppoll_errno != EINVAL || timeout.tv_sec < 0 || timeout.tv_sec > 3 ||
      timeout.tv_nsec < 0 || timeout.tv_nsec >= 1000000000 ||
      (timeout.tv_sec == 3 && timeout.tv_nsec > 456789123)) {
    fprintf(stderr,
            "invalid-nfds mismatch: poll=%ld/%d ppoll=%ld/%d nfds=%llu "
            "timeout=%ld.%09ld\n",
            poll_result, poll_errno, ppoll_result, ppoll_errno,
            (unsigned long long)nfds, timeout.tv_sec, timeout.tv_nsec);
    return 1;
  }
  return 0;
}

int main(int argc, char** argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s [poll|ppoll|invalid-nfds]\n", argv[0]);
    return 2;
  }
  if (strcmp(argv[1], "poll") == 0) {
    return partial_copyout("poll", 0);
  }
  if (strcmp(argv[1], "ppoll") == 0) {
    return partial_copyout("ppoll", 1);
  }
  if (strcmp(argv[1], "invalid-nfds") == 0) {
    return invalid_nfds();
  }
  fprintf(stderr, "unknown mode: %s\n", argv[1]);
  return 2;
}
