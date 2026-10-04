/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * The "shared" scenario of mmap_determinism.c as its own manifest program.
 *
 * A manifest program belongs to exactly one test, and a verify mode gives each
 * backend one argument vector, so c-programs/mmap-determinism can select only
 * one scenario per backend. This wrapper fixes the MAP_SHARED|MAP_ANONYMOUS
 * scenario so it has its own test and verify cells.
 */

#define main mmap_determinism_main
#include "mmap_determinism.c"
#undef main

int main(void) {
  char program[] = "mmap_determinism_shared";
  char scenario[] = "shared";
  char* argv[] = {program, scenario, NULL};
  return mmap_determinism_main(2, argv);
}
