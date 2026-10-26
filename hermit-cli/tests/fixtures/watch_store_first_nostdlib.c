// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

// Its first instructions store to a global, before any syscall: after an
// exec into it, `hermit run --watch` sees the store only if the thread armed
// its watches again before the new image ran (x86-64).

volatile long watched;

void _start(void) {
  watched = 1;
  asm volatile("movl $60, %%eax\n\t" // exit
               "xorl %%edi, %%edi\n\t"
               "syscall" ::
                   : "rax", "rdi", "rcx", "r11", "memory");
  __builtin_unreachable();
}
