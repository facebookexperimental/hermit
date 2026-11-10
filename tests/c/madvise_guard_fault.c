/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A direct access to a guard page (madvise MADV_GUARD_INSTALL, Linux 6.13)
 * raises SIGSEGV with SEGV_MAPERR and si_addr the faulting address, as Linux's
 * guard marker does (handle_pte_marker returns VM_FAULT_SIGSEGV, which the x86
 * fault path reports as bad_area_nosemaphore, SEGV_MAPERR). It does so after a
 * protection change, in a fork's child, and kills a child with no handler;
 * after MADV_GUARD_REMOVE the page is accessible again and reads as zeros.
 *
 * The KVM backend's page tables enforce no page protection, so there a direct
 * access to a guard page does not fault
 * (https://github.com/rrnewton/hermit/issues/5177); its manifest cell is
 * disabled until that is fixed. tests/c/madvise_guard_regions.c covers the
 * syscall-visible part of guard regions on every backend.
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <setjmp.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
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

static sigjmp_buf resume;
static volatile uintptr_t expected_address;
static volatile int fault_code;
static volatile int fault_address_exact;

static void on_segv(int signal, siginfo_t *info, void *context) {
  (void)signal;
  (void)context;
  fault_code = info->si_code;
  fault_address_exact = (uintptr_t)info->si_addr == expected_address;
  siglongjmp(resume, 1);
}

static const char *code_name(int code) {
  switch (code) {
  case SEGV_MAPERR:
    return "SEGV_MAPERR";
  case SEGV_ACCERR:
    return "SEGV_ACCERR";
  default:
    return "other";
  }
}

/* Reads (or writes) the byte at `address` and prints what happened. */
static void touch(const char *name, volatile char *address, int write) {
  expected_address = (uintptr_t)address;
  fault_code = 0;
  fault_address_exact = 0;
  if (sigsetjmp(resume, 1) == 0) {
    int value = 0;
    if (write) {
      *address = 1;
    } else {
      value = *address;
    }
    printf("%s: no fault, read %d\n", name, value);
  } else {
    printf("%s: SIGSEGV %s, si_addr %s\n", name, code_name(fault_code),
           fault_address_exact ? "exact" : "wrong");
  }
  fflush(stdout);
}

int main(void) {
  long page = sysconf(_SC_PAGESIZE);
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_sigaction = on_segv;
  action.sa_flags = SA_SIGINFO;
  if (sigaction(SIGSEGV, &action, NULL) != 0) {
    return 2;
  }
  char *mapping = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (mapping == MAP_FAILED) {
    return 3;
  }
  memset(mapping, 'a', 2 * page);
  char *guarded = mapping + page;
  if (madvise(guarded, page, MADV_GUARD_INSTALL) != 0) {
    perror("madvise(MADV_GUARD_INSTALL)");
    return 4;
  }
  touch("read guard", guarded, 0);
  touch("write guard", guarded + 17, 1);
  touch("read neighbour", mapping, 0);

  /* A protection change keeps the guard. */
  if (mprotect(mapping, 2 * page, PROT_READ) != 0) {
    return 5;
  }
  touch("read guard after mprotect", guarded, 0);
  mprotect(mapping, 2 * page, PROT_READ | PROT_WRITE);

  /* A fork's child inherits the guard; one with no handler dies of it. */
  fflush(stdout);
  pid_t child = fork();
  if (child == 0) {
    touch("child read guard", guarded, 0);
    signal(SIGSEGV, SIG_DFL);
    (void)*(volatile char *)guarded;
    _exit(0);
  }
  int status;
  if (waitpid(child, &status, 0) != child) {
    return 6;
  }
  if (WIFSIGNALED(status)) {
    printf("child killed by %s\n", strsignal(WTERMSIG(status)));
  } else {
    printf("child exited %d\n", WEXITSTATUS(status));
  }

  if (madvise(guarded, page, MADV_GUARD_REMOVE) != 0) {
    return 7;
  }
  touch("read removed guard", guarded, 0);
  touch("write removed guard", guarded, 1);
  return 0;
}
