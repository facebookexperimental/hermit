/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// RUN: %me | FileCheck %s
// CHECK: Hello world!

#include <stdio.h>

int main(int argc, char* argv[]) {
  printf("Hello world!\n");
  return 0;
}
