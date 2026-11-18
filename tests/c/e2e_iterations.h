/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * The optional iteration count of a guest that a benchmark may run many
 * times in one process (the e2e-cells-as-criterion design in the dev-hermit
 * workspace, ai_docs/plans/2026-11-18-e2e-cells-as-criterion-benchmarks.md).
 * E2E_ITERATIONS=N runs the guest's body N times; unset, the body runs once,
 * exactly as before, which is what every e2e cell does: no manifest sets the
 * variable. A benchmark fits wall time against N, so the slope is the cost of
 * one body and the intercept the cost of starting and ending the process.
 */

#ifndef HERMIT_TESTS_C_E2E_ITERATIONS_H
#define HERMIT_TESTS_C_E2E_ITERATIONS_H

#include <stdio.h>
#include <stdlib.h>

#define E2E_ITERATIONS_MAX 1000000UL

/* The count E2E_ITERATIONS asks for: 1 when unset, and exit 2 for anything
 * but a decimal integer from 1 to E2E_ITERATIONS_MAX, so a typo cannot
 * silently time a single body. */
static unsigned long e2e_iterations(void) {
  const char* text = getenv("E2E_ITERATIONS");
  if (text == NULL) {
    return 1;
  }
  unsigned long count = 0;
  const char* digit = text;
  for (; *digit >= '0' && *digit <= '9'; ++digit) {
    count = count * 10 + (unsigned long)(*digit - '0');
    if (count > E2E_ITERATIONS_MAX) {
      break;
    }
  }
  if (digit == text || *digit != '\0' || count == 0 ||
      count > E2E_ITERATIONS_MAX) {
    fprintf(
        stderr,
        "E2E_ITERATIONS must be a decimal integer from 1 to %lu, got \"%s\"\n",
        E2E_ITERATIONS_MAX,
        text);
    exit(2);
  }
  return count;
}

#endif
