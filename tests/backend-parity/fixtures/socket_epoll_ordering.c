/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * CONTRACT FIXTURE: ephemeral-port selection and epoll/poll READINESS ORDERING.
 *
 * Both are classic host-dependent divergences that pass silently, because every
 * syscall involved SUCCEEDS. bind(port 0) returns some port; epoll_wait returns some
 * set in some order. Nothing fails, so a status-and-stdout check sees nothing wrong
 * while two runs took different paths.
 *
 * THE DESIGN POINT THAT GIVES THIS FIXTURE ITS VALUE: the guest BRANCHES on which fd
 * is reported ready FIRST, and the branch is CONSEQUENTIAL -- the winner and loser
 * exchange different payloads, in a different order, so the branch changes the
 * subsequent syscall sequence and the final aggregate. A fixture that merely PRINTED
 * the ready set could be defeated by normalising or sorting that set; a taken branch
 * cannot be normalised away, because the divergence has already propagated into what
 * the program did next.
 *
 * Loopback only. No external egress, no DNS, no non-local address is ever used.
 *
 * WHAT IS ASSERTED
 *   - the ephemeral port chosen by bind(port 0) (deterministic selection);
 *   - which fd wins the readiness race, via the branch it causes;
 *   - the full ordered event sequence, which encodes both.
 *
 * WHY MULTIPLE SIMULTANEOUSLY-READY FDS: with one ready fd there is no ordering to
 * get wrong. This arms THREE connections and writes to all of them before the server
 * polls, so epoll_wait genuinely has a set to order and the winner is a real choice
 * rather than the only option.
 */

#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdarg.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/socket.h>
#include <unistd.h>

#define NCONN 3

static void ev(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
static void ev(const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    fputs("EV ", stdout);
    vprintf(fmt, ap);
    fputc('\n', stdout);
    fflush(stdout);
    va_end(ap);
}

int main(void) {
    /* ---- server on loopback with an EPHEMERAL port (bind to 0) ---- */
    int srv = socket(AF_INET, SOCK_STREAM, 0);
    if (srv < 0) { perror("socket"); return 1; }
    int one = 1;
    setsockopt(srv, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);

    struct sockaddr_in a;
    memset(&a, 0, sizeof a);
    a.sin_family = AF_INET;
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK); /* loopback only */
    a.sin_port = 0;                             /* ephemeral: the kernel picks */
    if (bind(srv, (struct sockaddr *)&a, sizeof a) < 0) { perror("bind"); return 1; }

    struct sockaddr_in bound;
    socklen_t bl = sizeof bound;
    if (getsockname(srv, (struct sockaddr *)&bound, &bl) < 0) { perror("getsockname"); return 1; }
    /* The chosen port IS part of the contract: ephemeral-port selection must be
     * deterministic, so it is printed rather than masked. */
    ev("ephemeral_port=%u", (unsigned)ntohs(bound.sin_port));

    if (listen(srv, NCONN + 1) < 0) { perror("listen"); return 1; }

    /* ---- connect NCONN clients, accept them all, THEN make them all ready ---- */
    int cli[NCONN], acc[NCONN];
    for (int i = 0; i < NCONN; i++) {
        cli[i] = socket(AF_INET, SOCK_STREAM, 0);
        if (connect(cli[i], (struct sockaddr *)&bound, sizeof bound) < 0) { perror("connect"); return 1; }
        acc[i] = accept(srv, NULL, NULL);
        if (acc[i] < 0) { perror("accept"); return 1; }
        ev("accepted idx=%d", i);
    }

    /* Write on every client BEFORE polling, so all NCONN server fds are
     * simultaneously readable and epoll_wait has a genuine ordering decision. */
    for (int i = 0; i < NCONN; i++) {
        char c = (char)('A' + i);
        if (write(cli[i], &c, 1) != 1) { perror("write"); return 1; }
    }

    int ep = epoll_create1(0);
    for (int i = 0; i < NCONN; i++) {
        struct epoll_event e = {.events = EPOLLIN, .data.u32 = (unsigned)i};
        epoll_ctl(ep, EPOLL_CTL_ADD, acc[i], &e);
    }

    struct epoll_event got[NCONN];
    int n = epoll_wait(ep, got, NCONN, 5000);
    ev("epoll_ready_count=%d", n);
    if (n <= 0) { ev("epoll_NO_READY_fds"); return 1; }

    /* ---- THE BRANCH. Not a printed set: the winner changes what happens next. ---- */
    unsigned winner = got[0].data.u32;
    ev("winner_idx=%u", winner);

    unsigned long checksum = 0;
    if (winner == 0) {
        /* Branch A: winner is served FIRST and the rest in ascending order, and the
         * winner receives a distinct reply. */
        ev("branch=A_winner_first_ascending");
        for (int i = 0; i < NCONN; i++) {
            char c = 0;
            if (read(acc[i], &c, 1) == 1) { checksum = checksum * 31 + (unsigned char)c; ev("served idx=%d ch=%c", i, c); }
        }
        const char *reply = "A";
        if (write(acc[winner], reply, 1) != 1) {
            perror("write reply");
            return 1;
        }
    } else {
        /* Branch B: a DIFFERENT service order and a different reply, so the taken
         * branch propagates into the syscall sequence rather than only a label. */
        ev("branch=B_winner_last_descending");
        for (int i = NCONN - 1; i >= 0; i--) {
            char c = 0;
            if (read(acc[i], &c, 1) == 1) { checksum = checksum * 31 + (unsigned char)c; ev("served idx=%d ch=%c", i, c); }
        }
        const char *reply = "B";
        if (write(acc[winner], reply, 1) != 1) {
            perror("write reply");
            return 1;
        }
    }
    ev("checksum=%lu", checksum);

    for (int i = 0; i < NCONN; i++) { close(cli[i]); close(acc[i]); }
    close(ep);
    close(srv);
    ev("done");
    return 0;
}
