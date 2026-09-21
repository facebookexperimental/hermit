/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Contract: the process STARTUP surface is deterministic.
 *
 * Every guest reads this surface before main() runs -- the kernel hands over
 * argv, envp, and the auxiliary vector, and the dynamic loader and libc consume
 * them immediately. A divergence here perturbs the entire rest of the run, and
 * it surfaces as a symptom somewhere much later, which is exactly why it is
 * worth pinning as a contract rather than rediscovering as a bug.
 *
 * AT_RANDOM deserves special mention: the kernel places sixteen fresh random
 * bytes there for glibc to seed the stack canary and the pointer guard. It is a
 * randomness source that arrives BEFORE any syscall the tool could intercept,
 * so it belongs to the same family as the RDRAND hole. If it is not
 * determinized, every run differs from the first instruction.
 *
 * WHAT IS ASSERTED, AND WHAT IS DELIBERATELY NOT.
 *
 * This program prints; it does not compare against golden constants. Identity
 * is asserted by the harness running it twice under `verify` and requiring the
 * two runs to match, and by running it across backends. Values may legitimately
 * differ between MACHINES (AT_HWCAP depends on the CPU); they must not differ
 * between two runs of the same program on the same machine.
 *
 * THE vDSO BASE IS PRINTED RAW AND MUST STAY THAT WAY. AT_SYSINFO_EHDR is a
 * mapping address. It is tempting to normalise it so backends agree -- do not.
 * If two backends place the vDSO differently, that difference IS the finding,
 * and normalising it away converts a real cross-backend divergence into a fake
 * pass. The same applies to the stack addresses below.
 *
 * The program also BRANCHES on auxv values rather than only printing them. A
 * printed difference is caught only by output comparison; a branch makes the
 * divergence change control flow, which is what actually happens to real guests
 * (glibc branches on AT_HWCAP to select string routines, and on AT_SECURE to
 * decide whether to honour the environment). The branch results are printed as
 * their own lines so the comparison covers the DECISION, not just the input.
 */

/* _GNU_SOURCE comes from the manifest's cflags; defining it here too is a
 * redefinition error under -Werror. */

#include <elf.h>
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <sys/auxv.h>

extern char** environ;

/* Every auxv key glibc and the loader actually consume, named so a divergence
 * report says which one moved rather than printing a bare number. */
struct auxv_key {
  unsigned long type;
  const char* name;
  /* Addresses vary legitimately with mapping layout; values do not. Printing
   * the class alongside makes a diff self-describing. */
  const char* kind;
};

static const struct auxv_key kKeys[] = {
    {AT_PAGESZ, "AT_PAGESZ", "value"},
    {AT_CLKTCK, "AT_CLKTCK", "value"},
    {AT_PHENT, "AT_PHENT", "value"},
    {AT_PHNUM, "AT_PHNUM", "value"},
    {AT_FLAGS, "AT_FLAGS", "value"},
    {AT_UID, "AT_UID", "value"},
    {AT_EUID, "AT_EUID", "value"},
    {AT_GID, "AT_GID", "value"},
    {AT_EGID, "AT_EGID", "value"},
    {AT_SECURE, "AT_SECURE", "value"},
    {AT_HWCAP, "AT_HWCAP", "value"},
    {AT_HWCAP2, "AT_HWCAP2", "value"},
    {AT_MINSIGSTKSZ, "AT_MINSIGSTKSZ", "value"},
    /* Addresses. Printed RAW on purpose -- see the header comment. */
    {AT_PHDR, "AT_PHDR", "address"},
    {AT_BASE, "AT_BASE", "address"},
    {AT_ENTRY, "AT_ENTRY", "address"},
    {AT_SYSINFO_EHDR, "AT_SYSINFO_EHDR(vdso)", "address"},
    {AT_PLATFORM, "AT_PLATFORM", "address"},
    {AT_EXECFN, "AT_EXECFN", "address"},
    {AT_RANDOM, "AT_RANDOM", "address"},
};

int main(int argc, char** argv) {
  /* --- argv, in order, plus its placement ------------------------------- */
  printf("ARGC %d\n", argc);
  for (int i = 0; i < argc; i++) {
    printf("ARGV %d %s\n", i, argv[i]);
  }

  /* --- environ, IN ORDER ------------------------------------------------
   * Order is part of the contract, not incidental: getenv returns the FIRST
   * match, so a reordered environment can change which value a program sees
   * even when the set of variables is identical. Deliberately not sorted. */
  size_t env_count = 0;
  for (char** e = environ; *e != NULL; e++) {
    printf("ENV %zu %s\n", env_count, *e);
    env_count++;
  }
  printf("ENVCOUNT %zu\n", env_count);

  /* --- the auxiliary vector --------------------------------------------- */
  for (size_t i = 0; i < sizeof(kKeys) / sizeof(kKeys[0]); i++) {
    unsigned long v = getauxval(kKeys[i].type);
    printf("AUXV %-22s %-7s 0x%lx\n", kKeys[i].name, kKeys[i].kind, v);
  }

  /* --- AT_RANDOM's sixteen bytes ----------------------------------------
   * The pointer is an address; the BYTES BEHIND IT are the randomness. Printing
   * only the pointer would miss a tool that determinizes the address but leaves
   * the contents host-random -- which is the failure that matters, since glibc
   * seeds the stack canary and pointer guard from these bytes. */
  const unsigned char* rnd = (const unsigned char*)getauxval(AT_RANDOM);
  if (rnd != NULL) {
    printf("AT_RANDOM_BYTES ");
    for (int i = 0; i < 16; i++) {
      printf("%02x", rnd[i]);
    }
    printf("\n");
  } else {
    printf("AT_RANDOM_BYTES absent\n");
  }

  /* --- initial stack layout ---------------------------------------------
   * Relative offsets as well as raw addresses. A tool could determinize the
   * absolute stack base while leaving the argv/envp spacing host-dependent, or
   * vice versa; the two are independent and both are part of the surface. */
  printf("STACK argv_ptr 0x%lx\n", (unsigned long)(uintptr_t)argv);
  printf("STACK environ_ptr 0x%lx\n", (unsigned long)(uintptr_t)environ);
  printf(
      "STACK environ_minus_argv %ld\n",
      (long)((intptr_t)(uintptr_t)environ - (intptr_t)(uintptr_t)argv));
  if (argc > 0 && argv[0] != NULL) {
    printf(
        "STACK argv0_string_minus_argv %ld\n",
        (long)((intptr_t)(uintptr_t)argv[0] - (intptr_t)(uintptr_t)argv));
  }
  {
    /* Address of a local: the actual stack pointer region at main(). */
    volatile int probe = 0;
    printf("STACK local_probe 0x%lx\n", (unsigned long)(uintptr_t)&probe);
  }

  /* --- BRANCH on auxv, do not merely print it ---------------------------
   * A printed difference is caught only by output comparison. A branch makes a
   * divergence change CONTROL FLOW, which is what really happens to guests:
   * glibc selects string routines from AT_HWCAP and decides whether to trust
   * the environment from AT_SECURE. Each decision is printed as its own line,
   * so the comparison covers the decision and not just its input. */
  const unsigned long hwcap = getauxval(AT_HWCAP);
  if (hwcap & (1UL << 25)) { /* bit 25 == SSE on x86_64 */
    printf("BRANCH hwcap_sse taken\n");
  } else {
    printf("BRANCH hwcap_sse not-taken\n");
  }

  const unsigned long pagesz = getauxval(AT_PAGESZ);
  if (pagesz == 4096) {
    printf("BRANCH pagesz eq4096\n");
  } else if (pagesz > 4096) {
    printf("BRANCH pagesz gt4096\n");
  } else {
    printf("BRANCH pagesz lt4096\n");
  }

  if (getauxval(AT_SECURE) != 0) {
    printf("BRANCH at_secure set\n");
  } else {
    printf("BRANCH at_secure clear\n");
  }

  /* Branch on AT_RANDOM itself. This is the load-bearing one: if the sixteen
   * bytes are not determinized, this branch flips between runs and the
   * divergence is a control-flow difference, exactly as it would be inside
   * glibc's canary setup. Parity of the first byte is a coarse but honest
   * summary -- it flips for roughly half of all nondeterministic values, and
   * the raw bytes are printed above for anything finer. */
  if (rnd != NULL) {
    printf("BRANCH at_random_first_byte %s\n", (rnd[0] & 1) ? "odd" : "even");
    unsigned long sum = 0;
    for (int i = 0; i < 16; i++) {
      sum += rnd[i];
    }
    printf("BRANCH at_random_sum_mod3 %lu\n", sum % 3);
  }

  /* Branch on the vDSO being present and on its ELF magic actually being
   * readable -- a mapped-but-wrong vDSO is a different failure from an absent
   * one, and both are reproducible observations worth distinguishing. */
  const unsigned char* vdso = (const unsigned char*)getauxval(AT_SYSINFO_EHDR);
  if (vdso == NULL) {
    printf("BRANCH vdso absent\n");
  } else if (memcmp(vdso, ELFMAG, SELFMAG) == 0) {
    printf("BRANCH vdso elf-magic-ok\n");
  } else {
    printf("BRANCH vdso mapped-but-not-elf\n");
  }

  return 0;
}
