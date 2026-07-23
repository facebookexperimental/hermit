/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A tiny, deterministic guest program used by the debugger integration tests
 * (tests/debugger/). It has stable, easy-to-assert symbols and values:
 *
 *   - compute(a, b) returns (a + b) + (a * b); with a=7, b=6 that is 55.
 *   - `sum` inside compute() is 13 (a + b) after the first statement.
 *   - `pid` in main() comes from getpid(). Outside Hermit that value is
 *     nondeterministic, but under `hermit run`/`hermit replay` it is
 *     virtualized to a fixed value, so a debugger observes the *same* pid on
 *     every replay. The replay tests rely on that to demonstrate determinism.
 *
 * Build non-PIE (-no-pie) so text addresses are fixed. This keeps remote
 * debugging simple and matches how the harness compiles it.
 */

#include <stdio.h>
#include <unistd.h>

/* Marked noinline so the breakpoint on `compute` always has a real frame. */
__attribute__((noinline)) int compute(int a, int b) {
    int sum = a + b;     /* line: BP_COMPUTE (breakpoint target) */
    int product = a * b;
    return sum + product;
}

int main(void) {
    int pid = (int)getpid();          /* deterministic under Hermit */
    int x = 7;
    int y = 6;
    int result = compute(x, y);       /* line: BP_MAIN */
    printf("pid=%d result=%d\n", pid, result);
    fflush(stdout);
    return 0;
}
