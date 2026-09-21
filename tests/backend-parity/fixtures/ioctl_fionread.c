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
//
// THE READABLE-BYTE COUNT IS PRINTED, and each flag check is named separately.
// "fionread ok=6" was blind twice over: six checks collapsed into one scalar, so
// a backend that misreported the byte count and a backend that dropped the
// FIONBIO toggle both printed "fionread ok=5" and compared EQUAL. The existing
// exit-status guard catches the lower total, but does not identify which
// observation was wrong.
// navail is the strongest observable here and it is fully guest-determined --
// the guest wrote exactly those six bytes -- so it is printed and compared
// exactly. Only the O_NONBLOCK BIT of the status word is emitted, not the raw
// F_GETFL result, because the surrounding access-mode bits are inherited rather
// than guest-chosen.
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <sys/ioctl.h>
#include <unistd.h>

#include "parity_probe.h"

static int get_flags(int fd) {
#ifdef HERMIT_TEST_FIONREAD_SECOND_GETFL_FAILURE
    static int calls;
    if (++calls == 2) {
        errno = EIO;
        return -1;
    }
#endif
    return fcntl(fd, F_GETFL);
}

int main(void) {
    enum { EXPECTED_CHECKS = 6, WROTE_BYTES = 6 };
    int fds[2];
    if (pipe(fds) != 0) {
        printf("fionread ok=0 [pipe fail]\n");
        return 1;
    }

    // Guest writes exactly WROTE_BYTES bytes into the pipe.
    if (write(fds[1], "hello\n", WROTE_BYTES) != WROTE_BYTES) {
        printf("fionread ok=0 [write fail]\n");
        return 1;
    }

    // (1) FIONREAD reports bytes readable; (2) the count equals what we wrote.
    int navail = -1;
    int fionread_ok = ioctl(fds[0], FIONREAD, &navail) == 0;
    navail = (int)parity_mutate_i64("navail", navail);
    fionread_ok = (int)parity_mutate_i64("fionread_ok", fionread_ok);
    int navail_exact = navail == WROTE_BYTES;

    // (3) FIONBIO sets non-blocking; (4) fcntl F_GETFL reflects O_NONBLOCK.
    int on = 1;
    int fionbio_set = ioctl(fds[0], FIONBIO, &on) == 0;
    fionbio_set = (int)parity_mutate_i64("fionbio_set", fionbio_set);
    int fl = get_flags(fds[0]);
    int nonblock_after_set = fl >= 0 && (fl & O_NONBLOCK) ? 1 : 0;
    nonblock_after_set =
        (int)parity_mutate_i64("nonblock_after_set", nonblock_after_set);

    // (5) FIONBIO clears non-blocking; (6) F_GETFL shows O_NONBLOCK cleared.
    int off = 0;
    int fionbio_clear = ioctl(fds[0], FIONBIO, &off) == 0;
    fionbio_clear = (int)parity_mutate_i64("fionbio_clear", fionbio_clear);
    fl = get_flags(fds[0]);
    int nonblock_after_clear = fl >= 0 ? ((fl & O_NONBLOCK) != 0) : -1;

    close(fds[0]);
    close(fds[1]);
    int clear_readback_ok = fl >= 0;
    clear_readback_ok =
        (int)parity_mutate_i64("clear_readback_ok", clear_readback_ok);
    nonblock_after_clear = (int)parity_mutate_i64(
        "nonblock_after_clear", nonblock_after_clear);
    int ok = fionread_ok + navail_exact + fionbio_set + nonblock_after_set +
        fionbio_clear + (clear_readback_ok && nonblock_after_clear == 0);
    printf(
        "fionread ok=%d navail=%d fionread_ok=%d fionbio_set=%d "
        "nonblock_after_set=%d fionbio_clear=%d clear_readback_ok=%d "
        "nonblock_after_clear=%d\n",
        ok,
        navail,
        fionread_ok,
        fionbio_set,
        nonblock_after_set,
        fionbio_clear,
        clear_readback_ok,
        nonblock_after_clear);
    return ok == EXPECTED_CHECKS ? 0 : 1;
}
