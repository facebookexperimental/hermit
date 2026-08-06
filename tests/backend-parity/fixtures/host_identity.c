/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * host_identity — backend-parity contract for the virtualized HOST IDENTITY:
 * uname(2), gethostname(2), sysinfo(2), and the visible CPU count.
 *
 * WHY THIS EMITS VALUES RATHER THAN A VERDICT
 * -------------------------------------------
 * The neighbouring uname_identity fixture prints `uname ok=%d`, and
 * cpu_virtualization prints `cpu-virtualization-ok`. Backend parity is decided
 * by comparing stdout, so a constant success token makes every passing backend
 * look identical NO MATTER WHAT IT OBSERVED, and a divergence has to be
 * re-derived by hand from stderr. That is not hypothetical here: this repo
 * already records that "DBI forwards the host uname(2) nodename instead of
 * pinning it ... DBI leaks the real host name" — a leak a boolean cannot show.
 *
 * So every observation below is PRINTED, and the checks are additional rather
 * than a replacement. A backend that leaks a real host value now differs in the
 * compared stream, naming the offending field directly.
 *
 * ASSERTED vs RECORDED
 * --------------------
 * Asserted: the fields measured to be genuinely virtualized and stable.
 * Recorded but NOT asserted: fields that are stable yet still carry host
 * topology. `sysconf(_SC_NPROCESSORS_ONLN)`, /sys/devices/system/cpu/online and
 * /proc/cpuinfo all report the REAL host CPU count inside the container (316 on
 * the development host) while sched_getaffinity correctly reports 1. That is an
 * open leak, not a property this fixture should bless by asserting the leaked
 * value — but it is printed so the leak is visible and so any change to it
 * shows up as a parity diff instead of passing silently.
 */

#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <sys/sysinfo.h>
#include <sys/utsname.h>
#include <unistd.h>

/* Hermit's documented virtual container identity. */
#define VIRT_NODENAME "hermetic-container.local"
#define VIRT_RELEASE  "5.2.0"
#define VIRT_SYSNAME  "Linux"
#define VIRT_MACHINE  "x86_64"
#define VIRT_TOTALRAM 1000000000UL
#define VIRT_PROCS    1U
#define VIRT_UPTIME   120L
#define VIRT_AFFINITY 1

static int fail(const char* field, const char* got) {
  fprintf(stderr, "host_identity: %s leaked or diverged: %s\n", field, got);
  return 1;
}

int main(void) {
  int bad = 0;

  struct utsname u;
  if (uname(&u) != 0) {
    return fail("uname", "syscall failed");
  }
  printf("uname.sysname=%s\n", u.sysname);
  printf("uname.nodename=%s\n", u.nodename);
  printf("uname.release=%s\n", u.release);
  printf("uname.machine=%s\n", u.machine);
  if (strcmp(u.sysname, VIRT_SYSNAME) != 0) bad |= fail("uname.sysname", u.sysname);
  if (strcmp(u.nodename, VIRT_NODENAME) != 0) bad |= fail("uname.nodename", u.nodename);
  if (strcmp(u.release, VIRT_RELEASE) != 0) bad |= fail("uname.release", u.release);
  if (strcmp(u.machine, VIRT_MACHINE) != 0) bad |= fail("uname.machine", u.machine);

  char host[256] = {0};
  if (gethostname(host, sizeof(host) - 1) != 0) {
    return fail("gethostname", "syscall failed");
  }
  printf("gethostname=%s\n", host);
  if (strcmp(host, VIRT_NODENAME) != 0) bad |= fail("gethostname", host);

  struct sysinfo si;
  if (sysinfo(&si) != 0) {
    return fail("sysinfo", "syscall failed");
  }
  printf("sysinfo.totalram=%lu\n", (unsigned long)si.totalram);
  printf("sysinfo.procs=%u\n", si.procs);
  printf("sysinfo.uptime=%ld\n", (long)si.uptime);
  if ((unsigned long)si.totalram != VIRT_TOTALRAM) bad |= fail("sysinfo.totalram", "host memory size");
  if (si.procs != VIRT_PROCS) bad |= fail("sysinfo.procs", "host process count");
  if ((long)si.uptime != VIRT_UPTIME) bad |= fail("sysinfo.uptime", "host uptime");

  cpu_set_t set;
  CPU_ZERO(&set);
  if (sched_getaffinity(0, sizeof(set), &set) != 0) {
    return fail("sched_getaffinity", "syscall failed");
  }
  printf("affinity_count=%d\n", CPU_COUNT(&set));
  if (CPU_COUNT(&set) != VIRT_AFFINITY) bad |= fail("affinity_count", "host CPU mask");

  /* RECORDED, NOT ASSERTED: still leaks host topology (see header). */
  printf("sysconf_nprocs_onln=%ld\n", sysconf(_SC_NPROCESSORS_ONLN));

  return bad ? 1 : 0;
}
