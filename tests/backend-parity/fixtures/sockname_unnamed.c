// Cross-backend parity contract: getsockname(2)/getpeername(2) on an unnamed
// AF_UNIX socket pair, with NO data transfer.
//
// A socketpair is connected but unnamed: neither endpoint is bound to a path,
// so both getsockname and getpeername report the AF_UNIX family with an address
// length of just the family field (sizeof(sa_family_t)) and no sun_path. This is
// a stable, host-independent property of the pair the guest itself created, so
// no host state enters the result. No byte is transferred, so there is no
// blocking wait to schedule (a blocking read would livelock the DBT cooperative
// scheduler).
//
// getsockname and getpeername are distinct syscalls from the getsockopt option
// reads exercised by socketpair_flags/socket_options.
//
// THE ADDRESS FAMILY AND LENGTH ARE PRINTED, not just a check count. "sockname
// ok=6" collapsed six independent observations into one scalar, so a backend
// reporting the wrong family and a backend reporting the wrong address length
// both printed "sockname ok=5" and compared EQUAL. The existing exit-status
// guard catches the lower total, but does not identify which observation was
// wrong.
// Both emitted numbers are ABI constants rather than host state -- AF_UNIX and
// sizeof(sa_family_t) are fixed by the Linux ABI, and the unnamed length is a
// property of the pair the guest itself created -- so printing them keeps the
// byte stream host-independent while making a wrong answer legible.
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

int main(void) {
    enum { EXPECTED_CHECKS = 6 };
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) {
        printf("sockname ok=0 [socketpair fail]\n");
        return 1;
    }

    struct sockaddr_un addr;
    socklen_t len;

    // (1)-(3) getsockname: succeeds, AF_UNIX, unnamed => family-only length.
    memset(&addr, 0, sizeof(addr));
    len = sizeof(addr);
    int sockname_rc = getsockname(sv[0], (struct sockaddr *)&addr, &len) == 0;
    int sockname_family = addr.sun_family;
    int sockname_len = (int)len;

    // (4)-(5) getpeername: succeeds and also reports the AF_UNIX peer.
    memset(&addr, 0, sizeof(addr));
    len = sizeof(addr);
    int peername_rc = getpeername(sv[0], (struct sockaddr *)&addr, &len) == 0;
    int peername_family = addr.sun_family;

    // (6) getsockname on an invalid descriptor fails deterministically EBADF.
    errno = 0;
    len = sizeof(addr);
    int ebadf =
        getsockname(-1, (struct sockaddr *)&addr, &len) == -1 && errno == EBADF;

    close(sv[0]);
    close(sv[1]);

    int ok = sockname_rc + (sockname_family == AF_UNIX) +
        (sockname_len == (int)sizeof(sa_family_t)) + peername_rc +
        (peername_family == AF_UNIX) + ebadf;
    printf(
        "sockname ok=%d sockname_rc=%d sockname_family=%d sockname_len=%d "
        "peername_rc=%d peername_family=%d ebadf=%d\n",
        ok,
        sockname_rc,
        sockname_family,
        sockname_len,
        peername_rc,
        peername_family,
        ebadf);
    return ok == EXPECTED_CHECKS ? 0 : 1;
}
