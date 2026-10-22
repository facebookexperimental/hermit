/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// /proc/cpuinfo and the CPUID instruction must describe the same CPU
// (https://github.com/rrnewton/hermit/issues/4161). Linux keeps them in step on
// real hardware, so this prints the same lines natively and under Hermit,
// whatever the host CPU is: only whether each value matches, never the value.

#include <cpuid.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static char block[16384];

// The value of the first-CPU line named `key`, or NULL.
static const char* field(const char* key) {
  static char value[8192];
  size_t key_len = strlen(key);
  for (const char* line = block; *line;) {
    const char* end = strchr(line, '\n');
    size_t len = end ? (size_t)(end - line) : strlen(line);
    if (len > key_len && strncmp(line, key, key_len) == 0 &&
        (line[key_len] == '\t' || line[key_len] == ' ' ||
         line[key_len] == ':')) {
      const char* colon = memchr(line, ':', len);
      if (!colon) {
        return NULL;
      }
      const char* start = colon + 1;
      while (*start == ' ') {
        start++;
      }
      size_t value_len = len - (size_t)(start - line);
      memcpy(value, start, value_len);
      value[value_len] = '\0';
      return value;
    }
    line += len + (end ? 1 : 0);
  }
  return NULL;
}

static int has_flag(const char* flags, const char* name) {
  size_t name_len = strlen(name);
  for (const char* at = flags; (at = strstr(at, name)) != NULL; at++) {
    if ((at == flags || at[-1] == ' ') &&
        (at[name_len] == ' ' || at[name_len] == '\0')) {
      return 1;
    }
  }
  return 0;
}

static void report(const char* what, int matches) {
  printf("%s: %s\n", what, matches ? "match" : "MISMATCH");
}

int main(void) {
  FILE* file = fopen("/proc/cpuinfo", "r");
  if (!file) {
    perror("open /proc/cpuinfo");
    return 1;
  }
  size_t used = 0;
  char line[8192];
  while (fgets(line, sizeof(line), file) && strcmp(line, "\n") != 0) {
    size_t len = strlen(line);
    if (used + len + 1 > sizeof(block)) {
      fprintf(stderr, "first cpuinfo block too large\n");
      return 1;
    }
    memcpy(block + used, line, len);
    used += len;
  }
  block[used] = '\0';
  fclose(file);

  unsigned eax, ebx, ecx, edx;
  __cpuid(0, eax, ebx, ecx, edx);
  unsigned max_leaf = eax;
  char vendor[13];
  memcpy(vendor, &ebx, 4);
  memcpy(vendor + 4, &edx, 4);
  memcpy(vendor + 8, &ecx, 4);
  vendor[12] = '\0';
  const char* value = field("vendor_id");
  report("vendor_id", value && strcmp(value, vendor) == 0);

  __cpuid(1, eax, ebx, ecx, edx);
  unsigned leaf1_ecx = ecx, leaf1_edx = edx;
  unsigned family = (eax >> 8) & 0xf;
  unsigned model = (eax >> 4) & 0xf;
  if (family == 0xf) {
    family += (eax >> 20) & 0xff;
  }
  if (family >= 6) {
    model |= ((eax >> 16) & 0xf) << 4;
  }
  value = field("cpu family");
  report("cpu family", value && strtoul(value, NULL, 10) == family);
  value = field("model");
  report("model", value && strtoul(value, NULL, 10) == model);
  value = field("stepping");
  report("stepping", value && strtoul(value, NULL, 10) == (eax & 0xf));

  unsigned leaf7_ebx = 0, leaf7_ecx = 0;
  if (max_leaf >= 7) {
    __cpuid_count(7, 0, eax, ebx, ecx, edx);
    leaf7_ebx = ebx;
    leaf7_ecx = ecx;
  }

  __cpuid(0x80000000, eax, ebx, ecx, edx);
  if (eax >= 0x80000004) {
    unsigned brand[13] = {0};
    for (unsigned i = 0; i < 3; i++) {
      __cpuid(
          0x80000002 + i,
          brand[4 * i],
          brand[4 * i + 1],
          brand[4 * i + 2],
          brand[4 * i + 3]);
    }
    char* name = (char*)brand;
    while (*name == ' ') {
      name++;
    }
    size_t len = strlen(name);
    while (len > 0 && (name[len - 1] == ' ' || name[len - 1] == '\t')) {
      name[--len] = '\0';
    }
    value = field("model name");
    report("model name", value && strcmp(value, name) == 0);
  }

  const char* flags = field("flags");
  if (!flags) {
    printf("flags: missing\n");
    return 1;
  }
  flags = strdup(flags);
  // Features a build commonly selects code by. Each is in a CPUID word Linux
  // copies into its capability array unchanged.
  const struct {
    const char* name;
    unsigned value;
    int bit;
  } features[] = {
      {"sse", leaf1_edx, 25},     {"sse2", leaf1_edx, 26},
      {"pni", leaf1_ecx, 0},      {"pclmulqdq", leaf1_ecx, 1},
      {"ssse3", leaf1_ecx, 9},    {"fma", leaf1_ecx, 12},
      {"sse4_1", leaf1_ecx, 19},  {"sse4_2", leaf1_ecx, 20},
      {"movbe", leaf1_ecx, 22},   {"popcnt", leaf1_ecx, 23},
      {"aes", leaf1_ecx, 25},     {"avx", leaf1_ecx, 28},
      {"f16c", leaf1_ecx, 29},    {"bmi1", leaf7_ebx, 3},
      {"avx2", leaf7_ebx, 5},     {"bmi2", leaf7_ebx, 8},
      {"avx512f", leaf7_ebx, 16}, {"adx", leaf7_ebx, 19},
      {"sha_ni", leaf7_ebx, 29},  {"avx512bw", leaf7_ebx, 30},
      {"vaes", leaf7_ecx, 9},     {"vpclmulqdq", leaf7_ecx, 10},
  };
  for (size_t i = 0; i < sizeof(features) / sizeof(features[0]); i++) {
    char what[64];
    snprintf(what, sizeof(what), "flag %s", features[i].name);
    int in_cpuid = (features[i].value >> features[i].bit) & 1;
    report(what, has_flag(flags, features[i].name) == in_cpuid);
  }
  return 0;
}
