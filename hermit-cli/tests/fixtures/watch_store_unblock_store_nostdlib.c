// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

// Stores to a global, unblocks SIGTRAP, then stores again, all before
// any other syscall: after a blocked exec into it, `hermit run --watch`
// sees only the second store, and must still report the first as
// uncovered (x86-64).

volatile long watched;

void _start(void) {
  watched = 1;
  // rt_sigprocmask(SIG_UNBLOCK, {SIGTRAP}, NULL, 8).
  unsigned long mask = 1UL << (5 - 1);
  register long how __asm__("rdi") = 1; // SIG_UNBLOCK
  register unsigned long* set __asm__("rsi") = &mask;
  register long oset __asm__("rdx") = 0;
  register long size __asm__("r10") = 8;
  __asm__ volatile("syscall"
                   :
                   : "a"(14L), "r"(how), "r"(set), "r"(oset), "r"(size)
                   : "rcx", "r11", "memory");
  watched = 2;
  __asm__ volatile("movl $60, %%eax\n\t" // exit
                   "xorl %%edi, %%edi\n\t"
                   "syscall" ::
                       : "rax", "rdi", "rcx", "r11", "memory");
  __builtin_unreachable();
}
