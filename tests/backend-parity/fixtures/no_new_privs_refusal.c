/*
 * Backend-parity fixture: deterministic refusal of prctl no_new_privs.
 *
 * PR_SET_NO_NEW_PRIVS / PR_GET_NO_NEW_PRIVS manage the per-process
 * "no_new_privs" bit, a sticky flag that suppresses privilege gains across
 * execve (set-user-ID binaries, file capabilities, and similar). Detcore does
 * not model the execve privilege lattice, so rather than silently accepting a
 * privilege-semantics change it cannot honor, Hermit refuses both the query and
 * the set uniformly with ENOSYS.
 *
 * This is a uniform deterministic non-support refusal across all three
 * backends, not a determinization of a nondeterminism source: the flag itself
 * is a plain boolean. Outside Hermit the full round-trip succeeds
 * (PR_GET_NO_NEW_PRIVS returns 0, PR_SET_NO_NEW_PRIVS returns 0, and the next
 * PR_GET_NO_NEW_PRIVS returns 1), so the uniform ENOSYS is Hermit's
 * deterministic disposition, not a host limitation. All assertions are
 * process-local; the output (`nnp ok=3`) is identical across runs, backends,
 * and hosts.
 */

#include <errno.h>
#include <stdio.h>
#include <sys/prctl.h>

#ifndef PR_SET_NO_NEW_PRIVS
#define PR_SET_NO_NEW_PRIVS 38
#endif
#ifndef PR_GET_NO_NEW_PRIVS
#define PR_GET_NO_NEW_PRIVS 39
#endif

static int refused(int op, unsigned long arg2)
{
	errno = 0;
	int r = prctl(op, arg2, 0UL, 0UL, 0UL);
	return (r == -1 && errno == ENOSYS) ? 1 : 0;
}

int main(void)
{
	int ok = 0;

	/* Initial query is refused. */
	ok += refused(PR_GET_NO_NEW_PRIVS, 0);

	/* Setting the sticky flag is refused. */
	ok += refused(PR_SET_NO_NEW_PRIVS, 1);

	/* A second query stays refused (no state was recorded). */
	ok += refused(PR_GET_NO_NEW_PRIVS, 0);

	printf("nnp ok=%d\n", ok);
	return 0;
}
