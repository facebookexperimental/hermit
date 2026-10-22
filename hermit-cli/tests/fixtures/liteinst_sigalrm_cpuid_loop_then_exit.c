/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Signal phase 1 (in-guest LiteInst): a SIGALRM that expires while the guest
 * runs only trapped CPUID instructions (no syscalls). With a logical target
 * timeslice the scheduler can yield at those traps, and the alarm commits
 * there. Its handler must still run before the guest's next syscall, here
 * exit_group, as on Linux, where it runs during the loop. The loop stops when
 * the handler has run, or after 200000 instructions.
 */

#include <cpuid.h>
#include <signal.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <unistd.h>

static volatile sig_atomic_t ran;

static void handler(int signal) {
    (void)signal;
    ran = 1;
    static const char line[] = "handler ran\n";
    if (write(STDOUT_FILENO, line, sizeof line - 1) < 0) {
        _exit(2);
    }
}

int main(void) {
    struct sigaction action;
    memset(&action, 0, sizeof action);
    action.sa_handler = handler;
    if (sigaction(SIGALRM, &action, NULL) != 0) {
        return 1;
    }
    struct itimerval timer;
    memset(&timer, 0, sizeof timer);
    timer.it_value.tv_usec = 1000;
    if (setitimer(ITIMER_REAL, &timer, NULL) != 0) {
        return 1;
    }
    unsigned eax, ebx, ecx, edx;
    for (unsigned long i = 0; !ran && i < 200000; i++) {
        __cpuid(0, eax, ebx, ecx, edx);
    }
    syscall(SYS_exit_group, 0);
    return 1;
}
