/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// libstdc++ chooses hardware randomness from CPUID when it is advertised.
// host-no-rdrand must make its ordinary random_device use Hermit's intercepted
// OS randomness instead. The CLI test compares two full strict executions.

#include <cstdio>
#include <random>

int main() {
  std::random_device source;
  for (unsigned i = 0; i < 16; ++i) {
    std::printf("sample %02u %08x\n", i, source());
  }
}
