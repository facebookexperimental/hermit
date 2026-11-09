/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A completed b-first target and an a-first path Hermit refuses. The "finish"
// mode makes both orders terminate so the test can select a seed; it has the
// same argument count and length as "refuse" before the race is decided.
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static atomic_int winner;

static void* claim(void* name) {
  int unclaimed = 0;
  atomic_compare_exchange_strong(&winner, &unclaimed, *(const char*)name);
  return NULL;
}

int main(int argc, char** argv) {
  const char* then = argc > 1 ? argv[1] : "refuse";
  pthread_t a, b;
  if (pthread_create(&a, NULL, claim, "a") != 0 ||
      pthread_create(&b, NULL, claim, "b") != 0) {
    return 99;
  }
  pthread_join(a, NULL);
  pthread_join(b, NULL);
  if (atomic_load(&winner) == 'b') {
    puts("b-first");
    return 0;
  }
  puts("a-first");
  fflush(stdout);
  if (strcmp(then, "refuse") == 0) {
    // A real interrupted wait may restart, but this call has no saved wait.
    // getpid consumes any stale restart record before the unsupported call.
    syscall(SYS_getpid);
    syscall(SYS_restart_syscall);
    return 99; // A returned errno is not the refusal this fixture requires.
  }
  return 122; // An ordinary completed guest outcome, not Hermit's refusal.
}
