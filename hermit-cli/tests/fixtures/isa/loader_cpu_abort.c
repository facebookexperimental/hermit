/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */


/*
 * Ends as glibc's dynamic loader ends on a CPU below the x86-64 level it was
 * built for (https://github.com/rrnewton/hermit/issues/4695): it writes
 * "Fatal glibc error: CPU does not support x86-64-v" ABORT_LEVEL "\n" to
 * standard error with one writev of three iovecs, the prefix, the level and
 * the newline, as _dl_fatal_printf fills the loader's format
 * "Fatal glibc error: CPU does not support x86-64-v%d\n", then calls
 * exit_group(ABORT_EXIT), 127 by default as the loader does. It uses no C
 * library and no instruction above baseline x86-64.
 *
 * Linked static-pie it is a program interpreter, which the kernel runs in
 * place of an image naming it, so the message comes from interpreter code
 * before anything is opened, as the loader's does. Linked static it is an
 * image with no interpreter.
 *
 * ABORT_WITH_WRITE writes the message with one write instead. ABORT_THEN_RUN
 * writes "ran\n" to standard output after the message and exits 0, so the
 * message is not followed by the loader's exit.
 *
 * ABORT_ARCH_PRCTL first calls arch_prctl with that command and the address of
 * a static buffer, as the el9 loader queries CET (0x3001) before its level
 * check, or as a loader sets its thread pointer (ARCH_SET_FS, 0x1002) after
 * it; ABORT_ARCH_PRCTL_FAILS passes a non-canonical address instead, which
 * ARCH_SET_FS refuses with EPERM. The call's result is ignored.
 */

#ifndef ABORT_LEVEL
#define ABORT_LEVEL "3"
#endif
#ifndef ABORT_EXIT
#define ABORT_EXIT 127
#endif

#define PREFIX "Fatal glibc error: CPU does not support x86-64-v"

struct iovec_ {
  const char* base;
  unsigned long len;
};

static long syscall3(long number, long a, long b, long c) {
  long result;
  __asm__ volatile("syscall"
                   : "=a"(result)
                   : "a"(number), "D"(a), "S"(b), "d"(c)
                   : "rcx", "r11", "memory");
  return result;
}

void _start(void) {
#if defined(ABORT_ARCH_PRCTL) && defined(ABORT_ARCH_PRCTL_FAILS)
  syscall3(158, ABORT_ARCH_PRCTL, (long)(1UL << 63), 0);
#elif defined(ABORT_ARCH_PRCTL)
  static unsigned long area[8];
  syscall3(158, ABORT_ARCH_PRCTL, (long)area, 0);
#endif
#ifdef ABORT_WITH_WRITE
  static const char message[] = PREFIX ABORT_LEVEL "\n";
  syscall3(1, 2, (long)message, sizeof message - 1);
#else
  static const char prefix[] = PREFIX;
  static const char level[] = ABORT_LEVEL;
  static const char newline[] = "\n";
  // Filled at run time: an initialized array of addresses would need the
  // relocations a static-pie image without a C library never applies.
  struct iovec_ iov[3];
  iov[0].base = prefix;
  iov[0].len = sizeof prefix - 1;
  iov[1].base = level;
  iov[1].len = sizeof level - 1;
  iov[2].base = newline;
  iov[2].len = sizeof newline - 1;
  syscall3(20, 2, (long)iov, 3);
#endif
#ifdef ABORT_THEN_RUN
  static const char ran[] = "ran\n";
  syscall3(1, 1, (long)ran, sizeof ran - 1);
  syscall3(231, 0, 0, 0);
#else
  syscall3(231, ABORT_EXIT, 0, 0);
#endif
  __builtin_unreachable();
}
