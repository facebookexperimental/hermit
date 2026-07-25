/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Execute 4 MiB of text so DBI must translate the full code path. */
__asm__(
    ".pushsection .text.benchmark_padding,\"ax\",@progbits\n"
    ".balign 16\n"
    ".global benchmark_padding\n"
    ".type benchmark_padding,@function\n"
    "benchmark_padding:\n"
    ".rept 4194304\n"
    "nop\n"
    ".endr\n"
    "ret\n"
    ".size benchmark_padding,.-benchmark_padding\n"
    ".popsection\n");

extern void benchmark_padding(void);

int main(void) {
  benchmark_padding();
  return 0;
}
