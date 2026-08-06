// Cross-backend parity contract: socketpair(2) creation flags + getsockopt(2)
// introspection, with NO data transfer.
//
// A prior socketpair data-transfer probe was rejected: a blocking cross-process
// socket read is scheduling-gated and livelocks under DBT. This contract instead
// exercises only the process-local, non-blocking facets of the socket family:
//   - socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK) creation
//   - the SOCK_CLOEXEC flag surfacing as FD_CLOEXEC (fcntl F_GETFD)
//   - the SOCK_NONBLOCK flag surfacing as O_NONBLOCK (fcntl F_GETFL)
//   - getsockopt SO_TYPE / SO_DOMAIN / SO_ACCEPTCONN constant reads
// Every value is a property of the guest's own creation arguments, so the
// answer is host-independent and identical across backends and native. No byte
// is ever written or read, so there is no blocking wait to schedule.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <sys/socket.h>
#include <unistd.h>

int main(void) {
    int ok = 0;
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK, 0, sv)
        != 0) {
        printf("socketpair ok=0 [socketpair fail]\n");
        return 0;
    }
    ok++;  // (1) socketpair with combined flags succeeded.

    // (2) SOCK_CLOEXEC surfaces as FD_CLOEXEC on the first end.
    int fd0 = fcntl(sv[0], F_GETFD);
    if (fd0 >= 0 && (fd0 & FD_CLOEXEC)) ok++;

    // (3) SOCK_CLOEXEC applies to the second end too.
    int fd1 = fcntl(sv[1], F_GETFD);
    if (fd1 >= 0 && (fd1 & FD_CLOEXEC)) ok++;

    // (4) SOCK_NONBLOCK surfaces as O_NONBLOCK on the first end.
    int fl = fcntl(sv[0], F_GETFL);
    if (fl >= 0 && (fl & O_NONBLOCK)) ok++;

    // (5) getsockopt SO_TYPE reports SOCK_STREAM.
    int type = -1;
    socklen_t tlen = sizeof(type);
    if (getsockopt(sv[0], SOL_SOCKET, SO_TYPE, &type, &tlen) == 0
        && type == SOCK_STREAM) {
        ok++;
    }

    // (6) getsockopt SO_DOMAIN reports AF_UNIX.
    int domain = -1;
    socklen_t dlen = sizeof(domain);
    if (getsockopt(sv[0], SOL_SOCKET, SO_DOMAIN, &domain, &dlen) == 0
        && domain == AF_UNIX) {
        ok++;
    }

    // (7) getsockopt SO_ACCEPTCONN reports 0 (not a listening socket).
    int accepting = -1;
    socklen_t alen = sizeof(accepting);
    if (getsockopt(sv[0], SOL_SOCKET, SO_ACCEPTCONN, &accepting, &alen) == 0
        && accepting == 0) {
        ok++;
    }

    close(sv[0]);
    close(sv[1]);
    printf("socketpair ok=%d\n", ok);
    return 0;
}
