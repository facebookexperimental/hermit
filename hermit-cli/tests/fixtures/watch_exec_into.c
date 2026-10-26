// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

// Replaces itself with the program its arguments name, on the same thread.

#include <unistd.h>

int main(int argc, char** argv) {
  if (argc < 2) {
    return 2;
  }
  execv(argv[1], argv + 1);
  return 1;
}
