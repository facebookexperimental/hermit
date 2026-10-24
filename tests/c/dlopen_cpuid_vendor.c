/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Executes CPUID leaf 0 in the main executable, then dlopens the shared library
 * named by argv[1] (tests/c/dlopen_cpuid_vendor_lib.c), whose constructor
 * executes CPUID leaf 0 in code mapped only now. Prints both vendor strings,
 * one per line, and exits 0; any loader failure exits nonzero.
 */

#include <cpuid.h>
#include <dlfcn.h>
#include <stdio.h>
#include <string.h>

int main(int argc, char** argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s LIBRARY\n", argv[0]);
    return 2;
  }
  unsigned int eax = 0, ebx = 0, ecx = 0, edx = 0;
  __cpuid(0, eax, ebx, ecx, edx);
  (void)eax;
  char vendor[13];
  memcpy(vendor, &ebx, 4);
  memcpy(vendor + 4, &edx, 4);
  memcpy(vendor + 8, &ecx, 4);
  vendor[12] = '\0';
  printf("main-vendor=%s\n", vendor);
  fflush(stdout);

  void* library = dlopen(argv[1], RTLD_NOW);
  if (library == NULL) {
    fprintf(stderr, "dlopen: %s\n", dlerror());
    return 3;
  }
  const char* (*library_vendor)(void) =
      (const char* (*)(void))dlsym(library, "dlopen_cpuid_vendor");
  if (library_vendor == NULL) {
    fprintf(stderr, "dlsym: %s\n", dlerror());
    return 4;
  }
  printf("library-vendor=%s\n", library_vendor());
  return 0;
}
