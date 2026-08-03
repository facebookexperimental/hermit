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
 *   nodename == "hermetic-container.local" (pinned hostname; native and the DBI
 *                                           backend leak the real host hostname)
 *
 * The ptrace and KVM backends determinize all four. The DBI (DynamoRIO) backend
 * pins release but forwards the *host* nodename, so it deterministically-but-
 * host-dependently fails the nodename check; matrix.tsv records that as a DBI
 * gap. Native Linux honors none of the pinned values, proving these are Hermit
 * determinization choices rather than host coincidences.
 *
 * Uses only the libc uname() wrapper and POSIX <sys/utsname.h>; no _GNU_SOURCE.
 */
#include <stdio.h>
#include <string.h>
#include <sys/utsname.h>

#define PINNED_SYSNAME "Linux"
#define PINNED_MACHINE "x86_64"
#define PINNED_RELEASE "5.2.0"
#define PINNED_NODENAME "hermetic-container.local"

int main(void) {
    int ok = 0;
    struct utsname u;

    memset(&u, 0, sizeof(u));
    if (uname(&u) == 0) {
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

    printf("uname ok=%d\n", ok);
    return 0;
}
