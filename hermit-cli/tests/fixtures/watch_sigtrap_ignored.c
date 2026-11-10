// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

// A watched store after the guest ignores SIGTRAP. Linux reports a hardware
// watchpoint hit by forcing a SIGTRAP, which resets an ignored SIGTRAP's
// action to the default, so the hit changes what the guest set: `hermit run
// --watch` must stop the run there rather than go on with a guest it changed.

#include <signal.h>
#include <stdio.h>

volatile long watched;

int main(void) {
  signal(SIGTRAP, SIG_IGN);
  watched = 1;
  printf("%ld\n", watched);
  return 0;
}
