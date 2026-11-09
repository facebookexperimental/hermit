/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A pipe reader beside two sleeping writers, for schedule replays of a read
 * that Detcore polls.
 *
 * usage: pipe_reader_replay
 *
 * The main thread reads a pipe that two writers fill: each sleeps a little
 * (300 or 500 us), writes one byte, and repeats 40 times, then exits. Under
 * Detcore the read retries as a poller while the pipe is empty. The reader
 * prints the order in which the bytes arrived and its monotonic time after it
 * joined both writers, so a replay that runs the threads in another order or
 * at other times prints another line.
 */

#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define ROUNDS 40

static int fds[2];

static void *writer(void *arg) {
    char tag = *(char *)arg;
    for (int i = 0; i < ROUNDS; i++) {
        struct timespec ts = {0, (tag == 'a' ? 300 : 500) * 1000};
        nanosleep(&ts, NULL);
        if (write(fds[1], &tag, 1) != 1) {
            return NULL;
        }
    }
    return NULL;
}

int main(void) {
    if (pipe(fds) != 0) {
        return 1;
    }
    pthread_t a, b;
    char ta = 'a', tb = 'b';
    pthread_create(&a, NULL, writer, &ta);
    pthread_create(&b, NULL, writer, &tb);
    char order[2 * ROUNDS + 1];
    int got = 0;
    while (got < 2 * ROUNDS) {
        char buf[2 * ROUNDS];
        ssize_t n = read(fds[0], buf, sizeof buf);
        if (n <= 0) {
            return 1;
        }
        memcpy(order + got, buf, (size_t)n);
        got += (int)n;
    }
    order[got] = '\0';
    pthread_join(a, NULL);
    pthread_join(b, NULL);
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    printf("order=%s last_ns=%lld\n", order, (long long)now.tv_sec * 1000000000LL + now.tv_nsec);
    return 0;
}
