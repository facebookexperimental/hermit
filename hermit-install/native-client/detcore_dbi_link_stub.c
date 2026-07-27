/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 *
 * Link-only stand-in that gives the packaged client a libdetcore_dbi.so
 * dependency before Cargo has linked the real workspace cdylib.
 */

#include <stdint.h>

uint64_t reverie_dbi_runtime_image_init(void) {
  return 0;
}
