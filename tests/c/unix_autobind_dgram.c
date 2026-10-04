/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include "unix_autobind_common.h"

int main(void) {
  return run_unix_autobind_probe(SOCK_DGRAM, "dgram");
}
