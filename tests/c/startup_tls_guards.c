/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Print the two x86-64 glibc TLS guards that are initialized from AT_RANDOM
 * before main(). Reading AT_RANDOM later is not equivalent: a backend can
 * rewrite those bytes after glibc has already copied them into TLS.
 */

#include <elf.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/auxv.h>

static uintptr_t read_stack_canary(void) {
  uintptr_t value;
  __asm__ volatile("movq %%fs:0x28, %0" : "=r"(value));
  return value;
}

static uintptr_t read_pointer_guard(void) {
  uintptr_t value;
  __asm__ volatile("movq %%fs:0x30, %0" : "=r"(value));
  return value;
}

int main(void) {
  const unsigned char* random = (const unsigned char*)getauxval(AT_RANDOM);

  printf("STACK_CANARY 0x%016" PRIxPTR "\n", read_stack_canary());
  printf("POINTER_GUARD 0x%016" PRIxPTR "\n", read_pointer_guard());
  printf("AT_RANDOM_BYTES ");
  for (int i = 0; i < 16; i++) {
    printf("%02x", random[i]);
  }
  printf("\n");
  return 0;
}
