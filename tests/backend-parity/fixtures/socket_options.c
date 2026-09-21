// Cross-backend parity contract: setsockopt(2)/getsockopt(2) boolean
// socket-option round-trip on an AF_UNIX socket pair, with NO data transfer.
//
// This is the write-then-read companion to socketpair_flags, which only reads
// the creation-derived options (SO_TYPE / SO_DOMAIN / SO_ACCEPTCONN). Here each
// check SETS a settable boolean option and then reads it back, so a backend
// passes only if setsockopt is accepted AND getsockopt returns the value that
// was just stored. Asserting the full round-trip (not merely that setsockopt
// returns 0) is deliberate: a backend that accepts setsockopt but drops the
// value must not be scored as parity.
//
// Only boolean options are exercised. Buffer-size options such as SO_SNDBUF are
// intentionally excluded: the kernel rounds and doubles the requested size to a
// host-configuration-dependent value, which is not a portable golden. Every
// value asserted here is a boolean the guest itself just set, so the answer is
// host-independent, and no byte is written or read, so there is no blocking
// wait to schedule.
//
// EACH ROUND-TRIP IS REPORTED SEPARATELY, and the fixture fails closed. Printing
// only "sockopt ok=6" was blind in the way a sum always is: six independent
// checks collapsed into one scalar, so a backend that dropped SO_KEEPALIVE and a
// backend that dropped SO_BROADCAST both printed "sockopt ok=5" and compared
// EQUAL to each other. The failing option was unrecoverable from the byte
// stream; the existing exit-status guard catches the lower total, but does not
// identify that option.
//
// There is no host-independent VALUE to print here -- the observable genuinely
// is a boolean, and the kernel canonicalises the readback -- so this fixture is
// de-aliased rather than value-printing, the same fallback cwd_roundtrip uses.
// The raw readback integer is deliberately NOT printed: it is kernel-normalised
// rather than guest-determined, so emitting it would trade blindness for
// host-dependence.
#include <errno.h>
#include <stdio.h>
#include <sys/socket.h>
#include <unistd.h>

// Set `opt` to `value`, read it back, and report whether the round-trip
// observed the expected truthiness (non-zero when set, zero when cleared).
static int roundtrip(int fd, int opt, int value) {
    if (setsockopt(fd, SOL_SOCKET, opt, &value, sizeof(value)) != 0) return 0;
    int readback = -1;
    socklen_t len = sizeof(readback);
    if (getsockopt(fd, SOL_SOCKET, opt, &readback, &len) != 0) return 0;
    if (value != 0) return readback != 0;
    return readback == 0;
}

int main(void) {
    enum { EXPECTED_CHECKS = 6 };
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) {
        printf("sockopt ok=0 [socketpair fail]\n");
        return 1;
    }

    int reuseaddr_set = roundtrip(sv[0], SO_REUSEADDR, 1);   // (1) reads back set
    int reuseaddr_clear = roundtrip(sv[0], SO_REUSEADDR, 0); // (2) reads back clear
    int keepalive_set = roundtrip(sv[0], SO_KEEPALIVE, 1);   // (3)
    int keepalive_clear = roundtrip(sv[0], SO_KEEPALIVE, 0); // (4)
    int broadcast_set = roundtrip(sv[0], SO_BROADCAST, 1);   // (5)
    int broadcast_clear = roundtrip(sv[0], SO_BROADCAST, 0); // (6)

    close(sv[0]);
    close(sv[1]);
    int ok = reuseaddr_set + reuseaddr_clear + keepalive_set + keepalive_clear +
        broadcast_set + broadcast_clear;
    printf(
        "sockopt ok=%d reuseaddr_set=%d reuseaddr_clear=%d keepalive_set=%d "
        "keepalive_clear=%d broadcast_set=%d broadcast_clear=%d\n",
        ok,
        reuseaddr_set,
        reuseaddr_clear,
        keepalive_set,
        keepalive_clear,
        broadcast_set,
        broadcast_clear);
    return ok == EXPECTED_CHECKS ? 0 : 1;
}
