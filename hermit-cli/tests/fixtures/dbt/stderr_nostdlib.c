// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

static const char message[] = "guest-stderr\n";

__attribute__((noreturn)) void _start(void) {
  register long syscall_number __asm__("rax") = 1;
  register long fd __asm__("rdi") = 2;
  register const char* buffer __asm__("rsi") = message;
  register long length __asm__("rdx") = sizeof(message) - 1;
  __asm__ volatile("syscall"
                   : "+r"(syscall_number)
                   : "r"(fd), "r"(buffer), "r"(length)
                   : "rcx", "r11", "memory");

  syscall_number = 60;
  fd = 0;
  __asm__ volatile("syscall"
                   : "+r"(syscall_number)
                   : "r"(fd)
                   : "rcx", "r11", "memory");
  __builtin_unreachable();
}
