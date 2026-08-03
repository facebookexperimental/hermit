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
    int ok = 0;
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) {
        printf("sockopt ok=0 [socketpair fail]\n");
        return 0;
    }

    if (roundtrip(sv[0], SO_REUSEADDR, 1)) ok++;  // (1) enable, reads back set.
    if (roundtrip(sv[0], SO_REUSEADDR, 0)) ok++;  // (2) disable, reads back clear.
    if (roundtrip(sv[0], SO_KEEPALIVE, 1)) ok++;  // (3)
    if (roundtrip(sv[0], SO_KEEPALIVE, 0)) ok++;  // (4)
    if (roundtrip(sv[0], SO_BROADCAST, 1)) ok++;  // (5)
    if (roundtrip(sv[0], SO_BROADCAST, 0)) ok++;  // (6)

    close(sv[0]);
    close(sv[1]);
    printf("sockopt ok=%d\n", ok);
    return 0;
}
