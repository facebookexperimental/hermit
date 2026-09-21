/*
 * signalfd_create: cross-backend parity contract for signalfd(2) descriptor
 * creation.
 *
 * The contract blocks a signal, creates a non-blocking close-on-exec signalfd
 * for it, then blocks a second signal and creates a second, distinct signalfd,
 * and closes both. It never reads a signal from either descriptor: a read would
 * block until a signal is delivered, which is a signal-delivery/timing channel
 * this matrix keeps out of the non-gated lane. Only descriptor creation and the
 * distinct-fd invariant are asserted.
 *
 * This deliberately avoids updating an existing signalfd's mask
 * (`signalfd(fd, ...)` on a live descriptor), which diverges across backends;
 * only fresh creation with `signalfd(-1, ...)` is exercised, and that is a
 * clean triple pass.
 *
 * EACH STEP IS REPORTED SEPARATELY and the fixture fails closed. "sfd ok=6"
 * summed six independent contracts, so a backend that failed to block the second
 * signal and a backend that handed back a duplicate descriptor both printed
 * "sfd ok=5" and compared EQUAL. The existing exit-status guard catches the
 * lower total, but does not identify which step failed.
 *
 * The raw descriptor NUMBERS are deliberately not printed. A file descriptor is
 * allocation state the guest inherits rather than a value it chooses, so
 * emitting it would put allocator behaviour into the observation; what this
 * contract actually asserts is validity and DISTINCTNESS, and fd2_distinct
 * carries exactly that. This fixture is therefore de-aliased rather than
 * value-printing, the fallback cwd_roundtrip uses.
 */

#define _GNU_SOURCE
#include <signal.h>
#include <stdio.h>
#include <sys/signalfd.h>
#include <unistd.h>

int main(void) {
    enum { EXPECTED_CHECKS = 6 };

    sigset_t m1;
    sigemptyset(&m1);
    sigaddset(&m1, SIGUSR1);
    int block1 = sigprocmask(SIG_BLOCK, &m1, NULL) == 0;
    int fd1 = signalfd(-1, &m1, SFD_NONBLOCK | SFD_CLOEXEC);
    int fd1_valid = fd1 >= 0;

    sigset_t m2;
    sigemptyset(&m2);
    sigaddset(&m2, SIGUSR2);
    int block2 = sigprocmask(SIG_BLOCK, &m2, NULL) == 0;
    int fd2 = signalfd(-1, &m2, SFD_NONBLOCK | SFD_CLOEXEC);
    int fd2_distinct = fd2 >= 0 && fd2 != fd1;

    int closed1 = fd1 >= 0 && close(fd1) == 0;
    int closed2 = fd2 >= 0 && close(fd2) == 0;

    int ok =
        block1 + fd1_valid + block2 + fd2_distinct + closed1 + closed2;
    printf(
        "sfd ok=%d block1=%d fd1_valid=%d block2=%d fd2_distinct=%d "
        "closed1=%d closed2=%d\n",
        ok,
        block1,
        fd1_valid,
        block2,
        fd2_distinct,
        closed1,
        closed2);
    return ok == EXPECTED_CHECKS ? 0 : 1;
}
