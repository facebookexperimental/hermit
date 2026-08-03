// epoll_pwait2(2) non-blocking readiness probe.
//
// This is the syscall-441 sibling of the epoll_readiness row (which drives
// epoll_wait). epoll_pwait2 is the newer epoll wait entry point that takes an
// absolute-precision struct timespec timeout and an optional signal mask.
// ptrace and DBI forward it faithfully; KVM's ElfExecutor does not implement
// syscall 441 and returns a deterministic ENOSYS, so this is a KVM gap rather
// than a triple-pass row.
//
// The probe uses a zero timespec ({0, 0}) so epoll_pwait2 polls without ever
// blocking — a blocking epoll wait would livelock the DBI no-preemption
// scheduler, and any real timeout would be a gated wall-clock channel. No time
// value is asserted: the contract only checks the readiness count, the woken
// descriptor, and its event bits over an eventfd that the fixture itself arms.
//
// ptrace and DBI pass all seven checks (ok=7). KVM passes only the three that
// do not depend on epoll_pwait2 (add, arm, del) and fails the four wait-driven
// checks because syscall 441 returns -1/ENOSYS (ok=3).

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#ifndef SYS_epoll_pwait2
#define SYS_epoll_pwait2 441
#endif

static int epoll_pwait2_call(int epfd, struct epoll_event *ev, int maxev,
                             const struct timespec *timeout) {
    // Raw syscall: sigmask NULL, sigsetsize 0 (no signal mask installed).
    return (int)syscall(SYS_epoll_pwait2, epfd, ev, maxev, timeout, (void *)0,
                        (size_t)0);
}

int main(void) {
    int ok = 0;
    struct timespec zero = {0, 0};

    int efd = eventfd(0, EFD_NONBLOCK);
    int ep = epoll_create1(EPOLL_CLOEXEC);
    if (efd < 0 || ep < 0) {
        printf("epoll_pwait2 SETUP_FAIL\n");
        return 1;
    }

    struct epoll_event ev;
    memset(&ev, 0, sizeof ev);
    ev.events = EPOLLIN;
    ev.data.fd = efd;
    // (1) Register the eventfd for read readiness.
    if (epoll_ctl(ep, EPOLL_CTL_ADD, efd, &ev) == 0) ok++;

    uint64_t one = 1;
    // (2) Arm the eventfd so it becomes readable.
    if (write(efd, &one, sizeof one) == (ssize_t)sizeof one) ok++;

    struct epoll_event out[4];
    memset(out, 0, sizeof out);
    int n = epoll_pwait2_call(ep, out, 4, &zero);
    // (3) Exactly one ready descriptor.
    if (n == 1) ok++;
    // (4) It is the eventfd we armed.
    if (n == 1 && out[0].data.fd == efd) ok++;
    // (5) Reported ready for read.
    if (n == 1 && (out[0].events & EPOLLIN)) ok++;

    // (6) Deregister the descriptor.
    if (epoll_ctl(ep, EPOLL_CTL_DEL, efd, &ev) == 0) ok++;
    int n2 = epoll_pwait2_call(ep, out, 4, &zero);
    // (7) Nothing ready after removal (non-blocking, returns 0 immediately).
    if (n2 == 0) ok++;

    close(efd);
    close(ep);
    printf("epoll_pwait2 ok=%d\n", ok);
    return 0;
}
