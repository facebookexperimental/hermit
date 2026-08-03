/*
 * Signal-disposition state-machine parity fixture.
 *
 * Exercises rt_sigaction as a pure process-local state machine: query the
 * default disposition, install SIG_IGN, install a plain handler, install a
 * SA_SIGINFO/SA_RESTART handler, then restore SIG_DFL -- reading the
 * disposition back after each transition. No signal is ever raised or
 * delivered, so this stays clear of the gated signal-delivery / frame-synthesis
 * path (see process_wait_lifecycle) and of any scheduling behavior. Every
 * comparison is intra-process (the queried handler equals the address we just
 * installed), so no cross-run address stability is required.
 */
#include <signal.h>
#include <stdio.h>
#include <string.h>

static volatile int g_dummy;

static void plain_handler(int sig) {
    g_dummy = sig;
}

static void siginfo_handler(int sig, siginfo_t *info, void *ucontext) {
    (void)info;
    (void)ucontext;
    g_dummy = sig;
}

int main(void) {
    struct sigaction act;
    struct sigaction old;
    int ok = 0;

    /* check 1: USR1 starts at its default disposition. */
    memset(&old, 0, sizeof old);
    if (sigaction(SIGUSR1, NULL, &old) == 0 && old.sa_handler == SIG_DFL) {
        ok++;
    }

    /* check 2: install SIG_IGN and read it back. */
    memset(&act, 0, sizeof act);
    act.sa_handler = SIG_IGN;
    sigemptyset(&act.sa_mask);
    memset(&old, 0, sizeof old);
    if (sigaction(SIGUSR1, &act, NULL) == 0 &&
        sigaction(SIGUSR1, NULL, &old) == 0 && old.sa_handler == SIG_IGN) {
        ok++;
    }

    /* check 3: install a plain handler and read it back. */
    memset(&act, 0, sizeof act);
    act.sa_handler = plain_handler;
    sigemptyset(&act.sa_mask);
    memset(&old, 0, sizeof old);
    if (sigaction(SIGUSR1, &act, NULL) == 0 &&
        sigaction(SIGUSR1, NULL, &old) == 0 && old.sa_handler == plain_handler) {
        ok++;
    }

    /* check 4: SA_SIGINFO|SA_RESTART round-trip via sa_sigaction. */
    memset(&act, 0, sizeof act);
    act.sa_sigaction = siginfo_handler;
    act.sa_flags = SA_SIGINFO | SA_RESTART;
    sigemptyset(&act.sa_mask);
    memset(&old, 0, sizeof old);
    if (sigaction(SIGUSR1, &act, NULL) == 0 &&
        sigaction(SIGUSR1, NULL, &old) == 0 && (old.sa_flags & SA_SIGINFO) &&
        old.sa_sigaction == siginfo_handler) {
        ok++;
    }

    /* check 5: restore the default disposition. */
    memset(&act, 0, sizeof act);
    act.sa_handler = SIG_DFL;
    sigemptyset(&act.sa_mask);
    memset(&old, 0, sizeof old);
    if (sigaction(SIGUSR1, &act, NULL) == 0 &&
        sigaction(SIGUSR1, NULL, &old) == 0 && old.sa_handler == SIG_DFL) {
        ok++;
    }

    printf("sigaction ok=%d\n", ok);
    return 0;
}
