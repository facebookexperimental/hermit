/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Prints every CPUID row the guest can ask for, so two backends that share
// Detcore's machine description print the same bytes. Each basic leaf up to the
// advertised maximum is printed for subleaves 0 to 2, and each extended leaf up
// to its maximum for subleaf 0. The last lines say whether /proc/cpuinfo lists
// the hle and rtm flags exactly when CPUID leaf 7 advertises them, the two
// bits DBT's own table used to clear.

#include <cpuid.h>
#include <stdio.h>
#include <string.h>

static void row(unsigned leaf, unsigned subleaf) {
  unsigned eax, ebx, ecx, edx;
  __cpuid_count(leaf, subleaf, eax, ebx, ecx, edx);
  printf(
      "%08x.%u: %08x %08x %08x %08x\n", leaf, subleaf, eax, ebx, ecx, edx);
}

// Whether the first "flags" line of /proc/cpuinfo names `flag`; -1 when the
// file or the line cannot be read.
static int cpuinfo_has_flag(const char* flag) {
  FILE* file = fopen("/proc/cpuinfo", "r");
  if (!file) {
    return -1;
  }
  static char line[16384];
  int found = -1;
  while (fgets(line, sizeof(line), file)) {
    if (strncmp(line, "flags", 5) != 0) {
      continue;
    }
    found = 0;
    size_t flag_len = strlen(flag);
    for (const char* at = line; (at = strstr(at, flag)) != NULL; at++) {
      if (at[-1] == ' ' &&
          (at[flag_len] == ' ' || at[flag_len] == '\n' ||
           at[flag_len] == '\0')) {
        found = 1;
        break;
      }
    }
    break;
  }
  fclose(file);
  return found;
}

int main(void) {
  unsigned max_basic, max_extended, ebx, ecx, edx;
  __cpuid_count(0, 0, max_basic, ebx, ecx, edx);
  for (unsigned leaf = 0; leaf <= max_basic; leaf++) {
    for (unsigned subleaf = 0; subleaf < 3; subleaf++) {
      row(leaf, subleaf);
    }
  }
  __cpuid_count(0x80000000u, 0, max_extended, ebx, ecx, edx);
  for (unsigned leaf = 0x80000000u; leaf <= max_extended; leaf++) {
    row(leaf, 0);
  }

  unsigned leaf7_ebx = 0;
  if (max_basic >= 7) {
    unsigned eax;
    __cpuid_count(7, 0, eax, leaf7_ebx, ecx, edx);
  }
  const struct {
    const char* name;
    int bit;
  } flags[] = {{"hle", 4}, {"rtm", 11}};
  for (size_t i = 0; i < sizeof(flags) / sizeof(flags[0]); i++) {
    int in_cpuid = (leaf7_ebx >> flags[i].bit) & 1;
    int in_cpuinfo = cpuinfo_has_flag(flags[i].name);
    printf(
        "flag %s: cpuid=%d cpuinfo=%d %s\n",
        flags[i].name,
        in_cpuid,
        in_cpuinfo,
        in_cpuinfo == in_cpuid ? "match" : "MISMATCH");
  }
  return 0;
}
