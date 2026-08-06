// Cross-backend parity contract: ioctl(2) FIONREAD/FIONBIO on a pipe.
//
// This exercises the ioctl syscall dispatch (distinct from the fcntl family
// covered by pipe2_flags/fcntl_status_flags/pipe_capacity) via two classic
// stream ioctls:
//   FIONREAD - report the number of bytes immediately readable
//   FIONBIO  - set/clear non-blocking mode (equivalent to the O_NONBLOCK
//              status flag, cross-checked here with fcntl F_GETFL)
// The guest writes the bytes it then counts, so the FIONREAD result is a pure
// function of the guest's own actions, and the FIONBIO round-trip is a
// process-local status-flag toggle. No blocking read is performed (a read on an
// empty non-blocking pipe would livelock under DBT), so the contract is a pure
// query/flag round-trip that every backend and native agree on.
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <sys/ioctl.h>
#include <unistd.h>

int main(void) {
    int ok = 0;
    int fds[2];
    if (pipe(fds) != 0) {
        printf("fionread ok=0 [pipe fail]\n");
        return 0;
    }

    // Guest writes exactly six bytes into the pipe.
    if (write(fds[1], "hello\n", 6) != 6) {
        printf("fionread ok=0 [write fail]\n");
        return 0;
    }

    // (1) FIONREAD reports bytes readable; (2) the count equals what we wrote.
    int navail = -1;
    if (ioctl(fds[0], FIONREAD, &navail) == 0) ok++;
    if (navail == 6) ok++;

    // (3) FIONBIO sets non-blocking; (4) fcntl F_GETFL reflects O_NONBLOCK.
    int on = 1;
    if (ioctl(fds[0], FIONBIO, &on) == 0) ok++;
    int fl = fcntl(fds[0], F_GETFL);
    if (fl >= 0 && (fl & O_NONBLOCK)) ok++;

    // (5) FIONBIO clears non-blocking; (6) F_GETFL shows O_NONBLOCK cleared.
    int off = 0;
    if (ioctl(fds[0], FIONBIO, &off) == 0) ok++;
    fl = fcntl(fds[0], F_GETFL);
    if (fl >= 0 && !(fl & O_NONBLOCK)) ok++;

    close(fds[0]);
    close(fds[1]);
    printf("fionread ok=%d\n", ok);
    return 0;
}
