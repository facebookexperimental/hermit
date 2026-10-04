/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <unistd.h>

int main(void) {
  static const char output[] = "captured-output\n";
  // The guest ignores a failed write by design: replay into a failing sink
  // must abort in Hermit, not through a guest-visible retry or exit status.
  // The result is stored because GCC ignores a (void) cast on a
  // warn_unused_result call, which glibc applies to write under
  // _FORTIFY_SOURCE (default at -O on Ubuntu).
  ssize_t written = write(STDOUT_FILENO, output, sizeof(output) - 1);
  (void)written;
  _exit(0);
}
