/*
 * Backend-parity fixture: deterministic statfs/fstatfs free-space fields.
 *
 * statfs(2)/fstatfs(2) report filesystem statistics. The free-block counts
 * f_bfree (blocks free) and f_bavail (blocks available to an unprivileged user)
 * are a live host-state nondeterminism channel: on a real filesystem they
 * change moment to moment as other processes allocate and release space, so a
 * guest that reads them observes uncontrolled host state. Hermit therefore
 * determinizes both to a fixed value (1000000 blocks) rather than forwarding the
 * host's live free-space, for both the path-based statfs and the fd-based
 * fstatfs entry points.
 *
 * Structural fields such as f_type, f_blocks (total blocks), f_bsize, and
 * f_namelen depend on the host filesystem the guest actually runs on, so this
 * fixture deliberately does not assert them; it checks only the determinized
 * free-space counts, which are host-independent under Hermit.
 *
 * The discriminator is exactly those counts: outside Hermit f_bfree/f_bavail
 * reflect the real disk and are not 1000000, so native prints `statfs ok=2`
 * (only the two "call succeeded" checks pass). All three Hermit backends hold
 * the determinized 1000000 and print `statfs ok=6`. The uniform Hermit result
 * is a determinization choice, not native parity. All assertions are
 * process-local and the output is identical across runs, backends, and hosts.
 *
 * THE DETERMINIZED COUNTS THEMSELVES ARE PRINTED. This fixture exists to assert
 * one specific number, and "statfs ok=6" hid exactly that number. The omission
 * was the worst kind for this contract: a backend that determinizes to the WRONG
 * constant scores ok=4, which is indistinguishable from a backend where statfs
 * simply failed -- and two backends that both determinize to the same wrong
 * constant AGREE with each other, so cross-backend comparison could never see
 * it. Only comparing against the expected value can, and that requires the value
 * to be in the byte stream.
 *
 * Emitting the counts does NOT introduce host dependence in the context this
 * fixture is contracted for: under Hermit they are the determinized constant on
 * every host. Outside Hermit they are live disk state and vary -- but this
 * fixture already fails natively by construction (native prints ok=2, as
 * documented above), so it is Hermit-only by design and its naked mode is not
 * enabled. If a naked cell is ever turned on for it, that cell must expect
 * host-varying output, which is the honest description of reading a real disk.
 */

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <sys/statfs.h>
#include <unistd.h>

#define DETERMINIZED_FREE 1000000UL

int main(void)
{
	enum { EXPECTED_CHECKS = 6 };
	int ok = 0;
	int statfs_rc = 0, fstatfs_rc = 0;
	unsigned long s_bfree = 0, s_bavail = 0, f_bfree = 0, f_bavail = 0;
	struct statfs s;

	/* Path-based statfs on the root filesystem succeeds. */
	if (statfs("/", &s) == 0) {
		statfs_rc = 1;
		ok += 1;
		/* Free and available blocks are determinized, not host disk. */
		s_bfree = (unsigned long)s.f_bfree;
		s_bavail = (unsigned long)s.f_bavail;
		if (s_bfree == DETERMINIZED_FREE)
			ok += 1;
		if (s_bavail == DETERMINIZED_FREE)
			ok += 1;
	}

	/* fd-based fstatfs takes the same determinization path. */
	int fd = open("/", O_RDONLY);
	if (fd >= 0) {
		struct statfs f;
		if (fstatfs(fd, &f) == 0) {
			fstatfs_rc = 1;
			ok += 1;
			f_bfree = (unsigned long)f.f_bfree;
			f_bavail = (unsigned long)f.f_bavail;
			if (f_bfree == DETERMINIZED_FREE)
				ok += 1;
			if (f_bavail == DETERMINIZED_FREE)
				ok += 1;
		}
		close(fd);
	}

	printf("statfs ok=%d statfs_rc=%d statfs_bfree=%lu statfs_bavail=%lu "
	       "fstatfs_rc=%d fstatfs_bfree=%lu fstatfs_bavail=%lu\n",
	       ok, statfs_rc, s_bfree, s_bavail, fstatfs_rc, f_bfree, f_bavail);
	return ok == EXPECTED_CHECKS ? 0 : 1;
}
