/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Check that the C stack and heap are deterministic, and print every kind of
// address a program can see, so two backends can be compared line by line:
// the initial stack and what the kernel put on it, the program's and libc's
// text and data, thread-local storage, the program break and malloc's arenas,
// and anonymous mappings. A line that differs between backends names the
// region whose layout differs.

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <unistd.h>

extern char** environ;

int initialized_global = 1;
int zeroed_global;
static __thread int thread_local_value = 1;

static void print_auxv(const char* name, unsigned long type) {
  printf("auxv %s: %#lx\n", name, getauxval(type));
}

int main(int argc, char** argv) {
  int x = argc * 99;
  printf("Stack address: %p\n", &x);
  void* p1 = malloc(100);
  void* p2 = malloc(1000);
  void* p3 = malloc(10000);
  void* p4 = malloc(100000);
  void* p5 = malloc(1000000);
  printf("Malloc'd pointers: %p %p %p %p %p\n", p1, p2, p3, p4, p5);

  // What the kernel placed on the initial stack. Their sizes decide where the
  // stack pointer starts, so a different environment moves every stack address.
  size_t environment_bytes = 0;
  int environment_count = 0;
  for (char** variable = environ; *variable != NULL; variable++) {
    environment_bytes += strlen(*variable) + 1;
    environment_count++;
  }
  printf("argv: %p argv[0]: %p\n", (void*)argv, (void*)argv[0]);
  printf(
      "environ: %p, %d variables, %zu bytes\n",
      (void*)environ,
      environment_count,
      environment_bytes);
  print_auxv("AT_PHDR", AT_PHDR);
  print_auxv("AT_ENTRY", AT_ENTRY);
  print_auxv("AT_BASE", AT_BASE);
  print_auxv("AT_SYSINFO_EHDR", AT_SYSINFO_EHDR);
  print_auxv("AT_EXECFN", AT_EXECFN);
  print_auxv("AT_RANDOM", AT_RANDOM);
  print_auxv("AT_PAGESZ", AT_PAGESZ);

  // The program's own image, libc's, and thread-local storage.
  printf("main: %p\n", (void*)(uintptr_t)main);
  printf("libc stdout: %p\n", (void*)stdout);
  printf("initialized global: %p\n", (void*)&initialized_global);
  printf("zeroed global: %p\n", (void*)&zeroed_global);
  printf("thread-local: %p\n", (void*)&thread_local_value);

  // The program break, before and after growing it.
  void* break_before = sbrk(0);
  void* grown = sbrk(4096);
  printf("program break: %p, grown from %p to %p\n", break_before, grown, sbrk(0));

  // malloc after free and realloc, and calloc.
  free(p2);
  void* reused = malloc(1000);
  void* resized = realloc(p3, 50000);
  void* zeroed = calloc(16, 64);
  printf("reused: %p resized: %p calloc: %p\n", reused, resized, zeroed);

  // Anonymous mappings: one page, and a large one.
  void* page =
      mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  void* large = mmap(
      NULL, 1 << 24, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (page == MAP_FAILED || large == MAP_FAILED) {
    perror("mmap");
    return 1;
  }
  printf("mmap page: %p mmap 16 MiB: %p\n", page, large);
  return 0;
}
