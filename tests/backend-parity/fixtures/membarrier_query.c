/*
 * membarrier_query: cross-backend parity contract for the membarrier(2)
 * feature-query and global-barrier commands.
 *
 * MEMBARRIER_CMD_QUERY returns a bitmask of the commands the kernel supports.
 * On a bare host that mask reflects the running kernel's capabilities and is
 * therefore host-dependent. Under Hermit every backend normalizes the mask to
 * the canonical value 31, so the query result is stable and identical across
 * ptrace, DBI, and KVM. MEMBARRIER_CMD_GLOBAL is the always-available
 * process-wide barrier and returns 0.
 *
 * The contract deliberately avoids the registered-private and expedited
 * command variants: those depend on prior per-process registration and on
 * scheduler-visible IPIs, which are exactly the host-timing-sensitive channels
 * this matrix keeps out of the non-gated lane. Only the query mask and the
 * unconditional global barrier are asserted.
 */

#include <linux/membarrier.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
    enum { EXPECTED_CHECKS = 2, CANONICAL_QUERY_MASK = 31 };
    int ok = 0;

    long mask = syscall(SYS_membarrier, MEMBARRIER_CMD_QUERY, 0, 0);
    if (mask == CANONICAL_QUERY_MASK) {
        ok++;
    }

    if (syscall(SYS_membarrier, MEMBARRIER_CMD_GLOBAL, 0, 0) == 0) {
        ok++;
    }

#ifdef HERMIT_TEST_ORACLE_NEGATIVE
    ok--; /* plant one failed contract check to bracket the exit oracle */
#endif
    printf("memb ok=%d q=%ld\n", ok, mask);
    return ok == EXPECTED_CHECKS ? EXIT_SUCCESS : EXIT_FAILURE;
}
