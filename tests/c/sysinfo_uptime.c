/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <locale.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/sysinfo.h>
#include <time.h>
#include <unistd.h>

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

static int original_workload(void) {
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

struct observation {
  long uptime;
  struct timespec clock;
};

static struct observation observe(const char* label) {
  struct sysinfo info;
  struct observation result;
  if (sysinfo(&info) != 0 ||
      clock_gettime(CLOCK_MONOTONIC, &result.clock) != 0) {
    perror("sysinfo/clock_gettime observation");
    exit(1);
  }
  result.uptime = info.uptime;
  printf("sysinfo observation %s uptime=%ld\n", label, result.uptime);
  return result;
}

static void require_order(struct observation before, struct observation after) {
  if (after.uptime < before.uptime ||
      after.clock.tv_sec < before.clock.tv_sec ||
      (after.clock.tv_sec == before.clock.tv_sec &&
       after.clock.tv_nsec < before.clock.tv_nsec)) {
    fprintf(stderr, "sysinfo uptime or monotonic clock moved backwards\n");
    exit(1);
  }
}

struct worker_observations {
  struct observation before;
  struct observation after;
};

static void* observation_worker(void* arg) {
  struct worker_observations* observations = arg;
  observations->before = observe("worker before work");
  // This advances time; pthread_create/join, not this delay, establishes the
  // observation order between the parent and worker.
  const struct timespec work = {.tv_sec = 0, .tv_nsec = 250000000};
  if (nanosleep(&work, NULL) != 0) {
    perror("observation nanosleep");
    exit(1);
  }
  observations->after = observe("worker after work");
  require_order(observations->before, observations->after);
  if (observations->before.clock.tv_sec == observations->after.clock.tv_sec &&
      observations->before.clock.tv_nsec == observations->after.clock.tv_nsec) {
    fprintf(stderr, "monotonic clock did not advance during subsecond work\n");
    exit(1);
  }
  return NULL;
}

static void require_pthread_success(int error, const char* operation) {
  if (error != 0) {
    fprintf(stderr, "%s: %s\n", operation, strerror(error));
    exit(1);
  }
}

static long observation_argument(const char* text) {
  char* end;
  errno = 0;
  long value = strtol(text, &end, 10);
  if (errno != 0 || text == end || *end != '\0' || value < 0) {
    fprintf(stderr, "invalid observation argument: %s\n", text);
    exit(1);
  }
  return value;
}

static int observe_thread_and_exec(const char* executable) {
  struct observation before = observe("parent before thread");
  struct worker_observations worker;
  pthread_t thread;
  require_pthread_success(
      pthread_create(&thread, NULL, observation_worker, &worker),
      "pthread_create");
  require_pthread_success(pthread_join(thread, NULL), "pthread_join");
  struct observation after = observe("parent after join");
  require_order(before, worker.before);
  require_order(worker.after, after);

  // Carry both observations through exec: integer uptime must not regress, and
  // the finer clock must retain progress that its integer projection can hide.
  char uptime[32], seconds[32], nanos[32];
  if (snprintf(uptime, sizeof(uptime), "%ld", after.uptime) < 0 ||
      snprintf(seconds, sizeof(seconds), "%ld", after.clock.tv_sec) < 0 ||
      snprintf(nanos, sizeof(nanos), "%ld", after.clock.tv_nsec) < 0 ||
      fflush(stdout) != 0) {
    perror("prepare observation exec");
    return 1;
  }
  execl(
      executable,
      executable,
      "--observe-after-exec",
      uptime,
      seconds,
      nanos,
      NULL);
  perror("observation execl");
  return 1;
}

int main(int argc, char** argv) {
  if (argc == 1) {
    return original_workload();
  }
  if (argc == 2 && strcmp(argv[1], "--observe-thread-exec") == 0) {
    return observe_thread_and_exec(argv[0]);
  }
  if (argc == 5 && strcmp(argv[1], "--observe-after-exec") == 0) {
    struct observation before = {
        .uptime = observation_argument(argv[2]),
        .clock =
            {.tv_sec = observation_argument(argv[3]),
             .tv_nsec = observation_argument(argv[4])},
    };
    if (before.clock.tv_nsec >= 1000000000) {
      fprintf(stderr, "invalid observation nanoseconds\n");
      return 1;
    }
    require_order(before, observe("after exec"));
    puts("sysinfo thread join and exec continuity checked");
    return 0;
  }
  fprintf(stderr, "unknown sysinfo_uptime arguments\n");
  return 1;
}
