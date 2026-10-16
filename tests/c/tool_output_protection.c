/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Detcore's emulated syscall outputs must respect page protection, as the
// kernel's own writes do (https://github.com/rrnewton/reverie/issues/1064).
// Adapted from the reviewer's counterexample guest for that issue.
//
//   stack          rt_sigprocmask with RSP chosen so that the eight-byte mask
//                  Detcore pushes on the guest stack lands on an executable,
//                  read-only page
//   private-stack  the same, aimed at Reverie's private page at 0x71000000,
//                  whose `ud2` at +2 guards the untraced syscall stub
//   scalar         time(tloc) with tloc four bytes before a read-only page
//
// Each prints one line: the syscall's raw result and whether the target
// bytes are unchanged. Natively the stack cases return 0 (the kernel uses no
// guest stack) and the scalar case returns -1 EFAULT.

#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

static void require(int condition, const char* what) {
  if (!condition) {
    fprintf(stderr, "FAIL %s errno=%d\n", what, errno);
    exit(90);
  }
}

// rt_sigprocmask(SIG_SETMASK, set, NULL, 8) issued with RSP = new_rsp.
static long mask_with_rsp(uintptr_t new_rsp, const uint64_t* set) {
  register long rax __asm__("rax") = SYS_rt_sigprocmask;
  register long r10 __asm__("r10") = 8;
  __asm__ volatile(
      "mov %%rsp, %%r12\n\t"
      "mov %[new_rsp], %%rsp\n\t"
      "syscall\n\t"
      "mov %%r12, %%rsp"
      : "+a"(rax)
      : "D"((long)SIG_SETMASK), "S"(set), "d"((long)0), "r"(r10),
        [new_rsp] "r"(new_rsp)
      : "rcx", "r11", "r12", "memory");
  return rax;
}

static int stack_case(int private_page) {
  uintptr_t address = private_page ? UINT64_C(0x71000000) : UINT64_C(0x72000000);
  unsigned char* page = mmap((void*)address, 4096, PROT_READ | PROT_WRITE,
                             MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
  if (page == MAP_FAILED) {
    // Under Hermit, Reverie's private page is already there.
    require(private_page && errno == EEXIST, "map the target page");
    page = (unsigned char*)address;
  } else {
    memset(page, 0x90, 4096);
    page[0] = 0x0f;
    page[1] = 0x05;
    page[2] = 0x0f;
    page[3] = 0x0b;
    require(mprotect(page, 4096, PROT_READ | PROT_EXEC) == 0, "protect the target page");
  }
  unsigned char before[8];
  memcpy(before, page + 2, 8);
  uint64_t set = 0;
  // Reverie's stack keeps a 128-byte red zone, so the pushed word lands at
  // RSP - 136 = page + 2.
  long result = mask_with_rsp(address + 2 + 128 + 8, &set);
  printf("stack target=%s result=%ld unchanged=%d\n", private_page ? "private" : "rx", result,
         memcmp(before, page + 2, 8) == 0);
  return 0;
}

static int scalar_case(void) {
  size_t size = (size_t)sysconf(_SC_PAGESIZE);
  unsigned char* pages =
      mmap(NULL, 2 * size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  require(pages != MAP_FAILED, "map the scalar pages");
  unsigned char* word = pages + size - 4;
  memset(word, 0xa5, 8);
  require(mprotect(pages + size, size, PROT_READ) == 0, "protect the second page");
  errno = 0;
  long result = syscall(SYS_time, word);
  int error = errno;
  unsigned char expected[8];
  memset(expected, 0xa5, 8);
  printf("scalar result=%ld errno=%d unchanged=%d\n", result, error,
         memcmp(word, expected, 8) == 0);
  return 0;
}

int main(int argc, char** argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  require(argc == 2, "argument: stack | private-stack | scalar");
  if (strcmp(argv[1], "stack") == 0)
    return stack_case(0);
  if (strcmp(argv[1], "private-stack") == 0)
    return stack_case(1);
  if (strcmp(argv[1], "scalar") == 0)
    return scalar_case();
  return 94;
}
