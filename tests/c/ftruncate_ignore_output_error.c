/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <unistd.h>

int main(void) {
  // The guest ignores a failed truncate by design: replay into a sink that
  // rejects it must abort in Hermit, not through a guest-visible exit status.
  // The result is stored because GCC ignores a (void) cast on a
  // warn_unused_result call, which glibc applies to ftruncate under
  // _FORTIFY_SOURCE (default at -O on Ubuntu).
  int truncated = ftruncate(STDOUT_FILENO, 0);
  (void)truncated;
  _exit(0);
}
