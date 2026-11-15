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
 * check, and exits 0 only if every ordinary check passed and every
 * known CPUID leak is still present exactly as named.
 *
 * The ordinary checks assert the x86-64-v2 features are present and
 * every x86-64-v3 feature (plus RDSEED, AVX-512F, and masked RDRAND)
 * is absent. Hermit's current v2 table
 * (detcore/src/cpuid.rs, CPUIDS) nevertheless advertises AVX,
 * BMI1, HLE, AVX2, BMI2, RTM, and ADX. That is
 * https://github.com/facebookexperimental/hermit/issues/124, not the
 * designed v2 contract. Those seven are therefore strict
 * expected-failure ("known leak") checks: the probe requires each
 * bit to still be set and reports it as LEAK. If one is fixed, the
 * probe reports FIXED and fails the whole run on purpose, so the
 * expected-failure cannot silently linger after the table is fixed;
 * removing it is part of the issue-124 change. glibc still computes
 * level v2 today because OSXSAVE/XSAVE are withheld, which the
 * loader oracle in cpu_profile_v2_rootfs.rs asserts separately; a
 * guest that trusts leaf 7 alone would still see the leaked bits.
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

/* A feature that must be absent under x86-64-v2 but is currently
 * advertised (facebookexperimental/hermit#124). Still advertised is
 * the expected state until that issue is fixed; absent flips this
 * run to FAIL on purpose so the expectation is removed with the fix. */
static void check_known_leak(const char* what, int advertised) {
  const char* suffix = advertised ? ": LEAK (facebookexperimental/hermit#124)\n"
                                  : ": FIXED (facebookexperimental/hermit#124)\n";
  write_all(what, str_len(what));
  write_all(suffix, str_len(suffix));
  if (!advertised) {
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

  check_known_leak("known leak avx hidden", bit(ecx1, 28));

  leaf(7, 0, r);
  unsigned ebx7 = r[1];
  check_known_leak("known leak bmi1 hidden", bit(ebx7, 3));
  check_known_leak("known leak hle hidden", bit(ebx7, 4));
  check_known_leak("known leak avx2 hidden", bit(ebx7, 5));
  check_known_leak("known leak bmi2 hidden", bit(ebx7, 8));
  check_known_leak("known leak rtm hidden", bit(ebx7, 11));
  check_known_leak("known leak adx hidden", bit(ebx7, 19));
  check("avx512f and rdseed hidden", !bit(ebx7, 16) && !bit(ebx7, 18));

  leaf(0x80000001, 0, r);
  unsigned ecx_ext = r[2];
  check("lahf advertised", bit(ecx_ext, 0));
  check("lzcnt hidden", !bit(ecx_ext, 5));

  long code = failures ? 1 : 0;
  __asm__ volatile("syscall" ::"a"(231), "D"(code));
  __builtin_unreachable();
}
