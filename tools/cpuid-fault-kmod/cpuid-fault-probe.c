// SPDX-License-Identifier: GPL-2.0-only
/*
 * cpuid-fault-probe: report whether Linux CPUID faulting works for this
 * process, the way Hermit uses it.
 *
 * It calls arch_prctl(ARCH_SET_CPUID, 0), confirms the state with
 * ARCH_GET_CPUID, then executes cpuid and expects SIGSEGV. Faulting is per
 * task, so this arms only the probe itself, and exit discards the state.
 *
 * Exit status:
 *   0  CPUID faulting works: the arm succeeded and cpuid trapped.
 *   1  CPUID faulting is unavailable: ARCH_SET_CPUID(0) failed (ENODEV when
 *      the kernel lacks X86_FEATURE_CPUID_FAULT).
 *   2  Broken: the arm reported success but cpuid did not trap, or
 *      ARCH_GET_CPUID did not confirm it.
 *   3  ARCH_GET_CPUID itself failed.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <setjmp.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#define ARCH_GET_CPUID 0x1011
#define ARCH_SET_CPUID 0x1012

static sigjmp_buf trapped;

static void on_segv(int sig)
{
	(void)sig;
	siglongjmp(trapped, 1);
}

int main(void)
{
	long state = syscall(SYS_arch_prctl, ARCH_GET_CPUID, 0);
	struct sigaction sa;

	if (state != 0 && state != 1) {
		printf("ARCH_GET_CPUID failed: %s (errno %d)\n", strerror(errno), errno);
		return 3;
	}
	if (syscall(SYS_arch_prctl, ARCH_SET_CPUID, 0) != 0) {
		int err = errno;

		printf("CPUID faulting is unavailable: ARCH_SET_CPUID(0) failed: %s (errno %d)\n",
		       strerror(err), err);
		return 1;
	}
	state = syscall(SYS_arch_prctl, ARCH_GET_CPUID, 0);
	if (state != 0) {
		printf("CPUID faulting is broken: ARCH_SET_CPUID(0) succeeded but ARCH_GET_CPUID returned %ld\n",
		       state);
		return 2;
	}
	memset(&sa, 0, sizeof(sa));
	sa.sa_handler = on_segv;
	sigemptyset(&sa.sa_mask);
	sigaction(SIGSEGV, &sa, NULL);
	if (sigsetjmp(trapped, 1) == 0) {
		unsigned int a = 0, b, c = 0, d;

		__asm__ volatile("cpuid" : "+a"(a), "=b"(b), "+c"(c), "=d"(d));
		printf("CPUID faulting is broken: ARCH_SET_CPUID(0) succeeded but cpuid ran natively\n");
		return 2;
	}
	printf("CPUID faulting works: ARCH_SET_CPUID(0) succeeded and cpuid trapped (SIGSEGV)\n");
	return 0;
}
