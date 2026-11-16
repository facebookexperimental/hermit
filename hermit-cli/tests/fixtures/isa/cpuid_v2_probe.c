/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * What a guest sees of Hermit's x86-64-v2 CPU profile, for the pinned
 * CentOS 9 rootfs test (cpu_profile_v2_rootfs.rs). It is the v2
 * counterpart of tests/c/cpuid_profile_v3.c, but links no libc symbols:
 * it is built -nostdlib as a dynamic PIE naming the fixture loader, with
 * a DT_NEEDED on libc.so.6 (--no-as-needed), so the fixture's real ld.so
 * loads and initializes the fixture libc before this entry point runs,
 * while the binary carries no host-libc symbol versions and can be
 * built on any host.
 *
 * It prints only host-independent lines: "v2", then one verdict per
 * check, and exits 0 only if every check passed: the x86-64-v2
 * features present, and every above-v2 feature absent — AVX, BMI1,
 * HLE, AVX2, BMI2, RTM and ADX included. Those seven were
 * advertised by the pre-psABI table until
 * https://github.com/facebookexperimental/hermit/issues/124 was
 * fixed; they are ordinary assertions now, so any of them
 * reappearing turns this probe red.
 */

static void write_all(const char* s, unsigned long len) {
  long result;
  __asm__ volatile("syscall"
                   : "=a"(result)
                   : "a"(1), "D"(1), "S"(s), "d"(len)
                   : "rcx", "r11", "memory");
  (void)result;
}

static unsigned long str_len(const char* s) {
  unsigned long n = 0;
  while (s[n] != '\0') {
    n++;
  }
  return n;
}

static int failures;

static void check(const char* what, int ok) {
  write_all(what, str_len(what));
  write_all(ok ? ": ok\n" : ": FAIL\n", ok ? 5 : 7);
  if (!ok) {
    failures++;
  }
}

static void leaf(unsigned l, unsigned s, unsigned r[4]) {
  __asm__ volatile("cpuid"
                   : "=a"(r[0]), "=b"(r[1]), "=c"(r[2]), "=d"(r[3])
                   : "a"(l), "c"(s));
}

static int bit(unsigned word, int b) { return (int)((word >> b) & 1u); }

void _start(void) {
  unsigned r[4];
  write_all("v2\n", 3);

  leaf(1, 0, r);
  unsigned ecx1 = r[2];
  unsigned edx1 = r[3];
  /* The x86-64-v2 additions over baseline (psABI): SSE3, SSSE3,
     CMPXCHG16B, SSE4.1, SSE4.2, POPCNT; plus SSE2 from baseline. */
  check("sse2 advertised", bit(edx1, 26));
  check(
      "v2 features advertised",
      bit(ecx1, 0) && bit(ecx1, 9) && bit(ecx1, 13) && bit(ecx1, 19) &&
          bit(ecx1, 20) && bit(ecx1, 23));
  /* The v3 gates in leaf 1: FMA, MOVBE, XSAVE, OSXSAVE, F16C. RDRAND is
     masked under every named profile. */
  check(
      "v3 leaf1 features hidden",
      !bit(ecx1, 12) && !bit(ecx1, 22) && !bit(ecx1, 26) &&
          !bit(ecx1, 27) && !bit(ecx1, 29));
  check("rdrand masked", !bit(ecx1, 30));

  check("avx hidden", !bit(ecx1, 28));

  leaf(7, 0, r);
  unsigned ebx7 = r[1];
  check("bmi1 hidden", !bit(ebx7, 3));
  check("hle hidden", !bit(ebx7, 4));
  check("avx2 hidden", !bit(ebx7, 5));
  check("bmi2 hidden", !bit(ebx7, 8));
  check("rtm hidden", !bit(ebx7, 11));
  check("adx hidden", !bit(ebx7, 19));
  check("avx512f and rdseed hidden", !bit(ebx7, 16) && !bit(ebx7, 18));

  leaf(0x80000001, 0, r);
  unsigned ecx_ext = r[2];
  check("lahf advertised", bit(ecx_ext, 0));
  check("lzcnt hidden", !bit(ecx_ext, 5));

  long code = failures ? 1 : 0;
  __asm__ volatile("syscall" ::"a"(231), "D"(code));
  __builtin_unreachable();
}
