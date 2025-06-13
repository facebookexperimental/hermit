/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <locale.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <sys/sysinfo.h>

static long long x = 0;
pthread_mutex_t mutex = PTHREAD_MUTEX_INITIALIZER;
static _Atomic unsigned long uptime_1;
static _Atomic unsigned long uptime_2;

void sleep_ms(int ms) {
  int secs = ms / 1000;
  int left = ms % 1000;
  const struct timespec req = {secs, 1000 * left};
  struct timespec rem = {0, 0};
  nanosleep(&req, &rem);
}
void __attribute__((noinline)) meaningless_work() {
  // Do some meaningless work but without overflowing the integer.
  if (x > 100000000) {
    x = x / 25;
  }
  x += 111;
}

// disabling optimizations here as at -O3 level the compiler precomputes the
// value of x statically
void __attribute__((optnone)) spin() {
  for (int i = 0; i < 3 * 1000; i++) {
    for (int j = 0; j < 1000; j++) {
      meaningless_work();
    }
    sleep_ms(3);
  }
}

void* thread1(void* vargp) {
  struct sysinfo info;
  sysinfo(&info);
  printf("thread1-> uptime: %lu sec\n", info.uptime);

  /*
   We spin lock lock here in order to let some time pass for thread1(and
   globally). This way if scheduler is not handling "uptime" properly via global
   clock uptime_1 and uptime_2 won't be properly ordered. Without spin lock we
   might get false positive as uptimes from both threads will be equal
   */
  spin();
  sysinfo(&info);
  atomic_store(&uptime_1, info.uptime);
  printf("thread1-> uptime: %lu sec\n", info.uptime);
  pthread_mutex_unlock(&mutex);
  return NULL;
}

void* thread2(void* vargp) {
  pthread_mutex_lock(&mutex);
  struct sysinfo info;
  sysinfo(&info);
  atomic_store(&uptime_2, info.uptime);
  printf("thread2-> uptime: %lu sec\n", info.uptime);

  printf("\n");
  printf(
      "thread2-> loads %lu %lu %lu\n",
      info.loads[0],
      info.loads[1],
      info.loads[0]);
  printf("thread2-> totalram: %lu \n", info.totalram);
  printf("thread2-> freeram: %lu \n", info.freeram);
  printf("thread2-> sharedram: %lu sec\n", info.sharedram);
  printf("thread2-> bufferram: %lu sec\n", info.bufferram);
  printf("thread2-> totalswap: %lu sec\n", info.totalswap);
  printf("thread2-> freeswap: %lu sec\n", info.freeswap);
  printf("thread2-> procs: %u sec\n", info.procs);
  printf("thread2-> totalhigh: %lu sec\n", info.totalhigh);
  printf("thread2-> freehigh: %lu sec\n", info.freehigh);
  printf("thread2-> mem_unit: %u sec\n", info.mem_unit);
  return NULL;
}

int main() {
  setlocale(LC_NUMERIC, ""); // Print large numbers with commas.
  pthread_t thread[2];
  pthread_mutex_lock(&mutex);
  pthread_create(&thread[0], NULL, thread1, NULL);
  pthread_create(&thread[1], NULL, thread2, NULL);
  pthread_join(thread[0], NULL);
  pthread_join(thread[1], NULL);
  if (atomic_load(&uptime_1) > atomic_load(&uptime_2)) {
    // The uptime in thread1 is guaranteed to not exceed the uptime of thread2
    // This assertion makes a stronger test for hermit in case scheduler is not
    // properly using global monotonic clock for both threads
    return 1;
  }
  return 0;
}
