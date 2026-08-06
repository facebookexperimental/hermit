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
 */

#define _GNU_SOURCE
#include <signal.h>
#include <stdio.h>
#include <sys/signalfd.h>
#include <unistd.h>

int main(void) {
    int ok = 0;

    sigset_t m1;
    sigemptyset(&m1);
    sigaddset(&m1, SIGUSR1);
    if (sigprocmask(SIG_BLOCK, &m1, NULL) == 0) {
        ok++;
    }
    int fd1 = signalfd(-1, &m1, SFD_NONBLOCK | SFD_CLOEXEC);
    if (fd1 >= 0) {
        ok++;
    }

    sigset_t m2;
    sigemptyset(&m2);
    sigaddset(&m2, SIGUSR2);
    if (sigprocmask(SIG_BLOCK, &m2, NULL) == 0) {
        ok++;
    }
    int fd2 = signalfd(-1, &m2, SFD_NONBLOCK | SFD_CLOEXEC);
    if (fd2 >= 0 && fd2 != fd1) {
        ok++;
    }

    if (fd1 >= 0 && close(fd1) == 0) {
        ok++;
    }
    if (fd2 >= 0 && close(fd2) == 0) {
        ok++;
    }

    printf("sfd ok=%d\n", ok);
    return 0;
}
