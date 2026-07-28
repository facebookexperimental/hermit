/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

enum {
  PARTICIPANT_COUNT = 2,
  ROUND_COUNT = 256,
  TRACE_CAPACITY = 64,
};

struct shared_state {
  pthread_mutex_t mutex;
  pthread_cond_t turn_changed;
  unsigned turn;
  unsigned counter;
  uint64_t checksum;
  size_t trace_length;
  unsigned char trace[TRACE_CAPACITY];
};

static void fail(const char *operation) {
  fprintf(stderr, "%s: %s\n", operation, strerror(errno));
  exit(EXIT_FAILURE);
}

static void check_pthread(int result, const char *operation) {
  if (result != 0) {
    errno = result;
    fail(operation);
  }
}

static void take_turns(struct shared_state *state, unsigned participant) {
  for (unsigned round = 0; round < ROUND_COUNT; ++round) {
    check_pthread(pthread_mutex_lock(&state->mutex), "pthread_mutex_lock");
    while (state->turn != participant) {
      check_pthread(pthread_cond_wait(&state->turn_changed, &state->mutex),
                    "pthread_cond_wait");
    }

    if (state->trace_length < TRACE_CAPACITY) {
      state->trace[state->trace_length++] = (unsigned char)participant;
    }
    state->counter += 1;
    state->checksum = state->checksum * UINT64_C(6364136223846793005) +
                      ((uint64_t)participant << 32) + round + 1;
    state->turn = (participant + 1) % PARTICIPANT_COUNT;

    check_pthread(pthread_cond_broadcast(&state->turn_changed),
                  "pthread_cond_broadcast");
    check_pthread(pthread_mutex_unlock(&state->mutex), "pthread_mutex_unlock");
  }
}

int main(void) {
  struct shared_state *state =
      mmap(NULL, sizeof(*state), PROT_READ | PROT_WRITE,
           MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  if (state == MAP_FAILED) {
    fail("mmap(MAP_SHARED)");
  }

  pthread_mutexattr_t mutex_attributes;
  pthread_condattr_t condition_attributes;
  check_pthread(pthread_mutexattr_init(&mutex_attributes),
                "pthread_mutexattr_init");
  check_pthread(
      pthread_mutexattr_setpshared(&mutex_attributes, PTHREAD_PROCESS_SHARED),
      "pthread_mutexattr_setpshared");
  check_pthread(pthread_condattr_init(&condition_attributes),
                "pthread_condattr_init");
  check_pthread(
      pthread_condattr_setpshared(&condition_attributes, PTHREAD_PROCESS_SHARED),
      "pthread_condattr_setpshared");
  check_pthread(pthread_mutex_init(&state->mutex, &mutex_attributes),
                "pthread_mutex_init");
  check_pthread(
      pthread_cond_init(&state->turn_changed, &condition_attributes),
      "pthread_cond_init");
  check_pthread(pthread_mutexattr_destroy(&mutex_attributes),
                "pthread_mutexattr_destroy");
  check_pthread(pthread_condattr_destroy(&condition_attributes),
                "pthread_condattr_destroy");

  state->checksum = UINT64_C(0xcbf29ce484222325);
  const pid_t child = fork();
  if (child < 0) {
    fail("fork");
  }
  if (child == 0) {
    take_turns(state, 1);
    _exit(EXIT_SUCCESS);
  }

  take_turns(state, 0);

  int child_status;
  pid_t waited;
  do {
    waited = waitpid(child, &child_status, 0);
  } while (waited < 0 && errno == EINTR);
  if (waited != child) {
    fail("waitpid");
  }
  if (!WIFEXITED(child_status) || WEXITSTATUS(child_status) != EXIT_SUCCESS) {
    fprintf(stderr, "child failed: status=%d\n", child_status);
    return EXIT_FAILURE;
  }

  const unsigned expected_counter = PARTICIPANT_COUNT * ROUND_COUNT;
  if (state->counter != expected_counter ||
      state->trace_length != TRACE_CAPACITY) {
    fprintf(stderr, "shared invariant failed: counter=%u/%u trace=%zu/%u\n",
            state->counter, expected_counter, state->trace_length,
            TRACE_CAPACITY);
    return EXIT_FAILURE;
  }
  for (size_t index = 0; index < state->trace_length; ++index) {
    if (state->trace[index] != index % PARTICIPANT_COUNT) {
      fprintf(stderr, "shared trace mismatch at %zu: got %u expected %zu\n",
              index, state->trace[index], index % PARTICIPANT_COUNT);
      return EXIT_FAILURE;
    }
  }

  printf("mmap-fork counter=%u checksum=%016llx trace=", state->counter,
         (unsigned long long)state->checksum);
  for (size_t index = 0; index < state->trace_length; ++index) {
    putchar('0' + state->trace[index]);
  }
  putchar('\n');

  check_pthread(pthread_cond_destroy(&state->turn_changed),
                "pthread_cond_destroy");
  check_pthread(pthread_mutex_destroy(&state->mutex), "pthread_mutex_destroy");
  if (munmap(state, sizeof(*state)) != 0) {
    fail("munmap");
  }
  return EXIT_SUCCESS;
}
