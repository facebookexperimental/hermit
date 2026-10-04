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
 * So this guard deliberately does the things such a check does not:
 *   1. it reads REPEATEDLY inside each generation, not once,
 *   2. it carries the previous generation's readings ACROSS AN EXEC and
 *      asserts continuity over that boundary,
 *   3. it brackets fixed amounts of guest work with reads, so committed
 *      progress is visible in the trajectory, and
 *   4. it samples from threads that run one at a time (each is joined before
 *      the next starts), so a per-thread clock origin or reset is visible
 *      while the emitted order stays deterministic without a scheduler.
 *
 * It must also not be satisfiable by making time COARSER -- that would be the
 * defect guarding itself. Note where that property actually comes from: every
 * leg below (round origin, strict advance, cross-exec continuity, distinct
 * per-exec origins, cross-thread continuity) is BROKEN by coarsening rather
 * than satisfied by it, so coarsening can never buy a pass here.
 *
 * There is deliberately NO absolute nanosecond floor on the gap between reads.
 * An earlier draft asserted one and it was wrong: the per-read advance is a
 * function of the run configuration, measured at ~10us under
 * `--strict --base-env=minimal` and ~5ms under the portable verify profile.
 * A constant calibrated on one of those fails the other, which would make this
 * guard a source of false reds rather than a detector of the defect. For the
 * same reason the work segments only assert that time advanced: exact costs
 * depend on the epoch, the timeslice, and the backend, none of which this
 * program controls. (Record mode currently refuses its clock_gettime.) Exact
 * virtual-time relations between segments belong to callers that pin those
 * inputs and compare whole trajectories.
 *
 * Which clock carries which claim follows Linux. Every leg above reads
 * CLOCK_MONOTONIC, which Linux keeps system-wide: it continues across exec and
 * threads and never steps, but its absolute value (time since an unspecified
 * point) is not a contract. Only CLOCK_REALTIME is defined relative to the Unix
 * epoch, so the two `realtime` records below, one after the opening
 * MONOTONIC reads and one after the threads, are the only reads a caller may
 * compare against a configured `--epoch`. No record relates the two clocks.
 * Hermit currently answers every clockid with the same epoch-anchored value
 * (handle_clock_gettime in detcore/src/syscalls/time.rs ignores the clockid);
 * that is a known deviation from Linux, tracked as TaskGraph task
 * hermit-clock-gettime-ignores-clockid, and nothing here may depend on it.
 *
 * Every stdout line before the final verdict is one closed trajectory record:
 *   sample gen=G source=main index=I ns=T
 *   realtime gen=G phase=open ns=T
 *   work gen=G index=I iterations=N before_ns=T after_ns=T checksum=X
 *   sample gen=G source=thread index=I thread=K ns=T
 *   realtime gen=G phase=close ns=T
 *   gen=G first=T last=T min_delta=D
 * Records appear in the order the reads happened. Within each clock, every
 * timestamp in emission order is strictly greater than that clock's previous
 * one, including across exec and thread boundaries; the fixture itself asserts
 * this for CLOCK_MONOTONIC and leaves the REALTIME anchors to callers.
 *
 * Usage (the guest re-execs itself; the arguments are internal):
 *   clock_exec_continuity [generation prev_last_ns gen0_first_ns]
 */

#include <errno.h>
#include <inttypes.h>
#include <limits.h>
#include <pthread.h>
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
/* Serialized sampling threads per generation, and reads by each. */
enum { THREADS_PER_GENERATION = 2 };
enum { READS_PER_THREAD = 4 };
/* Work segments: none, one unit twice, then two units. Under a pinned epoch,
 * parse_clock_trajectory in scripts/build-buck-release.rs requires equal work
 * to cost equal virtual time and more work to cost strictly more. */
enum { WORK_SEGMENTS = 4 };
static const long WORK_ITERATIONS[WORK_SEGMENTS] = {0, 100000, 100000, 200000};
#define NS_PER_SEC 1000000000LL

static int64_t read_clockid_ns(clockid_t clock) {
  struct timespec now;
  if (clock_gettime(clock, &now) != 0) {
    fprintf(stderr, "clock_gettime failed: %s\n", strerror(errno));
    exit(1);
  }
  return (int64_t)now.tv_sec * NS_PER_SEC + now.tv_nsec;
}

/* The continuity trajectory. */
static int64_t read_clock_ns(void) {
  return read_clockid_ns(CLOCK_MONOTONIC);
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

/* A fixed, data-dependent loop of conditional branches. The volatile sink
 * keeps -O0 or a smarter compiler from deleting it. */
static volatile uint64_t work_sink;

static uint64_t do_work(long iterations) {
  uint64_t value = 27;
  for (long i = 0; i < iterations; i++) {
    if (value & 1) {
      value = value * 3 + 1;
    } else {
      value >>= 1;
    }
  }
  work_sink = value;
  return value;
}

static int64_t thread_readings[READS_PER_THREAD];

static void* sample_thread(void* unused) {
  (void)unused;
  for (int i = 0; i < READS_PER_THREAD; i++) {
    thread_readings[i] = read_clock_ns();
  }
  return NULL;
}

/* Fails the generation unless `now` is strictly after `before`. */
static void
require_after(long generation, const char* what, int64_t now, int64_t before) {
  if (now <= before) {
    fprintf(
        stderr,
        "FAIL gen=%ld %s read %" PRId64
        " is not after the preceding read %" PRId64
        ": virtual time is frozen, coarsened, reset, or went backwards\n",
        generation,
        what,
        now,
        before);
    exit(1);
  }
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
    fprintf(
        stderr, "usage: %s [generation prev_last_ns gen0_first_ns]\n", argv[0]);
    return 1;
  }

  int64_t readings[READS_PER_GENERATION];
  for (int i = 0; i < READS_PER_GENERATION; i++) {
    readings[i] = read_clock_ns();
  }

  const int64_t first = readings[0];

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
          "FAIL gen=%ld read %d (%" PRId64
          ") did not advance past read %d (%" PRId64
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

  for (int i = 0; i < READS_PER_GENERATION; i++) {
    printf(
        "sample gen=%ld source=main index=%d ns=%" PRId64 "\n",
        generation,
        i,
        readings[i]);
  }
  /* The epoch anchor, read after the opening MONOTONIC reads so that the
   * first clock read of every process is still the one leg (1) checks. */
  printf(
      "realtime gen=%ld phase=open ns=%" PRId64 "\n",
      generation,
      read_clockid_ns(CLOCK_REALTIME));

  /*
   * (6) Committed work advances time. Each segment is bracketed by reads and
   * must end strictly after it began, and after the previous segment ended.
   */
  int64_t last = readings[READS_PER_GENERATION - 1];
  for (int i = 0; i < WORK_SEGMENTS; i++) {
    const int64_t before = read_clock_ns();
    require_after(generation, "work-start", before, last);
    const uint64_t checksum = do_work(WORK_ITERATIONS[i]);
    const int64_t after = read_clock_ns();
    require_after(generation, "work-end", after, before);
    printf(
        "work gen=%ld index=%d iterations=%ld before_ns=%" PRId64
        " after_ns=%" PRId64 " checksum=%" PRIu64 "\n",
        generation,
        i,
        WORK_ITERATIONS[i],
        before,
        after,
        checksum);
    last = after;
  }

  /*
   * (7) Threads share the process clock. Each thread runs alone (it is joined
   * before the next is created), so its reads must continue the trajectory:
   * a per-thread origin, freeze, or reset shows up as a non-advancing read.
   */
  for (int thread = 0; thread < THREADS_PER_GENERATION; thread++) {
    pthread_t handle;
    int error = pthread_create(&handle, NULL, sample_thread, NULL);
    if (error != 0) {
      fprintf(stderr, "pthread_create failed: %s\n", strerror(error));
      return 1;
    }
    error = pthread_join(handle, NULL);
    if (error != 0) {
      fprintf(stderr, "pthread_join failed: %s\n", strerror(error));
      return 1;
    }
    for (int i = 0; i < READS_PER_THREAD; i++) {
      require_after(generation, "thread", thread_readings[i], last);
      printf(
          "sample gen=%ld source=thread index=%d thread=%d ns=%" PRId64 "\n",
          generation,
          i,
          thread,
          thread_readings[i]);
      last = thread_readings[i];
    }
  }

  printf(
      "realtime gen=%ld phase=close ns=%" PRId64 "\n",
      generation,
      read_clockid_ns(CLOCK_REALTIME));

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

  /* Flush before exec: the replacement image does not inherit our stdio buffer.
   */
  fflush(stdout);

  char* next_argv[] = {argv[0], next_generation, last_text, first_text, NULL};
  execv("/proc/self/exe", next_argv);
  fprintf(stderr, "execv failed: %s\n", strerror(errno));
  return 1;
}
