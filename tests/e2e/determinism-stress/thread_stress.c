/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

enum {
  WORKER_COUNT = 4,
  LOCK_ROUNDS = 1000,
  LOCK_TRACE_CAPACITY = 64,
  CASCADE_ROUNDS = 8,
  CASCADE_EVENTS = CASCADE_ROUNDS * 3,
};

struct stress_state {
  pthread_barrier_t start;
  pthread_mutex_t mutex;
  unsigned counter;
  size_t trace_length;
  unsigned trace[LOCK_TRACE_CAPACITY];
};

struct worker_args {
  struct stress_state *state;
  unsigned id;
};

static int cascade_pipe[2] = {-1, -1};
static volatile sig_atomic_t usr1_deliveries;
static volatile sig_atomic_t usr2_deliveries;
static volatile sig_atomic_t alarm_deliveries;
static volatile sig_atomic_t cascade_error;

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

static void wait_at_barrier(pthread_barrier_t *barrier) {
  const int result = pthread_barrier_wait(barrier);
  if (result != 0 && result != PTHREAD_BARRIER_SERIAL_THREAD) {
    check_pthread(result, "pthread_barrier_wait");
  }
}

static void cascade_handler(int signal_number) {
  unsigned char event;
  int next_signal;

  if (signal_number == SIGUSR1) {
    event = '1';
    usr1_deliveries += 1;
    next_signal = SIGUSR2;
  } else if (signal_number == SIGUSR2) {
    event = '2';
    usr2_deliveries += 1;
    next_signal = SIGALRM;
  } else if (signal_number == SIGALRM) {
    event = 'A';
    alarm_deliveries += 1;
    next_signal = alarm_deliveries < CASCADE_ROUNDS ? SIGUSR1 : 0;
  } else {
    cascade_error = 1;
    return;
  }

  const ssize_t written = write(cascade_pipe[1], &event, sizeof(event));
  if (written != (ssize_t)sizeof(event)) {
    cascade_error = 1;
    return;
  }

  if (next_signal != 0 && raise(next_signal) != 0) {
    cascade_error = 1;
  }
}

static void *contend_for_mutex(void *opaque) {
  struct worker_args *worker = opaque;
  struct stress_state *state = worker->state;

  wait_at_barrier(&state->start);
  for (unsigned round = 0; round < LOCK_ROUNDS; ++round) {
    check_pthread(pthread_mutex_lock(&state->mutex), "pthread_mutex_lock");
    if (state->trace_length < LOCK_TRACE_CAPACITY) {
      state->trace[state->trace_length++] = worker->id;
    }
    state->counter += 1;
    check_pthread(pthread_mutex_unlock(&state->mutex), "pthread_mutex_unlock");

    if ((round + worker->id) % 5 == 0) {
      sched_yield();
    }
  }
  return NULL;
}

static void install_cascade_handlers(void) {
  struct sigaction action = {
      .sa_handler = cascade_handler,
      .sa_flags = SA_RESTART,
  };
  if (sigemptyset(&action.sa_mask) != 0 ||
      sigaddset(&action.sa_mask, SIGUSR1) != 0 ||
      sigaddset(&action.sa_mask, SIGUSR2) != 0 ||
      sigaddset(&action.sa_mask, SIGALRM) != 0 ||
      sigaction(SIGUSR1, &action, NULL) != 0 ||
      sigaction(SIGUSR2, &action, NULL) != 0 ||
      sigaction(SIGALRM, &action, NULL) != 0) {
    fail("sigaction(signal cascade)");
  }
}

static void read_cascade(unsigned char events[CASCADE_EVENTS]) {
  size_t received = 0;
  while (received < CASCADE_EVENTS) {
    ssize_t bytes =
        read(cascade_pipe[0], events + received, CASCADE_EVENTS - received);
    if (bytes < 0 && errno == EINTR) {
      continue;
    }
    if (bytes <= 0) {
      fail("read(signal cascade)");
    }
    received += (size_t)bytes;
  }
}

int main(void) {
  sigset_t cascade_signals;
  if (sigemptyset(&cascade_signals) != 0 ||
      sigaddset(&cascade_signals, SIGUSR1) != 0 ||
      sigaddset(&cascade_signals, SIGUSR2) != 0 ||
      sigaddset(&cascade_signals, SIGALRM) != 0) {
    fail("sigemptyset(signal cascade)");
  }
  check_pthread(pthread_sigmask(SIG_BLOCK, &cascade_signals, NULL),
                "pthread_sigmask(SIG_BLOCK)");

  if (pipe2(cascade_pipe, O_CLOEXEC) != 0) {
    fail("pipe2(SIGUSR cascade)");
  }
  install_cascade_handlers();

  struct stress_state state = {0};
  pthread_t workers[WORKER_COUNT];
  struct worker_args args[WORKER_COUNT];
  check_pthread(pthread_barrier_init(&state.start, NULL, WORKER_COUNT + 1),
                "pthread_barrier_init");
  check_pthread(pthread_mutex_init(&state.mutex, NULL), "pthread_mutex_init");

  for (unsigned id = 0; id < WORKER_COUNT; ++id) {
    args[id] = (struct worker_args){.state = &state, .id = id};
    check_pthread(
        pthread_create(&workers[id], NULL, contend_for_mutex, &args[id]),
        "pthread_create");
  }

  wait_at_barrier(&state.start);
  check_pthread(pthread_sigmask(SIG_UNBLOCK, &cascade_signals, NULL),
                "pthread_sigmask(SIG_UNBLOCK)");
  if (raise(SIGUSR1) != 0) {
    fail("raise(SIGUSR1)");
  }

  unsigned char signal_events[CASCADE_EVENTS];
  read_cascade(signal_events);
  check_pthread(pthread_sigmask(SIG_BLOCK, &cascade_signals, NULL),
                "pthread_sigmask(SIG_BLOCK final)");

  for (unsigned id = 0; id < WORKER_COUNT; ++id) {
    check_pthread(pthread_join(workers[id], NULL), "pthread_join");
  }

  const unsigned expected_counter = WORKER_COUNT * LOCK_ROUNDS;
  if (state.counter != expected_counter ||
      state.trace_length != LOCK_TRACE_CAPACITY || cascade_error != 0 ||
      usr1_deliveries != CASCADE_ROUNDS ||
      usr2_deliveries != CASCADE_ROUNDS ||
      alarm_deliveries != CASCADE_ROUNDS) {
    fprintf(stderr,
            "stress invariant failed: counter=%u/%u trace=%zu/%u usr1=%d "
            "usr2=%d alarm=%d error=%d\n",
            state.counter, expected_counter, state.trace_length,
            LOCK_TRACE_CAPACITY, usr1_deliveries, usr2_deliveries,
            alarm_deliveries, cascade_error);
    return EXIT_FAILURE;
  }
  const unsigned char expected_events[] = {'1', '2', 'A'};
  for (unsigned index = 0; index < CASCADE_EVENTS; ++index) {
    const unsigned char expected = expected_events[index % 3];
    if (signal_events[index] != expected) {
      fprintf(stderr,
              "signal cascade order mismatch at %u: got %c expected %c\n",
              index, signal_events[index], expected);
      return EXIT_FAILURE;
    }
  }

  printf("thread-stress counter=%u trace=", state.counter);
  for (size_t index = 0; index < state.trace_length; ++index) {
    printf("%s%u", index == 0 ? "" : ",", state.trace[index]);
  }
  printf(" signal-cascade=");
  for (unsigned index = 0; index < CASCADE_EVENTS; ++index) {
    putchar(signal_events[index]);
  }
  printf(" usr1=%d usr2=%d alarm=%d\n", usr1_deliveries, usr2_deliveries,
         alarm_deliveries);

  close(cascade_pipe[0]);
  close(cascade_pipe[1]);
  check_pthread(pthread_mutex_destroy(&state.mutex), "pthread_mutex_destroy");
  check_pthread(pthread_barrier_destroy(&state.start),
                "pthread_barrier_destroy");
  return EXIT_SUCCESS;
}
