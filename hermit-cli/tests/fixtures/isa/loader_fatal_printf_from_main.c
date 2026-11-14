/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */


/*
 * A dynamically linked program with no C library: linked against glibc's
 * loader alone, so its only DT_NEEDED is the loader, which is already mapped,
 * and its startup opens and maps no file. Its main prints glibc's loader
 * message, "Fatal glibc error: CPU does not support x86-64-v3\n", through the
 * loader's own exported _dl_fatal_printf, which writes it with writev and
 * calls _exit (127): the loader's bytes and status, from the loader's code,
 * after the loader has started the program. Hermit must not take it for the
 * loader's abort (https://github.com/rrnewton/hermit/issues/4695).
 *
 * MAIN_MARKER first writes "main\n" to standard output with a system call of
 * the program's own, showing that main ran. Without it, every system call
 * the program makes comes from the loader's code.
 */

extern void _dl_fatal_printf(const char* format, ...)
    __attribute__((noreturn, format(printf, 1, 2)));

int main(void) {
#ifdef MAIN_MARKER
  static const char ran[] = "main\n";
  long result;
  __asm__ volatile("syscall"
                   : "=a"(result)
                   : "a"(1L), "D"(1L), "S"(ran), "d"(sizeof ran - 1)
                   : "rcx", "r11", "memory");
  (void)result;
#endif
  _dl_fatal_printf("Fatal glibc error: CPU does not support x86-64-v%d\n", 3);
}

// The entry point: no C library, so no crt1. The stack is 16-byte aligned at
// the call, as the ABI requires.
__asm__(
    ".text\n"
    ".globl _start\n"
    "_start:\n"
    "  xor %ebp, %ebp\n"
    "  and $-16, %rsp\n"
    "  call main\n"
    "  ud2\n");
