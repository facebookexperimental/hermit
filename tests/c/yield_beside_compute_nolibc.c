/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A static guest with no libc and no dynamic loader (x86_64): the main task
// makes a second task with a raw clone(2), then calls sched_yield, runs a
// short computation and reads CLOCK_MONOTONIC by a raw syscall, YIELDS
// times, while the second task computes. Each task prints a digest of what
// it saw; the main task's digest is of its own clock reads.
//
//   cc -O1 -static -nostdlib -fno-stack-protector -fno-pie -no-pie
//      -o yield_beside_compute_nolibc yield_beside_compute_nolibc.c
//
// argv[1]: yields (default 200); argv[2]: the main task's work between a
// yield and its clock read, in loop iterations (default 300); argv[3]: the
// second task's work, in loop iterations (default 3000000).

#define SYS_write 1
#define SYS_sched_yield 24
#define SYS_clone 56
#define SYS_exit 60
#define SYS_clock_gettime 228
#define SYS_exit_group 231

#define CLONE_THREAD_FLAGS 0x50f00 /* VM|FS|FILES|SIGHAND|THREAD|SYSVSEM */

static long sys0(long n) {
  long ret;
  __asm__ volatile("syscall" : "=a"(ret) : "a"(n) : "rcx", "r11", "memory");
  return ret;
}

static long sys1(long n, long a) {
  long ret;
  __asm__ volatile("syscall" : "=a"(ret) : "a"(n), "D"(a) : "rcx", "r11", "memory");
  return ret;
}

static long sys3(long n, long a, long b, long c) {
  long ret;
  __asm__ volatile("syscall"
                   : "=a"(ret)
                   : "a"(n), "D"(a), "S"(b), "d"(c)
                   : "rcx", "r11", "memory");
  return ret;
}

static char stack[1 << 16] __attribute__((aligned(16)));
static volatile int done;
static volatile unsigned long sink;
static long second_work = 3000000;

static void write_str(const char* s) {
  long n = 0;
  while (s[n]) {
    n++;
  }
  sys3(SYS_write, 1, (long)s, n);
}

static void write_u64(const char* label, unsigned long v) {
  char buf[32];
  int i = sizeof buf;
  buf[--i] = '\n';
  do {
    buf[--i] = (char)('0' + v % 10);
    v /= 10;
  } while (v);
  write_str(label);
  sys3(SYS_write, 1, (long)(buf + i), (long)(sizeof buf - i));
}

static unsigned long work(long iterations, unsigned long x) {
  for (long i = 0; i < iterations; i++) {
    x = x * 6364136223846793005UL + 1442695040888963407UL;
    if (x & 0x100) {
      sink++;
    }
  }
  return x;
}

static void second_main(void) {
  unsigned long x = work(second_work, 0x9e3779b97f4a7c15UL);
  write_u64("second=", x);
  done = 1;
  sys1(SYS_exit, 0);
  for (;;) {
  }
}

// clone(flags, stack_top, NULL, NULL, 0): the child returns on the new stack
// and calls `fn` (held in rbx, which the syscall preserves); it never
// returns.
static long spawn(void (*fn)(void), void* stack_top) {
  long ret;
  register long r10 __asm__("r10") = 0;
  register long r8 __asm__("r8") = 0;
  __asm__ volatile(
      "syscall\n\t"
      "test %%rax, %%rax\n\t"
      "jnz 1f\n\t"
      "xor %%ebp, %%ebp\n\t"
      "call *%%rbx\n\t"
      "hlt\n\t"
      "1:\n\t"
      : "=a"(ret)
      : "a"(SYS_clone), "D"(CLONE_THREAD_FLAGS), "S"(stack_top), "d"(0L),
        "r"(r10), "r"(r8), "b"(fn)
      : "rcx", "r11", "memory");
  return ret;
}

static long parse(const char* s, long dflt) {
  if (!s) {
    return dflt;
  }
  long v = 0;
  while (*s >= '0' && *s <= '9') {
    v = v * 10 + (*s++ - '0');
  }
  return v;
}

struct timespec_raw {
  long sec;
  long nsec;
};

__attribute__((used)) void start_c(long* sp) {
  long argc = sp[0];
  char** argv = (char**)(sp + 1);
  long yields = parse(argc > 1 ? argv[1] : 0, 200);
  long main_work = parse(argc > 2 ? argv[2] : 0, 300);
  second_work = parse(argc > 3 ? argv[3] : 0, 3000000);
  if (spawn(second_main, stack + sizeof stack) < 0) {
    write_str("clone failed\n");
    sys1(SYS_exit_group, 1);
  }
  unsigned long digest = 0, x = 1;
  struct timespec_raw t = {0, 0};
  for (long i = 0; i < yields; i++) {
    sys0(SYS_sched_yield);
    x = work(main_work, x);
    sys3(SYS_clock_gettime, 1 /* CLOCK_MONOTONIC */, (long)&t, 0);
    digest = digest * 31 + (unsigned long)t.sec * 1000000000UL + (unsigned long)t.nsec;
  }
  while (!done) {
    sys0(SYS_sched_yield);
  }
  write_u64("main=", digest);
  write_u64("last_ns=", (unsigned long)t.sec * 1000000000UL + (unsigned long)t.nsec);
  sys1(SYS_exit_group, 0);
}

__asm__(
    ".globl _start\n"
    "_start:\n"
    "  xor %rbp, %rbp\n"
    "  mov %rsp, %rdi\n"
    "  and $-16, %rsp\n"
    "  call start_c\n"
    "  hlt\n");
