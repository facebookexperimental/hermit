// Cross-backend parity contract: bind(2) an AF_UNIX socket to an abstract-namespace
// name and read it back with getsockname(2). NO data transfer, NO listen/connect.
//
// An abstract-namespace address (sun_path[0] == '\0') lives entirely in the
// network namespace and never touches the filesystem, so there is no temp file to
// clean up for --verify idempotency and no path-randomization channel. The name is
// a fixed literal the guest chooses, so getsockname must echo exactly the bytes the
// guest supplied: this is a pure property of the socket the guest itself created,
// independent of any host state. No byte is transferred, so there is no blocking
// wait to schedule (a blocking read would livelock the DBT cooperative scheduler).
//
// bind is a distinct syscall from the getsockname/getpeername reads in
// sockname_unnamed and the getsockopt reads in socketpair_flags/socket_options.
//
// THE ADDRESS LENGTH THE GUEST BOUND AND THE ONE GETSOCKNAME ECHOED ARE BOTH
// PRINTED. "bind_name ok=6" collapsed six checks into one scalar, so a backend
// that echoed a truncated address length and a backend that corrupted the
// abstract path both printed "bind_name ok=5" and compared EQUAL. The existing
// exit-status guard catches the lower total, while emitting both
// lengths makes a truncation self-describing instead of merely absent.
//
// Both lengths are guest-determined: the name is a fixed literal this fixture
// chooses and addrlen is computed from it, so no host state enters the byte
// stream. The abstract path itself is compared but NOT printed -- it contains an
// embedded NUL, so emitting it would put a non-text byte in the observation for
// no added distinguishing power that path_matches does not already give.
#include <errno.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

int main(void) {
    enum { EXPECTED_CHECKS = 6 };
    // Fixed abstract name: leading NUL marks the abstract namespace.
    static const char name[] = "\0hermit-parity-bind";
    const size_t namelen = sizeof(name) - 1; // drop the implicit trailing NUL

    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) {
        printf("bind_name ok=0 [socket fail]\n");
        return 1;
    }

    struct sockaddr_un addr;
    memset(&addr, 0, sizeof(addr));
    addr.sun_family = AF_UNIX;
    memcpy(addr.sun_path, name, namelen);
    socklen_t addrlen = (socklen_t)(offsetof(struct sockaddr_un, sun_path) + namelen);

    // (1) bind succeeds on the abstract name.
    int bind_rc = bind(fd, (struct sockaddr *)&addr, addrlen) == 0;

    // (2)-(5) getsockname echoes the exact abstract address the guest bound.
    struct sockaddr_un got;
    memset(&got, 0, sizeof(got));
    socklen_t gotlen = sizeof(got);
    int getsockname_rc = getsockname(fd, (struct sockaddr *)&got, &gotlen) == 0;
    int got_family = got.sun_family;
    int path_matches =
        got.sun_path[0] == '\0' && memcmp(got.sun_path, name, namelen) == 0;

    // (6) rebinding an already-bound socket fails deterministically with EINVAL.
    errno = 0;
    int rebind_einval =
        bind(fd, (struct sockaddr *)&addr, addrlen) == -1 && errno == EINVAL;

    close(fd);

    int ok = bind_rc + getsockname_rc + (got_family == AF_UNIX) +
        (gotlen == addrlen) + path_matches + rebind_einval;
    printf(
        "bind_name ok=%d bind_rc=%d getsockname_rc=%d got_family=%d "
        "bound_len=%d got_len=%d path_matches=%d rebind_einval=%d\n",
        ok,
        bind_rc,
        getsockname_rc,
        got_family,
        (int)addrlen,
        (int)gotlen,
        path_matches,
        rebind_einval);
    return ok == EXPECTED_CHECKS ? 0 : 1;
}
