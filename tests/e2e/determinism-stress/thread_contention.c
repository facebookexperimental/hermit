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
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <unistd.h>

enum {
  THREAD_COUNT = 6,
  CONTENTION_ROUNDS = 256,
  TRACE_CAPACITY = 96,
  PIPE_COUNT = 6,
  EVENT_TIMEOUT_MS = 5000,
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

static void wait_at_barrier(pthread_barrier_t *barrier) {
  const int result = pthread_barrier_wait(barrier);
  if (result != 0 && result != PTHREAD_BARRIER_SERIAL_THREAD) {
    check_pthread(result, "pthread_barrier_wait");
  }
}

struct contention_state {
  pthread_barrier_t start;
  pthread_mutex_t mutex;
  unsigned counter;
  uint64_t checksum;
  size_t trace_length;
  unsigned trace[TRACE_CAPACITY];
};

struct contention_worker {
  struct contention_state *state;
  unsigned id;
};

static void *contend_for_mutex(void *opaque) {
  struct contention_worker *worker = opaque;
  struct contention_state *state = worker->state;

  wait_at_barrier(&state->start);
  for (unsigned round = 0; round < CONTENTION_ROUNDS; ++round) {
    check_pthread(pthread_mutex_lock(&state->mutex), "pthread_mutex_lock");
    if (state->trace_length < TRACE_CAPACITY) {
      state->trace[state->trace_length++] = worker->id;
    }
    state->counter += 1;
    state->checksum = (state->checksum * UINT64_C(1099511628211)) ^
                      ((uint64_t)worker->id << 32) ^ round;
    check_pthread(pthread_mutex_unlock(&state->mutex), "pthread_mutex_unlock");

    if ((round + worker->id) % 3 == 0) {
      sched_yield();
    }
  }
  return NULL;
}

static int run_contention(void) {
  struct contention_state state = {
      .counter = 0,
      .checksum = UINT64_C(1469598103934665603),
      .trace_length = 0,
  };
  pthread_t threads[THREAD_COUNT];
  struct contention_worker workers[THREAD_COUNT];

  check_pthread(pthread_barrier_init(&state.start, NULL, THREAD_COUNT + 1),
                "pthread_barrier_init");
  check_pthread(pthread_mutex_init(&state.mutex, NULL), "pthread_mutex_init");

  for (unsigned id = 0; id < THREAD_COUNT; ++id) {
    workers[id] = (struct contention_worker){.state = &state, .id = id};
    check_pthread(
        pthread_create(&threads[id], NULL, contend_for_mutex, &workers[id]),
        "pthread_create");
  }

  wait_at_barrier(&state.start);
  for (unsigned id = 0; id < THREAD_COUNT; ++id) {
    check_pthread(pthread_join(threads[id], NULL), "pthread_join");
  }

  const unsigned expected = THREAD_COUNT * CONTENTION_ROUNDS;
  if (state.counter != expected || state.trace_length != TRACE_CAPACITY) {
    fprintf(stderr, "contention invariant failed: counter=%u/%u trace=%zu/%u\n",
            state.counter, expected, state.trace_length, TRACE_CAPACITY);
    return EXIT_FAILURE;
  }

  printf("contention counter=%u checksum=%016llx trace=", state.counter,
         (unsigned long long)state.checksum);
  for (size_t index = 0; index < state.trace_length; ++index) {
    printf("%s%u", index == 0 ? "" : ",", state.trace[index]);
  }
  putchar('\n');

  check_pthread(pthread_mutex_destroy(&state.mutex), "pthread_mutex_destroy");
  check_pthread(pthread_barrier_destroy(&state.start),
                "pthread_barrier_destroy");
  return EXIT_SUCCESS;
}

struct pipe_writer {
  pthread_barrier_t *start;
  int fd;
  unsigned id;
};

static void *signal_pipe(void *opaque) {
  struct pipe_writer *writer = opaque;
  const unsigned char payload = (unsigned char)('A' + writer->id);

  wait_at_barrier(writer->start);
  for (unsigned attempt = 0; attempt < (writer->id * 5 + 1) % 7; ++attempt) {
    sched_yield();
  }

  ssize_t written;
  do {
    written = write(writer->fd, &payload, sizeof(payload));
  } while (written < 0 && errno == EINTR);
  if (written != (ssize_t)sizeof(payload)) {
    fail("write(pipe)");
  }
  return NULL;
}

static int run_event_loop(void) {
  int pipes[PIPE_COUNT][2];
  struct pollfd poll_fds[PIPE_COUNT];
  pthread_t threads[PIPE_COUNT];
  struct pipe_writer writers[PIPE_COUNT];
  pthread_barrier_t start;
  const int epoll_fd = epoll_create1(EPOLL_CLOEXEC);
  if (epoll_fd < 0) {
    fail("epoll_create1");
  }

  check_pthread(pthread_barrier_init(&start, NULL, PIPE_COUNT + 1),
                "pthread_barrier_init");
  for (unsigned id = 0; id < PIPE_COUNT; ++id) {
    if (pipe2(pipes[id], O_CLOEXEC | O_NONBLOCK) != 0) {
      fail("pipe2");
    }
    struct epoll_event registration = {
        .events = EPOLLIN,
        .data.u32 = id,
    };
    if (epoll_ctl(epoll_fd, EPOLL_CTL_ADD, pipes[id][0], &registration) != 0) {
      fail("epoll_ctl(ADD)");
    }
    poll_fds[id] = (struct pollfd){.fd = pipes[id][0], .events = POLLIN};
    writers[id] =
        (struct pipe_writer){.start = &start, .fd = pipes[id][1], .id = id};
    check_pthread(pthread_create(&threads[id], NULL, signal_pipe, &writers[id]),
                  "pthread_create");
  }

  wait_at_barrier(&start);
  int poll_ready;
  do {
    poll_ready = poll(poll_fds, PIPE_COUNT, EVENT_TIMEOUT_MS);
  } while (poll_ready < 0 && errno == EINTR);
  if (poll_ready <= 0) {
    if (poll_ready == 0) {
      fputs("poll timed out\n", stderr);
      return EXIT_FAILURE;
    }
    fail("poll");
  }

  unsigned poll_mask = 0;
  for (unsigned id = 0; id < PIPE_COUNT; ++id) {
    if ((poll_fds[id].revents & POLLIN) != 0) {
      poll_mask |= 1U << id;
    }
  }

  unsigned event_order[PIPE_COUNT];
  unsigned event_count = 0;
  unsigned seen_mask = 0;
  while (event_count < PIPE_COUNT) {
    struct epoll_event events[PIPE_COUNT];
    int ready;
    do {
      ready = epoll_wait(epoll_fd, events, PIPE_COUNT, EVENT_TIMEOUT_MS);
    } while (ready < 0 && errno == EINTR);
    if (ready <= 0) {
      if (ready == 0) {
        fputs("epoll_wait timed out\n", stderr);
        return EXIT_FAILURE;
      }
      fail("epoll_wait");
    }

    for (int index = 0; index < ready; ++index) {
      const unsigned id = events[index].data.u32;
      if (id >= PIPE_COUNT || (seen_mask & (1U << id)) != 0 ||
          (events[index].events & EPOLLIN) == 0) {
        fputs("epoll returned an invalid or duplicate event\n", stderr);
        return EXIT_FAILURE;
      }

      unsigned char payload = 0;
      ssize_t bytes;
      do {
        bytes = read(pipes[id][0], &payload, sizeof(payload));
      } while (bytes < 0 && errno == EINTR);
      if (bytes != (ssize_t)sizeof(payload) ||
          payload != (unsigned char)('A' + id)) {
        fputs("pipe payload did not match its epoll tag\n", stderr);
        return EXIT_FAILURE;
      }

      event_order[event_count++] = id;
      seen_mask |= 1U << id;
    }
  }

  for (unsigned id = 0; id < PIPE_COUNT; ++id) {
    check_pthread(pthread_join(threads[id], NULL), "pthread_join");
    close(pipes[id][0]);
    close(pipes[id][1]);
  }
  close(epoll_fd);
  check_pthread(pthread_barrier_destroy(&start), "pthread_barrier_destroy");

  const unsigned expected_mask = (1U << PIPE_COUNT) - 1;
  if (seen_mask != expected_mask || poll_mask == 0) {
    fprintf(stderr,
            "event-loop invariant failed: seen=%02x expected=%02x poll=%02x\n",
            seen_mask, expected_mask, poll_mask);
    return EXIT_FAILURE;
  }

  printf("event-loop poll-ready=%d poll-mask=%02x epoll-order=", poll_ready,
         poll_mask);
  for (unsigned index = 0; index < event_count; ++index) {
    printf("%s%u", index == 0 ? "" : ",", event_order[index]);
  }
  putchar('\n');
  return EXIT_SUCCESS;
}

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s contention|epoll\n", argv[0]);
    return EXIT_FAILURE;
  }
  if (strcmp(argv[1], "contention") == 0) {
    return run_contention();
  }
  if (strcmp(argv[1], "epoll") == 0) {
    return run_event_loop();
  }

  fprintf(stderr, "unknown mode: %s\n", argv[1]);
  return EXIT_FAILURE;
}
