/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <locale.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <sys/sysinfo.h>
#include <unistd.h>

void* allocateMemory(int size) {
  char* ptr = (char*)malloc(size);
  if (ptr == NULL) {
    return NULL;
  }
  for (int i = 0; i < size; ++i) {
    ptr[i] = 64;
  }
  return ptr;
}
const int MB = 1024 * 1024;
int main() {
  struct sysinfo info;
  sleep(5);

  void* allocation =
      allocateMemory(1 * MB); // allocating 1Mb of memory to check in sysinfo result
  if (allocation == NULL) {
    perror("malloc");
    return EXIT_FAILURE;
  }

  /* Hermit's configured virtual memory is 1,000,000,000 bytes by default.
     Detcore does not model allocation pressure within that limit, so the
     reported available memory equals the configured total. */
  const unsigned long VIRT_TOTALRAM = 1000000000UL;

  if (sysinfo(&info) != 0) {
    perror("sysinfo");
    free(allocation);
    return EXIT_FAILURE;
  }
  if (info.totalram != VIRT_TOTALRAM || info.freeram != VIRT_TOTALRAM ||
      info.mem_unit != 1 || info.bufferram != 0 || info.sharedram != 0 ||
      info.totalswap != 0 || info.freeswap != 0 || info.totalhigh != 0 ||
      info.freehigh != 0) {
    fprintf(
        stderr,
        "sysinfo memory mismatch: total=%lu free=%lu buffer=%lu shared=%lu "
        "swap=%lu/%lu high=%lu/%lu unit=%u\n",
        info.totalram,
        info.freeram,
        info.bufferram,
        info.sharedram,
        info.totalswap,
        info.freeswap,
        info.totalhigh,
        info.freehigh,
        info.mem_unit);
    free(allocation);
    return EXIT_FAILURE;
  }

  errno = 0;
  long null_result = syscall(SYS_sysinfo, NULL);
  if (null_result != -1 || errno != EFAULT) {
    fprintf(
        stderr,
        "sysinfo(NULL) returned %ld errno=%d, expected -1/EFAULT\n",
        null_result,
        errno);
    free(allocation);
    return EXIT_FAILURE;
  }

  setlocale(LC_NUMERIC, ""); // Print large numbers with commas.
  printf("uptime: %lu sec\n", info.uptime);
  printf("load_time_1: %lu\n", info.loads[0]);
  printf("load_time_5: %lu\n", info.loads[1]);
  printf("load_time_15: %lu\n", info.loads[2]);
  printf("total RAM: %'lu\n", info.totalram);
  printf("free RAM: %'lu\n", info.freeram);
  printf("shared RAM: %'lu\n", info.sharedram);
  printf("buffer RAM: %'lu\n", info.bufferram);
  printf("total swap: %lu\n", info.totalswap);
  printf("free swap: %'lu\n", info.freeswap);
  printf("total high size: %'lu\n", info.totalhigh);
  printf("free high: %'lu\n", info.freehigh);
  printf("\n");
  printf("mem_unit: %u\n", info.mem_unit);
  printf("Total - free = used: %'lu\n", info.totalram - info.freeram);
  free(allocation);
  return EXIT_SUCCESS;
}
