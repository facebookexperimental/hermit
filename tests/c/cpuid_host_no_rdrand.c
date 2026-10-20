/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Native rows are the oracle for host-no-rdrand. The Rust test applies the
// documented masks to them and compares every other bit with the guest rows.
// Guest mode also checks the hardware-randomness bits, RDTSCP's virtual AUX,
// and the native XSAVE layout used by the dynamic loader.

#include <cpuid.h>
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static int failures;

// A native-only availability oracle, independent of Hermit's admission code.
// EINVAL is the old-kernel unknown-command answer; ENODEV means the hardware
// lacks CPUID faulting. Other errors are failures, not unsupported-host skips.
static int faulting_probe(void) {
  const int get_cpuid = 0x1011, set_cpuid = 0x1012;
  long initial = syscall(SYS_arch_prctl, get_cpuid, 0);
  if (initial < 0) {
    if (errno == ENODEV || errno == EINVAL) {
      puts("faulting unavailable");
      return 77;
    }
    perror("ARCH_GET_CPUID");
    return 2;
  }
  if (initial != 0 && initial != 1) {
    fprintf(stderr, "invalid ARCH_GET_CPUID state: %ld\n", initial);
    return 2;
  }
  if (syscall(SYS_arch_prctl, set_cpuid, 0) < 0) {
    if (errno == ENODEV || errno == EINVAL) {
      puts("faulting unavailable");
      return 77;
    }
    perror("ARCH_SET_CPUID disable");
    return 2;
  }
  long disabled = syscall(SYS_arch_prctl, get_cpuid, 0);
  // Restore before any library operation other than the already-resolved
  // syscall wrapper: libc itself may execute CPUID during feature detection.
  if (syscall(SYS_arch_prctl, set_cpuid, initial) < 0) {
    _exit(2);
  }
  if (disabled != 0) {
    fprintf(stderr, "ARCH_SET_CPUID did not disable CPUID: %ld\n", disabled);
    return 2;
  }
  puts("faulting available");
  return 0;
}

static void check(const char* name, int ok) {
  printf("check %s: %s\n", name, ok ? "ok" : "FAIL");
  failures += !ok;
}

static void row(unsigned leaf, unsigned subleaf) {
  unsigned a, b, c, d;
  __cpuid_count(leaf, subleaf, a, b, c, d);
  printf("row %08x %08x %08x %08x %08x %08x\n", leaf, subleaf, a, b, c, d);
}

static void rows(unsigned first, unsigned last) {
  for (unsigned leaf = first; leaf <= last; ++leaf) {
    unsigned subleaves = leaf == 0xd ? 64 : (first == 0 ? 3 : 1);
    for (unsigned subleaf = 0; subleaf < subleaves; ++subleaf) {
      row(leaf, subleaf);
    }
  }
}

static void xcomp_queries(void) {
  const unsigned queries[] = {0x1021, 0x1022, 0x1024};
  for (unsigned i = 0; i < sizeof(queries) / sizeof(queries[0]); ++i) {
    unsigned long long mask = 0;
    errno = 0;
    long result = syscall(SYS_arch_prctl, queries[i], &mask);
    printf("xcomp %x %ld %d %016llx\n", queries[i], result, errno, mask);
  }
}

static void xsave_checks(unsigned leaf1_ecx) {
  if (!(leaf1_ecx & (1u << 27))) {
    return; // The host OS does not enable XSAVE.
  }
  unsigned a, b, c, d, lo, hi;
  __asm__ volatile("xgetbv" : "=a"(lo), "=d"(hi) : "c"(0));
  uint64_t xcr0 = ((uint64_t)hi << 32) | lo;
  __cpuid_count(0xd, 0, a, b, c, d);
  uint64_t supported = ((uint64_t)d << 32) | a;
  check("xcr0 covered by leaf 0xd", (xcr0 & supported) == xcr0);
  check("xsave enabled size fits supported size", b >= 576 && b <= c);
  if (b < 576 || b > c) {
    return;
  }
  size_t size = b;
  size_t allocated = (size + 128 + 63) & ~(size_t)63;
  unsigned char* block = aligned_alloc(64, allocated);
  if (!block) {
    check("xsave allocation", 0);
    return;
  }
  memset(block, 0xa5, allocated);
  unsigned char* area = block + 64;
  memset(area, 0, size);
  __asm__ volatile("xsave (%0)"
                   :
                   : "r"(area), "a"((uint32_t)xcr0), "d"((uint32_t)(xcr0 >> 32))
                   : "memory");
  int guards = 1;
  for (unsigned i = 0; i < 64; ++i) {
    guards &= block[i] == 0xa5 && block[64 + size + i] == 0xa5;
  }
  check("xsave stays inside advertised size", guards);
  free(block);
}

int main(int argc, char** argv) {
  if (argc == 2 && strcmp(argv[1], "--faulting-probe") == 0) {
    return faulting_probe();
  }
  int native = argc == 2 && strcmp(argv[1], "--native") == 0;
  int inject_rdrand = argc == 2 && strcmp(argv[1], "--inject-rdrand") == 0;
  if (argc != 1 && !native && !inject_rdrand) {
    return 2;
  }
  unsigned a, b, c, d, max_basic, max_extended;
  __cpuid_count(0, 0, max_basic, b, c, d);
  __cpuid_count(0x80000000u, 0, max_extended, b, c, d);
  // Bound the dump even if a broken CPUID emulator returns an invalid maximum.
  if (max_basic > 0x100 || max_extended < 0x80000000u ||
      max_extended > 0x80000100u) {
    fprintf(stderr, "invalid CPUID maximum\n");
    return 2;
  }
  rows(0, max_basic);
  row(max_basic + 1, 0); // Hosts may alias an unsupported leaf to another row.
  rows(0x40000000u, 0x40000005u); // Hypervisor timing interfaces must be hidden.
  rows(0x80000000u, max_extended);
  row(max_extended + 1, 0);
  __cpuid_count(0xc0000000u, 0, a, b, c, d);
  if (a >= 0xc0000001u && a <= 0xc0000100u) {
    rows(0xc0000000u, a);
  }
  xcomp_queries();
  if (native) {
    return 0;
  }

  __cpuid_count(1, 0, a, b, c, d);
  if (inject_rdrand) {
    c |= 1u << 30; // Mutation arm: the oracle must reject an advertised RDRAND.
  }
  check("rdrand hidden", !(c & (1u << 30)));
  xsave_checks(c);
  if (max_basic >= 7) {
    __cpuid_count(7, 0, a, b, c, d);
    check("rdseed hidden", !(b & (1u << 18)));
    check("rdpid hidden", !(c & (1u << 22)));
  }
  if (max_extended >= 0x80000001u) {
    __cpuid_count(0x80000001u, 0, a, b, c, d);
    if (d & (1u << 27)) {
      unsigned aux;
      __asm__ volatile("rdtscp" : "=a"(a), "=d"(d), "=c"(aux));
      check("rdtscp aux is virtual cpu zero", aux == 0);
    }
  }
  return failures ? 1 : 0;
}
