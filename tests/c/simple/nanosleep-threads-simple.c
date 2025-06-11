/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <pthread.h>
#include <stdlib.h>
#include <sys/prctl.h>
#include <unistd.h>

int arr[1] = {10};
int* lets_break_it = arr;

void* myThreadFun(void* vargp) {
  prctl(PR_SET_NAME, "myThreadFun\0", NULL, NULL, NULL);
  sleep(1);
  lets_break_it = NULL;
  return NULL;
}

int main() {
  prctl(PR_SET_NAME, "mainThread\0", NULL, NULL, NULL);
  pthread_t thread_id;
  pthread_create(&thread_id, NULL, myThreadFun, NULL);

  sleep(1);

  lets_break_it[0] = 1;

  pthread_join(thread_id, NULL);

  return 0;
}
