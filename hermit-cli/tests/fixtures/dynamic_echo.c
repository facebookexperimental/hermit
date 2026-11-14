/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * dynamic_echo: writes its arguments joined by single spaces and followed by
 * a newline, in one write() to stdout, as `/bin/echo` writes plain words. A
 * first argument `-n` drops the newline, as for `/bin/echo`, so `-n` alone
 * writes nothing and exits 0, as `/bin/true` does.
 * Exits 0, or 1 when the output does not fit its buffer or the write is short.
 *
 * libhermit's kvm_runs_dynamic_echo_through_detcore and
 * liteinst_public_dispatch_runs_in_guest_detcore run it as their guest under
 * Buck, where the host's /bin/echo and /bin/true may declare x86-64-v3, a
 * profile the KVM backend does not run. Buck links it dynamically against the
 * platform's C library, so it still loads through its program interpreter;
 * the tests check that, and the KVM test also checks that auto does not
 * resolve it to x86-64-v3.
 */
#include <string.h>
#include <unistd.h>

int main(int argc, char** argv) {
  char buffer[4096];
  size_t length = 0;
  int first = 1;
  int newline = 1;
  if (argc > 1 && strcmp(argv[1], "-n") == 0) {
    first = 2;
    newline = 0;
  }
  for (int i = first; i < argc; i++) {
    size_t word = strlen(argv[i]);
    // The word, its separator or newline, and nothing past the buffer.
    if (word + 1 > sizeof(buffer) - length) {
      return 1;
    }
    memcpy(buffer + length, argv[i], word);
    length += word;
    if (i + 1 < argc) {
      buffer[length++] = ' ';
    }
  }
  if (newline) {
    buffer[length++] = '\n';
  }
  if (length == 0) {
    return 0;
  }
  return write(STDOUT_FILENO, buffer, length) == (ssize_t)length ? 0 : 1;
}
