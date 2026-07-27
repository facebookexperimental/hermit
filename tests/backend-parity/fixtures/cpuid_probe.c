/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
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
  if (eax != UINT32_C(0x0000000d) || strcmp(vendor, "GenuineIntel") != 0) {
    fprintf(stderr, "unexpected CPUID identity: max=%08x vendor=%s\n", eax, vendor);
    return 1;
  }

  __cpuid_count(1, 0, eax, ebx, ecx, edx);
  if (eax != UINT32_C(0x00000663) || (ecx & (UINT32_C(1) << 30)) != 0) {
    fprintf(stderr, "unexpected CPUID leaf 1: eax=%08x ecx=%08x\n", eax, ecx);
    return 2;
  }

  printf("CPUID-SUCCESS vendor=%s signature=%08x\n", vendor, eax);
  return 0;
}
