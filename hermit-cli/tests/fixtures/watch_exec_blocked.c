// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

// Blocks SIGTRAP, then replaces itself with the program its arguments
// name, on the same thread. Exec keeps the blocked mask and clears the
// debug registers, so the new image cannot re-arm its watches.

#include <signal.h>
#include <unistd.h>

int main(int argc, char** argv) {
  if (argc < 2) {
    return 2;
  }
  sigset_t trap;
  sigemptyset(&trap);
  sigaddset(&trap, SIGTRAP);
  if (sigprocmask(SIG_BLOCK, &trap, NULL) != 0) {
    return 3;
  }
  execv(argv[1], argv + 1);
  return 1;
}
