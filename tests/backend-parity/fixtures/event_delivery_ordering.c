/*
 * Event-delivery ordering: inotify coalescing and signalfd delivery order.
 *
 * This is the deliberate COMPLEMENT to the two fixtures already in this
 * directory. inotify_watch.c states it exercises the descriptor lifecycle
 * "without ever reading an event", and signalfd_create.c states it "never
 * reads a signal", both because delivery is a host-timing channel. Determinizing
 * exactly that channel is what Hermit is for, so the excluded half is testable
 * here even though it is not testable natively.
 *
 * Two surfaces, both of which a backend can get wrong with NO syscall failing:
 *
 *   INOTIFY   the ORDER events arrive and WHETHER THEY COALESCE. The kernel may
 *             merge identical consecutive events on the same watch, so N
 *             operations can legitimately yield fewer than N events -- and
 *             which ones merge depends on timing. A backend that shifts timing
 *             shifts the coalescing, and the guest sees a different stream for
 *             identical actions.
 *   SIGNALFD  delivery order with several signals pending behind a blocked
 *             mask. A backend can be correct for handler-based delivery and
 *             wrong through the fd interface; these are different paths.
 *
 * DENOMINATOR. An event count alone cannot distinguish "coalesced" from
 * "dropped", so every count is reported with the operation count that produced
 * it: OPS/EVENTS and RAISED/DELIVERED. A bare EVENTS=3 is unreadable; EVENTS=3
 * OPS=6 says coalescing, and EVENTS=3 OPS=3 says none.
 *
 * NO SLEEPS BETWEEN OPERATIONS. Spacing the writes out is what would suppress
 * the coalescing under test, so the operations run back to back. That is also
 * why this fixture is only expected to be stable under a determinizing backend.
 *
 * THE GUEST BRANCHES ON WHAT IT OBSERVED rather than printing a stream and
 * hoping the harness notices: COALESCED/DISTINCT selects a different summary,
 * and the signal order selects between the POSIX lowest-first contract and
 * anything else. A backend that changes the observation changes the branch.
 *
 * _GNU_SOURCE is supplied by the harness compile flags; do not define it here.
 */

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/signalfd.h>
#include <sys/stat.h>
#include <unistd.h>

enum { EVBUF = 8192, MAX_EVENTS = 64, MAX_SIGS = 8 };

/* ---------------- inotify leg ---------------- */

/* Perform `ops` back-to-back modifications of one file, then drain the watch.
 * Returns the number of events observed, or -1 on a hard error. */
static int inotify_leg(const char *dir, int ops, unsigned *first_mask,
                       bool *all_same_mask) {
    int fd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    if (fd < 0) {
        return -1;
    }
    /* IN_MODIFY only: repeated writes to the same file are the canonical
     * coalescing case, and restricting the mask keeps the stream readable. */
    int wd = inotify_add_watch(fd, dir, IN_MODIFY);
    if (wd < 0) {
        close(fd);
        return -1;
    }

    char path[512];
    snprintf(path, sizeof path, "%s/target", dir);
    int target = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (target < 0) {
        close(fd);
        return -1;
    }

    /* Back to back, deliberately. Each write is an IN_MODIFY on the same name,
     * which is exactly what the kernel is permitted to merge. */
    for (int i = 0; i < ops; i++) {
        if (write(target, "x", 1) != 1) {
            close(target);
            close(fd);
            return -1;
        }
    }
    close(target);

    int seen = 0;
    *first_mask = 0;
    *all_same_mask = true;
    for (;;) {
        char buf[EVBUF] __attribute__((aligned(__alignof__(struct inotify_event))));
        ssize_t n = read(fd, buf, sizeof buf);
        if (n < 0) {
            if (errno == EAGAIN) {
                break; /* drained */
            }
            if (errno == EINTR) {
                continue;
            }
            close(fd);
            return -1;
        }
        if (n == 0) {
            break;
        }
        for (char *p = buf; p < buf + n;) {
            const struct inotify_event *e = (const struct inotify_event *)p;
            if (seen == 0) {
                *first_mask = e->mask;
            } else if (e->mask != *first_mask) {
                *all_same_mask = false;
            }
            if (seen < MAX_EVENTS) {
                seen++;
            }
            p += sizeof(struct inotify_event) + e->len;
        }
    }
    close(fd);
    return seen;
}

/* ---------------- signalfd leg ---------------- */

/* Raise three signals behind a blocked mask, then read them back through a
 * signalfd, recording the delivery order. Returns count delivered or -1. */
static int signalfd_leg(int *order, int cap) {
    /* Chosen so raise order and numeric order DISAGREE: raising 14,10,12 while
     * POSIX delivers the lowest pending signal first means the observed order
     * distinguishes "delivery order" from "raise order". */
    const int raise_order[3] = {SIGALRM, SIGUSR1, SIGUSR2}; /* 14, 10, 12 */

    sigset_t mask;
    sigemptyset(&mask);
    for (int i = 0; i < 3; i++) {
        sigaddset(&mask, raise_order[i]);
    }
    if (sigprocmask(SIG_BLOCK, &mask, NULL) != 0) {
        return -1;
    }

    int sfd = signalfd(-1, &mask, SFD_NONBLOCK | SFD_CLOEXEC);
    if (sfd < 0) {
        return -1;
    }

    /* Back to back, no spacing: all three are pending before the first read. */
    for (int i = 0; i < 3; i++) {
        if (raise(raise_order[i]) != 0) {
            close(sfd);
            return -1;
        }
    }

    int got = 0;
    for (;;) {
        struct signalfd_siginfo si;
        ssize_t n = read(sfd, &si, sizeof si);
        if (n < 0) {
            if (errno == EAGAIN) {
                break;
            }
            if (errno == EINTR) {
                continue;
            }
            close(sfd);
            return -1;
        }
        if (n != (ssize_t)sizeof si) {
            break;
        }
        if (got < cap) {
            order[got] = (int)si.ssi_signo;
        }
        got++;
    }
    close(sfd);
    return got;
}

int main(void) {
    char dir[] = "/tmp/evordXXXXXX";
    if (mkdtemp(dir) == NULL) {
        printf("evorder ERROR mkdtemp\n");
        return EXIT_FAILURE;
    }

    /* ---- inotify ---- */
    const int OPS = 6;
    unsigned first_mask = 0;
    bool all_same = false;
    int events = inotify_leg(dir, OPS, &first_mask, &all_same);
#ifdef HERMIT_TEST_EVORDER_DROP_EVENT
    if (events > 0) {
        events--; /* plant a dropped event: the denominator must expose it */
    }
#endif

    /* DENOMINATOR: the count is meaningless without the operation count. */
    printf("INOTIFY OPS=%d EVENTS=%d\n", OPS, events);

    /* BRANCH on what was observed, rather than printing the stream and hoping
     * the harness notices a difference. */
    if (events < 0) {
        printf("INOTIFY VERDICT=error\n");
    } else if (events == 0) {
        /* Non-vacuity: zero events would mean the leg proved nothing. */
        printf("INOTIFY VERDICT=vacuous-no-events\n");
    } else if (events < OPS) {
        printf("INOTIFY VERDICT=coalesced merged=%d\n", OPS - events);
    } else if (events == OPS) {
        printf("INOTIFY VERDICT=distinct\n");
    } else {
        printf("INOTIFY VERDICT=amplified extra=%d\n", events - OPS);
    }
    printf("INOTIFY MASK_UNIFORM=%s FIRST_IS_MODIFY=%s\n",
           all_same ? "yes" : "no",
           (first_mask & IN_MODIFY) ? "yes" : "no");

    /* ---- signalfd ---- */
    int order[MAX_SIGS];
    memset(order, 0, sizeof order);
    const int RAISED = 3;
    int delivered = signalfd_leg(order, MAX_SIGS);
#ifdef HERMIT_TEST_EVORDER_SCRAMBLE_SIGNAL_ORDER
    if (delivered >= 2) {
        int t = order[0];
        order[0] = order[1];
        order[1] = t; /* plant a delivery-order violation */
    }
#endif

    printf("SIGNALFD RAISED=%d DELIVERED=%d\n", RAISED, delivered);
    if (delivered <= 0) {
        printf("SIGNALFD VERDICT=vacuous-no-signals\n");
    } else {
        /* POSIX delivers the lowest-numbered pending signal first. We raised
         * 14,10,12, so raise order and numeric order disagree and the observed
         * sequence tells them apart. */
        bool ascending = true;
        for (int i = 1; i < delivered; i++) {
            if (order[i] < order[i - 1]) {
                ascending = false;
            }
        }
        bool matches_raise = delivered == 3 && order[0] == SIGALRM
            && order[1] == SIGUSR1 && order[2] == SIGUSR2;
        if (ascending) {
            printf("SIGNALFD VERDICT=lowest-first\n");
        } else if (matches_raise) {
            printf("SIGNALFD VERDICT=raise-order\n");
        } else {
            printf("SIGNALFD VERDICT=other\n");
        }
        printf("SIGNALFD ORDER=");
        for (int i = 0; i < delivered && i < MAX_SIGS; i++) {
            printf("%s%d", i ? "," : "", order[i]);
        }
        printf("\n");
    }

    /* Cleanup: remove the target then the directory. */
    char path[512];
    snprintf(path, sizeof path, "%s/target", dir);
    unlink(path);
    rmdir(dir);

    /* Exit status carries only the things that are true on ANY correct system:
     * both legs produced something, and neither errored. The interesting
     * observations are in stdout, where the harness pins them. */
    bool usable = events > 0 && delivered > 0;
    return usable ? EXIT_SUCCESS : EXIT_FAILURE;
}
