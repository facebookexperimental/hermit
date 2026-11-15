/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Executes one BMI2 instruction (PDEP, an x86-64-v3 feature) while
 * declaring no ISA level: no GNU property note is emitted, and the
 * instruction is encoded as bytes so no compiler target feature
 * makes this image declare v3. For cpu_profile_v2_rootfs.rs's
 * undeclared-image case: Hermit's admission is declaration-based,
 * so this is not refused before it runs. On a host whose CPU has
 * BMI2 it runs and prints its marker even under the v2 profile;
 * on a CPU without BMI2 the PDEP bytes take the hardware's #UD.
 * That split is the named limit the test documents: presenting a
 * v2 CPUID table is not instruction-level sandboxing on a v3 host.
 */

static void write_all(const char* s, unsigned long len) {
  long result;
  __asm__ volatile("syscall"
                   : "=a"(result)
                   : "a"(1), "D"(1), "S"(s), "d"(len)
                   : "rcx", "r11", "memory");
  (void)result;
}

static unsigned long str_len(const char* s) {
  unsigned long n = 0;
  while (s[n] != '\0') {
    n++;
  }
  return n;
}

void _start(void) {
  unsigned long result;
  /* PDEP %rsi,%rdi,%rax with value 0b101 and mask 0xf0: the two set
   * bits land at mask positions 4 and 6, giving 0x50. The bytes are
   * c4 e2 c3 f5 c6 (VEX.LZ.F2.0F38.W1 F5 with those registers). */
  __asm__ volatile(
      "mov $5, %%rdi\n"
      "mov $240, %%rsi\n"
      ".byte 0xc4, 0xe2, 0xc3, 0xf5, 0xc6\n"
      : "=a"(result)
      :
      : "rdi", "rsi");
  const char* message = result == 0x50 ? "undeclared-v3: ran pdep\n"
                                       : "undeclared-v3: wrong result\n";
  write_all(message, str_len(message));
  long code = result == 0x50 ? 0 : 1;
  __asm__ volatile("syscall" ::"a"(231), "D"(code));
  __builtin_unreachable();
}
