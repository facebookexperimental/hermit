/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* The first pending standard signal keeps its complete siginfo when another
 * source sends the same signal. Observe both source orders and guest-only
 * controls against native Linux, including a legal self-queued SI_USER.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

_Static_assert(sizeof(siginfo_t) == 128, "this S0 fixture requires Linux 128-byte siginfo");

#define QUEUED_ERRNO 73
#define QUEUED_VALUE 0x13579bdf
#define POLL_LIMIT 65536U

static volatile sig_atomic_t hits;
static volatile unsigned char captured[128];

static void fail(const char *message, size_t length) {
    /* One bounded diagnostic, no retry loop; the outer existing test owner
     * retains stderr and enforces its unchanged timeout/output caps. */
    (void)write(STDERR_FILENO, message, length);
    _exit(2);
}
#define REQUIRE(predicate) do { \
    if (!(predicate)) { \
        static const char message[] = "first-copy observation failed: " #predicate "\n"; \
        fail(message, sizeof message - 1); \
    } \
} while (0)

static void capture(int signo, siginfo_t *info, void *context) {
    (void)context;
    if (signo != SIGALRM || info == NULL || hits != 0) {
        static const char message[] = "first-copy handler invocation failed\n";
        fail(message, sizeof message - 1);
    }
    const volatile unsigned char *source = (const volatile unsigned char *)info;
    for (size_t index = 0; index < sizeof captured; index++) {
        captured[index] = source[index];
    }
    hits = 1;
}

static int pending_alarm(void) {
    sigset_t pending;
    REQUIRE(sigpending(&pending) == 0);
    int member = sigismember(&pending, SIGALRM);
    REQUIRE(member == 0 || member == 1);
    return member;
}

static int64_t monotonic_ns(void) {
    struct timespec time;
    REQUIRE(clock_gettime(CLOCK_MONOTONIC, &time) == 0);
    REQUIRE(time.tv_sec >= 0 && time.tv_nsec >= 0 && time.tv_nsec < 1000000000L);
    return (int64_t)time.tv_sec * INT64_C(1000000000) + time.tv_nsec;
}

static int64_t arm_timer(void) {
    struct itimerval timer;
    memset(&timer, 0, sizeof timer);
    timer.it_value.tv_usec = 1000;
    REQUIRE(setitimer(ITIMER_REAL, &timer, NULL) == 0);
    return monotonic_ns();
}

static void await_expiry_with_pending(int64_t armed_after) {
    for (unsigned int attempt = 0; attempt < POLL_LIMIT; attempt++) {
        /* Order matters: a time sample taken AFTER getitimer could cross
         * expiry after an early truncated-zero result had already returned.
         * The post-arm origin is conservative: it is later than the actual
         * relative timer start. No elapsed value is printed or tolerated. */
        int64_t before_query = monotonic_ns();
        REQUIRE(before_query >= armed_after);
        struct itimerval timer;
        REQUIRE(getitimer(ITIMER_REAL, &timer) == 0);
        int pending = pending_alarm();
        if (before_query - armed_after >= INT64_C(1000000) &&
            timer.it_value.tv_sec == 0 && timer.it_value.tv_usec == 0 && pending) {
            REQUIRE(hits == 0);
            return;
        }
        /* Bound failure, not a successful timing tolerance. The existing
         * 57-second command cap is unchanged. Pending/expiry are required;
         * a timeout/iteration exhaustion is always a nonzero failure. */
        REQUIRE(monotonic_ns() - armed_after < INT64_C(2000000000));
        /* Cooperative setup requires a scheduler turn after an unsuccessful
         * poll. This tests first-copy metadata once expiry is pending, not
         * polling-only timer progress with both timeslices disabled. */
        REQUIRE(syscall(SYS_sched_yield) == 0);
    }
    REQUIRE(0 && "timer expiry/pending poll exhausted");
}

static void queue_self(pid_t self, const siginfo_t *info) {
    /* Linux permits a nonnegative si_code only for the caller's own process.
     * This is the real rt_sigqueueinfo API, not kill/raise with invented
     * expected metadata. The syscall supplies the exact initialized object. */
    long result = syscall(SYS_rt_sigqueueinfo, self, SIGALRM, info);
    int saved_errno = result < 0 ? errno : 0;
    REQUIRE(result == 0 && saved_errno == 0);
    REQUIRE(pending_alarm() == 1 && hits == 0);
}

int main(int argc, char **argv) {
    REQUIRE(argc == 2);
    int guest_first = strcmp(argv[1], "guest-first") == 0;
    int timer_first = strcmp(argv[1], "timer-first") == 0;
    int guest_only = strcmp(argv[1], "guest-only") == 0;
    int forged_user = strcmp(argv[1], "self-si-user") == 0;
    REQUIRE(guest_first || timer_first || guest_only || forged_user);

    sigset_t blocked;
    sigset_t original;
    REQUIRE(sigemptyset(&blocked) == 0);
    REQUIRE(sigaddset(&blocked, SIGALRM) == 0);
    REQUIRE(sigprocmask(SIG_BLOCK, &blocked, &original) == 0);
    REQUIRE(pending_alarm() == 0);
    struct itimerval initial;
    REQUIRE(getitimer(ITIMER_REAL, &initial) == 0);
    REQUIRE(initial.it_value.tv_sec == 0 && initial.it_value.tv_usec == 0);

    struct sigaction action;
    memset(&action, 0, sizeof action);
    action.sa_sigaction = capture;
    action.sa_flags = SA_SIGINFO;
    REQUIRE(sigemptyset(&action.sa_mask) == 0);
    REQUIRE(sigaction(SIGALRM, &action, NULL) == 0);

    pid_t self = getpid();
    uid_t uid = getuid();
    REQUIRE(self > 0);
    siginfo_t queued;
    memset(&queued, 0, sizeof queued);
    queued.si_signo = SIGALRM;
    queued.si_code = forged_user ? SI_USER : SI_QUEUE;
    queued.si_errno = QUEUED_ERRNO;
    queued.si_pid = self;
    queued.si_uid = uid;
    queued.si_value.sival_int = QUEUED_VALUE;

    if (timer_first) {
        int64_t armed_after = arm_timer();
        await_expiry_with_pending(armed_after);
        /* The timer is the only send so far, so pending authenticates which
         * source won before the actual queued guest send below. */
        queue_self(self, &queued);
    } else {
        queue_self(self, &queued);
        if (guest_first) {
            int64_t armed_after = arm_timer();
            await_expiry_with_pending(armed_after);
        }
    }
    REQUIRE(pending_alarm() == 1 && hits == 0);
    REQUIRE(sigprocmask(SIG_UNBLOCK, &blocked, NULL) == 0);
    REQUIRE(hits == 1);
    REQUIRE(sigprocmask(SIG_BLOCK, &blocked, NULL) == 0);
    REQUIRE(pending_alarm() == 0);

    siginfo_t observed;
    unsigned char *bytes = (unsigned char *)&observed;
    for (size_t index = 0; index < sizeof captured; index++) {
        bytes[index] = captured[index];
    }
    siginfo_t expected;
    if (timer_first) {
        /* All 128 bytes: signo+SI_KERNEL, including zero errno, header
         * padding, pid/uid, value, union and expansion bytes. */
        memset(&expected, 0, sizeof expected);
        expected.si_signo = SIGALRM;
        expected.si_code = SI_KERNEL;
    } else {
        expected = queued;
    }
    REQUIRE(memcmp(&observed, &expected, sizeof observed) == 0);
    REQUIRE(observed.si_signo == SIGALRM && observed.si_code == expected.si_code);
    REQUIRE(observed.si_errno == expected.si_errno);
    REQUIRE(observed.si_pid == expected.si_pid && observed.si_uid == expected.si_uid);
    REQUIRE(observed.si_value.sival_int == expected.si_value.sival_int);

    /* The only PID/UID observations exposed are equality booleans; native
     * Linux and Hermit's virtual identities need not be numerically equal. */
    int printed = printf(
        "mode=%s hits=1 pending_before=1 pending_after=0 expired=%d "
        "signo=%d code=%d errno=%d value=%d pid_ok=1 uid_ok=1 raw128=1\n",
        argv[1], guest_first || timer_first, observed.si_signo,
        observed.si_code, observed.si_errno, observed.si_value.sival_int);
    REQUIRE(printed > 0 && fflush(stdout) == 0);
    REQUIRE(sigprocmask(SIG_SETMASK, &original, NULL) == 0);
    return 0;
}
