// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>

#define NUM_THREADS 5

void* PrintHello(void* threadid) {
  printf("Hello");
  int x = 0;
  while (x < 1000)
    x++;
  return NULL;
}
int main() {
  pthread_t threads[NUM_THREADS];
  int rc;
  int i;
  for (i = 0; i < NUM_THREADS; i++) {
    rc = pthread_create(&threads[i], NULL, PrintHello, NULL);
    if (rc) {
      printf("Error:unable to create thread, %d\n", rc);
      return -1;
    }
  }
  for (i = 0; i < NUM_THREADS; i++) {
    pthread_join(threads[i], NULL);
  }
  return 0;
}
