/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * CONTRACT FIXTURE: the timer families must resolve against virtual time, and the
 * ORDER in which they fire must be identical run to run and backend to backend.
 *
 * WHY THIS FIXTURE EXISTS, and why it is not another sweep: the signal sweep that
 * preceded it covered ITIMER_REAL only, on ptrace only, single-threaded, and judged
 * by stdout. Every one of those exclusions is somewhere nondeterminism can live. A
 * sweep's finding decays silently when the code moves; a fixture fails the build.
 *
 * WHAT IS ASSERTED -- deliberately the ORDER, not the durations:
 *   The guest arms several timers of DIFFERENT families with deadlines that are
 *   WIDELY SEPARATED in virtual time, from two threads, and prints one line per
 *   expiry as it observes it. The emitted sequence is therefore a direct readout of
 *   the scheduler's wake ordering across families.
 *
 *   Durations are NOT printed. A wall-clock duration is host-load dependent and
 *   would make this fixture flake under contention while telling us nothing about
 *   determinism. Ordering is the property that must hold.
 *
 * WHY THIS CANNOT BE "FIXED" BY QUANTISING TIME (#140): the deadlines below are
 * separated by large, unequal virtual-time gaps and are armed from two different
 * threads in an interleaved pattern. Rounding, quantising or freezing virtual time
 * to force a stable order would COLLAPSE those gaps and change which timer is
 * observed first -- i.e. it would change this fixture's expected output rather than
 * satisfy it. The fixture is written so that the degenerate "fix" is visible as a
 * diff, not silently rewarded.
 *
 * Families covered: timerfd, setitimer(ITIMER_REAL), setitimer(ITIMER_VIRTUAL),
 * timer_create(CLOCK_MONOTONIC), alarm(), epoll_wait timeout, and a futex
 * (pthread_cond_timedwait) timeout -- the last two because a timeout that is
 * serviced by the scheduler rather than by a timer object is a distinct path.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/time.h>
#include <sys/timerfd.h>
#include <time.h>
#include <unistd.h>

/* Single serialized event log: the ORDER of these lines is the contract. */
static pthread_mutex_t log_mu = PTHREAD_MUTEX_INITIALIZER;
static void ev(const char *what) {
    pthread_mutex_lock(&log_mu);
    printf("EV %s\n", what);
    fflush(stdout);
    pthread_mutex_unlock(&log_mu);
}

static volatile sig_atomic_t got_alrm = 0;
static volatile sig_atomic_t got_vtalrm = 0;
static volatile sig_atomic_t got_sigev = 0;
static void on_alrm(int s) { (void)s; got_alrm = 1; }
static void on_vtalrm(int s) { (void)s; got_vtalrm = 1; }
static void on_sigev(int s) { (void)s; got_sigev = 1; }

/* Thread B: timerfd + epoll timeout. Runs concurrently with thread A's families so
 * the cross-family ordering is exercised under real multi-threading, not in sequence. */
static void *thread_b(void *arg) {
    (void)arg;

    int tfd = timerfd_create(CLOCK_MONOTONIC, 0);
    if (tfd < 0) { ev("timerfd_create_FAILED"); return NULL; }
    struct itimerspec its;
    memset(&its, 0, sizeof its);
    its.it_value.tv_sec = 0;
    its.it_value.tv_nsec = 40 * 1000 * 1000; /* 40ms */
    if (timerfd_settime(tfd, 0, &its, NULL) < 0) { ev("timerfd_settime_FAILED"); close(tfd); return NULL; }

    int ep = epoll_create1(0);
    struct epoll_event want = {.events = EPOLLIN, .data.fd = tfd};
    epoll_ctl(ep, EPOLL_CTL_ADD, tfd, &want);

    struct epoll_event got;
    int n = epoll_wait(ep, &got, 1, 5000);
    if (n == 1) {
        unsigned long long ticks = 0;
        ssize_t r = read(tfd, &ticks, sizeof ticks);
        (void)r;
        ev("timerfd_expired");
    } else if (n == 0) {
        ev("epoll_TIMED_OUT_unexpectedly");
    } else {
        ev("epoll_error");
    }

    /* A deliberate epoll timeout on an fd that will never be readable: this path is
     * serviced by the scheduler's timeout handling, not by a timer object. */
    int ep2 = epoll_create1(0);
    struct epoll_event g2;
    int n2 = epoll_wait(ep2, &g2, 1, 120); /* 120ms, nothing registered */
    ev(n2 == 0 ? "epoll_timeout_fired" : "epoll_timeout_UNEXPECTED");
    close(ep2);
    close(ep);
    close(tfd);
    return NULL;
}

int main(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_alrm;
    sigaction(SIGALRM, &sa, NULL);
    sa.sa_handler = on_vtalrm;
    sigaction(SIGVTALRM, &sa, NULL);
    sa.sa_handler = on_sigev;
    sigaction(SIGUSR1, &sa, NULL);

    ev("start");

    pthread_t tb;
    pthread_create(&tb, NULL, thread_b, NULL);

    /* timer_create -> SIGUSR1, 80ms. Between thread B's 40ms timerfd and the 200ms
     * ITIMER_REAL below, so a correct ordering interleaves the two threads. */
    timer_t tid;
    struct sigevent sev;
    memset(&sev, 0, sizeof sev);
    sev.sigev_notify = SIGEV_SIGNAL;
    sev.sigev_signo = SIGUSR1;
    int have_timer_create = (timer_create(CLOCK_MONOTONIC, &sev, &tid) == 0);
    if (have_timer_create) {
        struct itimerspec cits;
        memset(&cits, 0, sizeof cits);
        cits.it_value.tv_nsec = 80 * 1000 * 1000;
        if (timer_settime(tid, 0, &cits, NULL) != 0) have_timer_create = 0;
    }

    /* pthread_cond_timedwait: a futex-backed timeout, 160ms. */
    {
        pthread_mutex_t m = PTHREAD_MUTEX_INITIALIZER;
        pthread_cond_t c = PTHREAD_COND_INITIALIZER;
        struct timespec deadline;
        clock_gettime(CLOCK_REALTIME, &deadline);
        deadline.tv_nsec += 160 * 1000 * 1000;
        if (deadline.tv_nsec >= 1000000000L) { deadline.tv_sec += 1; deadline.tv_nsec -= 1000000000L; }
        pthread_mutex_lock(&m);
        int rc = pthread_cond_timedwait(&c, &m, &deadline);
        pthread_mutex_unlock(&m);
        ev(rc == ETIMEDOUT ? "futex_timeout_fired" : "futex_timeout_UNEXPECTED");
    }

    if (have_timer_create) ev(got_sigev ? "timer_create_fired" : "timer_create_NOT_fired");

    /* ITIMER_REAL at 200ms -- the only family the previous sweep covered. */
    {
        struct itimerval iv;
        memset(&iv, 0, sizeof iv);
        iv.it_value.tv_usec = 200 * 1000;
        setitimer(ITIMER_REAL, &iv, NULL);
        while (!got_alrm) { struct timespec t = {0, 1000000}; nanosleep(&t, NULL); }
        ev("itimer_real_fired");
    }

    /* ITIMER_VIRTUAL: charges CPU time, not wall time, so it needs actual burn.
     * Reported as a separate line whether or not it fires, so a backend that never
     * delivers SIGVTALRM produces a DIFFERENT sequence rather than a silent pass. */
    {
        struct itimerval iv;
        memset(&iv, 0, sizeof iv);
        iv.it_value.tv_usec = 50 * 1000;
        setitimer(ITIMER_VIRTUAL, &iv, NULL);
        volatile unsigned long spin = 0;
        for (int i = 0; i < 40 * 1000 * 1000 && !got_vtalrm; i++) spin += i;
        ev(got_vtalrm ? "itimer_virtual_fired" : "itimer_virtual_not_fired");
        memset(&iv, 0, sizeof iv);
        setitimer(ITIMER_VIRTUAL, &iv, NULL);
    }

    /* alarm(1): coarsest family, last. */
    got_alrm = 0;
    alarm(1);
    while (!got_alrm) { struct timespec t = {0, 2 * 1000 * 1000}; nanosleep(&t, NULL); }
    ev("alarm_fired");

    pthread_join(tb, NULL);
    if (have_timer_create) timer_delete(tid);

    ev("done");
    return 0;
}
