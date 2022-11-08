// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

static int x = 0;
void __attribute__((noinline)) foo() {
  x *= 2;
  x -= 1;
}
void __attribute__((optnone)) spin() {
  for (int i = 0; i < 15 * 1000 * 1000; i++) {
    foo();
  }
}

void* thread(void* vargp) {
  spin();
  return NULL;
}

int main() {
  pthread_t thread_id;
  pthread_create(&thread_id, NULL, thread, NULL);
  spin();
  pthread_join(thread_id, NULL);
  return 0;
}
