/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A guest for hermit-cli's library unit tests, which launch it in place of the
// host's /bin/sh and /bin/echo (crate::unit_guest). Those host programs, and
// the loader they name, can declare that they need more of the CPU than the
// x86-64-v2 profile presents (an x86-64-v3 ISA marker), and Hermit refuses to
// run them under it. This guest uses raw system calls only: no C library, a
// static image with no program interpreter, compiled for x86-64-v2. It
// declares no ISA level and loads nothing that could, which the tests check
// before they use it.
//
// Modes, the first argument:
//   touch PATH          create PATH (truncated, mode 0600), then exit 0
//   echo WORD...        write the words, separated by spaces, and a newline
//                       to standard output, then exit 0
//   orphan FIFO LINK    write "before-orphan\n" to standard output, then fork:
//                       the parent exits 0 at once; the child waits until it
//                       is orphaned, then execs LINK as "cat open FIFO"
//   open FIFO           open FIFO for reading with openat, which blocks
//                       while the FIFO has no writer; writes nothing
// Any failure exits nonzero: 1 for a failed system call, 2 for a usage error,
// 127 for a failed exec.
//
// Built with -ffreestanding -fno-builtin -fno-stack-protector, so nothing here
// may need a C library routine: no aggregate initializers or copies that a
// compiler could turn into memcpy or memset calls.

#define AT_FDCWD -100
#define O_RDONLY 0
#define O_WRONLY 1
#define O_CREAT 0100
#define O_TRUNC 01000

#define SYS_write 1
#define SYS_close 3
#define SYS_sched_yield 24
#define SYS_getpid 39
#define SYS_fork 57
#define SYS_execve 59
#define SYS_getppid 110
#define SYS_exit_group 231
#define SYS_openat 257

static long syscall1(long number, long arg1) {
  long result;
  asm volatile("syscall"
               : "=a"(result)
               : "a"(number), "D"(arg1)
               : "rcx", "r11", "memory");
  return result;
}

static long syscall3(long number, long arg1, long arg2, long arg3) {
  long result;
  asm volatile("syscall"
               : "=a"(result)
               : "a"(number), "D"(arg1), "S"(arg2), "d"(arg3)
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
  for (;;) {
    syscall1(SYS_exit_group, status);
  }
}

static long length(const char* text) {
  long n = 0;
  while (text[n] != '\0') {
    n++;
  }
  return n;
}

// Writes all of `text` to `fd`, or exits 1.
static void write_all(int fd, const char* text, long size) {
  while (size > 0) {
    long written = syscall3(SYS_write, fd, (long)text, size);
    if (written <= 0) {
      exit_group(1);
    }
    text += written;
    size -= written;
  }
}

static void write_text(int fd, const char* text) {
  write_all(fd, text, length(text));
}

static int same(const char* left, const char* right) {
  while (*left != '\0' && *left == *right) {
    left++;
    right++;
  }
  return *left == *right;
}

_Noreturn static void usage(void) {
  write_text(
      2,
      "usage: unit_guest_nostdlib touch PATH | echo WORD... | "
      "orphan FIFO LINK | open FIFO\n");
  exit_group(2);
}

_Noreturn static void touch(const char* path) {
  long fd = syscall4(
      SYS_openat, AT_FDCWD, (long)path, O_WRONLY | O_CREAT | O_TRUNC, 0600);
  if (fd < 0 || syscall1(SYS_close, fd) != 0) {
    exit_group(1);
  }
  exit_group(0);
}

_Noreturn static void echo(int count, char** words) {
  for (int i = 0; i < count; i++) {
    if (i > 0) {
      write_text(1, " ");
    }
    write_text(1, words[i]);
  }
  write_text(1, "\n");
  exit_group(0);
}

// The child's name comes from the basename of the path it execs, so a LINK
// named "cat" makes the orphan a "cat", as a shell's `exec /bin/cat` would.
_Noreturn static void orphan(const char* fifo, const char* link, char** envp) {
  write_text(1, "before-orphan\n");
  long parent = syscall1(SYS_getpid, 0);
  long pid = syscall1(SYS_fork, 0);
  if (pid < 0) {
    exit_group(1);
  }
  if (pid == 0) {
    // Open only once orphaned, as the tests require of the survivor. A
    // shell's `exec /bin/cat FIFO & exit 0` gets there by the dynamic
    // loader's many system calls before cat's openat; a static exec reaches
    // openat at once, and its blocking openat would hold the scheduler's
    // turn from a parent that has not exited yet.
    while (syscall1(SYS_getppid, 0) == parent) {
      syscall1(SYS_sched_yield, 0);
    }
    // Element by element, not an initializer, which could become a memcpy.
    char* argv[4];
    argv[0] = "cat";
    argv[1] = "open";
    argv[2] = (char*)fifo;
    argv[3] = 0;
    syscall3(SYS_execve, (long)link, (long)argv, (long)envp);
    exit_group(127);
  }
  exit_group(0);
}

// openat, not open: the tests' report of the blocked survivor names the
// system call it is in.
_Noreturn static void open_fifo(const char* fifo) {
  long fd = syscall4(SYS_openat, AT_FDCWD, (long)fifo, O_RDONLY, 0);
  exit_group(fd < 0 ? 1 : 0);
}

// The entry from _start below, with the initial stack the kernel built:
// argc, then argv's pointers and a null one, then envp's and a null one.
_Noreturn void unit_guest_main(long* stack) {
  int argc = (int)stack[0];
  char** argv = (char**)(stack + 1);
  char** envp = argv + argc + 1;
  if (argc < 2) {
    usage();
  }
  const char* mode = argv[1];
  if (same(mode, "touch") && argc == 3) {
    touch(argv[2]);
  }
  if (same(mode, "echo") && argc >= 3) {
    echo(argc - 2, argv + 2);
  }
  if (same(mode, "orphan") && argc == 4) {
    orphan(argv[2], argv[3], envp);
  }
  if (same(mode, "open") && argc == 3) {
    open_fifo(argv[2]);
  }
  usage();
}

// The kernel enters with the stack pointer at argc, 16-byte aligned; the call
// leaves the callee the alignment the ABI expects at a function's entry.
asm(".text\n"
    ".global _start\n"
    ".type _start, @function\n"
    "_start:\n"
    "  xor %ebp, %ebp\n"
    "  mov %rsp, %rdi\n"
    "  and $-16, %rsp\n"
    "  call unit_guest_main\n"
    "  hlt\n");
