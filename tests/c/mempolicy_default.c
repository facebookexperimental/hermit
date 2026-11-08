/*
 * Backend-parity fixture: no NUMA memory policy at all.
 *
 * Hermit presents a kernel built without NUMA
 * (https://github.com/rrnewton/hermit/issues/5269). Linux compiles
 * mm/mempolicy.o only under CONFIG_NUMA and the memory-policy entry points
 * are cond_syscall stubs (kernel/sys_ni.c), so get_mempolicy(2) and
 * set_mempolicy(2) fail with ENOSYS before any argument is looked at --
 * even set_mempolicy(MPOL_DEFAULT, NULL, 0), which needs no node mask on a
 * NUMA kernel. Every backend therefore agrees, host-independently: each
 * call below returns -1 with errno ENOSYS, and writes nothing.
 *
 * The fixture prints only a count of satisfied checks, so the output is
 * invariant across runs, backends, and hosts. (Run natively on a NUMA
 * host it fails by design: it pins Hermit's platform, not the host's.)
 */

#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_get_mempolicy
#define SYS_get_mempolicy 239
#endif
#ifndef SYS_set_mempolicy
#define SYS_set_mempolicy 238
#endif

/* mempolicy modes / flags may be absent without <numaif.h>. */
#ifndef MPOL_DEFAULT
#define MPOL_DEFAULT 0
#endif
#ifndef MPOL_F_ADDR
#define MPOL_F_ADDR (1 << 1)
#endif

/* Whether a get_mempolicy answered ENOSYS and left *mode untouched. */
static int get_mode_enosys(void* addr, unsigned long flags) {
  int mode = -777;
  errno = 0;
  long rc = syscall(SYS_get_mempolicy, &mode, (void*)0, 0UL, addr, flags);
  return rc == -1 && errno == ENOSYS && mode == -777;
}

int main(void) {
  enum { EXPECTED_CHECKS = 4 };
  int ok = 0;
  int probe = 7; /* an address whose governing policy we would query */

  if (get_mode_enosys((void*)0, 0UL))
    ok += 1;
  if (get_mode_enosys(&probe, (unsigned long)MPOL_F_ADDR))
    ok += 1;

  errno = 0;
  if (syscall(SYS_set_mempolicy, MPOL_DEFAULT, (void*)0, 0UL) == -1 &&
      errno == ENOSYS)
    ok += 1;

  if (get_mode_enosys((void*)0, 0UL))
    ok += 1;

  printf("mempolicy ok=%d\n", ok);
  /* Route a behavioural failure into the exit status. Without this the guest
     exits 0 whatever `ok` reached, so a regression only lowered the printed
     number -- and under --verify both runs lower it identically, so the
     comparison still matches and the cell stays green. Every check above is
     unchanged; this only requires all of them. */
  if (ok != EXPECTED_CHECKS) {
    fprintf(
        stderr, "mempolicy completed %d of %d checks\n", ok, EXPECTED_CHECKS);
    return 1;
  }
  return 0;
}
