/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Backend-parity contract: a HARDWARE-GENERATED trap is reported to the guest
 * identically on every backend.
 *
 * This is the surface that signal_waitstatus_identity.c deliberately does not
 * cover. That fixture raises signals with raise(3), which is a plain kill(2) on
 * the calling thread: the kernel queues a signal and no CPU exception is
 * involved. Here the CPU itself faults -- #UD, #DE, #PF, #BP -- and the kernel
 * must synthesize the signal from the trap frame.
 *
 * The distinction matters most for the backends that REWRITE INSTRUCTIONS. DBI
 * executes the guest out of a code cache and e9patch rewrites the binary ahead
 * of time, so the faulting instruction a backend actually runs need not be the
 * one the source wrote, and the trap may be taken in backend-owned code. A
 * backend that mis-attributes such a trap turns a deterministic crash into a
 * wrong signal, a swallowed fault, or a hang -- all guest-observable, none
 * visible to a fixture that only uses raise(3).
 *
 * SCOPE: this fixture covers the traps that every enabled backend delivers --
 * #UD, #DE and #PF. The fourth interesting trap, #BP (int3), is deliberately NOT
 * pinned here, because measurement showed it is not yet a stable contract:
 *
 *   - ptrace and e9patch SWALLOW a guest int3 in every program tried (2026-08-06).
 *     The child runs past the instruction and exits normally; SIGTRAP never
 *     reaches the guest. On the default backend a guest breakpoint silently does
 *     nothing, which matters for debuggers, JITs and sanitizers.
 *   - DBI is PROGRAM-DEPENDENT: it delivered SIGTRAP in a two-case minimal repro
 *     (both with and without a preceding ud2, so ordering is NOT the variable)
 *     yet swallowed it 5/5 in a larger int3-only guest.
 *
 * Adding int3 here would either make this fixture red from birth on the default
 * backend -- and a test that is red from birth gets bypassed rather than fixed --
 * or encode a DBI behaviour that is not yet understood. Neither is a contract.
 * The finding is reported separately for investigation.
 *
 * Two observables per case, both guest-visible and both host-independent:
 *   1. The WAIT STATUS the parent decodes for a child killed by the trap
 *      (WIFSIGNALED / WTERMSIG).
 *   2. The si_code a SA_SIGINFO handler sees for the same trap -- ILL_ILLOPN,
 *      FPE_INTDIV, SEGV_MAPERR. This is the kernel's classification of WHY the
 *      CPU faulted, and it is a strictly finer contract than the signal number
 *      alone: a backend could deliver the right signal for the wrong reason.
 *      si_code is ASSERTED, not merely printed -- printing it while only
 *      checking that the handler ran would leave the finer contract vacuous.
 *
 * DELIBERATELY NOT OBSERVED: siginfo si_addr and the faulting instruction
 * pointer. Both are addresses, and addresses legitimately differ between
 * backends -- DynamoRIO relocates guest memory, and a rewritten instruction has
 * a different address by construction. Comparing them would report a real and
 * expected relocation as a determinism failure.
 *
 * Traps are produced with inline asm (ud2, int3, div by zero) and a volatile
 * NULL store rather than with C that the optimizer is entitled to delete: a
 * compiler that proves the division or the NULL store is undefined may remove
 * it outright, which would silently make the case vacuous at -O2.
 *
 * x86_64 only -- the trap-producing instructions are architecture-specific. The
 * manifest entry requires x86_64.
 */

#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static int failures = 0;

/* ---- the four hardware traps, each as the CPU exception named ---- */

/* #UD: an instruction that is architecturally guaranteed to be undefined. */
static void trap_ud2(void) { __asm__ __volatile__("ud2"); }

/* #DE: integer divide by zero. Written in asm so the optimizer cannot fold it
 * away as undefined behaviour. */
static void trap_divzero(void) {
    __asm__ __volatile__("xorl %%edx, %%edx\n\t"
                         "movl $1, %%eax\n\t"
                         "xorl %%ecx, %%ecx\n\t"
                         "divl %%ecx"
                         :
                         :
                         : "eax", "ecx", "edx", "cc");
}

/* #PF: a store to an unmapped page. volatile keeps the store observable. */
static void trap_nullstore(void) { *(volatile int *)0 = 1; }

struct trap_case {
    const char *label;
    void (*body)(void);
    int expect_sig;
    int expect_code;
    const char *expect_code_name;
};

static const struct trap_case TRAPS[] = {
    {"ud2", trap_ud2, SIGILL, ILL_ILLOPN, "ILL_ILLOPN"},
    {"divzero", trap_divzero, SIGFPE, FPE_INTDIV, "FPE_INTDIV"},
    {"nullstore", trap_nullstore, SIGSEGV, SEGV_MAPERR, "SEGV_MAPERR"},
};
static const int NTRAPS = (int)(sizeof(TRAPS) / sizeof(TRAPS[0]));

/* ---- observable 1: wait status decoded by the parent ---- */

static void check_wait_status(const struct trap_case *t) {
    pid_t pid = fork();
    if (pid == 0) {
        t->body();
        _exit(99); /* reached only if the trap did not fire */
    }
    int status = 0;
    if (pid < 0 || waitpid(pid, &status, 0) != pid) {
        printf("%s/status: FAIL fork or waitpid did not complete\n", t->label);
        failures++;
        return;
    }
    if (WIFEXITED(status)) {
        /* Exit code 99 means the trap never fired -- the case went vacuous. */
        printf("%s/status: FAIL trap did not fire, child exited code=%d\n",
               t->label, WEXITSTATUS(status));
        failures++;
        return;
    }
    if (!WIFSIGNALED(status)) {
        printf("%s/status: FAIL neither exited nor signalled\n", t->label);
        failures++;
        return;
    }
    printf("%s/status: signalled sig=%d\n", t->label, WTERMSIG(status));
    if (WTERMSIG(status) != t->expect_sig) {
        printf("%s/status: FAIL expected signal %d, got %d\n", t->label,
               t->expect_sig, WTERMSIG(status));
        failures++;
    }
}

/* ---- observable 2: si_code seen by a SA_SIGINFO handler ---- */

static volatile sig_atomic_t seen_signo;
static volatile sig_atomic_t seen_code;
/* Set before the trap is triggered so the handler can CHECK, not just report. */
static volatile sig_atomic_t want_code;

static void trap_handler(int signo, siginfo_t *info, void *ucontext) {
    (void)ucontext;
    seen_signo = signo;
    seen_code = info->si_code;
    /* Returning from a hardware trap handler would re-execute the faulting
     * instruction and loop forever, so leave via _exit with a fixed code. The
     * handler-visible values are printed by the child before it exits. */
    char buf[160];
    int ok = (seen_code == want_code);
    int n = snprintf(buf, sizeof(buf), "  handler: signo=%d si_code=%d%s\n",
                     (int)seen_signo, (int)seen_code,
                     ok ? "" : " FAIL unexpected si_code");
    if (n > 0) {
        ssize_t ignored = write(STDOUT_FILENO, buf, (size_t)n);
        (void)ignored;
    }
    /* 70 = handler ran AND si_code matched; 71 = ran with the wrong si_code.
     * The parent distinguishes them, so a wrong si_code fails the test rather
     * than merely changing a printed line. */
    _exit(ok ? 70 : 71);
}

static void check_si_code(const struct trap_case *t) {
    pid_t pid = fork();
    if (pid == 0) {
        struct sigaction sa;
        memset(&sa, 0, sizeof(sa));
        want_code = (sig_atomic_t)t->expect_code;
        sa.sa_sigaction = trap_handler;
        sa.sa_flags = SA_SIGINFO;
        sigemptyset(&sa.sa_mask);
        if (sigaction(t->expect_sig, &sa, NULL) != 0) {
            _exit(98);
        }
        t->body();
        _exit(99); /* trap did not fire */
    }
    int status = 0;
    if (pid < 0 || waitpid(pid, &status, 0) != pid) {
        printf("%s/sicode: FAIL fork or waitpid did not complete\n", t->label);
        failures++;
        return;
    }
    if (WIFEXITED(status) && WEXITSTATUS(status) == 71) {
        printf("%s/sicode: FAIL handler saw the wrong si_code (expected %s=%d)\n",
               t->label, t->expect_code_name, t->expect_code);
        failures++;
        return;
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 70) {
        printf("%s/sicode: FAIL handler did not run (status decoded as ", t->label);
        if (WIFEXITED(status)) {
            printf("exit %d)\n", WEXITSTATUS(status));
        } else if (WIFSIGNALED(status)) {
            printf("signal %d)\n", WTERMSIG(status));
        } else {
            printf("neither)\n");
        }
        failures++;
    }
}

int main(void) {
    for (int i = 0; i < NTRAPS; i++) {
        printf("%s: expect sig=%d si_code=%s\n", TRAPS[i].label,
               TRAPS[i].expect_sig, TRAPS[i].expect_code_name);
        fflush(stdout);
        check_wait_status(&TRAPS[i]);
        fflush(stdout);
        check_si_code(&TRAPS[i]);
        fflush(stdout);
    }
    printf("failures=%d\n", failures);
    return failures == 0 ? 0 : 1;
}
