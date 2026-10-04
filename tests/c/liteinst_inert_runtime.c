/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <stdint.h>

void reverie_liteinst_initialize(void) {}

uint64_t reverie_liteinst_site_trap_count(uint64_t address) {
  (void)address;
  return 0;
}

uint64_t reverie_liteinst_site_hook_count(uint64_t address) {
  (void)address;
  return 0;
}
