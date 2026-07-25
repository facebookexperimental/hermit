/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define AT_FDCWD -100
#define O_RDONLY 0

#define SYS_close 3
#define SYS_pread64 17
#define SYS_exit 60
#define SYS_openat 257

static char buffer[4096];

static long syscall1(long number, long arg1) {
  long result;
  asm volatile("syscall"
               : "=a"(result)
               : "a"(number), "D"(arg1)
               : "rcx", "r11", "memory");
  return result;
}

static long syscall4(long number, long arg1, long arg2, long arg3, long arg4) {
  long result;
  register long r10 asm("r10") = arg4;
  asm volatile("syscall"
               : "=a"(result)
               : "a"(number), "D"(arg1), "S"(arg2), "d"(arg3), "r"(r10)
               : "rcx", "r11", "memory");
  return result;
}

_Noreturn static void exit_group(int status) {
  syscall1(SYS_exit, status);
  __builtin_unreachable();
}

void _start(void) {
  static const char path[] = "/bin/true";
  long fd = syscall4(SYS_openat, AT_FDCWD, (long)path, O_RDONLY, 0);
  if (fd < 0) {
    exit_group(1);
  }

  long bytes_read = syscall4(SYS_pread64, fd, (long)buffer, sizeof(buffer), 0);
  long close_result = syscall1(SYS_close, fd);
  if (bytes_read <= 0 || close_result != 0) {
    exit_group(2);
  }

  exit_group(0);
}
