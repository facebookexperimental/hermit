/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Syscall-dense multithreaded guest for the chaos preemption-replay
// selfcheck regression test (rrnewton/hermit#4496): several threads
// issuing short syscalls in a tight loop, so chaos slice ends
// frequently coincide with syscall post-hooks in a recording while a
// replay reaches the same branch counts on its precise timer. The
// iteration count is an argument so a test can replay the record
// against a genuinely different execution.

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <unistd.h>

static void *worker(void *arg) {
    long iterations = (long)arg;
    int fd = open("/dev/null", O_WRONLY);
    if (fd < 0) {
        return (void *)1;
    }
    char byte = 'x';
    for (long i = 0; i < iterations; i++) {
        syscall(SYS_getpid);
        if (write(fd, &byte, 1) != 1) {
            return (void *)1;
        }
        syscall(SYS_gettid);
    }
    close(fd);
    return NULL;
}

int main(int argc, char **argv) {
    long iterations = argc > 1 ? atol(argv[1]) : 4000;
    pthread_t threads[3];
    for (int i = 0; i < 3; i++) {
        if (pthread_create(&threads[i], NULL, worker, (void *)iterations) != 0) {
            return 1;
        }
    }
    for (int i = 0; i < 3; i++) {
        void *result = NULL;
        pthread_join(threads[i], &result);
        if (result != NULL) {
            return 1;
        }
    }
    puts("done");
    return 0;
}
