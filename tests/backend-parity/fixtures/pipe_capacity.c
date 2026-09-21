/*
 * F_GETPIPE_SZ / F_SETPIPE_SZ pipe-capacity round-trip parity fixture.
 *
 * Exercises the pipe-buffer sizing fcntl command family (distinct from the
 * F_GETFD/F_SETFD descriptor-flag and F_GETFL/F_SETFL status-flag namespaces
 * already covered elsewhere). Detcore pins pipes used by its deterministic
 * scheduler to 8192 bytes so the guest cannot observe host-wide per-UID pipe
 * pressure. This fixture locks that guest-visible contract:
 *   1. pipe2 opens a pipe.
 *   2. the initial capacity reported by F_GETPIPE_SZ is exactly 8192 bytes.
 *   3. shrinking the capacity to one page returns a positive rounded size
 *      (shrinking is always permitted for an unprivileged process; growing a
 *      pipe can require CAP_SYS_RESOURCE under per-user page accounting and is
 *      deliberately never attempted).
 *   4. the shrunk capacity does not exceed the original default.
 *   5. a subsequent F_GETPIPE_SZ echoes exactly the size the shrink returned.
 *
 * Every observable is process-local pipe-object state with no host-derived,
 * timing, or cross-thread input, so it is identical across repeated runs.
 *
 * Both guest-visible capacities are printed and checked exactly. This makes an
 * unpatched build fail as a compatibility-matrix cell on the usual 64-KiB host,
 * while the output remains host-independent once Detcore applies its contract.
 */
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

int main(void) {
    enum {
        EXPECTED_CHECKS = 5,
        EXPECTED_DEFAULT_PIPE_SZ = 8192,
        WANT_PIPE_SZ = 4096
    };
    int ok = 0;
    int fds[2] = {-1, -1};
    if (pipe2(fds, 0) != 0) {
        printf("pipecap ok=0\n");
        return EXIT_FAILURE;
    }
    ok++;
    int def = fcntl(fds[0], F_GETPIPE_SZ);
    if (def == EXPECTED_DEFAULT_PIPE_SZ) {
        ok++;
    }
    int shrunk = fcntl(fds[1], F_SETPIPE_SZ, WANT_PIPE_SZ);
    /* Exact, not "> 0": the guest ASKED for WANT_PIPE_SZ, so any other positive
     * answer is wrong. The old `shrunk > 0` accepted every positive value, which
     * is the hole -- two backends could both return a wrong-but-positive size and
     * both score ok=5. */
    if (shrunk == WANT_PIPE_SZ) {
        ok++;
    }
    if (shrunk > 0 && shrunk <= def) {
        ok++;
    }
    int readback = fcntl(fds[1], F_GETPIPE_SZ);
    if (readback == shrunk) {
        ok++;
    }
    close(fds[0]);
    close(fds[1]);
#ifdef HERMIT_TEST_ORACLE_NEGATIVE
    ok--; /* plant one failed contract check to bracket the exit oracle */
#endif
    /* Emit the OBSERVED VALUES, not just the check sum. `ok=%d` is a SUM, so two
     * backends that fail DIFFERENT checks alias to the same total and compare
     * equal; and a sum can never expose a wrong-but-accepted value. */
    printf("pipecap ok=%d default=%d set=%d readback=%d fits_default=%d\n",
           ok, def, shrunk, readback, (shrunk > 0 && shrunk <= def) ? 1 : 0);
    return ok == EXPECTED_CHECKS ? EXIT_SUCCESS : EXIT_FAILURE;
}
