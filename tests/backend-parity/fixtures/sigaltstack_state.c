/* Backend-parity fixture: sigaltstack alternate-signal-stack state.
 *
 * Exercises the sigaltstack(2) state machine without ever delivering a signal,
 * so it stays a pure process-local register/state contract (no scheduling or
 * signal-frame synthesis):
 *   1. No alternate stack is installed initially -> SS_DISABLE.
 *   2. Installing a fixed-size alternate stack succeeds.
 *   3. Querying reports our exact ss_sp/ss_size and clears SS_DISABLE.
 *   4. Disabling restores SS_DISABLE.
 *
 * The stack buffer is a fixed 64 KiB static object, deliberately avoiding the
 * host-dependent runtime SIGSTKSZ value, so every observed field is a constant
 * and output is "sigaltstack ok=4" on every run. All three backends pass.
 */
#include <signal.h>
#include <stdio.h>
#include <string.h>

static char altbuf[65536]; /* fixed size: avoid host-dependent SIGSTKSZ */

int main(void) {
    stack_t ss, old;
    int ok = 0;

    memset(&old, 0, sizeof old);
    if (sigaltstack(NULL, &old) == 0 && (old.ss_flags & SS_DISABLE)) {
        ok++;
    }

    memset(&ss, 0, sizeof ss);
    ss.ss_sp = altbuf;
    ss.ss_size = sizeof altbuf;
    ss.ss_flags = 0;
    if (sigaltstack(&ss, NULL) == 0) {
        ok++;
    }

    memset(&old, 0, sizeof old);
    if (sigaltstack(NULL, &old) == 0 && old.ss_sp == altbuf &&
        old.ss_size == sizeof altbuf && !(old.ss_flags & SS_DISABLE)) {
        ok++;
    }

    memset(&ss, 0, sizeof ss);
    ss.ss_flags = SS_DISABLE;
    memset(&old, 0, sizeof old);
    if (sigaltstack(&ss, NULL) == 0 && sigaltstack(NULL, &old) == 0 &&
        (old.ss_flags & SS_DISABLE)) {
        ok++;
    }

    printf("sigaltstack ok=%d\n", ok);
    return 0;
}
