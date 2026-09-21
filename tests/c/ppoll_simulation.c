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
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static void* delayed_writer(void* argument) {
  int fd = *(int*)argument;
  struct timespec delay = {.tv_sec = 0, .tv_nsec = 1000000};
  if (nanosleep(&delay, NULL) != 0 || write(fd, "w", 1) != 1) {
    return (void*)(uintptr_t)1;
  }
  return NULL;
}

static int run_raw_timeout_copyout(void) {
  int pipefd[2];
  if (pipe(pipefd) != 0 || write(pipefd[1], "r", 1) != 1) {
    perror("record-replay ready pipe");
    return 1;
  }
  struct pollfd ready = {.fd = pipefd[0], .events = POLLIN};

  const struct timespec raw_ready_input = {
      .tv_sec = 3, .tv_nsec = 456789123};
  struct timespec raw_ready_timeout = raw_ready_input;
  if (syscall(SYS_ppoll, &ready, 1, &raw_ready_timeout, NULL,
              sizeof(uint64_t)) != 1 ||
      !(ready.revents & POLLIN) || raw_ready_timeout.tv_sec < 0 ||
      raw_ready_timeout.tv_nsec < 0 ||
      raw_ready_timeout.tv_nsec >= 1000000000 ||
      (raw_ready_timeout.tv_sec == 0 && raw_ready_timeout.tv_nsec == 0) ||
      raw_ready_timeout.tv_sec > raw_ready_input.tv_sec ||
      (raw_ready_timeout.tv_sec == raw_ready_input.tv_sec &&
       raw_ready_timeout.tv_nsec >= raw_ready_input.tv_nsec)) {
    fprintf(stderr,
            "raw ready ppoll did not preserve a positive irregular remainder: "
            "%ld.%09ld errno=%d revents=%d\n",
            raw_ready_timeout.tv_sec, raw_ready_timeout.tv_nsec, errno,
            ready.revents);
    return 1;
  }

  long page_size = sysconf(_SC_PAGESIZE);
  if (page_size <= 0) {
    perror("sysconf page size");
    return 1;
  }
  void* timeout_page = mmap(NULL, (size_t)page_size, PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (timeout_page == MAP_FAILED) {
    perror("mmap readonly timeout");
    return 1;
  }
  struct timespec* readonly_timeout = timeout_page;
  *readonly_timeout = raw_ready_input;
  if (mprotect(timeout_page, (size_t)page_size, PROT_READ) != 0) {
    int saved_errno = errno;
    munmap(timeout_page, (size_t)page_size);
    errno = saved_errno;
    perror("mprotect readonly timeout");
    return 1;
  }
  struct pollfd ready_before_timeout_fault = {
      .fd = pipefd[0], .events = POLLIN};
  errno = 0;
  long timeout_fault_result =
      syscall(SYS_ppoll, &ready_before_timeout_fault, 1, readonly_timeout,
              NULL, sizeof(uint64_t));
  int timeout_fault_errno = errno;
  int timeout_fault_revents = ready_before_timeout_fault.revents;
  struct timespec timeout_after_write_fault = *readonly_timeout;
  if (munmap(timeout_page, (size_t)page_size) != 0) {
    perror("munmap readonly timeout");
    return 1;
  }
  if (timeout_fault_result != 1 || timeout_fault_errno != 0 ||
      !(timeout_fault_revents & POLLIN) ||
      timeout_after_write_fault.tv_sec != raw_ready_input.tv_sec ||
      timeout_after_write_fault.tv_nsec != raw_ready_input.tv_nsec) {
    fprintf(stderr,
            "raw ready ppoll did not preserve its result across timeout "
            "writeback failure: result=%ld errno=%d revents=%d "
            "timeout=%ld.%09ld\n",
            timeout_fault_result, timeout_fault_errno, timeout_fault_revents,
            timeout_after_write_fault.tv_sec,
            timeout_after_write_fault.tv_nsec);
    return 1;
  }

  if (close(pipefd[0]) != 0 || close(pipefd[1]) != 0) {
    perror("close record-replay descriptors");
    return 1;
  }
  return 0;
}

static int run_invalid_nfds(void) {
  struct rlimit limit;
  if (getrlimit(RLIMIT_NOFILE, &limit) != 0) {
    perror("getrlimit");
    return 1;
  }
  if (limit.rlim_cur == RLIM_INFINITY ||
      limit.rlim_cur >= (rlim_t)((nfds_t)-1)) {
    fputs("RLIMIT_NOFILE cannot be incremented as nfds_t\n", stderr);
    return 1;
  }

  nfds_t nfds = (nfds_t)limit.rlim_cur + 1;
  errno = 0;
  long result = syscall(SYS_ppoll, (void*)1, nfds, NULL, NULL,
                        sizeof(uint64_t));
  if (result != -1 || errno != EINVAL) {
    fprintf(stderr,
            "invalid nfds ppoll did not return EINVAL: result=%ld errno=%d\n",
            result, errno);
    return 1;
  }
  return 0;
}

static int run_zero_timeout_alias(void) {
  int saved_stdin = dup(STDIN_FILENO);
  int pipefd[2];
  if (saved_stdin < 0 || pipe(pipefd) != 0) {
    perror("zero timeout alias setup");
    return 1;
  }
  if (dup2(pipefd[0], STDIN_FILENO) < 0) {
    perror("zero timeout alias dup2");
    return 1;
  }
  if (close(pipefd[0]) != 0 || close(pipefd[1]) != 0) {
    perror("zero timeout alias close pipe");
    return 1;
  }

  union {
    struct timespec timeout;
    struct pollfd pfd;
  } aliased;
  memset(&aliased, 0, sizeof(aliased));
  errno = 0;
  long result = syscall(SYS_ppoll, &aliased.pfd, 1, &aliased.timeout, NULL,
                        sizeof(uint64_t));
  int observed_errno = errno;
  short observed_revents = aliased.pfd.revents;

  if (dup2(saved_stdin, STDIN_FILENO) < 0 || close(saved_stdin) != 0) {
    perror("zero timeout alias restore stdin");
    return 1;
  }
  if (result != 1 || observed_errno != 0 || !(observed_revents & POLLHUP)) {
    fprintf(stderr,
            "zero timeout alias ppoll diverged: result=%ld errno=%d "
            "revents=%d\n",
            result, observed_errno, observed_revents);
    return 1;
  }
  return 0;
}

static int run_masked_readonly_timeout(void) {
  int pipefd[2];
  if (pipe(pipefd) != 0 || write(pipefd[1], "r", 1) != 1) {
    perror("masked readonly timeout ready pipe");
    return 1;
  }

  long page_size = sysconf(_SC_PAGESIZE);
  if (page_size <= 0) {
    perror("sysconf page size");
    return 1;
  }
  void* timeout_page = mmap(NULL, (size_t)page_size,
                            PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (timeout_page == MAP_FAILED) {
    perror("mmap masked readonly timeout");
    return 1;
  }
  const struct timespec input = {.tv_sec = 3, .tv_nsec = 456789123};
  struct timespec* timeout = timeout_page;
  *timeout = input;
  if (mprotect(timeout_page, (size_t)page_size, PROT_READ) != 0) {
    int saved_errno = errno;
    munmap(timeout_page, (size_t)page_size);
    errno = saved_errno;
    perror("mprotect masked readonly timeout");
    return 1;
  }

  sigset_t mask;
  sigemptyset(&mask);
  sigaddset(&mask, SIGUSR1);
  struct pollfd ready = {.fd = pipefd[0], .events = POLLIN};
  errno = 0;
  long result = syscall(SYS_ppoll, &ready, 1, timeout, &mask,
                        sizeof(uint64_t));
  int observed_errno = errno;
  struct timespec observed_timeout = *timeout;

  if (munmap(timeout_page, (size_t)page_size) != 0) {
    perror("munmap masked readonly timeout");
    return 1;
  }
  if (close(pipefd[0]) != 0 || close(pipefd[1]) != 0) {
    perror("close masked readonly timeout descriptors");
    return 1;
  }
  if (result != 1 || observed_errno != 0 || !(ready.revents & POLLIN) ||
      observed_timeout.tv_sec != input.tv_sec ||
      observed_timeout.tv_nsec != input.tv_nsec) {
    fprintf(stderr,
            "masked readonly timeout ppoll diverged: result=%ld errno=%d "
            "revents=%d timeout=%ld.%09ld\n",
            result, observed_errno, ready.revents, observed_timeout.tv_sec,
            observed_timeout.tv_nsec);
    return 1;
  }
  return 0;
}

static int run_masked_readonly_zero_timeout(void) {
  int pipefd[2];
  if (pipe(pipefd) != 0 || write(pipefd[1], "r", 1) != 1) {
    perror("masked readonly zero timeout ready pipe");
    return 1;
  }

  long page_size = sysconf(_SC_PAGESIZE);
  if (page_size <= 0) {
    perror("sysconf page size");
    return 1;
  }
  void* timeout_page = mmap(NULL, (size_t)page_size,
                            PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (timeout_page == MAP_FAILED) {
    perror("mmap masked readonly zero timeout");
    return 1;
  }
  const struct timespec input = {.tv_sec = 0, .tv_nsec = 0};
  struct timespec* timeout = timeout_page;
  *timeout = input;
  if (mprotect(timeout_page, (size_t)page_size, PROT_READ) != 0) {
    int saved_errno = errno;
    munmap(timeout_page, (size_t)page_size);
    errno = saved_errno;
    perror("mprotect masked readonly zero timeout");
    return 1;
  }

  sigset_t mask;
  sigemptyset(&mask);
  sigaddset(&mask, SIGUSR1);
  struct pollfd ready = {.fd = pipefd[0], .events = POLLIN};
  errno = 0;
  long result = syscall(SYS_ppoll, &ready, 1, timeout, &mask,
                        sizeof(uint64_t));
  int observed_errno = errno;
  struct timespec observed_timeout = *timeout;

  if (munmap(timeout_page, (size_t)page_size) != 0) {
    perror("munmap masked readonly zero timeout");
    return 1;
  }
  if (close(pipefd[0]) != 0 || close(pipefd[1]) != 0) {
    perror("close masked readonly zero timeout descriptors");
    return 1;
  }
  if (result != 1 || observed_errno != 0 || !(ready.revents & POLLIN) ||
      observed_timeout.tv_sec != input.tv_sec ||
      observed_timeout.tv_nsec != input.tv_nsec) {
    fprintf(stderr,
            "masked readonly zero timeout ppoll diverged: result=%ld "
            "errno=%d revents=%d timeout=%ld.%09ld\n",
            result, observed_errno, ready.revents, observed_timeout.tv_sec,
            observed_timeout.tv_nsec);
    return 1;
  }
  return 0;
}

static int run_masked_fail_closed(void) {
  int pipefd[2];
  if (pipe(pipefd) != 0 || write(pipefd[1], "r", 1) != 1) {
    perror("masked fail-closed ready pipe");
    return 1;
  }

  sigset_t mask;
  sigemptyset(&mask);
  sigaddset(&mask, SIGUSR1);
  struct pollfd fd = {.fd = pipefd[0], .events = POLLIN};
  struct timespec ready_timeout = {.tv_sec = 1, .tv_nsec = 0};
  if (ppoll(&fd, 1, &ready_timeout, &mask) != 1 ||
      !(fd.revents & POLLIN)) {
    perror("masked ready ppoll");
    return 1;
  }

  char byte;
  if (read(pipefd[0], &byte, 1) != 1) {
    perror("drain masked ready pipe");
    return 1;
  }
  fd.revents = 0;
  struct timespec blocked_timeout = {.tv_sec = 0, .tv_nsec = 1000000};
  errno = 0;
  if (ppoll(&fd, 1, &blocked_timeout, &mask) != -1 || errno != ENOSYS) {
    fprintf(stderr,
            "blocking masked ppoll did not fail closed: errno=%d revents=%d\n",
            errno, fd.revents);
    return 1;
  }

  if (close(pipefd[0]) != 0 || close(pipefd[1]) != 0) {
    perror("close masked fail-closed descriptors");
    return 1;
  }
  return 0;
}

static int run_inaccessible_mask_kernel_wait(void) {
  long page_size = sysconf(_SC_PAGESIZE);
  if (page_size <= 0) {
    perror("sysconf page size");
    return 1;
  }
  void* mask_page = mmap(NULL, (size_t)page_size, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (mask_page == MAP_FAILED) {
    perror("mmap inaccessible ppoll mask");
    return 1;
  }
  if (mprotect(mask_page, (size_t)page_size, PROT_NONE) != 0) {
    int saved_errno = errno;
    munmap(mask_page, (size_t)page_size);
    errno = saved_errno;
    perror("mprotect inaccessible ppoll mask");
    return 1;
  }

  struct timespec timeout = {.tv_sec = 0, .tv_nsec = 1};
  errno = 0;
  long result = syscall(SYS_ppoll, NULL, 0, &timeout, mask_page,
                        sizeof(uint64_t));
  int observed_errno = errno;
  if (munmap(mask_page, (size_t)page_size) != 0) {
    perror("munmap inaccessible ppoll mask");
    return 1;
  }
  if (result != -1 || observed_errno != EFAULT) {
    fprintf(stderr,
            "inaccessible ppoll mask did not return EFAULT: result=%ld "
            "errno=%d\n",
            result, observed_errno);
    return 1;
  }
  return 0;
}

static int run_default_workload(void) {
  int pipefd[2];
  if (pipe(pipefd) != 0 || write(pipefd[1], "r", 1) != 1) {
    perror("ready pipe");
    return 1;
  }

  sigset_t mask;
  sigemptyset(&mask);
  sigaddset(&mask, SIGUSR1);
  struct pollfd ready = {.fd = pipefd[0], .events = POLLIN};

  struct timespec ready_timeout = {.tv_sec = 1, .tv_nsec = 0};
  if (ppoll(&ready, 1, &ready_timeout, &mask) != 1 ||
      !(ready.revents & POLLIN)) {
    perror("ready ppoll");
    return 1;
  }

  char byte;
  if (read(pipefd[0], &byte, 1) != 1) {
    perror("drain ready pipe");
    return 1;
  }

  ready.revents = 0;
  struct timespec zero = {.tv_sec = 0, .tv_nsec = 0};
  if (ppoll(&ready, 1, &zero, NULL) != 0) {
    perror("zero ppoll");
    return 1;
  }

  struct timespec finite = {.tv_sec = 0, .tv_nsec = 20000000};
  if (ppoll(&ready, 1, &finite, NULL) != 0) {
    perror("finite ppoll");
    return 1;
  }

  struct timespec raw_timeout = {.tv_sec = 0, .tv_nsec = 5000000};
  if (syscall(SYS_ppoll, NULL, 0, &raw_timeout, NULL, sizeof(uint64_t)) != 0 ||
      raw_timeout.tv_sec != 0 || raw_timeout.tv_nsec != 0) {
    fprintf(stderr, "raw ppoll timeout was not consumed: %ld.%09ld\n",
            raw_timeout.tv_sec, raw_timeout.tv_nsec);
    return 1;
  }

  struct timespec invalid = {.tv_sec = 0, .tv_nsec = 1000000000};
  errno = 0;
  if (syscall(SYS_ppoll, NULL, 0, &invalid, NULL, sizeof(uint64_t)) != -1 ||
      errno != EINVAL) {
    fprintf(stderr, "invalid ppoll timeout: errno=%d\n", errno);
    return 1;
  }

  errno = 0;
  if (syscall(SYS_ppoll, NULL, 0, (void*)1, NULL, sizeof(uint64_t)) != -1 ||
      errno != EFAULT) {
    fprintf(stderr, "bad ppoll timeout pointer: errno=%d\n", errno);
    return 1;
  }

  struct timespec masked_timeout = {.tv_sec = 0, .tv_nsec = 1000000};
  errno = 0;
  if (ppoll(&ready, 1, &masked_timeout, &mask) != -1 || errno != ENOSYS) {
    fprintf(stderr, "blocking masked ppoll did not fail closed: errno=%d\n",
            errno);
    return 1;
  }

  int delayed_pipe[2];
  if (pipe(delayed_pipe) != 0) {
    perror("delayed pipe");
    return 1;
  }
  pthread_t writer;
  if (pthread_create(&writer, NULL, delayed_writer, &delayed_pipe[1]) != 0) {
    perror("pthread_create");
    return 1;
  }
  struct pollfd delayed = {.fd = delayed_pipe[0], .events = POLLIN};
  if (ppoll(&delayed, 1, NULL, NULL) != 1 || !(delayed.revents & POLLIN)) {
    perror("infinite ppoll");
    return 1;
  }
  void* writer_result = NULL;
  if (pthread_join(writer, &writer_result) != 0 || writer_result != NULL) {
    fputs("delayed writer failed\n", stderr);
    return 1;
  }

  if (run_zero_timeout_alias() != 0) {
    return 1;
  }

  puts("ppoll-simulation-ok");
  return 0;
}

int main(int argc, char** argv) {
  if (argc == 1) {
    return run_default_workload();
  }
  if (argc != 2) {
    fprintf(stderr,
            "usage: %s [raw-timeout-copyout|masked-readonly-timeout|"
            "masked-readonly-zero-timeout|masked-fail-closed|"
            "inaccessible-mask-kernel-wait|record-replay|zero-timeout-alias]\n",
            argv[0]);
    return 2;
  }
  if (strcmp(argv[1], "raw-timeout-copyout") == 0) {
    int result = run_raw_timeout_copyout();
    if (result == 0) {
      puts("ppoll-simulation-ok");
    }
    return result;
  }
  if (strcmp(argv[1], "record-replay") == 0) {
    int result = run_raw_timeout_copyout();
    if (result == 0) {
      result = run_invalid_nfds();
    }
    return result == 0 ? run_default_workload() : result;
  }
  if (strcmp(argv[1], "masked-readonly-timeout") == 0) {
    int result = run_masked_readonly_timeout();
    if (result == 0) {
      puts("ppoll-simulation-ok");
    }
    return result;
  }
  if (strcmp(argv[1], "masked-readonly-zero-timeout") == 0) {
    int result = run_masked_readonly_zero_timeout();
    if (result == 0) {
      puts("ppoll-simulation-ok");
    }
    return result;
  }
  if (strcmp(argv[1], "masked-fail-closed") == 0) {
    int result = run_masked_fail_closed();
    if (result == 0) {
      puts("ppoll-simulation-ok");
    }
    return result;
  }
  if (strcmp(argv[1], "inaccessible-mask-kernel-wait") == 0) {
    int result = run_inaccessible_mask_kernel_wait();
    if (result == 0) {
      puts("ppoll-simulation-ok");
    }
    return result;
  }
  if (strcmp(argv[1], "zero-timeout-alias") == 0) {
    int result = run_zero_timeout_alias();
    if (result == 0) {
      puts("ppoll-simulation-ok");
    }
    return result;
  }

  fprintf(stderr,
          "usage: %s [raw-timeout-copyout|masked-readonly-timeout|"
          "masked-readonly-zero-timeout|masked-fail-closed|"
          "inaccessible-mask-kernel-wait|record-replay|zero-timeout-alias]\n",
          argv[0]);
  return 2;
}
