/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Shared contract for backend-parity identity fixtures.
 *
 * A parity identity fixture drives one syscall, observes a value that Hermit is
 * responsible for making deterministic (a value the guest sets on itself, or a
 * canonicalized kernel result), and reports whether every backend observes the
 * same thing as the golden ptrace reference. Historically each fixture rolled
 * its own pass/fail convention, and two incompatible ones grew up side by side:
 *
 *   Pattern A (cpuid_probe.c): a failed check does `return 1;` -- exit status is
 *   load-bearing, so a regression is caught even when only the exit code is
 *   observed.
 *   Pattern B (fchmod_bits.c, uname_identity.c): the result is smuggled into an
 *   `ok=N` line and the program ALWAYS `return 0;`. Under `hermit run --verify`,
 *   which diverts guest stdout into per-run logs and keys the verdict on exit
 *   status, a Pattern-B fixture passes even when its contract fails. It reports
 *   backend coverage that does not exist -- exactly the vacuous-test shape.
 *
 * This header gives every fixture ONE convention so the coverage is real:
 *
 *   - parity_check(cond, label): accumulate contract checks.
 *   - parity_emit(...):          print the single canonical stdout line.
 *   - parity_finish():           exit NONZERO if any check failed. Every fixture
 *                                ends with `return parity_finish();`, so exit
 *                                status is always load-bearing.
 *   - parity_mutate_*(field, v): the observation seam the mutation harness
 *                                drives (see below).
 *
 * The mutation seam. `tests/backend-parity/parity_mutation.py` proves a fixture
 * is non-vacuous by planting a divergence: it runs the fixture once clean and
 * once with HERMIT_PARITY_MUTATE naming one of the fixture's fields. A fixture
 * observes every syscall result it reports through parity_mutate_*(field, v),
 * which returns v unchanged normally but a deterministically perturbed value
 * when <field> is named. If the field is genuinely load-bearing -- threaded into
 * the fixture's checks and its emitted line -- the mutated run's (exit status,
 * stdout) observation diverges from the clean golden run and the harness records
 * the divergence as caught. If mutating the field changes nothing, the field is
 * not actually exercised and the harness flags the fixture VACUOUS. This is the
 * "does the test fail if the mechanism does not run?" question, applied
 * mechanically to every family member.
 *
 * _GNU_SOURCE is supplied by the compile flags (both parity_mutation.py and
 * ci/test_harness.sh compile with -D_GNU_SOURCE); do not define it here.
 */

#ifndef HERMIT_BACKEND_PARITY_PROBE_H
#define HERMIT_BACKEND_PARITY_PROBE_H

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* Count of failed contract checks; consulted only by parity_finish(). */
static int parity__failures = 0;

/*
 * Record a contract check. Non-fatal on its own so a fixture can report every
 * failing check in one run; parity_finish() converts any failure into a nonzero
 * exit. Never let a fixture reach `return 0;` with checks outstanding.
 */
#define parity_check(cond, label)                                              \
  do {                                                                         \
    if (!(cond)) {                                                             \
      fprintf(stderr, "parity-check FAILED: %s\n", (label));                  \
      parity__failures++;                                                      \
    }                                                                          \
  } while (0)

/*
 * True when HERMIT_PARITY_MUTATE names <field>. The variable is a list of field
 * names separated by commas or spaces; the single token "*" matches every
 * field. Absent/empty variable means no mutation (the normal path).
 */
static inline int parity__mutating(const char *field) {
  const char *spec = getenv("HERMIT_PARITY_MUTATE");
  if (spec == NULL || spec[0] == '\0' || field == NULL) {
    return 0;
  }
  size_t field_len = strlen(field);
  const char *cursor = spec;
  while (*cursor != '\0') {
    while (*cursor == ',' || *cursor == ' ') {
      cursor++;
    }
    const char *start = cursor;
    while (*cursor != '\0' && *cursor != ',' && *cursor != ' ') {
      cursor++;
    }
    size_t token_len = (size_t)(cursor - start);
    if (token_len == 1 && start[0] == '*') {
      return 1;
    }
    if (token_len == field_len && memcmp(start, field, field_len) == 0) {
      return 1;
    }
  }
  return 0;
}

/*
 * Observation seam for unsigned/signed syscall results. Returns v unchanged
 * unless <field> is being mutated, in which case it returns a deterministically
 * perturbed value (low bit flipped -- always different, never host-dependent).
 * A fixture must thread the RESULT of this call through both its checks and its
 * emitted line for the field to be load-bearing.
 */
static inline uint64_t parity_mutate_u64(const char *field, uint64_t v) {
  return parity__mutating(field) ? (v ^ (uint64_t)0x1) : v;
}

static inline int64_t parity_mutate_i64(const char *field, int64_t v) {
  return parity__mutating(field) ? (v ^ (int64_t)0x1) : v;
}

/*
 * Observation seam for string results (e.g. a name round-trip). Rewrites the
 * first byte deterministically when <field> is mutated. buf must be a writable,
 * NUL-terminated buffer with at least one character.
 */
static inline char *parity_mutate_str(const char *field, char *buf) {
  if (parity__mutating(field) && buf != NULL && buf[0] != '\0') {
    buf[0] = (char)(buf[0] ^ 0x20);
  }
  return buf;
}

/* Print the single canonical, deterministic identity line to stdout. */
#define parity_emit(...)                                                        \
  do {                                                                          \
    printf(__VA_ARGS__);                                                        \
  } while (0)

/*
 * Terminal step for every fixture: `return parity_finish();`. Exits nonzero if
 * any parity_check failed, which is what keeps exit status load-bearing under
 * runners (like `hermit run --verify`) that observe only the exit code.
 */
static inline int parity_finish(void) {
  if (parity__failures > 0) {
    fprintf(stderr, "parity: %d contract check(s) failed\n", parity__failures);
    return 1;
  }
  return 0;
}

#endif /* HERMIT_BACKEND_PARITY_PROBE_H */
