// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <unistd.h>

// With the right schedule, one thread can replace the value of global_result
// in-between the others read of the global_data and it's write to
// global_result, because the lock is incorrectly unlocked in-between.

#define DO_WORK                    \
  do {                             \
    volatile int _work_var = 1000; \
    while (_work_var > 0) {        \
      _work_var--;                 \
    }                              \
  } while (0);

uint64_t compute(uint64_t u) {
  u |= 1;
  const uint64_t MULT = 0x4385DF649FCCF645ull;
  for (int i = 0; i < 1000; i++) {
    u *= MULT;
  }
  return u;
}

pthread_mutex_t mtx; // protects global and result
uint64_t global_data;
uint64_t global_result;

void* Thread1(void* x) {
  pthread_mutex_lock(&mtx);
  global_data = 42;
  pthread_mutex_unlock(&mtx);

  DO_WORK;

  pthread_mutex_lock(&mtx);
  global_data = 43;
  pthread_mutex_unlock(&mtx);
  return NULL;
}

void* Thread2(void* x) {
  for (int i = 0; i < 30; i++) {
    DO_WORK;
  }

  uint64_t data;
  pthread_mutex_lock(&mtx);
  data = global_data;
  pthread_mutex_unlock(&mtx);

  uint64_t result = compute(data);

  pthread_mutex_lock(&mtx);
  global_result = result;
  pthread_mutex_unlock(&mtx);

  return NULL;
}

int main() {
  pthread_t t[2];
  pthread_mutex_init(&mtx, 0);
  pthread_create(&t[0], NULL, Thread1, NULL);
  pthread_create(&t[1], NULL, Thread2, NULL);
  pthread_join(t[0], NULL);
  pthread_join(t[1], NULL);
  pthread_mutex_destroy(&mtx);

  printf(
      "Result: %lu, data: %lu, c(data): %lu\n",
      global_result,
      global_data,
      compute(global_data));
  int success = global_result == compute(global_data);
  printf("%s\n", success ? "Success" : "Fail");
  return success ? 0 : 1;
}
