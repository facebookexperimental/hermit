/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Regression guard for the PR #1095 clock freeze.
 *
 * #1095 ("detcore: normalize guest clock after exec") gave each process a
 * GuestClock that subtracted a per-exec origin and re-added the configured
 * epoch, and reset that origin in handle_post_exec. The consequence was that
 * the FIRST clock read after EVERY exec returned exactly the epoch.
 *
 * That is why it survived review: a frozen clock reads IDENTICALLY across
 * processes and across backends, so it arrives looking like a parity WIN. A
 * check that samples the clock once per process cannot see it at all --
 * the first reads agree perfectly, which is precisely the bug.
 *
 * So this guard deliberately does the two things such a check does not:
 *   1. it reads REPEATEDLY inside each generation, not once, and
 *   2. it carries the previous generation's readings ACROSS AN EXEC and
 *      asserts continuity over that boundary.
 *
 * It must also not be satisfiable by making time COARSER -- that would be the
 * defect guarding itself. Note where that property actually comes from: every
 * leg below (round origin, strict advance, cross-exec continuity, distinct
 * per-exec origins) is BROKEN by coarsening rather than satisfied by it, so
 * coarsening can never buy a pass here.
 *
 * There is deliberately NO absolute nanosecond floor on the gap between reads.
 * An earlier draft asserted one and it was wrong: the per-read advance is a
 * function of the run configuration, measured at ~10us under
 * `--strict --base-env=minimal` and ~5ms under the portable verify profile.
 * A constant calibrated on one of those fails the other, which would make this
 * guard a source of false reds rather than a detector of the defect.
 *
 * Usage (the guest re-execs itself; the arguments are internal):
 *   clock_exec_continuity [generation prev_last_ns gen0_first_ns]
 */

#include <errno.h>
#include <inttypes.h>
#include <limits.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

/* Reads per generation. Enough that a coarsened clock repeats a value. */
enum { READS_PER_GENERATION = 8 };
/* Generations, i.e. execs performed. Two boundaries is enough to prove the
 * per-exec reset; more only lengthens the test. */
enum { FINAL_GENERATION = 2 };
#define NS_PER_SEC 1000000000LL

static int64_t read_clock_ns(void) {
  struct timespec now;
  if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
    fprintf(stderr, "clock_gettime failed: %s\n", strerror(errno));
    exit(1);
  }
  return (int64_t)now.tv_sec * NS_PER_SEC + now.tv_nsec;
}

static int64_t parse_ns(const char* text) {
  errno = 0;
  char* end = NULL;
  long long value = strtoll(text, &end, 10);
  if (errno != 0 || end == text || *end != '\0') {
    fprintf(stderr, "unparseable timestamp argument: %s\n", text);
    exit(1);
  }
  return (int64_t)value;
}

int main(int argc, char** argv) {
  long generation = 0;
  int64_t previous_last = 0;
  int64_t generation0_first = 0;

  if (argc == 4) {
    generation = strtol(argv[1], NULL, 10);
    previous_last = parse_ns(argv[2]);
    generation0_first = parse_ns(argv[3]);
  } else if (argc != 1) {
    fprintf(stderr, "usage: %s [generation prev_last_ns gen0_first_ns]\n", argv[0]);
    return 1;
  }

  int64_t readings[READS_PER_GENERATION];
  for (int i = 0; i < READS_PER_GENERATION; i++) {
    readings[i] = read_clock_ns();
  }

  const int64_t first = readings[0];
  const int64_t last = readings[READS_PER_GENERATION - 1];

  /*
   * (1) A round origin is the #1095 signature. The configured epoch is a whole
   * second, so a clock rebased onto it reads exactly N*10^9 -- nanoseconds all
   * zero. A genuine read lands on epoch + accumulated startup work.
   */
  if (first % NS_PER_SEC == 0) {
    fprintf(
        stderr,
        "FAIL gen=%ld first read %" PRId64
        " sits exactly on a whole second: the clock was rebased onto a round"
        " origin (PR #1095 signature)\n",
        generation,
        first);
    return 1;
  }

  /*
   * (2) Repeated reads must keep moving. This is the leg that a coarsened
   * clock fails: quantise to a tick larger than the gap between two adjacent
   * reads and consecutive values collapse to equal.
   */
  for (int i = 1; i < READS_PER_GENERATION; i++) {
    if (readings[i] <= readings[i - 1]) {
      fprintf(
          stderr,
          "FAIL gen=%ld read %d (%" PRId64 ") did not advance past read %d (%" PRId64
          "): virtual time is frozen or coarsened\n",
          generation,
          i,
          readings[i],
          i - 1,
          readings[i - 1]);
      return 1;
    }
  }

  /* Reported, never asserted against a constant: the per-read advance is
   * configuration-dependent, so it is evidence for a reader rather than a
   * threshold. See the header comment. */
  int64_t smallest_delta = INT64_MAX;
  for (int i = 1; i < READS_PER_GENERATION; i++) {
    const int64_t delta = readings[i] - readings[i - 1];
    if (delta < smallest_delta) {
      smallest_delta = delta;
    }
  }

  if (generation > 0) {
    /*
     * (4) THE LOAD-BEARING LEG. Time may not go backwards across an exec.
     * #1095 reset the origin in handle_post_exec, so this generation's first
     * read returned the epoch -- far BELOW the previous generation's last
     * read. Nothing that samples within a single process can observe this.
     */
    if (first <= previous_last) {
      fprintf(
          stderr,
          "FAIL gen=%ld first read %" PRId64
          " is not after the previous generation's last read %" PRId64
          ": the clock was reset or rebased across exec (PR #1095 signature)\n",
          generation,
          first,
          previous_last);
      return 1;
    }

    /*
     * (5) And the per-exec first reads must differ from one another. Under
     * #1095 every generation opened on the same epoch value; that identity is
     * the thing that masqueraded as cross-backend agreement.
     */
    if (first == generation0_first) {
      fprintf(
          stderr,
          "FAIL gen=%ld first read %" PRId64
          " is identical to generation 0's first read: every exec is opening on"
          " the same frozen origin (PR #1095 signature)\n",
          generation,
          first);
      return 1;
    }
  }

  printf(
      "gen=%ld first=%" PRId64 " last=%" PRId64 " min_delta=%" PRId64 "\n",
      generation,
      first,
      last,
      smallest_delta);

  if (generation >= FINAL_GENERATION) {
    printf("clock exec continuity holds across %d execs\n", FINAL_GENERATION);
    return 0;
  }

  char next_generation[32];
  char last_text[32];
  char first_text[32];
  snprintf(next_generation, sizeof(next_generation), "%ld", generation + 1);
  snprintf(last_text, sizeof(last_text), "%" PRId64, last);
  snprintf(
      first_text,
      sizeof(first_text),
      "%" PRId64,
      generation == 0 ? first : generation0_first);

  /* Flush before exec: the replacement image does not inherit our stdio buffer. */
  fflush(stdout);

  char* next_argv[] = {argv[0], next_generation, last_text, first_text, NULL};
  execv("/proc/self/exe", next_argv);
  fprintf(stderr, "execv failed: %s\n", strerror(errno));
  return 1;
}
