/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Multi-writer pipe ordering parity probe.
 *
 * The existing pipe fixtures do not reach this case: pipe_ipc forks exactly ONE
 * child, and pipe_capacity / pipe2_flags do not fork at all. So nothing yet
 * exercises SEVERAL CONCURRENT WRITERS sharing one pipe, which is the shape
 * where process scheduling becomes guest-observable.
 *
 * The parent creates a pipe and forks N children. Each child burns a
 * DELIBERATELY UNEQUAL amount of CPU -- child i does (N-i) units, so on a real
 * machine the later children finish FIRST -- then writes one identifying line
 * and exits. The parent drains to EOF, prints what it read, then reaps all N
 * with wait(-1) and prints the reap order.
 *
 * Two guest-observable orderings therefore fall out of the schedule, and both
 * are printed to stdout:
 *   1. the ORDER OF LINES in the pipe (which writer won the race), and
 *   2. the REAP ORDER from wait(-1).
 * Natively both vary run to run precisely because the CPU burns are unequal.
 * Under Hermit's deterministic scheduler both must be fixed, and --verify
 * compares this stdout byte-for-byte across two runs, so a scheduling
 * divergence fails the test rather than merely looking different.
 *
 * The unequal burn is load-bearing: with equal work the children would tend to
 * finish in fork order on any machine, and the fixture would pass without
 * discriminating a deterministic scheduler from an accidental one.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/wait.h>

#define NKIDS 5

int main(void) {
    int failures = 0;
    int fd[2];
    if (pipe(fd) != 0) {
        perror("pipe");
        return 2;
    }

    pid_t kids[NKIDS];
    for (int i = 0; i < NKIDS; i++) {
        pid_t p = fork();
        if (p < 0) {
            perror("fork");
            return 2;
        }
        if (p == 0) {
            close(fd[0]);
            /* Unequal work: later children finish sooner on a real machine. */
            volatile unsigned long acc = 0;
            for (unsigned long k = 0; k < (unsigned long)(NKIDS - i) * 200000UL; k++) {
                acc += k;
            }
            char buf[32];
            int n = snprintf(buf, sizeof buf, "w%d\n", i);
            if (write(fd[1], buf, (size_t)n) != n) {
                _exit(3);
            }
            /*
             * `acc` is volatile, so every iteration of the busy loop above is
             * an observable access the compiler must emit; nothing further is
             * needed to keep the unequal work alive. The previous form here
             * guarded the exit code with `(acc & 1) == 2`, which cannot hold
             * for any value of a one-bit mask -- gcc rejects it under the
             * harness's own `-Werror=tautological-compare`
             * (ci/test_harness.sh:2395), so this guest never compiled and the
             * cell could never have produced a witness. The `4` arm was
             * unreachable by construction, so dropping it leaves the observed
             * exit code exactly as it always would have been: `i + 1`.
             */
            _exit(i + 1);
        }
        kids[i] = p;
    }
    close(fd[1]);

    char buf[512];
    ssize_t total = 0, r;
    while ((r = read(fd[0], buf + total, sizeof buf - (size_t)total - 1)) > 0) {
        total += r;
    }
    if (r < 0) {
        perror("read");
        return 2;
    }
    buf[total] = '\0';
    fputs(buf, stdout);

    if (total != (ssize_t)(NKIDS * 3)) {
        fprintf(stderr, "expected %d pipe bytes, got %zd\n", NKIDS * 3, total);
        failures++;
    }
    for (int i = 0; i < NKIDS; i++) {
        char needle[4];
        snprintf(needle, sizeof needle, "w%d\n", i);
        char* first = strstr(buf, needle);
        if (first == NULL || strstr(first + 1, needle) != NULL) {
            fprintf(stderr, "writer %d did not contribute exactly one complete line\n", i);
            failures++;
        }
    }

    int reaped[NKIDS] = {0};
    for (int i = 0; i < NKIDS; i++) {
        int st = 0;
        pid_t got = wait(&st);
        if (got < 0) {
            perror("wait");
            return 2;
        }
        int slot = -1;
        for (int j = 0; j < NKIDS; j++) {
            if (kids[j] == got) {
                slot = j;
            }
        }
        int code = WIFEXITED(st) ? WEXITSTATUS(st) : -1;
        printf("reap%d slot=%d code=%d\n", i, slot, code);
        if (slot < 0 || reaped[slot] || code != slot + 1) {
            failures++;
        } else {
            reaped[slot] = 1;
        }
    }
    fflush(stdout);
    return failures == 0 ? 0 : 1;
}
