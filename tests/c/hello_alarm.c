/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

int flag = 0;

void sig_handler(int signo) {
  flag = 1;
  printf("++ Received signal: %d = %s\n", signo, strsignal(signo));
}

int main() {
  puts("== Start test\n");
  if (signal(SIGALRM, sig_handler) == SIG_ERR) {
    printf("\nError: Cannot register signal handler for SIGINT\n");
    return 1;
  }

  alarm(1);
  sleep(3);

  if (flag) {
    puts("== Success: Delivered control to the signal handler and back.\n");
    return 0;
  } else {
    puts("Error: failed to invoke signal handler\n");
    return 1;
  }
}
