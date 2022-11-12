/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Check that the C stack and heap are deterministic.

#include <stdio.h>
#include <stdlib.h>

int main(int argc, char** argv) {
  int x = argc * 99;
  printf("Stack address: %p\n", &x);
  void* p1 = malloc(100);
  void* p2 = malloc(1000);
  void* p3 = malloc(10000);
  void* p4 = malloc(100000);
  void* p5 = malloc(1000000);
  printf("Malloc'd pointers: %p %p %p %p %p\n", p1, p2, p3, p4, p5);
}
