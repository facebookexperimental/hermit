/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */


/*
 * An ordinary dynamically linked program whose main writes "main\n" to
 * standard output, showing that it ran, then glibc's loader message, "Fatal
 * glibc error: CPU does not support x86-64-v3\n", to standard error, and exits
 * 127: the loader's bytes and status, from the program's own code after its
 * loader opened and mapped its libraries. Hermit must not take it for the
 * loader's abort (https://github.com/rrnewton/hermit/issues/4695).
 */

#include <unistd.h>

int main(void) {
  static const char ran[] = "main\n";
  static const char message[] =
      "Fatal glibc error: CPU does not support x86-64-v3\n";
  if (write(STDOUT_FILENO, ran, sizeof ran - 1) < 0 ||
      write(STDERR_FILENO, message, sizeof message - 1) < 0) {
    return 1;
  }
  _exit(127);
}
