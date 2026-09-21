/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

static size_t page_size(void) {
  long page = sysconf(_SC_PAGESIZE);
  if (page <= 0) {
    perror("sysconf");
    exit(1);
  }
  return (size_t)page;
}

static void* checked_mmap(void* addr, size_t len, int flags) {
  void* result = mmap(addr, len, PROT_READ | PROT_WRITE, flags, -1, 0);
  if (result == MAP_FAILED) {
    perror("mmap");
    exit(1);
  }
  return result;
}

static void checked_munmap(void* addr, size_t len) {
  if (munmap(addr, len) != 0) {
    perror("munmap");
    exit(1);
  }
}

static void multiple_mmaps(void) {
  size_t page = page_size();
  void* first = checked_mmap(NULL, page, MAP_PRIVATE | MAP_ANONYMOUS);
  void* second = checked_mmap(NULL, page * 2, MAP_PRIVATE | MAP_ANONYMOUS);
  void* third = checked_mmap(NULL, page * 3, MAP_PRIVATE | MAP_ANONYMOUS);

  printf("multiple %p %p %p\n", first, second, third);
}

static void fixed_mmap(void) {
  size_t page = page_size();
  void* expected = (void*)(uintptr_t)0x500000000000ULL;
  void* result = checked_mmap(
      expected, page, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED);
  if (result != expected) {
    fprintf(stderr, "MAP_FIXED returned %p, expected %p\n", result, expected);
    exit(1);
  }

  ((volatile unsigned char*)result)[0] = 0x5a;
  printf("fixed %p\n", result);
  checked_munmap(result, page);
}

static void heap_growth(void) {
  size_t page = page_size();
  void* initial = sbrk(0);
  if (initial == (void*)-1) {
    perror("sbrk");
    exit(1);
  }

  void* previous = sbrk((intptr_t)page);
  if (previous == (void*)-1) {
    perror("sbrk");
    exit(1);
  }
  void* after_sbrk = sbrk(0);
  void* requested = (void*)((uintptr_t)after_sbrk + page * 2);
  if (brk(requested) != 0) {
    perror("brk");
    exit(1);
  }
  void* after_brk = sbrk(0);

  if (previous != initial || after_brk != requested) {
    fprintf(stderr, "unexpected heap growth sequence\n");
    exit(1);
  }
  printf("heap %p %p %p %p\n", initial, previous, after_sbrk, after_brk);
}

static void shared_mmap(void) {
  size_t len = page_size() * 2;
  void* result = checked_mmap(NULL, len, MAP_SHARED | MAP_ANONYMOUS);
  ((volatile unsigned char*)result)[0] = 0xa5;
  ((volatile unsigned char*)result)[len - 1] = 0x5a;

  printf("shared %p\n", result);
  checked_munmap(result, len);
}

static void munmap_reuse(void) {
  size_t page = page_size();
  void* first = checked_mmap(NULL, page, MAP_PRIVATE | MAP_ANONYMOUS);
  checked_munmap(first, page);
  void* second = checked_mmap(NULL, page, MAP_PRIVATE | MAP_ANONYMOUS);

  if (second != first) {
    fprintf(stderr, "mmap did not reuse %p: got %p\n", first, second);
    exit(1);
  }
  printf("reuse %p %p\n", first, second);
  checked_munmap(second, page);
}

int main(int argc, char** argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s SCENARIO\n", argv[0]);
    return 2;
  }

  if (strcmp(argv[1], "multiple") == 0) {
    multiple_mmaps();
  } else if (strcmp(argv[1], "fixed") == 0) {
    fixed_mmap();
  } else if (strcmp(argv[1], "heap") == 0) {
    heap_growth();
  } else if (strcmp(argv[1], "shared") == 0) {
    shared_mmap();
  } else if (strcmp(argv[1], "reuse") == 0) {
    munmap_reuse();
  } else {
    fprintf(stderr, "unknown scenario: %s\n", argv[1]);
    return 2;
  }

  return 0;
}
