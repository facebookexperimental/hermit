/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <locale.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/sysinfo.h>
#include <unistd.h>

void allocateMemory(int size) {
  char* ptr;
  ptr = (char*)malloc(size);
  for (int i = 0; i < size; ++i) {
    ptr[i] = 64;
  }
}
const int MB = 1024 * 1024;
int main() {
  struct sysinfo info;
  sleep(5);

  allocateMemory(1 * MB); // allocating 1Mb of memory to check in sysinfo result

  sysinfo(&info);

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
}
