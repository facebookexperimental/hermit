/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/select.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

// Closed malformed-wrapper validation contract; the full pselect workload
// and its valid-timeout permission assertions remain in pselect6_simulation.c.
struct pselect6_sigmask_arg {
  const void* sigmask;
  size_t sigsetsize;
};

// Keep the pending-signal oracle identical to the native/ptrace/DBT control.
static volatile sig_atomic_t handled;
static void handler(int signal) { (void)signal; ++handled; }

static int run_validation_pending(int queued) {
    struct sigaction action = {.sa_handler = handler};
    sigemptyset(&action.sa_mask);
    sigset_t block, old, pending, current, temporary;
    sigemptyset(&block);
    sigaddset(&block, SIGCHLD);
    sigemptyset(&temporary);
    if (sigaction(SIGCHLD, &action, NULL) || sigprocmask(SIG_BLOCK, &block, &old)) return 3;
    if (queued && kill(getpid(), SIGCHLD)) return 4;
    if (sigpending(&pending) || sigismember(&pending, SIGCHLD) != queued || handled) return 5;
    const struct { const void *mask; size_t bytes; } wrapper = {&temporary, sizeof(uint64_t)};
    struct timespec malformed = {.tv_sec = 0, .tv_nsec = 1000000000};
    errno = 0;
    long result = syscall(SYS_pselect6, 0, NULL, NULL, NULL, &malformed, &wrapper);
    int error = errno;
    if (sigpending(&pending) || sigprocmask(SIG_SETMASK, NULL, &current)) return 6;
    int after_pending = sigismember(&pending, SIGCHLD);
    int after_blocked = sigismember(&current, SIGCHLD);
    int after_handled = handled;
    printf("mode=%s result=%ld errno=%d pending=%d blocked=%d handled=%d timeout=%ld,%ld\n",
           queued ? "pending" : "empty", result, error, after_pending, after_blocked, after_handled,
           (long)malformed.tv_sec, malformed.tv_nsec);
    int failed = result != -1 || error != EINVAL || after_pending != queued ||
                 after_blocked != 1 || after_handled != 0 ||
                 malformed.tv_sec != 0 || malformed.tv_nsec != 1000000000;
    if (sigprocmask(SIG_SETMASK, &old, NULL)) return 7;
    if (handled != queued) return 8;
    return failed ? 1 : 0;
}

static int run_wrapper_boundary(int unmap_tail) {
  long page_size = sysconf(_SC_PAGESIZE);
  if (page_size <= 0) {
    fputs("invalid pselect wrapper page size\n", stderr);
    return 1;
  }
  char* mapping = mmap(
      NULL,
      (size_t)page_size * 2,
      PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS,
      -1,
      0);
  if (mapping == MAP_FAILED) {
    perror("pselect wrapper mapping");
    return 1;
  }
  struct pselect6_sigmask_arg* wrapper =
      (struct pselect6_sigmask_arg*)(mapping + page_size - sizeof(void*));
  *wrapper = (struct pselect6_sigmask_arg){
      .sigmask = NULL,
      .sigsetsize = sizeof(uint64_t),
  };
  struct timespec malformed = {.tv_sec = 0, .tv_nsec = 1000000000};
  errno = 0;
  long result = syscall(SYS_pselect6, 0, NULL, NULL, NULL, &malformed, wrapper);
  if (result != -1 || errno != EINVAL) {
    fprintf(
        stderr,
        "readable split pselect6 wrapper: result=%ld errno=%d\n",
        result,
        errno);
    munmap(mapping, (size_t)page_size * 2);
    return 1;
  }
  int changed = unmap_tail
      ? munmap(mapping + page_size, (size_t)page_size)
      : mprotect(mapping + page_size, (size_t)page_size, PROT_NONE);
  if (changed != 0) {
    perror("pselect wrapper tail access");
    munmap(mapping, (size_t)page_size * 2);
    return 1;
  }
  int failed = 0;
  const void* timeouts[] = {&malformed, (void*)1};
  for (size_t i = 0; i < sizeof(timeouts) / sizeof(timeouts[0]); ++i) {
    errno = 0;
    result = syscall(SYS_pselect6, 0, NULL, NULL, NULL, timeouts[i], wrapper);
    if (result != -1 || errno != EFAULT) {
      fprintf(
          stderr,
          "%s pselect6 wrapper tail before %s timeout: "
          "result=%ld errno=%d (expected EFAULT)\n",
          unmap_tail ? "unmapped" : "protected",
          i == 0 ? "malformed" : "unreadable",
          result,
          errno);
      failed = 1;
    }
  }
  if (munmap(mapping, (size_t)page_size * (unmap_tail ? 1 : 2)) != 0) {
    perror("munmap pselect wrapper");
    return 1;
  }
  return failed;
}

static int run_wrapper_permissions(void) {
  long page = sysconf(_SC_PAGESIZE);
  if (page <= 0) return 1;
  void* mapping = mmap(NULL, (size_t)page, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (mapping == MAP_FAILED) return 1;
  sigset_t temporary;
  sigemptyset(&temporary);
  struct pselect6_sigmask_arg* wrapper = mapping;
  *wrapper = (struct pselect6_sigmask_arg){&temporary, sizeof(uint64_t)};
  const int protections[] = {PROT_READ, PROT_WRITE, PROT_READ | PROT_WRITE};
  for (unsigned i = 0; i < sizeof(protections) / sizeof(protections[0]); ++i) {
    if (mprotect(mapping, (size_t)page, protections[i])) return 1;
    struct timespec malformed = {0, 1000000000};
    errno = 0;
    long result = syscall(SYS_pselect6, 0, NULL, NULL, NULL, &malformed, wrapper);
    if (result != -1 || errno != EINVAL || wrapper->sigmask != &temporary ||
        wrapper->sigsetsize != sizeof(uint64_t) || malformed.tv_sec != 0 ||
        malformed.tv_nsec != 1000000000) {
      fprintf(stderr, "pselect input protection=%d result=%ld errno=%d\n",
              protections[i], result, errno);
      return 1;
    }
  }
  return munmap(mapping, (size_t)page) != 0;
}

static int run_argument_validation_order(void) {
  struct timespec malformed = {.tv_sec = 0, .tv_nsec = 1000000000};
  errno = 0;
  long result =
      syscall(SYS_pselect6, 0, NULL, NULL, NULL, &malformed, (void*)1);
  if (result != -1 || errno != EFAULT) {
    fprintf(
        stderr,
        "unreadable pselect6 signal-mask wrapper did not precede timeout "
        "validation: result=%ld errno=%d\n",
        result,
        errno);
    return 1;
  }

  sigset_t mask;
  sigemptyset(&mask);
  struct pselect6_sigmask_arg bad_size = {
      .sigmask = &mask,
      .sigsetsize = 0,
  };
  errno = 0;
  result = syscall(SYS_pselect6, 0, NULL, NULL, NULL, (void*)1, &bad_size);
  if (result != -1 || errno != EFAULT) {
    fprintf(
        stderr,
        "pselect6 timeout access did not precede signal-mask size "
        "validation: result=%ld errno=%d\n",
        result,
        errno);
    return 1;
  }

  struct pselect6_sigmask_arg unreadable_mask = {
      .sigmask = (void*)1,
      .sigsetsize = sizeof(uint64_t),
  };
  errno = 0;
  result =
      syscall(SYS_pselect6, 0, NULL, NULL, NULL, &malformed, &unreadable_mask);
  if (result != -1 || errno != EINVAL) {
    fprintf(
        stderr,
        "pselect6 timeout validation did not precede signal-mask access: "
        "result=%ld errno=%d\n",
        result,
        errno);
    return 1;
  }
  if (run_wrapper_boundary(0) != 0 || run_wrapper_boundary(1) != 0 ||
      run_wrapper_permissions() != 0) {
    return 1;
  }
  return 0;
}

int main(int argc, char** argv) {
  if (argc != 2) {
    fprintf(
        stderr,
        "usage: %s [argument-validation-order|wrapper-protected-tail|"
        "wrapper-unmapped-tail|validation-pending|validation-empty]\n",
        argv[0]);
    return 2;
  }
  if (strcmp(argv[1], "validation-pending") == 0 ||
      strcmp(argv[1], "validation-empty") == 0) {
    return run_validation_pending(strcmp(argv[1], "validation-pending") == 0);
  }
  if (strcmp(argv[1], "wrapper-protected-tail") == 0 ||
      strcmp(argv[1], "wrapper-unmapped-tail") == 0) {
    int result =
        run_wrapper_boundary(strcmp(argv[1], "wrapper-unmapped-tail") == 0);
    if (result == 0) {
      puts("pselect6-simulation-ok");
    }
    return result;
  }
  if (strcmp(argv[1], "argument-validation-order") == 0) {
    int result = run_argument_validation_order();
    if (result == 0) {
      puts("pselect6-simulation-ok");
    }
    return result;
  }

  fprintf(
      stderr,
      "usage: %s [argument-validation-order|wrapper-protected-tail|"
      "wrapper-unmapped-tail|validation-pending|validation-empty]\n",
      argv[0]);
  return 2;
}
