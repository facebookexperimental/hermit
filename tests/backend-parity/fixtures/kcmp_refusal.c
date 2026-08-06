/*
 * Backend-parity fixture: deterministic refusal of kcmp(2).
 *
 * kcmp(2) compares two processes' kernel resources -- whether two file
 * descriptors share an open file description, whether two tasks share an
 * address space or file-descriptor table, and so on. Its ordering result for
 * KCMP_FILE leaks the kernel's internal pointer ordering of those objects, an
 * uncontrolled nondeterminism channel. Hermit therefore refuses kcmp uniformly
 * with EPERM, the same disposition it gives process_vm_readv/process_vm_writev:
 * faithful Linux behavior for a caller without ptrace access to the target.
 *
 * The probe compares the process to *itself*, which succeeds outside Hermit
 * (kcmp returns 0, "equal"), so the deterministic EPERM under every backend is a
 * determinization choice, not a host limitation. All assertions are
 * process-local; the output (`kcmp ok=3`) is identical across runs, backends,
 * and hosts.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>
#include <sys/types.h>

#ifndef SYS_kcmp
#define SYS_kcmp 312
#endif

#define KCMP_FILE 0
#define KCMP_VM 1
#define KCMP_FILES 2

static int refused(int type, unsigned long idx1, unsigned long idx2)
{
	pid_t me = getpid();
	errno = 0;
	long r = syscall(SYS_kcmp, me, me, type, idx1, idx2);
	return (r == -1 && errno == EPERM) ? 1 : 0;
}

int main(void)
{
	int ok = 0;

	/* Ordering of two descriptors' open file descriptions is refused. */
	ok += refused(KCMP_FILE, 1, 1);

	/* Address-space identity comparison is refused. */
	ok += refused(KCMP_VM, 0, 0);

	/* File-descriptor-table identity comparison is refused. */
	ok += refused(KCMP_FILES, 0, 0);

	printf("kcmp ok=%d\n", ok);
	return 0;
}
