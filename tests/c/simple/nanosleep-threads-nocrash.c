// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/prctl.h>
#include <unistd.h>

int shared_var = 0;

void sleep_ms(int ms) {
  int secs = ms / 1000;
  int left = ms % 1000;
  const struct timespec req = {secs, 1000 * left};
  struct timespec rem = {0, 0};
  nanosleep(&req, &rem);
}

void* myThreadFun(void* vargp) {
  prctl(PR_SET_NAME, "myThreadFun\0", NULL, NULL, NULL);
  sleep_ms(1000);
  shared_var = 1;
  return NULL;
}

int main() {
  prctl(PR_SET_NAME, "mainThread\0", NULL, NULL, NULL);
  pthread_t thread_id;
  pthread_create(&thread_id, NULL, myThreadFun, NULL);

  sleep_ms(1000);
  shared_var = 2;

  pthread_join(thread_id, NULL);

  if (shared_var == 2) {
    return 0;
  } else {
    return 1;
  }
}
