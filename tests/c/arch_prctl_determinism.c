/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <asm/prctl.h>
#include <cpuid.h>
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef ARCH_GET_XCOMP_SUPP
#define ARCH_GET_XCOMP_SUPP 0x1021
#define ARCH_GET_XCOMP_PERM 0x1022
#define ARCH_REQ_XCOMP_PERM 0x1023
#define ARCH_GET_XCOMP_GUEST_PERM 0x1024
#define ARCH_REQ_XCOMP_GUEST_PERM 0x1025
#endif

#ifndef ARCH_SHSTK_ENABLE
#define ARCH_SHSTK_ENABLE 0x5001
#define ARCH_SHSTK_DISABLE 0x5002
#define ARCH_SHSTK_LOCK 0x5003
#define ARCH_SHSTK_UNLOCK 0x5004
#define ARCH_SHSTK_STATUS 0x5005
#define ARCH_SHSTK_SHSTK (1ULL << 0)
#endif

static int failures;

static void expect_result(const char* name, long actual, long expected) {
  if (actual != expected) {
    fprintf(
        stderr,
        "%s: actual=%ld expected=%ld errno=%d\n",
        name,
        actual,
        expected,
        errno);
    failures++;
  }
}

static void expect_errno(
    const char* name,
    int command,
    unsigned long argument,
    int expected_errno) {
  errno = 0;
  long result = syscall(SYS_arch_prctl, command, argument);
  if (result != -1 || errno != expected_errno) {
    fprintf(
        stderr,
        "%s: result=%ld errno=%d expected=-1/%d\n",
        name,
        result,
        errno,
        expected_errno);
    failures++;
  }
}

static void expect_cpuid_instruction(void) {
  unsigned int signature = 0;
  if (__get_cpuid_max(0, &signature) == 0) {
    fprintf(stderr, "CPUID instruction unexpectedly unavailable\n");
    failures++;
  }
}

static int host_cpuid_mode(void) {
  expect_result(
      "host ARCH_GET_CPUID", syscall(SYS_arch_prctl, ARCH_GET_CPUID, 0), 1);
  expect_errno("host ARCH_SET_CPUID(0)", ARCH_SET_CPUID, 0, EPERM);
  expect_result(
      "host enabled ARCH_GET_CPUID",
      syscall(SYS_arch_prctl, ARCH_GET_CPUID, 0),
      1);

  expect_cpuid_instruction();

  expect_result(
      "host ARCH_SET_CPUID(1)", syscall(SYS_arch_prctl, ARCH_SET_CPUID, 1), 0);
  return failures == 0 ? 0 : 1;
}

int main(int argc, char** argv) {
  if (argc == 2 && strcmp(argv[1], "--host-cpuid") == 0) {
    return host_cpuid_mode();
  }

  unsigned long fs = 0;
  unsigned long original_gs = 0;
  unsigned long observed_gs = 0;

  expect_result("ARCH_GET_FS", syscall(SYS_arch_prctl, ARCH_GET_FS, &fs), 0);
  expect_result("ARCH_SET_FS", syscall(SYS_arch_prctl, ARCH_SET_FS, fs), 0);
  expect_result(
      "ARCH_GET_GS", syscall(SYS_arch_prctl, ARCH_GET_GS, &original_gs), 0);

  unsigned long requested_gs = (unsigned long)(uintptr_t)&observed_gs;
  expect_result(
      "ARCH_SET_GS", syscall(SYS_arch_prctl, ARCH_SET_GS, requested_gs), 0);
  expect_result(
      "changed ARCH_GET_GS",
      syscall(SYS_arch_prctl, ARCH_GET_GS, &observed_gs),
      0);
  expect_result("changed GS value", (long)observed_gs, (long)requested_gs);
  expect_result(
      "restore ARCH_SET_GS",
      syscall(SYS_arch_prctl, ARCH_SET_GS, original_gs),
      0);

  errno = 0;
  long cpuid_state = syscall(SYS_arch_prctl, ARCH_GET_CPUID, 0);
  if (cpuid_state == 0) {
    expect_errno("ARCH_SET_CPUID(1)", ARCH_SET_CPUID, 1, EPERM);
    expect_result(
        "ARCH_SET_CPUID(0)", syscall(SYS_arch_prctl, ARCH_SET_CPUID, 0), 0);
    expect_result(
        "normalized ARCH_GET_CPUID",
        syscall(SYS_arch_prctl, ARCH_GET_CPUID, 0),
        0);
  } else if (cpuid_state == 1) {
    expect_cpuid_instruction();
    expect_result(
        "fallback ARCH_SET_CPUID(1)",
        syscall(SYS_arch_prctl, ARCH_SET_CPUID, 1),
        0);
    expect_result(
        "fallback ARCH_GET_CPUID",
        syscall(SYS_arch_prctl, ARCH_GET_CPUID, 0),
        1);
  } else if (cpuid_state != -1 || errno != ENODEV) {
    fprintf(
        stderr,
        "ARCH_GET_CPUID: result=%ld errno=%d expected=0,1,or ENODEV\n",
        cpuid_state,
        errno);
    failures++;
  }

  unsigned long xcomp = ~0UL;
  expect_result(
      "ARCH_GET_XCOMP_SUPP",
      syscall(SYS_arch_prctl, ARCH_GET_XCOMP_SUPP, &xcomp),
      0);
  expect_result("normalized ARCH_GET_XCOMP_SUPP", (long)xcomp, 0);
  expect_errno("null ARCH_GET_XCOMP_SUPP", ARCH_GET_XCOMP_SUPP, 0, EFAULT);

  xcomp = ~0UL;
  expect_result(
      "ARCH_GET_XCOMP_PERM",
      syscall(SYS_arch_prctl, ARCH_GET_XCOMP_PERM, &xcomp),
      0);
  expect_result("normalized ARCH_GET_XCOMP_PERM", (long)xcomp, 0);

  xcomp = ~0UL;
  expect_result(
      "ARCH_GET_XCOMP_GUEST_PERM",
      syscall(SYS_arch_prctl, ARCH_GET_XCOMP_GUEST_PERM, &xcomp),
      0);
  expect_result("normalized ARCH_GET_XCOMP_GUEST_PERM", (long)xcomp, 0);
  expect_errno("ARCH_REQ_XCOMP_PERM", ARCH_REQ_XCOMP_PERM, 17, EINVAL);
  expect_errno(
      "ARCH_REQ_XCOMP_GUEST_PERM", ARCH_REQ_XCOMP_GUEST_PERM, 17, EINVAL);

  unsigned long shstk = ~0UL;
  expect_result(
      "ARCH_SHSTK_STATUS",
      syscall(SYS_arch_prctl, ARCH_SHSTK_STATUS, &shstk),
      0);
  expect_result("normalized ARCH_SHSTK_STATUS", (long)shstk, 0);
  expect_errno("null ARCH_SHSTK_STATUS", ARCH_SHSTK_STATUS, 0, EFAULT);
  expect_result(
      "ARCH_SHSTK_DISABLE",
      syscall(SYS_arch_prctl, ARCH_SHSTK_DISABLE, ARCH_SHSTK_SHSTK),
      0);
  expect_errno("zero ARCH_SHSTK_DISABLE", ARCH_SHSTK_DISABLE, 0, EINVAL);
  expect_errno(
      "invalid ARCH_SHSTK_DISABLE", ARCH_SHSTK_DISABLE, 1UL << 63, EINVAL);
  expect_errno(
      "ARCH_SHSTK_ENABLE", ARCH_SHSTK_ENABLE, ARCH_SHSTK_SHSTK, EINVAL);
  expect_errno("ARCH_SHSTK_LOCK", ARCH_SHSTK_LOCK, ARCH_SHSTK_SHSTK, EINVAL);
  expect_errno(
      "ARCH_SHSTK_UNLOCK", ARCH_SHSTK_UNLOCK, ARCH_SHSTK_SHSTK, EINVAL);

  expect_errno("unknown arch_prctl", 0x7fffffff, 0, EINVAL);

  if (failures != 0) {
    return 1;
  }

  puts("arch-prctl-deterministic");
  return 0;
}
