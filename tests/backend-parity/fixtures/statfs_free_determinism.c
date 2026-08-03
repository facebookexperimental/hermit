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
 */

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <sys/statfs.h>
#include <unistd.h>

#define DETERMINIZED_FREE 1000000UL

int main(void)
{
	int ok = 0;
	struct statfs s;

	/* Path-based statfs on the root filesystem succeeds. */
	if (statfs("/", &s) == 0) {
		ok += 1;
		/* Free and available blocks are determinized, not host disk. */
		if ((unsigned long)s.f_bfree == DETERMINIZED_FREE)
			ok += 1;
		if ((unsigned long)s.f_bavail == DETERMINIZED_FREE)
			ok += 1;
	}

	/* fd-based fstatfs takes the same determinization path. */
	int fd = open("/", O_RDONLY);
	if (fd >= 0) {
		struct statfs f;
		if (fstatfs(fd, &f) == 0) {
			ok += 1;
			if ((unsigned long)f.f_bfree == DETERMINIZED_FREE)
				ok += 1;
			if ((unsigned long)f.f_bavail == DETERMINIZED_FREE)
				ok += 1;
		}
		close(fd);
	}

	printf("statfs ok=%d\n", ok);
	return 0;
}
