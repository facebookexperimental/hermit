/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <unistd.h>

int main(void) {
  (void)ftruncate(STDOUT_FILENO, 0);
  _exit(0);
}
