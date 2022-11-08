// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

// RUN: %me | FileCheck %s
// CHECK: Hello world!

#include <stdio.h>

int main(int argc, char* argv[]) {
  printf("Hello world!\n");
  return 0;
}
