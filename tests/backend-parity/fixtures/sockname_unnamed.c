// Cross-backend parity contract: getsockname(2)/getpeername(2) on an unnamed
// AF_UNIX socket pair, with NO data transfer.
//
// A socketpair is connected but unnamed: neither endpoint is bound to a path,
// so both getsockname and getpeername report the AF_UNIX family with an address
// length of just the family field (sizeof(sa_family_t)) and no sun_path. This is
// a stable, host-independent property of the pair the guest itself created, so
// no host state enters the result. No byte is transferred, so there is no
// blocking wait to schedule (a blocking read would livelock the DBI cooperative
// scheduler).
//
// getsockname and getpeername are distinct syscalls from the getsockopt option
// reads exercised by socketpair_flags/socket_options.
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

int main(void) {
    int ok = 0;
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) {
        printf("sockname ok=0 [socketpair fail]\n");
        return 0;
    }

    struct sockaddr_un addr;
    socklen_t len;

    // (1)-(3) getsockname: succeeds, AF_UNIX, unnamed => family-only length.
    memset(&addr, 0, sizeof(addr));
    len = sizeof(addr);
    if (getsockname(sv[0], (struct sockaddr *)&addr, &len) == 0) ok++;
    if (addr.sun_family == AF_UNIX) ok++;
    if (len == sizeof(sa_family_t)) ok++;

    // (4)-(5) getpeername: succeeds and also reports the AF_UNIX peer.
    memset(&addr, 0, sizeof(addr));
    len = sizeof(addr);
    if (getpeername(sv[0], (struct sockaddr *)&addr, &len) == 0) ok++;
    if (addr.sun_family == AF_UNIX) ok++;

    // (6) getsockname on an invalid descriptor fails deterministically EBADF.
    errno = 0;
    len = sizeof(addr);
    if (getsockname(-1, (struct sockaddr *)&addr, &len) == -1 && errno == EBADF)
        ok++;

    close(sv[0]);
    close(sv[1]);
    printf("sockname ok=%d\n", ok);
    return 0;
}
