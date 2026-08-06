/*
 * Backend-parity fixture: deterministic default NUMA memory policy.
 *
 * A freshly started process has the MPOL_DEFAULT memory policy on every Linux
 * host, regardless of NUMA topology. get_mempolicy(2) with a mode pointer and
 * no flags reports that policy mode; with MPOL_F_ADDR it reports the policy
 * governing a given address. set_mempolicy(MPOL_DEFAULT, NULL, 0) resets the
 * process policy to the default and needs no node mask.
 *
 * This contract deliberately queries only the *default policy mode*. It never
 * requests MPOL_F_MEMS_ALLOWED and never supplies or inspects a node mask, so
 * no host-specific NUMA node identity or topology can leak into the result.
 * All three Hermit backends and native execution therefore agree on the same
 * process-local, host-independent MPOL_DEFAULT round-trip.
 *
 * The fixture prints only a count of satisfied checks (mode values are
 * asserted against MPOL_DEFAULT, not printed), so the output is invariant
 * across runs, backends, and hosts.
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

static long get_mode(int *mode, void *addr, unsigned long flags)
{
	*mode = -1;
	errno = 0;
	return syscall(SYS_get_mempolicy, mode, (void *)0, 0UL, addr, flags);
}

int main(void)
{
	int ok = 0;
	int mode;
	int probe = 7; /* an address whose governing policy we query */

	/* Default process policy mode is MPOL_DEFAULT. */
	if (get_mode(&mode, (void *)0, 0UL) == 0 && mode == MPOL_DEFAULT)
		ok += 1;

	/* Policy governing a specific address is also the default. */
	if (get_mode(&mode, &probe, (unsigned long)MPOL_F_ADDR) == 0 &&
	    mode == MPOL_DEFAULT)
		ok += 1;

	/* Resetting to the default policy requires no node mask. */
	errno = 0;
	if (syscall(SYS_set_mempolicy, MPOL_DEFAULT, (void *)0, 0UL) == 0)
		ok += 1;

	/* The policy remains the default after the reset. */
	if (get_mode(&mode, (void *)0, 0UL) == 0 && mode == MPOL_DEFAULT)
		ok += 1;

	printf("mempolicy ok=%d\n", ok);
	return 0;
}
