/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 *
 * Link-only stand-in that gives the packaged client a libdetcore_dbt.so
 * dependency before Cargo has linked the real workspace cdylib.
 */

#include <stddef.h>
#include <stdint.h>

typedef struct {
  uint32_t size;
  uint32_t abi;
  uint64_t capabilities;
  uint64_t image_generation;
  uint8_t runtime_input_identity[32];
} image_descriptor_t;

_Static_assert(sizeof(image_descriptor_t) == 56, "image descriptor ABI size");
_Static_assert(offsetof(image_descriptor_t, capabilities) == 8,
    "image descriptor capabilities ABI offset");
_Static_assert(offsetof(image_descriptor_t, image_generation) == 16,
    "image descriptor generation ABI offset");
_Static_assert(offsetof(image_descriptor_t, runtime_input_identity) == 24,
    "image descriptor identity ABI offset");

image_descriptor_t reverie_dbt_runtime_image_init(void) {
  /* This link-only stand-in must never describe a runnable runtime. */
  return (image_descriptor_t){0};
}
