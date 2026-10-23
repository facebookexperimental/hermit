/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Signal phase 1 (in-guest LiteInst): a SIGALRM delivery the runtime cannot
 * make as prepared. The test starts Hermit with RLIMIT_SIGPENDING at 0
 * (Detcore keeps a guest's own setrlimit virtual), so the kernel cannot queue
 * the siginfo of the runtime's instance. For a signal below SIGRTMIN it still
 * sends the signal, without that siginfo, so the trampoline cannot
 * authenticate it and ends the process before any guest code runs. Hermit
 * must record a determinism loss; the guest must never resume as if the
 * alarm had not fired ("resumed" never printed).
 */

#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static void handler(int signal) { (void)signal; }

int main(void) {
    struct sigaction action;
    memset(&action, 0, sizeof action);
    action.sa_handler = handler;
    if (sigaction(SIGALRM, &action, NULL) != 0) {
        perror("sigaction");
        return 1;
    }
    printf("armed\n");
    fflush(stdout);
    alarm(1);
    pause();
    printf("resumed\n");
    return 0;
}
