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
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/random.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

#define PROFILE_DUP_CLOSE 1
#define PROFILE_OPEN_READ_CLOSE 2
#define PROFILE_SOCKETPAIR_DGRAM 3
#define PROFILE_PIPE_NONBLOCK 4
#define PROFILE_GETRANDOM 5
#define PROFILE_CLOCK_QUERY 6
#define PROFILE_SPIN_TRYLOCK 7
#define PROFILE_COND_SIGNAL 8
#define PROFILE_RWLOCK_MIXED 9
#define PROFILE_BARRIER_YIELD 10

#ifndef STEADY_PROFILE
#error "a steady-state profile wrapper must define STEADY_PROFILE"
#endif

#ifndef PROFILE_NAME
#error "a steady-state profile wrapper must define PROFILE_NAME"
#endif

static void fail(const char *operation, int error) {
  fprintf(stderr, "%s: %s\n", operation, strerror(error));
  exit(EXIT_FAILURE);
}

static void check_pthread(const char *operation, int error) {
  if (error != 0) {
    fail(operation, error);
  }
}

#if STEADY_PROFILE == PROFILE_OPEN_READ_CLOSE ||                         \
    STEADY_PROFILE == PROFILE_SOCKETPAIR_DGRAM ||                        \
    STEADY_PROFILE == PROFILE_PIPE_NONBLOCK
static void read_exact(int fd, void *buffer, size_t length) {
  unsigned char *next = buffer;
  while (length != 0) {
    ssize_t count = read(fd, next, length);
    if (count < 0 && errno == EINTR) {
      continue;
    }
    if (count <= 0) {
      fail("read", count == 0 ? EPIPE : errno);
    }
    next += (size_t)count;
    length -= (size_t)count;
  }
}
#endif

#if STEADY_PROFILE == PROFILE_SOCKETPAIR_DGRAM || \
    STEADY_PROFILE == PROFILE_PIPE_NONBLOCK
static void write_exact(int fd, const void *buffer, size_t length) {
  const unsigned char *next = buffer;
  while (length != 0) {
    ssize_t count = write(fd, next, length);
    if (count < 0 && errno == EINTR) {
      continue;
    }
    if (count <= 0) {
      fail("write", errno);
    }
    next += (size_t)count;
    length -= (size_t)count;
  }
}
#endif

#if STEADY_PROFILE == PROFILE_DUP_CLOSE

enum { THREADS = 4, ROUNDS = 800 };

struct dup_worker {
  int source;
  uint64_t completed;
};

static void *dup_worker(void *opaque) {
  struct dup_worker *worker = opaque;
  for (unsigned round = 0; round < ROUNDS; ++round) {
    int copy = dup(worker->source);
    if (copy < 0) {
      fail("dup", errno);
    }
    if (fcntl(copy, F_SETFD, FD_CLOEXEC) != 0) {
      fail("fcntl", errno);
    }
    if (close(copy) != 0) {
      fail("close", errno);
    }
    worker->completed += 1;
  }
  return NULL;
}

static uint64_t run_profile(void) {
  int source = open("/dev/null", O_RDONLY | O_CLOEXEC);
  if (source < 0) {
    fail("open /dev/null", errno);
  }
  pthread_t threads[THREADS];
  struct dup_worker workers[THREADS] = {0};
  uint64_t checksum = 0;
  for (unsigned index = 0; index < THREADS; ++index) {
    workers[index].source = source;
    check_pthread("pthread_create", pthread_create(&threads[index], NULL,
                                                    dup_worker,
                                                    &workers[index]));
  }
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_join", pthread_join(threads[index], NULL));
    checksum += workers[index].completed;
  }
  if (close(source) != 0) {
    fail("close source", errno);
  }
  return checksum;
}

#elif STEADY_PROFILE == PROFILE_OPEN_READ_CLOSE

enum { THREADS = 4, ROUNDS = 500 };

static void *open_worker(void *opaque) {
  uint64_t *completed = opaque;
  for (unsigned round = 0; round < ROUNDS; ++round) {
    int descriptor = open("/dev/zero", O_RDONLY | O_CLOEXEC);
    if (descriptor < 0) {
      fail("open /dev/zero", errno);
    }
    unsigned char byte = 1;
    read_exact(descriptor, &byte, sizeof(byte));
    if (byte != 0) {
      fail("read /dev/zero", EIO);
    }
    if (close(descriptor) != 0) {
      fail("close /dev/zero", errno);
    }
    *completed += 1;
  }
  return NULL;
}

static uint64_t run_profile(void) {
  pthread_t threads[THREADS];
  uint64_t completed[THREADS] = {0};
  uint64_t checksum = 0;
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_create", pthread_create(&threads[index], NULL,
                                                    open_worker,
                                                    &completed[index]));
  }
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_join", pthread_join(threads[index], NULL));
    checksum += completed[index];
  }
  return checksum;
}

#elif STEADY_PROFILE == PROFILE_SOCKETPAIR_DGRAM

enum { THREADS = 4, ROUNDS = 250 };

struct socket_worker {
  unsigned id;
  uint64_t checksum;
};

static void *socket_worker(void *opaque) {
  struct socket_worker *worker = opaque;
  for (unsigned round = 0; round < ROUNDS; ++round) {
    int sockets[2];
    if (socketpair(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0, sockets) != 0) {
      fail("socketpair", errno);
    }
    unsigned char token = (unsigned char)((round + worker->id) % 251 + 1);
    unsigned char observed = 0;
    write_exact(sockets[0], &token, sizeof(token));
    read_exact(sockets[1], &observed, sizeof(observed));
    worker->checksum += observed;
    if (close(sockets[0]) != 0 || close(sockets[1]) != 0) {
      fail("close socketpair", errno);
    }
  }
  return NULL;
}

static uint64_t run_profile(void) {
  pthread_t threads[THREADS];
  struct socket_worker workers[THREADS] = {0};
  uint64_t checksum = 0;
  for (unsigned index = 0; index < THREADS; ++index) {
    workers[index].id = index;
    check_pthread("pthread_create", pthread_create(&threads[index], NULL,
                                                    socket_worker,
                                                    &workers[index]));
  }
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_join", pthread_join(threads[index], NULL));
    checksum += workers[index].checksum;
  }
  return checksum;
}

#elif STEADY_PROFILE == PROFILE_PIPE_NONBLOCK

enum { THREADS = 4, ROUNDS = 300 };

struct pipe_worker {
  unsigned id;
  uint64_t checksum;
};

static void *pipe_worker(void *opaque) {
  struct pipe_worker *worker = opaque;
  for (unsigned round = 0; round < ROUNDS; ++round) {
    int descriptors[2];
    if (pipe2(descriptors, O_CLOEXEC | O_NONBLOCK) != 0) {
      fail("pipe2", errno);
    }
    unsigned char token = (unsigned char)((round + worker->id) % 251 + 1);
    unsigned char observed = 0;
    write_exact(descriptors[1], &token, sizeof(token));
    read_exact(descriptors[0], &observed, sizeof(observed));
    worker->checksum += observed;
    if (close(descriptors[0]) != 0 || close(descriptors[1]) != 0) {
      fail("close pipe", errno);
    }
  }
  return NULL;
}

static uint64_t run_profile(void) {
  pthread_t threads[THREADS];
  struct pipe_worker workers[THREADS] = {0};
  uint64_t checksum = 0;
  for (unsigned index = 0; index < THREADS; ++index) {
    workers[index].id = index;
    check_pthread("pthread_create", pthread_create(&threads[index], NULL,
                                                    pipe_worker,
                                                    &workers[index]));
  }
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_join", pthread_join(threads[index], NULL));
    checksum += workers[index].checksum;
  }
  return checksum;
}

#elif STEADY_PROFILE == PROFILE_GETRANDOM

enum { THREADS = 4, ROUNDS = 800 };

static void *random_worker(void *opaque) {
  uint64_t *checksum = opaque;
  for (unsigned round = 0; round < ROUNDS; ++round) {
    unsigned char bytes[8];
    size_t offset = 0;
    while (offset != sizeof(bytes)) {
      ssize_t count = getrandom(bytes + offset, sizeof(bytes) - offset, 0);
      if (count < 0 && errno == EINTR) {
        continue;
      }
      if (count <= 0) {
        fail("getrandom", count == 0 ? EIO : errno);
      }
      offset += (size_t)count;
    }
    *checksum += sizeof(bytes);
  }
  return NULL;
}

static uint64_t run_profile(void) {
  pthread_t threads[THREADS];
  uint64_t checksums[THREADS] = {0};
  uint64_t checksum = 0;
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_create", pthread_create(&threads[index], NULL,
                                                    random_worker,
                                                    &checksums[index]));
  }
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_join", pthread_join(threads[index], NULL));
    checksum += checksums[index];
  }
  return checksum;
}

#elif STEADY_PROFILE == PROFILE_CLOCK_QUERY

enum { THREADS = 4, ROUNDS = 1000 };

static void *clock_worker(void *opaque) {
  uint64_t *completed = opaque;
  for (unsigned round = 0; round < ROUNDS; ++round) {
    struct timespec monotonic;
    struct timespec realtime;
    if (clock_gettime(CLOCK_MONOTONIC, &monotonic) != 0 ||
        clock_gettime(CLOCK_REALTIME, &realtime) != 0) {
      fail("clock_gettime", errno);
    }
    *completed += 1;
  }
  return NULL;
}

static uint64_t run_profile(void) {
  pthread_t threads[THREADS];
  uint64_t completed[THREADS] = {0};
  uint64_t checksum = 0;
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_create", pthread_create(&threads[index], NULL,
                                                    clock_worker,
                                                    &completed[index]));
  }
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_join", pthread_join(threads[index], NULL));
    checksum += completed[index];
  }
  return checksum;
}

#elif STEADY_PROFILE == PROFILE_SPIN_TRYLOCK

enum { THREADS = 4, ROUNDS = 800 };

struct spin_state {
  pthread_spinlock_t lock;
  uint64_t count;
};

static void *spin_worker(void *opaque) {
  struct spin_state *state = opaque;
  for (unsigned round = 0; round < ROUNDS; ++round) {
    int error;
    while ((error = pthread_spin_trylock(&state->lock)) == EBUSY) {
      if (sched_yield() != 0) {
        fail("sched_yield", errno);
      }
    }
    check_pthread("pthread_spin_trylock", error);
    state->count += 1;
    if (sched_yield() != 0) {
      fail("sched_yield", errno);
    }
    check_pthread("pthread_spin_unlock", pthread_spin_unlock(&state->lock));
  }
  return NULL;
}

static uint64_t run_profile(void) {
  struct spin_state state = {.count = 0};
  check_pthread("pthread_spin_init",
                pthread_spin_init(&state.lock, PTHREAD_PROCESS_PRIVATE));
  pthread_t threads[THREADS];
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_create", pthread_create(&threads[index], NULL,
                                                    spin_worker, &state));
  }
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_join", pthread_join(threads[index], NULL));
  }
  check_pthread("pthread_spin_destroy", pthread_spin_destroy(&state.lock));
  return state.count;
}

#elif STEADY_PROFILE == PROFILE_COND_SIGNAL

enum { ROUNDS = 700 };

struct cond_state {
  pthread_mutex_t mutex;
  pthread_cond_t condvar;
  unsigned turn;
  uint64_t completed[2];
};

struct cond_worker {
  struct cond_state *state;
  unsigned id;
};

static void *cond_worker(void *opaque) {
  struct cond_worker *worker = opaque;
  for (unsigned round = 0; round < ROUNDS; ++round) {
    check_pthread("pthread_mutex_lock",
                  pthread_mutex_lock(&worker->state->mutex));
    while (worker->state->turn != worker->id) {
      check_pthread("pthread_cond_wait",
                    pthread_cond_wait(&worker->state->condvar,
                                      &worker->state->mutex));
    }
    worker->state->completed[worker->id] += 1;
    worker->state->turn = 1 - worker->id;
    check_pthread("pthread_cond_signal",
                  pthread_cond_signal(&worker->state->condvar));
    check_pthread("pthread_mutex_unlock",
                  pthread_mutex_unlock(&worker->state->mutex));
  }
  return NULL;
}

static uint64_t run_profile(void) {
  struct cond_state state = {
      .mutex = PTHREAD_MUTEX_INITIALIZER,
      .condvar = PTHREAD_COND_INITIALIZER,
      .turn = 0,
      .completed = {0, 0},
  };
  struct cond_worker workers[2] = {
      {.state = &state, .id = 0},
      {.state = &state, .id = 1},
  };
  pthread_t threads[2];
  for (unsigned index = 0; index < 2; ++index) {
    check_pthread("pthread_create", pthread_create(&threads[index], NULL,
                                                    cond_worker,
                                                    &workers[index]));
  }
  for (unsigned index = 0; index < 2; ++index) {
    check_pthread("pthread_join", pthread_join(threads[index], NULL));
  }
  check_pthread("pthread_cond_destroy", pthread_cond_destroy(&state.condvar));
  check_pthread("pthread_mutex_destroy", pthread_mutex_destroy(&state.mutex));
  return state.completed[0] + state.completed[1];
}

#elif STEADY_PROFILE == PROFILE_RWLOCK_MIXED

enum { THREADS = 4, ROUNDS = 800 };

struct rwlock_worker {
  pthread_rwlock_t *lock;
  unsigned id;
  uint64_t completed;
};

static void *rwlock_worker(void *opaque) {
  struct rwlock_worker *worker = opaque;
  for (unsigned round = 0; round < ROUNDS; ++round) {
    if (worker->id == 0) {
      check_pthread("pthread_rwlock_wrlock",
                    pthread_rwlock_wrlock(worker->lock));
    } else {
      check_pthread("pthread_rwlock_rdlock",
                    pthread_rwlock_rdlock(worker->lock));
    }
    worker->completed += 1;
    if (sched_yield() != 0) {
      fail("sched_yield", errno);
    }
    check_pthread("pthread_rwlock_unlock",
                  pthread_rwlock_unlock(worker->lock));
  }
  return NULL;
}

static uint64_t run_profile(void) {
  pthread_rwlock_t lock = PTHREAD_RWLOCK_INITIALIZER;
  pthread_t threads[THREADS];
  struct rwlock_worker workers[THREADS] = {0};
  uint64_t checksum = 0;
  for (unsigned index = 0; index < THREADS; ++index) {
    workers[index].lock = &lock;
    workers[index].id = index;
    check_pthread("pthread_create", pthread_create(&threads[index], NULL,
                                                    rwlock_worker,
                                                    &workers[index]));
  }
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_join", pthread_join(threads[index], NULL));
    checksum += workers[index].completed;
  }
  check_pthread("pthread_rwlock_destroy", pthread_rwlock_destroy(&lock));
  return checksum;
}

#elif STEADY_PROFILE == PROFILE_BARRIER_YIELD

enum { THREADS = 4, ROUNDS = 700 };

struct barrier_worker {
  pthread_barrier_t *barrier;
  uint64_t completed;
};

static void wait_at_barrier(pthread_barrier_t *barrier) {
  int error = pthread_barrier_wait(barrier);
  if (error != 0 && error != PTHREAD_BARRIER_SERIAL_THREAD) {
    fail("pthread_barrier_wait", error);
  }
}

static void *barrier_worker(void *opaque) {
  struct barrier_worker *worker = opaque;
  for (unsigned round = 0; round < ROUNDS; ++round) {
    wait_at_barrier(worker->barrier);
    if (sched_yield() != 0) {
      fail("sched_yield", errno);
    }
    wait_at_barrier(worker->barrier);
    worker->completed += 1;
  }
  return NULL;
}

static uint64_t run_profile(void) {
  pthread_barrier_t barrier;
  check_pthread("pthread_barrier_init",
                pthread_barrier_init(&barrier, NULL, THREADS));
  pthread_t threads[THREADS];
  struct barrier_worker workers[THREADS] = {0};
  uint64_t checksum = 0;
  for (unsigned index = 0; index < THREADS; ++index) {
    workers[index].barrier = &barrier;
    check_pthread("pthread_create", pthread_create(&threads[index], NULL,
                                                    barrier_worker,
                                                    &workers[index]));
  }
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_join", pthread_join(threads[index], NULL));
    checksum += workers[index].completed;
  }
  check_pthread("pthread_barrier_destroy", pthread_barrier_destroy(&barrier));
  return checksum;
}

#else
#error "unknown steady-state profile"
#endif

int main(void) {
  uint64_t checksum = run_profile();
  printf("steady-state profile=%s checksum=%llu\n", PROFILE_NAME,
         (unsigned long long)checksum);
  return EXIT_SUCCESS;
}
