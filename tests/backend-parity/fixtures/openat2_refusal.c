/*
 * Backend-parity fixture: deterministic refusal of openat2(2).
 *
 * openat2(2) (Linux 5.6+) is a newer path-resolution syscall that extends
 * openat(2) with an extensible struct open_how and RESOLVE_* scoping flags.
 * Detcore does not model openat2's resolution semantics, so all three Hermit
 * backends refuse it uniformly with ENOSYS; guests fall back to the supported
 * openat(2). Outside Hermit the same calls succeed on a modern kernel, so the
 * refusal is a determinization choice, not a host limitation -- the same shape
 * as the io_uring, listmount, and machine-check kill-policy refusal contracts.
 *
 * The contract asserts that each openat2 variant returns -1 with errno ENOSYS.
 * Because the syscall is rejected before any path resolution, the target path
 * is irrelevant; "/" is used so no temporary file is required.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/types.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_openat2
#define SYS_openat2 437
#endif

/* struct open_how and RESOLVE_* may be absent on older toolchains. */
struct parity_open_how {
	__u64 flags;
	__u64 mode;
	__u64 resolve;
};

#ifndef RESOLVE_NO_SYMLINKS
#define RESOLVE_NO_SYMLINKS 0x04
#endif
#ifndef RESOLVE_BENEATH
#define RESOLVE_BENEATH 0x08
#endif

static int expect_enosys(const struct parity_open_how *how)
{
	errno = 0;
	long r = syscall(SYS_openat2, AT_FDCWD, "/", how, sizeof(*how));
	return (r == -1 && errno == ENOSYS) ? 1 : 0;
}

int main(void)
{
	int ok = 0;

	/* Plain read-only open via the extensible interface. */
	struct parity_open_how basic = {
		.flags = O_RDONLY,
		.mode = 0,
		.resolve = 0,
	};
	ok += expect_enosys(&basic);

	/* Scoped resolution request (RESOLVE_NO_SYMLINKS). */
	struct parity_open_how scoped = {
		.flags = O_RDONLY,
		.mode = 0,
		.resolve = RESOLVE_NO_SYMLINKS,
	};
	ok += expect_enosys(&scoped);

	/* Beneath-scoped resolution request (RESOLVE_BENEATH). */
	struct parity_open_how beneath = {
		.flags = O_RDONLY,
		.mode = 0,
		.resolve = RESOLVE_BENEATH,
	};
	ok += expect_enosys(&beneath);

	printf("openat2 ok=%d\n", ok);
	return 0;
}
