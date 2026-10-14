/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// What a guest sees of Hermit's x86-64-v3 CPU profile
// (https://github.com/rrnewton/hermit/issues/4243).
//
// It prints only host-independent lines: each check's verdict, never a host
// value. Under the default x86-64-v2 profile it prints "v2" and the v2 checks;
// under v3 it also executes one instruction of each new feature, compares
// XGETBV with leaf 0xD, writes an XSAVE area of exactly the advertised size
// between guard bytes, and compares Linux's xstate queries with leaf 0xD.

#include <cpuid.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#define ARCH_GET_XCOMP_SUPP 0x1021
#define ARCH_GET_XCOMP_PERM 0x1022
#define ARCH_REQ_XCOMP_PERM 0x1023
#define ARCH_GET_XCOMP_GUEST_PERM 0x1024

static int failures;

static void check(const char* what, int ok) {
  printf("%s: %s\n", what, ok ? "ok" : "FAIL");
  if (!ok) {
    failures++;
  }
}

static void leaf(unsigned l, unsigned s, unsigned r[4]) {
  __cpuid_count(l, s, r[0], r[1], r[2], r[3]);
}

static uint64_t xgetbv0(void) {
  uint32_t lo, hi;
  __asm__ volatile("xgetbv" : "=a"(lo), "=d"(hi) : "c"(0));
  return ((uint64_t)hi << 32) | lo;
}

// One instruction of each v3 feature, with fixed operands and results.
static int fma_ok(void) {
  double a = 2.0, b = 3.0, c = 1.0;
  __asm__ volatile("vfmadd213sd %2, %1, %0" : "+x"(a) : "x"(b), "x"(c));
  return a == 7.0;
}

static int movbe_ok(void) {
  uint32_t in = 0x11223344, out;
  __asm__ volatile("movbe %1, %0" : "=r"(out) : "m"(in));
  return out == 0x44332211;
}

static int lzcnt_ok(void) {
  uint32_t out;
  __asm__ volatile("lzcnt %1, %0" : "=r"(out) : "r"(0x00010000u));
  return out == 15;
}

static int bmi2_ok(void) {
  uint64_t out;
  __asm__ volatile("pdep %2, %1, %0"
                   : "=r"(out)
                   : "r"(0x5ull), "r"(0xf0ull));
  return out == 0x50;
}

static int f16c_ok(void) {
  // 1.0 as binary16 is 0x3c00; convert it to single precision.
  float out[4];
  __asm__ volatile(
      "vmovd %1, %%xmm1\n"
      "vcvtph2ps %%xmm1, %%xmm0\n"
      "vmovups %%xmm0, %0"
      : "=m"(out)
      : "r"(0x3c00u)
      : "xmm0", "xmm1");
  return out[0] == 1.0f;
}

static long arch_prctl_u64(int cmd, uint64_t* out) {
  return syscall(SYS_arch_prctl, cmd, out);
}

int main(void) {
  unsigned r[4];
  leaf(1, 0, r);
  unsigned ecx1 = r[2];
  int v3 = (ecx1 >> 27) & 1; // OSXSAVE: the profile's XSAVE support
  printf("%s\n", v3 ? "v3" : "v2");

  leaf(7, 0, r);
  unsigned ebx7 = r[1];
  check("rdrand and rdseed masked", !((ecx1 >> 30) & 1) && !((ebx7 >> 18) & 1));
  check("avx512 hidden", !((ebx7 >> 16) & 1));

  if (!v3) {
    return failures ? 1 : 0;
  }

  check(
      "fma movbe xsave osxsave avx f16c advertised",
      ((ecx1 >> 12) & 1) && ((ecx1 >> 22) & 1) && ((ecx1 >> 26) & 1) &&
          ((ecx1 >> 28) & 1) && ((ecx1 >> 29) & 1));
  check("hypervisor bit clear", !((ecx1 >> 31) & 1));
  check(
      "avx2 bmi1 bmi2 advertised",
      ((ebx7 >> 5) & 1) && ((ebx7 >> 3) & 1) && ((ebx7 >> 8) & 1));
  check("hle and rtm clear", !((ebx7 >> 4) & 1) && !((ebx7 >> 11) & 1));
  leaf(0x80000001, 0, r);
  check("lzcnt advertised", (r[2] >> 5) & 1);

  check("fma executes", fma_ok());
  check("movbe executes", movbe_ok());
  check("lzcnt executes", lzcnt_ok());
  check("bmi2 executes", bmi2_ok());
  check("f16c executes", f16c_ok());

  unsigned d0[4], d1[4];
  leaf(0xd, 0, d0);
  leaf(0xd, 1, d1);
  uint64_t xcr0 = xgetbv0();
  check("xgetbv matches leaf 0xd", xcr0 == (((uint64_t)d0[3] << 32) | d0[0]));
  check("leaf 0xd.0 ecx equals ebx", d0[1] == d0[2]);
  check(
      "leaf 0xd.1 all zero", d1[0] == 0 && d1[1] == 0 && d1[2] == 0 && d1[3] == 0);

  // An XSAVE area of exactly leaf 0xD.0 EBX bytes, between guard bytes.
  size_t size = d0[1];
  unsigned char* block = aligned_alloc(64, size + 128);
  memset(block, 0xa5, size + 128);
  unsigned char* area = block + 64;
  memset(area, 0, size);
  __asm__ volatile("xsave (%0)"
                   :
                   : "r"(area), "a"((uint32_t)xcr0), "d"((uint32_t)(xcr0 >> 32))
                   : "memory");
  int guards = 1;
  for (int i = 0; i < 64; i++) {
    guards &= block[i] == 0xa5 && block[64 + size + i] == 0xa5;
  }
  check("xsave stays within leaf 0xd.0 ebx bytes", guards);
  free(block);

  uint64_t supp = 0, perm = 0, guest = 0;
  check(
      "xcomp queries answer",
      arch_prctl_u64(ARCH_GET_XCOMP_SUPP, &supp) == 0 &&
          arch_prctl_u64(ARCH_GET_XCOMP_PERM, &perm) == 0 &&
          arch_prctl_u64(ARCH_GET_XCOMP_GUEST_PERM, &guest) == 0);
  check("xcomp perm covers xcr0", (perm & xcr0) == xcr0);
  long req = syscall(SYS_arch_prctl, ARCH_REQ_XCOMP_PERM, 63);
  check("xcomp request beyond the last feature is refused", req == -1);

  return failures ? 1 : 0;
}
