/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A freestanding guest is needed because the chroot intentionally contains no
 * dynamic loader or libc.  Keep this probe to the four syscalls required to
 * compare the visible procfs mount row with the fdinfo for that open file.
 */

#define AT_FDCWD -100
#define O_RDONLY 0

static long syscall1(long number, long arg1) {
  long result;
  __asm__ volatile("syscall"
                   : "=a"(result)
                   : "a"(number), "D"(arg1)
                   : "rcx", "r11", "memory");
  return result;
}

static long syscall3(long number, long arg1, long arg2, long arg3) {
  long result;
  __asm__ volatile("syscall"
                   : "=a"(result)
                   : "a"(number), "D"(arg1), "S"(arg2), "d"(arg3)
                   : "rcx", "r11", "memory");
  return result;
}

static long syscall4(long number, long arg1, long arg2, long arg3, long arg4) {
  register long r10 __asm__("r10") = arg4;
  long result;
  __asm__ volatile("syscall"
                   : "=a"(result)
                   : "a"(number), "D"(arg1), "S"(arg2), "d"(arg3), "r"(r10)
                   : "rcx", "r11", "memory");
  return result;
}

static unsigned long string_length(const char *text) {
  unsigned long length = 0;
  while (text[length] != '\0') {
    length++;
  }
  return length;
}

static int write_all(int fd, const char *bytes, unsigned long length) {
  while (length > 0) {
    long written = syscall3(1, fd, (long)bytes, length);
    if (written <= 0) {
      return -1;
    }
    bytes += written;
    length -= (unsigned long)written;
  }
  return 0;
}

static int copy_file_to_stdout(int fd) {
  char buffer[4096];
  for (;;) {
    long count = syscall3(0, fd, (long)buffer, sizeof(buffer));
    if (count == 0) {
      return 0;
    }
    if (count < 0 || write_all(1, buffer, (unsigned long)count) != 0) {
      return -1;
    }
  }
}

static char *append_decimal(char *cursor, unsigned long value) {
  char reverse[24];
  unsigned long digits = 0;
  do {
    reverse[digits++] = (char)('0' + value % 10);
    value /= 10;
  } while (value != 0);
  while (digits > 0) {
    *cursor++ = reverse[--digits];
  }
  *cursor = '\0';
  return cursor;
}

void _start(void) {
  static const char mountinfo_path[] = "/proc/self/mountinfo";
  static const char mountinfo_marker[] = "__MOUNTINFO__\n";
  static const char fdinfo_prefix[] = "/proc/self/fdinfo/";
  static const char fdinfo_marker[] = "__FDINFO__\n";
  char fdinfo_path[64];
  unsigned long index;

  long mountinfo_fd = syscall4(257, AT_FDCWD, (long)mountinfo_path, O_RDONLY, 0);
  if (mountinfo_fd < 0 ||
      write_all(1, mountinfo_marker, sizeof(mountinfo_marker) - 1) != 0 ||
      copy_file_to_stdout((int)mountinfo_fd) != 0 ||
      write_all(1, fdinfo_marker, sizeof(fdinfo_marker) - 1) != 0) {
    syscall1(60, 1);
  }

  for (index = 0; index < sizeof(fdinfo_prefix) - 1; index++) {
    fdinfo_path[index] = fdinfo_prefix[index];
  }
  append_decimal(fdinfo_path + index, (unsigned long)mountinfo_fd);
  long fdinfo_fd = syscall4(257, AT_FDCWD, (long)fdinfo_path, O_RDONLY, 0);
  if (fdinfo_fd < 0 || copy_file_to_stdout((int)fdinfo_fd) != 0) {
    syscall1(60, 2);
  }

  syscall1(3, fdinfo_fd);
  syscall1(3, mountinfo_fd);
  syscall1(60, 0);
  __builtin_unreachable();
}
