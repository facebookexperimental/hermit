// Cross-backend parity contract: bind(2) an AF_UNIX socket to an abstract-namespace
// name and read it back with getsockname(2). NO data transfer, NO listen/connect.
//
// An abstract-namespace address (sun_path[0] == '\0') lives entirely in the
// network namespace and never touches the filesystem, so there is no temp file to
// clean up for --verify idempotency and no path-randomization channel. The name is
// a fixed literal the guest chooses, so getsockname must echo exactly the bytes the
// guest supplied: this is a pure property of the socket the guest itself created,
// independent of any host state. No byte is transferred, so there is no blocking
// wait to schedule (a blocking read would livelock the DBI cooperative scheduler).
//
// bind is a distinct syscall from the getsockname/getpeername reads in
// sockname_unnamed and the getsockopt reads in socketpair_flags/socket_options.
#include <errno.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

int main(void) {
    int ok = 0;
    // Fixed abstract name: leading NUL marks the abstract namespace.
    static const char name[] = "\0hermit-parity-bind";
    const size_t namelen = sizeof(name) - 1; // drop the implicit trailing NUL

    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) {
        printf("bind_name ok=0 [socket fail]\n");
        return 0;
    }

    struct sockaddr_un addr;
    memset(&addr, 0, sizeof(addr));
    addr.sun_family = AF_UNIX;
    memcpy(addr.sun_path, name, namelen);
    socklen_t addrlen = (socklen_t)(offsetof(struct sockaddr_un, sun_path) + namelen);

    // (1) bind succeeds on the abstract name.
    if (bind(fd, (struct sockaddr *)&addr, addrlen) == 0) ok++;

    // (2)-(5) getsockname echoes the exact abstract address the guest bound.
    struct sockaddr_un got;
    memset(&got, 0, sizeof(got));
    socklen_t gotlen = sizeof(got);
    if (getsockname(fd, (struct sockaddr *)&got, &gotlen) == 0) ok++;
    if (got.sun_family == AF_UNIX) ok++;
    if (gotlen == addrlen) ok++;
    if (got.sun_path[0] == '\0' &&
        memcmp(got.sun_path, name, namelen) == 0) ok++;

    // (6) rebinding an already-bound socket fails deterministically with EINVAL.
    errno = 0;
    if (bind(fd, (struct sockaddr *)&addr, addrlen) == -1 && errno == EINVAL) ok++;

    close(fd);
    printf("bind_name ok=%d\n", ok);
    return 0;
}
