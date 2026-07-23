/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/sysinfo.h>
#include <unistd.h>

struct resource_name {
  int resource;
  const char* name;
};

static const struct resource_name resources[] = {
    {RLIMIT_CPU, "CPU"},
    {RLIMIT_FSIZE, "FSIZE"},
    {RLIMIT_DATA, "DATA"},
    {RLIMIT_STACK, "STACK"},
    {RLIMIT_CORE, "CORE"},
    {RLIMIT_RSS, "RSS"},
    {RLIMIT_NPROC, "NPROC"},
    {RLIMIT_NOFILE, "NOFILE"},
    {RLIMIT_MEMLOCK, "MEMLOCK"},
    {RLIMIT_AS, "AS"},
    {RLIMIT_LOCKS, "LOCKS"},
    {RLIMIT_SIGPENDING, "SIGPENDING"},
    {RLIMIT_MSGQUEUE, "MSGQUEUE"},
    {RLIMIT_NICE, "NICE"},
    {RLIMIT_RTPRIO, "RTPRIO"},
    {RLIMIT_RTTIME, "RTTIME"},
};

static void fail(const char* operation) {
  fprintf(stderr, "%s failed: %s\n", operation, strerror(errno));
  exit(1);
}

static void require_limit(
    const char* operation,
    const struct rlimit* actual,
    const struct rlimit* expected) {
  if (actual->rlim_cur != expected->rlim_cur ||
      actual->rlim_max != expected->rlim_max) {
    fprintf(
        stderr,
        "%s mismatch: got %" PRIu64 ":%" PRIu64
        ", expected %" PRIu64 ":%" PRIu64 "\n",
        operation,
        (uint64_t)actual->rlim_cur,
        (uint64_t)actual->rlim_max,
        (uint64_t)expected->rlim_cur,
        (uint64_t)expected->rlim_max);
    exit(1);
  }
}

static rlim_t lower_soft_limit(rlim_t current, rlim_t amount) {
  if (current == RLIM_INFINITY) {
    return 4096 - amount;
  }
  if (current > amount) {
    return current - amount;
  }
  return current;
}

static void check_limit_queries(void) {
  for (size_t i = 0; i < sizeof(resources) / sizeof(resources[0]); ++i) {
    struct rlimit libc_limit = {0};
    struct rlimit syscall_limit = {0};
    struct rlimit prlimit_limit = {0};

    if (getrlimit(resources[i].resource, &libc_limit) != 0) {
      fail("getrlimit");
    }
    if (syscall(SYS_getrlimit, resources[i].resource, &syscall_limit) != 0) {
      fail("SYS_getrlimit");
    }
    if (syscall(
            SYS_prlimit64,
            0,
            resources[i].resource,
            NULL,
            &prlimit_limit) != 0) {
      fail("SYS_prlimit64 query");
    }

    require_limit("getrlimit/syscall", &syscall_limit, &libc_limit);
    require_limit("getrlimit/prlimit64", &prlimit_limit, &libc_limit);
    printf(
        "limit %s %" PRIu64 ":%" PRIu64 "\n",
        resources[i].name,
        (uint64_t)libc_limit.rlim_cur,
        (uint64_t)libc_limit.rlim_max);
  }
}

static void check_limit_mutations(void) {
  struct rlimit original = {0};
  struct rlimit changed = {0};
  struct rlimit observed = {0};
  struct rlimit previous = {0};

  if (getrlimit(RLIMIT_NOFILE, &original) != 0) {
    fail("getrlimit before mutation");
  }

  changed = original;
  changed.rlim_cur = lower_soft_limit(original.rlim_cur, 1);
  if (setrlimit(RLIMIT_NOFILE, &changed) != 0) {
    fail("setrlimit libc");
  }
  if (syscall(SYS_getrlimit, RLIMIT_NOFILE, &observed) != 0) {
    fail("getrlimit after libc setrlimit");
  }
  require_limit("libc setrlimit", &observed, &changed);
  printf(
      "setrlimit libc %" PRIu64 ":%" PRIu64 "\n",
      (uint64_t)observed.rlim_cur,
      (uint64_t)observed.rlim_max);
  if (setrlimit(RLIMIT_NOFILE, &original) != 0) {
    fail("restore after libc setrlimit");
  }

  changed = original;
  changed.rlim_cur = lower_soft_limit(original.rlim_cur, 2);
  if (syscall(SYS_setrlimit, RLIMIT_NOFILE, &changed) != 0) {
    fail("SYS_setrlimit");
  }
  if (getrlimit(RLIMIT_NOFILE, &observed) != 0) {
    fail("getrlimit after SYS_setrlimit");
  }
  require_limit("syscall setrlimit", &observed, &changed);
  printf(
      "setrlimit syscall %" PRIu64 ":%" PRIu64 "\n",
      (uint64_t)observed.rlim_cur,
      (uint64_t)observed.rlim_max);
  if (setrlimit(RLIMIT_NOFILE, &original) != 0) {
    fail("restore after SYS_setrlimit");
  }

  changed = original;
  changed.rlim_cur = lower_soft_limit(original.rlim_cur, 3);
  if (syscall(
          SYS_prlimit64,
          getpid(),
          RLIMIT_NOFILE,
          &changed,
          &previous) != 0) {
    fail("SYS_prlimit64 mutation");
  }
  require_limit("prlimit64 previous", &previous, &original);
  if (syscall(SYS_prlimit64, 0, RLIMIT_NOFILE, NULL, &observed) != 0) {
    fail("SYS_prlimit64 after mutation");
  }
  require_limit("prlimit64 mutation", &observed, &changed);
  printf(
      "prlimit64 old=%" PRIu64 ":%" PRIu64 " new=%" PRIu64 ":%" PRIu64
      "\n",
      (uint64_t)previous.rlim_cur,
      (uint64_t)previous.rlim_max,
      (uint64_t)observed.rlim_cur,
      (uint64_t)observed.rlim_max);
  if (syscall(SYS_prlimit64, 0, RLIMIT_NOFILE, &original, NULL) != 0) {
    fail("restore after SYS_prlimit64");
  }
}

// Every rusage field except ru_maxrss must be a deterministic zero. When
// expect_maxrss is set, ru_maxrss must be positive (the guest's peak RSS);
// otherwise it must also be zero (e.g. RUSAGE_CHILDREN with no children).
static void check_rusage(int who, const char* name, int expect_maxrss) {
  struct rusage usage;
  memset(&usage, 0xa5, sizeof(usage));
  if (getrusage(who, &usage) != 0) {
    fail("getrusage");
  }

  if (expect_maxrss) {
    if (usage.ru_maxrss <= 0) {
      fprintf(
          stderr,
          "getrusage %s reported non-positive ru_maxrss %ld\n",
          name,
          (long)usage.ru_maxrss);
      exit(1);
    }
    // Clear the field we allow to be nonzero so the byte scan below can prove
    // everything else is a deterministic zero.
    usage.ru_maxrss = 0;
  }

  const unsigned char* bytes = (const unsigned char*)&usage;
  for (size_t i = 0; i < sizeof(usage); ++i) {
    if (bytes[i] != 0) {
      fprintf(
          stderr,
          "getrusage %s byte %zu was not deterministic zero\n",
          name,
          i);
      exit(1);
    }
  }
  printf("rusage %s %s\n", name, expect_maxrss ? "maxrss" : "zero");
}

static void check_rusage_errors(void) {
  struct rusage usage = {0};
  errno = 0;
  if (getrusage(1234, &usage) != -1 || errno != EINVAL) {
    fprintf(stderr, "invalid getrusage selector did not return EINVAL\n");
    exit(1);
  }
  errno = 0;
  if (syscall(SYS_getrusage, RUSAGE_SELF, NULL) != -1 || errno != EFAULT) {
    fprintf(stderr, "null getrusage destination did not return EFAULT\n");
    exit(1);
  }
}

static void check_sysinfo(void) {
  struct sysinfo info = {0};
  if (sysinfo(&info) != 0) {
    fail("sysinfo");
  }
  printf(
      "sysinfo uptime=%ld loads=%lu,%lu,%lu ram=%lu,%lu,%lu,%lu "
      "swap=%lu,%lu procs=%u high=%lu,%lu unit=%u\n",
      info.uptime,
      info.loads[0],
      info.loads[1],
      info.loads[2],
      info.totalram,
      info.freeram,
      info.sharedram,
      info.bufferram,
      info.totalswap,
      info.freeswap,
      info.procs,
      info.totalhigh,
      info.freehigh,
      info.mem_unit);
}

int main(void) {
  check_limit_queries();
  check_limit_mutations();
  check_rusage(RUSAGE_SELF, "self", 1);
  check_rusage(RUSAGE_THREAD, "thread", 1);
  check_rusage(RUSAGE_CHILDREN, "children", 0);
  check_rusage_errors();
  check_sysinfo();
  return 0;
}
