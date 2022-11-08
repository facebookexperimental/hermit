/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
RUN: %me mask | FileCheck --check-prefix=NORMAL1  %s
NORMAL1: SIGHUP is masked
NORMAL1-NEXT: SIGSTKFLT is masked

RUN: %me block | FileCheck --check-prefix=NORMAL2  %s
NORMAL2: SIGHUP is masked
NORMAL2-NEXT: SIGSTKFLT is masked
*/

#include <assert.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void checksig(const char* desc, const sigset_t* set, int sig) {
  printf("%s %s masked\n", desc, sigismember(set, sig) ? "is" : "is not");
}

static void block(void) {
  sigset_t set, oldset;

  sigfillset(&set);

  assert(sigprocmask(SIG_BLOCK, &set, NULL) == 0);
  assert(sigprocmask(SIG_BLOCK, NULL, &oldset) == 0);

  checksig("SIGHUP", &oldset, SIGHUP);
  checksig("SIGSTKFLT", &oldset, SIGSTKFLT);
}

static void mask(void) {
  sigset_t set, oldset;

  sigfillset(&set);

  assert(sigprocmask(SIG_SETMASK, &set, NULL) == 0);
  assert(sigprocmask(SIG_SETMASK, NULL, &oldset) == 0);

  checksig("SIGHUP", &oldset, SIGHUP);
  checksig("SIGSTKFLT", &oldset, SIGSTKFLT);
}

int main(int argc, char* argv[]) {
  if (argc != 2) {
    fprintf(stderr, "%s [block | mask]", argv[0]);
    exit(1);
  }

  if (strcmp(argv[1], "block") == 0) {
    block();
  } else if (strcmp(argv[1], "mask") == 0) {
    mask();
  } else {
    fprintf(stderr, "%s [block | mask]", argv[0]);
    exit(1);
  }

  return 0;
}
