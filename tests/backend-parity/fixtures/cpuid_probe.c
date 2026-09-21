/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Backend-parity contract: Detcore virtualizes CPUID to one fixed synthetic CPU
 * identity, so leaf 0 reports max=0000000d / "GenuineIntel", leaf 1 reports
 * signature 00000663 with the RDRAND bit (ECX bit 30) clear, and unsupported
 * subleaf 1 of each indexed basic leaf at or below the advertised maximum
 * returns zero in every register, on every host.
 *
 * EMISSION CONTRACT: stdout carries the observed CPUID identity on EVERY path.
 * Previously the values were printed only on the success path, so every failure
 * produced an EMPTY stdout with the detail on stderr -- and the cell's
 * observation excludes stderr
 * (observation = { status = true, stdout = true, stderr = false }). A failing
 * run therefore told an observer nothing at all beyond a status code, which is
 * how this fixture came to be the one EMPTY entry in the emission census.
 *
 * The success line is byte-for-byte unchanged, because two consumers assert it
 * exactly: hermit-cli/tests/cli.rs::run_kvm_cpuid_policy_is_deterministic
 * compiles THIS file and compares stdout literally, and
 * tests/backend-parity/run_matrix.py's "cpuid_policy" row expects the same
 * bytes. Only the previously-silent failure paths gained output.
 */

#include <cpuid.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

int main(void) {
  uint32_t eax;
  uint32_t ebx;
  uint32_t ecx;
  uint32_t edx;
  char vendor[13] = {0};

  __cpuid_count(0, 0, eax, ebx, ecx, edx);
  memcpy(vendor, &ebx, sizeof(ebx));
  memcpy(vendor + 4, &edx, sizeof(edx));
  memcpy(vendor + 8, &ecx, sizeof(ecx));
  const uint32_t max_leaf = eax;

  uint32_t sig_eax;
  uint32_t sig_ecx;
  __cpuid_count(1, 0, sig_eax, ebx, sig_ecx, edx);

  if (max_leaf != UINT32_C(0x0000000d) || strcmp(vendor, "GenuineIntel") != 0) {
    /* stdout, not just stderr: the cell observes stdout and status only. */
    printf("CPUID-MISMATCH max=%08x vendor=%s signature=%08x rdrand=%u\n",
           max_leaf, vendor, sig_eax, (unsigned)((sig_ecx >> 30) & 1u));
    fprintf(stderr, "unexpected CPUID identity: max=%08x vendor=%s\n", max_leaf,
            vendor);
    return 1;
  }

  if (sig_eax != UINT32_C(0x00000663) || (sig_ecx & (UINT32_C(1) << 30)) != 0) {
    printf("CPUID-MISMATCH max=%08x vendor=%s signature=%08x rdrand=%u\n",
           max_leaf, vendor, sig_eax, (unsigned)((sig_ecx >> 30) & 1u));
    fprintf(stderr, "unexpected CPUID leaf 1: eax=%08x ecx=%08x\n", sig_eax,
            sig_ecx);
    return 2;
  }

  const uint32_t indexed_leaves[] = {UINT32_C(0x04), UINT32_C(0x07),
                                     UINT32_C(0x0b), UINT32_C(0x0d)};
  for (unsigned int i = 0;
       i < sizeof(indexed_leaves) / sizeof(indexed_leaves[0]); ++i) {
    uint32_t subleaf_eax;
    uint32_t subleaf_ebx;
    uint32_t subleaf_ecx;
    uint32_t subleaf_edx;
    __cpuid_count(indexed_leaves[i], 1, subleaf_eax, subleaf_ebx, subleaf_ecx,
                  subleaf_edx);
    if (subleaf_eax != 0 || subleaf_ebx != 0 || subleaf_ecx != 0 ||
        subleaf_edx != 0) {
      printf("CPUID-MISMATCH max=%08x vendor=%s signature=%08x rdrand=%u\n",
             max_leaf, vendor, sig_eax, (unsigned)((sig_ecx >> 30) & 1u));
      fprintf(stderr,
              "unexpected CPUID leaf %x subleaf 1: eax=%08x ebx=%08x "
              "ecx=%08x edx=%08x\n",
              indexed_leaves[i], subleaf_eax, subleaf_ebx, subleaf_ecx,
              subleaf_edx);
      return 3;
    }
  }

  /* Success line held byte-identical for cli.rs and run_matrix.py. */
  printf("CPUID-SUCCESS vendor=%s signature=%08x\n", vendor, sig_eax);
  return 0;
}
