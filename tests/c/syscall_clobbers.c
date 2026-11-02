/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// The x86-64 `syscall` instruction loads %rcx with the address of the next
// instruction and %r11 with RFLAGS, and Linux's return (SYSRET, or IRET with the
// same values) leaves them there. A guest that reads them after a syscall must
// see those values whichever way the backend serviced the call: emulated by the
// tool, run by the kernel, or replaced by an injected call. This program issues
// raw syscalls of each kind and checks both registers after every one.

#include <stdint.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

struct observed {
  long ret;
  uint64_t rcx;
  uint64_t r11;
  uint64_t next;
  uint64_t flags;
};

// Issues syscall `nr` and returns the registers it left. The RFLAGS sample is
// taken below the red zone, since this function's locals may live there, and
// `lea` moves the stack pointer because it leaves the flags alone.
__attribute__((noinline)) static struct observed
raw_syscall(long nr, long a0, long a1, long a2) {
  struct observed out;
  register long rax __asm__("rax") = nr;
  register long rdi __asm__("rdi") = a0;
  register long rsi __asm__("rsi") = a1;
  register long rdx __asm__("rdx") = a2;
  register uint64_t rcx __asm__("rcx");
  register uint64_t r11 __asm__("r11");
  uint64_t flags;
  uint64_t next;
  __asm__ volatile(
      "leaq -128(%%rsp), %%rsp\n\t"
      "pushfq\n\t"
      "popq %[flags]\n\t"
      "leaq 128(%%rsp), %%rsp\n\t"
      "syscall\n"
      "1:\n\t"
      "leaq 1b(%%rip), %[next]\n\t"
      : "+r"(rax),
        "=r"(rcx),
        "=r"(r11),
        [flags] "=&r"(flags),
        [next] "=r"(next)
      : "r"(rdi), "r"(rsi), "r"(rdx)
      : "memory", "cc");
  out.ret = rax;
  out.rcx = rcx;
  out.r11 = r11;
  out.next = next;
  out.flags = flags;
  return out;
}

static int check(const char* name, long nr, long a0, long a1, long a2) {
  struct observed o = raw_syscall(nr, a0, a1, a2);
  int rcx_ok = o.rcx == o.next;
  int r11_ok = o.r11 == o.flags;
  printf(
      "%s: rcx %s, r11 %s\n",
      name,
      rcx_ok ? "ok" : "WRONG",
      r11_ok ? "ok" : "WRONG");
  return rcx_ok && r11_ok;
}

int main(void) {
  struct timespec ts;
  struct timespec nap = {0, 1000};
  char buf[512]; /* a struct utsname is 390 bytes */
  int all = 1;

  all &= check("getpid", SYS_getpid, 0, 0, 0);
  all &= check("getppid", SYS_getppid, 0, 0, 0);
  all &= check("gettid", SYS_gettid, 0, 0, 0);
  all &= check("getuid", SYS_getuid, 0, 0, 0);
  all &= check("sched_yield", SYS_sched_yield, 0, 0, 0);
  all &= check("uname", SYS_uname, (long)buf, 0, 0);
  all &= check("getcwd", SYS_getcwd, (long)buf, sizeof buf, 0);
  all &= check(
      "clock_gettime", SYS_clock_gettime, CLOCK_MONOTONIC, (long)&ts, 0);
  all &= check("getrandom", SYS_getrandom, (long)buf, 16, 0);
  all &= check("brk", SYS_brk, 0, 0, 0);
  all &= check("write-empty", SYS_write, 1, (long)buf, 0);
  all &= check("close-ebadf", SYS_close, -1, 0, 0);
  all &= check("nanosleep", SYS_nanosleep, (long)&nap, 0, 0);
  fflush(stdout);
  puts(all ? "clobbers-ok" : "clobbers-WRONG");
  return all ? 0 : 1;
}
