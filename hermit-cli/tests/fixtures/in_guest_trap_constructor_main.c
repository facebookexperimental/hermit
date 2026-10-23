/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * The program for in_guest_trap_constructor_lib.c: it prints what the
 * library's constructor did, whether its getenv restored the variable, and
 * the variable as main sees it, so a run that reaches main says so.
 */

#include <stdio.h>
#include <stdlib.h>

const char* in_guest_trap_fixture_mode(void);
int in_guest_trap_fixture_restored(void);

int main(void) {
  const char* value = getenv("REVERIE_LITEINST_SITE_PATCHING");
  printf(
      "main ran; constructor %s; restored %d; REVERIE_LITEINST_SITE_PATCHING=%s\n",
      in_guest_trap_fixture_mode(),
      in_guest_trap_fixture_restored(),
      value == NULL ? "(unset)" : value);
  return 0;
}
