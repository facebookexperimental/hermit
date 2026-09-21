/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Multi-producer / multi-consumer bounded-queue determinism stress.
 *
 * Several producer threads push a fixed number of items into a small ring
 * buffer that is protected by one mutex and two condition variables
 * (not_full / not_empty). Several consumer threads pop items until they each
 * receive a poison pill. The queue is deliberately small so that both the
 * "wait until not full" (producer backpressure) and "wait until not empty"
 * (consumer starvation) paths are exercised repeatedly, forcing many condvar
 * waits and futex wakeups.
 *
 * The emitted output is intentionally schedule-sensitive: the program records,
 * under the queue lock, the exact global order in which items are consumed and
 * which consumer consumed each item. If Hermit's thread scheduling, futex
 * ordering, or condvar wakeup order were nondeterministic, this consumption log
 * and the per-consumer tallies would differ between two runs, so
 * `hermit run --strict --verify` would report a divergence. An order-independent
 * XOR checksum is printed last as a correctness anchor: it must always equal the
 * XOR of every produced value regardless of interleaving.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <pthread.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

enum {
  PRODUCER_COUNT = 3,
  CONSUMER_COUNT = 2,
  ITEMS_PER_PRODUCER = 40,
  QUEUE_CAPACITY = 4,
  REAL_ITEMS = PRODUCER_COUNT * ITEMS_PER_PRODUCER,
  POISON = -1,
};

struct bounded_queue {
  pthread_mutex_t mutex;
  pthread_cond_t not_full;
  pthread_cond_t not_empty;
  int buffer[QUEUE_CAPACITY];
  int head;
  int tail;
  int size;

  /* Schedule-sensitive consumption log, written under `mutex`. */
  int log_consumer[REAL_ITEMS];
  int log_value[REAL_ITEMS];
  int consumed;
  int per_consumer[CONSUMER_COUNT];
};

struct producer_args {
  struct bounded_queue *queue;
  int producer_id;
};

struct consumer_args {
  struct bounded_queue *queue;
  int consumer_id;
};

static void fail(const char *operation, int err) {
  fprintf(stderr, "%s: %s\n", operation, strerror(err));
  exit(EXIT_FAILURE);
}

#define CHECK(call)                        \
  do {                                     \
    int rc__ = (call);                     \
    if (rc__ != 0) {                       \
      fail(#call, rc__);                   \
    }                                      \
  } while (0)

static void queue_push(struct bounded_queue *queue, int value) {
  CHECK(pthread_mutex_lock(&queue->mutex));
  while (queue->size == QUEUE_CAPACITY) {
    CHECK(pthread_cond_wait(&queue->not_full, &queue->mutex));
  }
  queue->buffer[queue->tail] = value;
  queue->tail = (queue->tail + 1) % QUEUE_CAPACITY;
  queue->size += 1;
  CHECK(pthread_cond_signal(&queue->not_empty));
  CHECK(pthread_mutex_unlock(&queue->mutex));
}

static int queue_pop(struct bounded_queue *queue, int consumer_id) {
  CHECK(pthread_mutex_lock(&queue->mutex));
  while (queue->size == 0) {
    CHECK(pthread_cond_wait(&queue->not_empty, &queue->mutex));
  }
  int value = queue->buffer[queue->head];
  queue->head = (queue->head + 1) % QUEUE_CAPACITY;
  queue->size -= 1;
  if (value != POISON) {
    int index = queue->consumed;
    queue->log_consumer[index] = consumer_id;
    queue->log_value[index] = value;
    queue->consumed = index + 1;
    queue->per_consumer[consumer_id] += 1;
  }
  CHECK(pthread_cond_signal(&queue->not_full));
  CHECK(pthread_mutex_unlock(&queue->mutex));
  return value;
}

static void *producer_main(void *opaque) {
  struct producer_args *args = opaque;
  for (int seq = 0; seq < ITEMS_PER_PRODUCER; ++seq) {
    /* Encode producer and sequence so every produced value is unique. */
    int value = args->producer_id * 1000 + seq;
    queue_push(args->queue, value);
  }
  return NULL;
}

static void *consumer_main(void *opaque) {
  struct consumer_args *args = opaque;
  for (;;) {
    int value = queue_pop(args->queue, args->consumer_id);
    if (value == POISON) {
      break;
    }
  }
  return NULL;
}

int main(void) {
  struct bounded_queue queue;
  memset(&queue, 0, sizeof(queue));
  CHECK(pthread_mutex_init(&queue.mutex, NULL));
  CHECK(pthread_cond_init(&queue.not_full, NULL));
  CHECK(pthread_cond_init(&queue.not_empty, NULL));

  pthread_t producers[PRODUCER_COUNT];
  pthread_t consumers[CONSUMER_COUNT];
  struct producer_args producer_args[PRODUCER_COUNT];
  struct consumer_args consumer_args[CONSUMER_COUNT];

  for (int i = 0; i < CONSUMER_COUNT; ++i) {
    consumer_args[i].queue = &queue;
    consumer_args[i].consumer_id = i;
    CHECK(pthread_create(&consumers[i], NULL, consumer_main, &consumer_args[i]));
  }
  for (int i = 0; i < PRODUCER_COUNT; ++i) {
    producer_args[i].queue = &queue;
    producer_args[i].producer_id = i;
    CHECK(pthread_create(&producers[i], NULL, producer_main, &producer_args[i]));
  }

  for (int i = 0; i < PRODUCER_COUNT; ++i) {
    CHECK(pthread_join(producers[i], NULL));
  }
  /* All real items produced; retire each consumer with one poison pill. */
  for (int i = 0; i < CONSUMER_COUNT; ++i) {
    queue_push(&queue, POISON);
  }
  for (int i = 0; i < CONSUMER_COUNT; ++i) {
    CHECK(pthread_join(consumers[i], NULL));
  }

  printf("consumed %d items\n", queue.consumed);
  for (int i = 0; i < queue.consumed; ++i) {
    printf("c%d %d\n", queue.log_consumer[i], queue.log_value[i]);
  }
  for (int i = 0; i < CONSUMER_COUNT; ++i) {
    printf("consumer %d total %d\n", i, queue.per_consumer[i]);
  }

  uint32_t checksum = 0;
  for (int i = 0; i < queue.consumed; ++i) {
    checksum ^= (uint32_t)queue.log_value[i];
  }
  uint32_t expected = 0;
  for (int p = 0; p < PRODUCER_COUNT; ++p) {
    for (int seq = 0; seq < ITEMS_PER_PRODUCER; ++seq) {
      expected ^= (uint32_t)(p * 1000 + seq);
    }
  }
  if (queue.consumed != REAL_ITEMS || checksum != expected) {
    fprintf(stderr,
            "correctness violation: consumed=%d (want %d) checksum=%u (want %u)\n",
            queue.consumed, REAL_ITEMS, checksum, expected);
    return EXIT_FAILURE;
  }
  printf("checksum %u\n", checksum);

  CHECK(pthread_mutex_destroy(&queue.mutex));
  CHECK(pthread_cond_destroy(&queue.not_full));
  CHECK(pthread_cond_destroy(&queue.not_empty));
  return EXIT_SUCCESS;
}
