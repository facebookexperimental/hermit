/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#if !defined(__x86_64__)
#error "This DBI regression guest requires x86-64."
#endif

typedef int (*mapped_function)(void);

static void fail(const char* operation) {
  perror(operation);
  exit(1);
}

int main(void) {
  static const unsigned char return_42[] = {
      0xb8,
      0x2a,
      0x00,
      0x00,
      0x00, /* mov $42, %eax */
      0xc3, /* ret */
  };
  long page_size = sysconf(_SC_PAGESIZE);
  if (page_size <= 0) {
    fail("sysconf");
  }

  void* mapping = mmap(
      NULL,
      (size_t)page_size,
      PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS,
      -1,
      0);
  if (mapping == MAP_FAILED) {
    fail("mmap");
  }

  memcpy(mapping, return_42, sizeof(return_42));
  if (mprotect(mapping, (size_t)page_size, PROT_READ | PROT_EXEC) != 0) {
    fail("mprotect");
  }
  __builtin___clear_cache(mapping, (char*)mapping + sizeof(return_42));

  mapped_function function = (mapped_function)mapping;
  if (function() != 42) {
    fputs("mapped function returned the wrong value\n", stderr);
    return 1;
  }
  if (munmap(mapping, (size_t)page_size) != 0) {
    fail("munmap");
  }

  puts("dbi-mmap-exec-ok");
  return 0;
}
