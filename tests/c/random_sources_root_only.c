/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * The --root-only mode of random_sources.c as its own manifest program.
 *
 * Root-only output is the random stream drawn by the root thread alone:
 * getrandom(2) through the raw syscall, /dev/urandom, /dev/random and the
 * vDSO getrandom leg. The raw syscall keeps the golden independent of the C
 * library: glibc 2.41 and later derive getrandom(3) bytes in userspace. It
 * leaves out the per-thread samples, so it is the part of the guest's output
 * that must be byte-identical across backends, not only across two runs on
 * one backend. A manifest program belongs to exactly one test, and
 * c-programs/random-sources already checks the full output with threads, so
 * this wrapper gives the root-only contract its own test and verify cells.
 */

#define main random_sources_main
#include "random_sources.c"
#undef main

int main(void) {
  char program[] = "random_sources_root_only";
  char mode[] = "--root-only";
  char* argv[] = {program, mode, NULL};
  return random_sources_main(2, argv);
}
