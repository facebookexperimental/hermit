/* @lint-ignore-every LICENSELINT */

#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

enum { THREADS = 4, INCREMENTS = 5000 };

struct shared_state {
  pthread_barrier_t start;
  atomic_uint_fast64_t cas_counter;
  atomic_uint_fast64_t fetch_add_counter;
  uint64_t retries[THREADS];
};

struct worker_args {
  struct shared_state *state;
  int id;
};

static void *worker(void *opaque) {
  struct worker_args *args = opaque;
  struct shared_state *state = args->state;
  int barrier_result = pthread_barrier_wait(&state->start);
  if (barrier_result != 0 && barrier_result != PTHREAD_BARRIER_SERIAL_THREAD) {
    return (void *)1;
  }

  uint64_t retries = 0;
  for (int iteration = 0; iteration < INCREMENTS; iteration++) {
    uint64_t current =
        atomic_load_explicit(&state->cas_counter, memory_order_relaxed);
    while (!atomic_compare_exchange_weak_explicit(
        &state->cas_counter, &current, current + 1, memory_order_seq_cst,
        memory_order_relaxed)) {
      retries++;
    }
    atomic_fetch_add_explicit(&state->fetch_add_counter, 1,
                              memory_order_seq_cst);
  }
  state->retries[args->id] = retries;
  return NULL;
}

int main(void) {
  struct shared_state state = {0};
  if (pthread_barrier_init(&state.start, NULL, THREADS + 1) != 0) {
    return 1;
  }

  pthread_t threads[THREADS];
  struct worker_args args[THREADS];
  for (int id = 0; id < THREADS; id++) {
    args[id] = (struct worker_args){.state = &state, .id = id};
    if (pthread_create(&threads[id], NULL, worker, &args[id]) != 0) {
      return 2;
    }
  }
  int barrier_result = pthread_barrier_wait(&state.start);
  if (barrier_result != 0 && barrier_result != PTHREAD_BARRIER_SERIAL_THREAD) {
    return 3;
  }

  for (int id = 0; id < THREADS; id++) {
    void *result = NULL;
    if (pthread_join(threads[id], &result) != 0 || result != NULL) {
      return 4;
    }
  }

  const uint64_t expected = THREADS * INCREMENTS;
  uint64_t cas_value =
      atomic_load_explicit(&state.cas_counter, memory_order_seq_cst);
  uint64_t fetch_add_value =
      atomic_load_explicit(&state.fetch_add_counter, memory_order_seq_cst);
  if (cas_value != expected || fetch_add_value != expected) {
    return 5;
  }

  printf("lock-free final=%lu fetch-add=%lu retries=", (unsigned long)cas_value,
         (unsigned long)fetch_add_value);
  for (int id = 0; id < THREADS; id++) {
    printf("%s%lu", id == 0 ? "" : ",", (unsigned long)state.retries[id]);
  }
  putchar('\n');
  return pthread_barrier_destroy(&state.start) == 0 ? 0 : 6;
}
