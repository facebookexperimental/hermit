/*
 * uname_identity: cross-backend determinization contract for uname(2).
 *
 * Hermit pins the guest kernel identity so that a program's view of the host
 * does not leak nondeterministic, host-specific facts. This fixture asserts the
 * fields Hermit is expected to determinize identically for every guest:
 *
 *   sysname  == "Linux"                    (kernel family, host-generic)
 *   machine  == "x86_64"                   (ISA, host-generic on this corpus)
 *   release  == "5.2.0"                    (pinned kernel release; native leaks
 *                                           the real running kernel, e.g. 6.x)
 *   nodename == "hermetic-container.local" (pinned hostname; native and the DBT
 *                                           backend leak the real host hostname)
 *
 * The ptrace and KVM backends determinize all four. The DBT (DynamoRIO) backend
 * pins release but forwards the *host* nodename, so it deterministically-but-
 * host-dependently fails the nodename check; matrix.tsv records that as a DBT
 * gap. Native Linux honors none of the pinned values, proving these are Hermit
 * determinization choices rather than host coincidences.
 *
 * Uses only the libc uname() wrapper and POSIX <sys/utsname.h>; no _GNU_SOURCE.
 */
#include <stdio.h>
#include <string.h>
#include <sys/utsname.h>

/*
 * THE PINNED IDENTITY STRINGS ARE EMITTED, not just a match count. These four
 * strings are the entire substance of this contract, and "uname ok=4" hid every
 * one of them: a backend virtualizing the WRONG release and a backend
 * virtualizing the wrong nodename both printed "uname ok=3" and compared EQUAL,
 * and two backends pinning the SAME wrong value agree with each other, so
 * cross-backend comparison could never see it. Only comparison against the
 * expected string can, and that needs the string in the byte stream.
 *
 * Under Hermit these are the virtualized identity and are identical on every
 * host, so emitting them keeps the output host-independent in the context this
 * fixture is contracted for. Natively they are the real host identity and this
 * fixture already fails by construction (native scores ok=2: sysname and machine
 * happen to match, release and nodename do not).
 */
#define PINNED_SYSNAME "Linux"
#define PINNED_MACHINE "x86_64"
#define PINNED_RELEASE "5.2.0"
#define PINNED_NODENAME "hermetic-container.local"

int main(void) {
    enum { EXPECTED_CHECKS = 4 };
    int ok = 0;
    struct utsname u;

    memset(&u, 0, sizeof(u));
    int uname_rc = uname(&u) == 0;
    if (uname_rc) {
        if (strcmp(u.sysname, PINNED_SYSNAME) == 0) {
            ok += 1;
        }
        if (strcmp(u.machine, PINNED_MACHINE) == 0) {
            ok += 1;
        }
        if (strcmp(u.release, PINNED_RELEASE) == 0) {
            ok += 1;
        }
        if (strcmp(u.nodename, PINNED_NODENAME) == 0) {
            ok += 1;
        }
    }

    /* The four pinned strings ARE the contract, so they are emitted. Under
     * Hermit they are the same virtualized identity on every host. */
    printf(
        "uname ok=%d uname_rc=%d sysname=%s machine=%s release=%s nodename=%s\n",
        ok,
        uname_rc,
        u.sysname,
        u.machine,
        u.release,
        u.nodename);
    return ok == EXPECTED_CHECKS ? 0 : 1;
}
