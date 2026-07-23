/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/time.h>
#include <time.h>

struct clock_case {
  const char* name;
  clockid_t id;
  int expected_sleep_error;
};

static int64_t timespec_ns(const struct timespec* value) {
  return (int64_t)value->tv_sec * 1000000000LL + value->tv_nsec;
}

static int check_clock(const struct clock_case* test) {
  struct timespec before;
  if (clock_gettime(test->id, &before) != 0) {
    fprintf(stderr, "%s clock_gettime failed: %s\n", test->name, strerror(errno));
    return 1;
  }

  const struct timespec request = {
      .tv_sec = 0,
      .tv_nsec = 1000,
  };
  const int sleep_error = clock_nanosleep(test->id, 0, &request, NULL);
  if (sleep_error != test->expected_sleep_error) {
    fprintf(
        stderr,
        "%s clock_nanosleep result mismatch: got=%d expected=%d (%s)\n",
        test->name,
        sleep_error,
        test->expected_sleep_error,
        strerror(sleep_error));
    return 1;
  }

  struct timespec after;
  if (clock_gettime(test->id, &after) != 0) {
    fprintf(stderr, "%s second clock_gettime failed: %s\n", test->name, strerror(errno));
    return 1;
  }

  const int64_t delta = timespec_ns(&after) - timespec_ns(&before);
  const int64_t minimum_delta = sleep_error == 0 ? request.tv_nsec : 0;
  if (delta < minimum_delta) {
    fprintf(
        stderr,
        "%s moved backwards or ignored the sleep: %lld ns\n",
        test->name,
        (long long)delta);
    return 1;
  }

  printf(
      "%s gettime=%lld.%09ld nanosleep_rc=%d delta_ns=%lld\n",
      test->name,
      (long long)before.tv_sec,
      before.tv_nsec,
      sleep_error,
      (long long)delta);
  return 0;
}

static int check_gettimeofday_consistency(void) {
  struct timespec before;
  struct timespec after;
  struct timeval middle;

  if (clock_gettime(CLOCK_REALTIME, &before) != 0) {
    perror("clock_gettime before gettimeofday");
    return 1;
  }
  if (gettimeofday(&middle, NULL) != 0) {
    perror("gettimeofday");
    return 1;
  }
  if (clock_gettime(CLOCK_REALTIME, &after) != 0) {
    perror("clock_gettime after gettimeofday");
    return 1;
  }

  const int64_t before_ns = timespec_ns(&before);
  const int64_t middle_ns =
      (int64_t)middle.tv_sec * 1000000000LL + (int64_t)middle.tv_usec * 1000LL;
  const int64_t after_ns = timespec_ns(&after);
  if (middle_ns < before_ns || middle_ns > after_ns) {
    fprintf(
        stderr,
        "gettimeofday escaped realtime bounds: before=%lld middle=%lld after=%lld\n",
        (long long)before_ns,
        (long long)middle_ns,
        (long long)after_ns);
    return 1;
  }

  printf(
      "gettimeofday consistent offset_before_ns=%lld offset_after_ns=%lld\n",
      (long long)(middle_ns - before_ns),
      (long long)(after_ns - middle_ns));
  return 0;
}

int main(void) {
  static const struct clock_case clocks[] = {
      {"CLOCK_REALTIME", CLOCK_REALTIME, 0},
      {"CLOCK_MONOTONIC", CLOCK_MONOTONIC, 0},
      {"CLOCK_PROCESS_CPUTIME_ID", CLOCK_PROCESS_CPUTIME_ID, 0},
      {"CLOCK_THREAD_CPUTIME_ID", CLOCK_THREAD_CPUTIME_ID, EINVAL},
      {"CLOCK_BOOTTIME", CLOCK_BOOTTIME, 0},
  };

  for (size_t i = 0; i < sizeof(clocks) / sizeof(clocks[0]); ++i) {
    if (check_clock(&clocks[i]) != 0) {
      return 1;
    }
  }
  if (check_gettimeofday_consistency() != 0) {
    return 1;
  }

  puts("clock matrix success");
  return 0;
}
