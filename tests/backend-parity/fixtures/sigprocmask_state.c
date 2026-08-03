/*
 * Signal-mask state-machine parity fixture.
 *
 * Exercises rt_sigprocmask as a pure process-local state machine: block,
 * query, selective unblock, selective re-block, and full clear. No signal is
 * ever raised or delivered, so this stays clear of the gated signal-delivery /
 * frame-synthesis path (see process_wait_lifecycle) and of any scheduling
 * behavior. Every observable is the blocked-mask membership read straight back
 * from the kernel, which must be identical across ptrace, DBI, and KVM.
 */
#include <signal.h>
#include <stdio.h>
#include <string.h>

int main(void) {
    sigset_t set;
    sigset_t cur;
    int ok = 0;

    /* check 1: SIG_SETMASK installs a mask blocking USR1 and USR2. */
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    sigaddset(&set, SIGUSR2);
    if (sigprocmask(SIG_SETMASK, &set, NULL) == 0) {
        ok++;
    }

    /* check 2: query with a NULL action reflects both signals blocked. */
    sigemptyset(&cur);
    if (sigprocmask(SIG_SETMASK, NULL, &cur) == 0 &&
        sigismember(&cur, SIGUSR1) == 1 && sigismember(&cur, SIGUSR2) == 1) {
        ok++;
    }

    /* check 3: SIG_UNBLOCK USR1 leaves only USR2 blocked. */
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    sigemptyset(&cur);
    if (sigprocmask(SIG_UNBLOCK, &set, NULL) == 0 &&
        sigprocmask(SIG_SETMASK, NULL, &cur) == 0 &&
        sigismember(&cur, SIGUSR1) == 0 && sigismember(&cur, SIGUSR2) == 1) {
        ok++;
    }

    /* check 4: SIG_BLOCK USR1 restores both blocked. */
    sigemptyset(&cur);
    if (sigprocmask(SIG_BLOCK, &set, NULL) == 0 &&
        sigprocmask(SIG_SETMASK, NULL, &cur) == 0 &&
        sigismember(&cur, SIGUSR1) == 1 && sigismember(&cur, SIGUSR2) == 1) {
        ok++;
    }

    /* check 5: clearing the mask leaves neither signal blocked. */
    sigemptyset(&set);
    sigemptyset(&cur);
    if (sigprocmask(SIG_SETMASK, &set, NULL) == 0 &&
        sigprocmask(SIG_SETMASK, NULL, &cur) == 0 &&
        sigismember(&cur, SIGUSR1) == 0 && sigismember(&cur, SIGUSR2) == 0) {
        ok++;
    }

    printf("sigprocmask ok=%d\n", ok);
    return 0;
}
