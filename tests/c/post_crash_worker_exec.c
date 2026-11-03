/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Guest for the post-crash worker-exec regression (revcodex3 F3 on
 * PR #4867): a thread catches SIGABRT and execs /bin/true from its
 * handler. With the argument "leader" the main thread does it; without,
 * a pthread worker does it while the leader joins it. The exec'ing
 * thread survives under the leader's identity either way, so the
 * post-crash switch must be taken in both cases. */

#define _GNU_SOURCE

#include <pthread.h>
#include <signal.h>
#include <unistd.h>

static void on_abort(int signo) {
    (void)signo;
    char *const argv[] = {"/bin/true", (char *)0};
    execve("/bin/true", argv, (char *const *)0);
    _exit(127);
}

static void *worker(void *unused) {
    (void)unused;
    signal(SIGABRT, on_abort);
    raise(SIGABRT);
    return (void *)0;
}

int main(int argc, char **argv) {
    (void)argv;
    if (argc > 1) {
        signal(SIGABRT, on_abort);
        raise(SIGABRT);
        return 1;
    }
    pthread_t thread;
    if (pthread_create(&thread, (const pthread_attr_t *)0, worker, (void *)0) != 0) {
        return 2;
    }
    pthread_join(thread, (void **)0);
    return 3;
}
