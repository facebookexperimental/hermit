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
	/* Each refusal is reported separately. Summing three into one scalar
	 * meant a backend that leaked the initial query and a backend that
	 * recorded state on the SET (so the second query answered) both printed
	 * "nnp ok=2" and compared equal -- and the second-query check exists
	 * precisely to catch state being recorded, so collapsing it defeated the
	 * point of having it. The observables are refusals, so this fixture is
	 * de-aliased rather than value-printing. */
	int query1_refused = refused(PR_GET_NO_NEW_PRIVS, 0);
	int set_refused = refused(PR_SET_NO_NEW_PRIVS, 1);
	int query2_refused = refused(PR_GET_NO_NEW_PRIVS, 0);

	int ok = query1_refused + set_refused + query2_refused;
	printf("nnp ok=%d query1_refused=%d set_refused=%d query2_refused=%d\n",
	       ok, query1_refused, set_refused, query2_refused);
	return ok == 3 ? 0 : 1;
}
