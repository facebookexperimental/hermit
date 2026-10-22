/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A shared library whose constructor executes CPUID leaf 0 and keeps the
 * vendor string. tests/c/dlopen_cpuid_vendor.c loads it with dlopen
 * after the program has started, so its code is mapped after a preloaded
 * runtime finished its own start-up: the case Python's _hashlib extension
 * reaches when it loads libcrypto.
 */

#include <cpuid.h>
#include <string.h>

static char vendor[13];

__attribute__((constructor)) static void probe_vendor(void) {
  unsigned int eax = 0, ebx = 0, ecx = 0, edx = 0;
  __cpuid(0, eax, ebx, ecx, edx);
  (void)eax;
  memcpy(vendor, &ebx, 4);
  memcpy(vendor + 4, &edx, 4);
  memcpy(vendor + 8, &ecx, 4);
  vendor[12] = '\0';
}

const char* dlopen_cpuid_vendor(void) {
  return vendor;
}
