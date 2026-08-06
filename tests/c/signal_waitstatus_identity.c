/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Backend-parity contract: the wait status a parent observes for a terminated
 * child is identical on every backend.
 *
 * A terminated child is reported to its parent through one encoded integer.
 * Whether that integer says "exited with code N" or "killed by signal S" is a
 * guest-observable fact, and it must not depend on which backend executed the
 * guest. This pins that contract as a build-failing test rather than as a sweep
 * result, because a sweep's finding decays the moment the code changes.
 *
 * MEASURED STATUS (2026-08-06, release binary, --strict --verify, pinned env,
 * this exact guest). State it precisely, because the prior sweep notes and the
 * measurement disagree and the measurement wins:
 *   - ptrace  PASS. Output byte-identical to a native run.
 *   - dbi     PASS. Output byte-identical to a native run.
 *   - sabre   HANG. Prints through the sigkill case, then never returns from the
 *             SIGILL case; the harness kills it at the manifest timeout.
 *   - kvm     Not measured; it cannot complete a guest on the current host.
 *
 * The sweep that motivated this fixture reported two further DBI defects --
 * death-by-signal reported as a normal exit with code 1, and a hang on SIGILL
 * and on exit from a non-main thread. Neither reproduced here. That is a
 * narrowing, not a refutation of the sweep: this guest raises signals with
 * raise(3) from a forked child, so it does not exercise a hardware-generated
 * trap, which is the likelier trigger. For DBI this fixture is therefore
 * regression prevention rather than capture of a live defect.
 *
 * A violation shows up either as a differing line in this fixture's output or as
 * a nonzero exit from its own assertions, and a hang shows up as a timeout. All
 * three fail the test rather than being written down somewhere and forgotten.
 *
 * Cases, each run in a forked child so the parent survives to decode and print:
 *   - normal exit with a nonzero code (WIFEXITED / WEXITSTATUS)
 *   - death by SIGTERM, SIGKILL, SIGILL, SIGFPE, SIGABRT via abort()
 *     (WIFSIGNALED / WTERMSIG)
 *   - the core flag (WCOREDUMP)
 *   - process exit driven from a NON-MAIN thread
 *
 * DETERMINISM. Nothing host-derived is printed: no pids, no addresses, no
 * timings -- only decoded status fields and a fixed case label, so the output is
 * byte-identical across runs and across backends.
 *
 * The core flag deserves its own note, because it is the one field that would
 * otherwise be host-dependent: whether a core is written depends on RLIMIT_CORE
 * and on host-wide core patterns. The guest therefore sets RLIMIT_CORE to 0
 * itself before forking, as a best-effort narrowing. That is NOT sufficient:
 * measured on this host core_pattern is a PIPE ("|/usr/local/bin/coredumper"),
 * and for piped dumps
 * the kernel IGNORES RLIMIT_CORE, so WCOREDUMP comes back set anyway. The flag
 * is therefore printed as a constant for core-generating signals, and asserted
 * only for signals that never dump core (SIGTERM, SIGKILL), where "core flag
 * clear" is true on every host. That keeps the contract real without importing
 * the host's core policy into the compared output.
 *
 * Signals are raised with raise(3) rather than by executing a trapping
 * instruction. raise() is deterministic and codegen-independent; a
 * hardware-generated trap is a harder and separate contract, and is deliberately
 * not covered here.
 */

#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <unistd.h>

static int failures = 0;

/* Print the decoded status. Only decoded fields are printed -- never the raw
 * integer, which encodes nothing extra but would be noisier to diff. */
static void report(const char *label, int status, int core_is_host_policy) {
    if (WIFEXITED(status)) {
        printf("%s: exited code=%d\n", label, WEXITSTATUS(status));
    } else if (WIFSIGNALED(status)) {
        /* The core flag is only a host-INDEPENDENT observation for signals that
         * never dump core. For core-generating signals it depends on the host's
         * core_pattern and RLIMIT_CORE -- and a PIPED core_pattern makes the
         * kernel ignore RLIMIT_CORE entirely, so pinning the limit is not enough
         * to make it deterministic. Print a constant there instead of a
         * host-derived bit, so the compared output stays byte-identical across
         * hosts while the flag is still asserted where it is meaningful. */
        if (core_is_host_policy) {
            printf("%s: signalled sig=%d core=host-policy\n", label, WTERMSIG(status));
        } else {
            printf("%s: signalled sig=%d core=%d\n", label, WTERMSIG(status),
                   WCOREDUMP(status) ? 1 : 0);
        }
    } else {
        printf("%s: UNEXPECTED neither-exited-nor-signalled\n", label);
        failures++;
    }
    fflush(stdout);
}

static void expect_exited(const char *label, int status, int code) {
    if (!WIFEXITED(status) || WEXITSTATUS(status) != code) {
        printf("%s: FAIL expected exit code=%d\n", label, code);
        failures++;
    }
}

/* A signal that never dumps core must never be reported with the core flag set,
 * on any host and any backend. */
static void expect_no_core(const char *label, int status) {
    if (WIFSIGNALED(status) && WCOREDUMP(status)) {
        printf("%s: FAIL core flag set for a non-core-generating signal\n", label);
        failures++;
    }
}

static void expect_signalled(const char *label, int status, int sig) {
    if (!WIFSIGNALED(status)) {
        printf("%s: FAIL expected death by signal %d, got a normal exit\n", label, sig);
        failures++;
        return;
    }
    if (WTERMSIG(status) != sig) {
        printf("%s: FAIL expected signal %d, got %d\n", label, sig, WTERMSIG(status));
        failures++;
    }
}

/* Fork, run `body` in the child, wait, and hand the status back. */
static int run_child(void (*body)(void)) {
    pid_t pid = fork();
    if (pid == 0) {
        body();
        _exit(99); /* body must not return */
    }
    int status = 0;
    if (pid < 0 || waitpid(pid, &status, 0) != pid) {
        printf("harness: FAIL fork/waitpid did not complete\n");
        failures++;
        return -1;
    }
    return status;
}

static void body_exit_7(void) { _exit(7); }
static void body_sigterm(void) { raise(SIGTERM); }
static void body_sigkill(void) { raise(SIGKILL); }
static void body_sigill(void) { raise(SIGILL); }
static void body_sigfpe(void) { raise(SIGFPE); }
static void body_abort(void) { abort(); }

static void *thread_exits_process(void *arg) {
    (void)arg;
    /* Terminate the whole process from a NON-MAIN thread. */
    _exit(11);
}

static void body_exit_from_thread(void) {
    pthread_t t;
    if (pthread_create(&t, NULL, thread_exits_process, NULL) != 0) {
        _exit(98);
    }
    pthread_join(t, NULL); /* not reached: the thread exits the process */
    _exit(97);
}

int main(void) {
    /* Narrow RLIMIT_CORE rather than trusting an inherited one. Best-effort
     * only, and deliberately NOT load-bearing: the compared output
     * below is independent of whether this succeeds, so neither the result nor
     * the errno is printed. Measured 2026-08-06: hermit returns EPERM here on
     * all four backends (ptrace, e9patch, DBI, SaBRe) even though Linux always
     * permits an unprivileged process to LOWER a soft limit. That is a real
     * hermit-vs-Linux divergence, but it is consistent across backends, so it is
     * not a parity gap and it is not this fixture's contract. Failing on it here
     * would hold this test red for an unrelated defect. */
    struct rlimit no_core = {.rlim_cur = 0, .rlim_max = 0};
    (void)setrlimit(RLIMIT_CORE, &no_core);

    int s;

    s = run_child(body_exit_7);
    report("normal_exit", s, 0);
    expect_exited("normal_exit", s, 7);

    s = run_child(body_sigterm);
    report("sigterm", s, 0);
    expect_signalled("sigterm", s, SIGTERM);
    expect_no_core("sigterm", s);

    s = run_child(body_sigkill);
    report("sigkill", s, 0);
    expect_signalled("sigkill", s, SIGKILL);
    expect_no_core("sigkill", s);

    s = run_child(body_sigill);
    report("sigill", s, 1);
    expect_signalled("sigill", s, SIGILL);

    s = run_child(body_sigfpe);
    report("sigfpe", s, 1);
    expect_signalled("sigfpe", s, SIGFPE);

    s = run_child(body_abort);
    report("abort", s, 1);
    expect_signalled("abort", s, SIGABRT);

    s = run_child(body_exit_from_thread);
    report("exit_from_non_main_thread", s, 0);
    expect_exited("exit_from_non_main_thread", s, 11);

    printf("failures=%d\n", failures);
    return failures == 0 ? 0 : 1;
}
