// Cross-backend parity contract: shutdown(2) on an AF_UNIX socket pair.
//
// Half-closes and fully closes connected socket-pair endpoints and checks the
// return value of each shutdown. shutdown returns immediately and transfers no
// data, so there is no blocking cross-endpoint wait for the scheduler to order
// (a blocking read would livelock the DBT cooperative scheduler). No byte is
// ever written after a shutdown, so no SIGPIPE is raised — signal delivery is
// deliberately avoided, keeping this a pure return-value contract.
//
// Every outcome asserted here is a property of the guest's own socket lifecycle
// (a connected pair the guest just created, or the invalid descriptor -1), so
// the answer carries no host state and is identical across hosts.
//
// EACH SHUTDOWN IS REPORTED SEPARATELY, and the fixture fails closed. "shutdown
// ok=5" collapsed five independent return-value contracts into one scalar, so a
// backend that broke SHUT_RD and a backend that broke the EBADF refusal both
// printed "shutdown ok=4" and compared EQUAL. The existing exit-status guard
// catches the lower total, but does not identify which outcome was wrong.
//
// The datagram check had a third, invisible state: it was nested inside the
// dgram socketpair() succeeding, so a FAILURE TO CREATE THE PAIR and a FAILURE
// OF shutdown ON IT produced the same ok=4. dgram_pair is now emitted so those
// two are distinguishable.
//
// These observables are genuinely booleans -- a return value of 0, or -1 with a
// specific errno -- so there is no host-independent value to print and the
// fixture is de-aliased rather than value-printing, the cwd_roundtrip fallback.
#include <errno.h>
#include <stdio.h>
#include <sys/socket.h>
#include <unistd.h>

int main(void) {
    enum { EXPECTED_CHECKS = 5 };
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) {
        printf("shutdown ok=0 [stream socketpair fail]\n");
        return 1;
    }

    int shut_rd = shutdown(sv[0], SHUT_RD) == 0;      // (1) half-close read side.
    int shut_wr = shutdown(sv[0], SHUT_WR) == 0;      // (2) half-close write side.
    int shut_rdwr = shutdown(sv[1], SHUT_RDWR) == 0;  // (3) full-close other end.
    close(sv[0]);
    close(sv[1]);

    int dv[2];
    int dgram_pair = socketpair(AF_UNIX, SOCK_DGRAM, 0, dv) == 0;
    int shut_dgram = 0;
    if (dgram_pair) {
        shut_dgram = shutdown(dv[0], SHUT_RDWR) == 0;  // (4) datagram endpoint.
        close(dv[0]);
        close(dv[1]);
    }

    // (5) shutdown on an invalid descriptor fails deterministically with EBADF.
    int ebadf = shutdown(-1, SHUT_RDWR) == -1 && errno == EBADF;

    int ok = shut_rd + shut_wr + shut_rdwr + shut_dgram + ebadf;
    printf(
        "shutdown ok=%d shut_rd=%d shut_wr=%d shut_rdwr=%d dgram_pair=%d "
        "shut_dgram=%d ebadf=%d\n",
        ok,
        shut_rd,
        shut_wr,
        shut_rdwr,
        dgram_pair,
        shut_dgram,
        ebadf);
    return ok == EXPECTED_CHECKS ? 0 : 1;
}
