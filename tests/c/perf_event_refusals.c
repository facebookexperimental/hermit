/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * The three perf_event_open refusals (hardware, software and watchpoint
 * events), each member the unchanged program it was before (its main renamed
 * by the preprocessor), run in a fresh child process (see probe_family.h).
 * This one program replaces the
 * c-programs/perf-event-{hardware,software,watchpoint}-enosys rows; the member
 * sources stay the only copy of each check, and
 * hermit-cli/tests/perf_event_refusal.rs still compiles them directly.
 */

#define _GNU_SOURCE

#include "probe_family.h"

#define main perf_event_hardware_enosys_main
#include "perf_event_hardware_enosys.c"
#undef main

#define main perf_event_software_enosys_main
#include "perf_event_software_enosys.c"
#undef main

#define main perf_event_watchpoint_enosys_main
#include "perf_event_watchpoint_enosys.c"
#undef main

int main(void) {
  const probe_member members[] = {
      perf_event_hardware_enosys_main,
      perf_event_software_enosys_main,
      perf_event_watchpoint_enosys_main,
  };
  return run_probe_family(members, sizeof(members) / sizeof(members[0]));
}
