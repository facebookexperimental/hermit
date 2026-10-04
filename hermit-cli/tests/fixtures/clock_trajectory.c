/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * clock_trajectory: print the guest's view of time from the start of main,
 * before and after an exec of itself.
 *
 * The first sample is taken before main does anything else, so its values
 * show how much virtual time the guest was charged before main. For the
 * LiteInst backend, that includes the time around the runtime preload
 * constructor, which runs again after the exec. Later samples show that time
 * keeps advancing, including across the exec. The fixture only prints
 * values; the Rust test decides what they must be. See
 * https://github.com/rrnewton/hermit/issues/3338.
 */

#include <stdio.h>
#include <string.h>
#include <sys/sysinfo.h>
#include <time.h>
#include <unistd.h>

#define SAMPLES 5
#define AFTER_EXEC "after-exec"

int main(int argc, char** argv) {
  struct timespec monotonic[SAMPLES];
  long uptime[SAMPLES];

  for (int i = 0; i < SAMPLES; i++) {
    struct sysinfo info;
    if (clock_gettime(CLOCK_MONOTONIC, &monotonic[i]) != 0) {
      perror("clock_gettime(CLOCK_MONOTONIC)");
      return 1;
    }
    if (sysinfo(&info) != 0) {
      perror("sysinfo");
      return 1;
    }
    uptime[i] = info.uptime;
  }

  int after_exec = argc > 1 && strcmp(argv[1], AFTER_EXEC) == 0;
  int first = after_exec ? SAMPLES : 0;
  for (int i = 0; i < SAMPLES; i++) {
    printf(
        "sample=%d monotonic_ns=%lld uptime=%ld\n",
        first + i,
        (long long)monotonic[i].tv_sec * 1000000000LL + monotonic[i].tv_nsec,
        uptime[i]);
  }
  if (after_exec) {
    return 0;
  }

  if (fflush(stdout) != 0) {
    perror("fflush");
    return 1;
  }
  execl(argv[0], argv[0], AFTER_EXEC, (char*)NULL);
  perror("execl");
  return 1;
}
