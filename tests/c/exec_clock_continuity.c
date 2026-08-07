/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Guest-side check that the container's virtual clock is CONTINUOUS across an
 * `execve` boundary, and fine-grained within each image.
 *
 * Detcore's virtual clock is a deterministic function of guest progress, and
 * `execve` replaces the process image but not the container. A guest must
 * therefore never observe time restarting at the configured epoch after an
 * exec: an exec chain has to see a strictly advancing clock, exactly as it
 * would on Linux. This is the property hermit#705 was ultimately about --
 * first-read agreement on a tidy origin is not clock virtualization, so this
 * guest samples the whole trajectory instead of a single value.
 *
 * The guest prints only a fixed verdict token, never a timestamp, so its
 * stdout stays byte-identical across runs and the program is usable under
 * `hermit run --verify`.
 *
 * Phase 1 (no argv):   sample repeatedly, then exec phase 2 passing the last
 *                      observed time in argv.
 * Phase 2 (argv[1]=T): sample repeatedly and require every sample > T.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define SAMPLES 8

static int read_clock_nanos(uint64_t* out) {
  struct timespec now;
  if (clock_gettime(CLOCK_REALTIME, &now) != 0) {
    fprintf(stderr, "clock_gettime failed: %s\n", strerror(errno));
    return -1;
  }
  *out = (uint64_t)now.tv_sec * 1000000000ULL + (uint64_t)now.tv_nsec;
  return 0;
}

/*
 * Samples the clock `SAMPLES` times, requiring the sequence to be
 * non-decreasing and to advance at least once. A frozen clock returns the same
 * value for every sample and is rejected here; checking only the first sample
 * would accept it.
 */
static int sample_trajectory(const char* phase, uint64_t* first, uint64_t* last) {
  uint64_t previous = 0;
  int advanced = 0;

  for (int index = 0; index < SAMPLES; index++) {
    uint64_t now;
    if (read_clock_nanos(&now) != 0) {
      return -1;
    }
    if (index == 0) {
      *first = now;
    } else {
      if (now < previous) {
        fprintf(
            stderr,
            "%s: clock moved backwards: %" PRIu64 " -> %" PRIu64 "\n",
            phase,
            previous,
            now);
        return -1;
      }
      if (now > previous) {
        advanced = 1;
      }
    }
    previous = now;
  }

  if (!advanced) {
    fprintf(
        stderr,
        "%s: clock never advanced across %d samples (frozen at %" PRIu64 ")\n",
        phase,
        SAMPLES,
        previous);
    return -1;
  }

  *last = previous;
  return 0;
}

int main(int argc, char** argv) {
  uint64_t first = 0;
  uint64_t last = 0;

  if (argc == 1) {
    if (sample_trajectory("pre-exec", &first, &last) != 0) {
      return 1;
    }

    char handoff[32];
    if (snprintf(handoff, sizeof(handoff), "%" PRIu64, last) < 0) {
      fprintf(stderr, "pre-exec: failed to format hand-off time\n");
      return 1;
    }

    fflush(stdout);
    char* const child[] = {argv[0], handoff, NULL};
    execv(argv[0], child);
    fprintf(stderr, "execv(%s) failed: %s\n", argv[0], strerror(errno));
    return 1;
  }

  if (argc != 2) {
    fprintf(stderr, "usage: %s [pre-exec-nanos]\n", argv[0]);
    return 2;
  }

  errno = 0;
  char* end = NULL;
  const unsigned long long parsed = strtoull(argv[1], &end, 10);
  if (errno != 0 || end == argv[1] || (end != NULL && *end != '\0')) {
    fprintf(stderr, "post-exec: unparsable hand-off time %s\n", argv[1]);
    return 2;
  }
  const uint64_t before_exec = (uint64_t)parsed;

  if (sample_trajectory("post-exec", &first, &last) != 0) {
    return 1;
  }

  /*
   * The decisive check: the first read after `execve` must be strictly later
   * than the last read before it. A per-image clock that restarts at the
   * configured epoch fails here even though each image is internally
   * self-consistent and repeats identically run to run.
   */
  if (first <= before_exec) {
    fprintf(
        stderr,
        "post-exec: virtual clock did not advance across execve: "
        "pre-exec last=%" PRIu64 " post-exec first=%" PRIu64 "%s\n",
        before_exec,
        first,
        first == before_exec ? " (clock reset to the same origin)" : "");
    return 1;
  }

  printf("exec-clock-continuity-ok\n");
  return 0;
}
