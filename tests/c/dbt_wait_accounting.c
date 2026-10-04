/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * The --accounting-only mode of dbt_wait_lifecycle.c as its own manifest
 * program.
 *
 * Without a SIGCHLD handler installed, the child exits are reaped and their
 * wait4/waitid status and rusage are checked with no signal delivery at all,
 * which is a different path through the backend from the handler-observing
 * default mode. A manifest program belongs to exactly one test, so this
 * wrapper gives that mode its own test and verify cells.
 */

#define main dbt_wait_lifecycle_main
#include "dbt_wait_lifecycle.c"
#undef main

int main(void) {
  char program[] = "dbt_wait_accounting";
  char mode[] = "--accounting-only";
  char* argv[] = {program, mode, NULL};
  return dbt_wait_lifecycle_main(2, argv);
}
