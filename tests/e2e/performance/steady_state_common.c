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
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#define PROFILE_MUTEX 1
#define PROFILE_RWLOCK 2
#define PROFILE_CONDVAR 3
#define PROFILE_BARRIER 4
#define PROFILE_YIELD 5
#define PROFILE_SIGMASK 6
#define PROFILE_PIPE 7
#define PROFILE_SOCKETPAIR 8
#define PROFILE_SYSCALL 9
#define PROFILE_MMAP 10

#ifndef STEADY_PROFILE
#error "a steady-state profile wrapper must define STEADY_PROFILE"
#endif

#ifndef PROFILE_NAME
#error "a steady-state profile wrapper must define PROFILE_NAME"
#endif

enum { THREADS = 4 };

static void fail(const char *operation, int error) {
  fprintf(stderr, "%s: %s\n", operation, strerror(error));
  exit(EXIT_FAILURE);
}

static void check_pthread(const char *operation, int error) {
  if (error != 0) {
    fail(operation, error);
  }
}

#if STEADY_PROFILE == PROFILE_PIPE || STEADY_PROFILE == PROFILE_SOCKETPAIR
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

#if STEADY_PROFILE == PROFILE_MUTEX

enum { ROUNDS = 800 };

struct mutex_state {
  pthread_mutex_t mutex;
  uint64_t count;
};

static void *mutex_worker(void *opaque) {
  struct mutex_state *state = opaque;
  for (unsigned round = 0; round < ROUNDS; ++round) {
    check_pthread("pthread_mutex_lock", pthread_mutex_lock(&state->mutex));
    state->count += 1;
    if (sched_yield() != 0) {
      fail("sched_yield", errno);
    }
    check_pthread("pthread_mutex_unlock", pthread_mutex_unlock(&state->mutex));
  }
  return NULL;
}

static uint64_t run_profile(void) {
  struct mutex_state state = {.mutex = PTHREAD_MUTEX_INITIALIZER, .count = 0};
  pthread_t threads[THREADS];
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_create",
                  pthread_create(&threads[index], NULL, mutex_worker, &state));
  }
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_join", pthread_join(threads[index], NULL));
  }
  check_pthread("pthread_mutex_destroy", pthread_mutex_destroy(&state.mutex));
  return state.count;
}

#elif STEADY_PROFILE == PROFILE_RWLOCK

enum { ROUNDS = 800 };

struct rwlock_state {
  pthread_rwlock_t lock;
  uint64_t count;
};

static void *rwlock_worker(void *opaque) {
  struct rwlock_state *state = opaque;
  for (unsigned round = 0; round < ROUNDS; ++round) {
    check_pthread("pthread_rwlock_wrlock",
                  pthread_rwlock_wrlock(&state->lock));
    state->count += 1;
    if (sched_yield() != 0) {
      fail("sched_yield", errno);
    }
    check_pthread("pthread_rwlock_unlock",
                  pthread_rwlock_unlock(&state->lock));
  }
  return NULL;
}

static uint64_t run_profile(void) {
  struct rwlock_state state = {.lock = PTHREAD_RWLOCK_INITIALIZER, .count = 0};
  pthread_t threads[THREADS];
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_create",
                  pthread_create(&threads[index], NULL, rwlock_worker, &state));
  }
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_join", pthread_join(threads[index], NULL));
  }
  check_pthread("pthread_rwlock_destroy", pthread_rwlock_destroy(&state.lock));
  return state.count;
}

#elif STEADY_PROFILE == PROFILE_CONDVAR

enum { ROUNDS = 500 };

struct condvar_state {
  pthread_mutex_t mutex;
  pthread_cond_t condvar;
  unsigned turn;
  uint64_t count;
};

struct condvar_worker {
  struct condvar_state *state;
  unsigned id;
};

static void *condvar_worker(void *opaque) {
  struct condvar_worker *worker = opaque;
  struct condvar_state *state = worker->state;
  for (unsigned round = 0; round < ROUNDS; ++round) {
    check_pthread("pthread_mutex_lock", pthread_mutex_lock(&state->mutex));
    while (state->turn != worker->id) {
      check_pthread("pthread_cond_wait",
                    pthread_cond_wait(&state->condvar, &state->mutex));
    }
    state->count += 1;
    state->turn = (state->turn + 1) % THREADS;
    check_pthread("pthread_cond_broadcast",
                  pthread_cond_broadcast(&state->condvar));
    check_pthread("pthread_mutex_unlock", pthread_mutex_unlock(&state->mutex));
  }
  return NULL;
}

static uint64_t run_profile(void) {
  struct condvar_state state = {
      .mutex = PTHREAD_MUTEX_INITIALIZER,
      .condvar = PTHREAD_COND_INITIALIZER,
      .turn = 0,
      .count = 0,
  };
  pthread_t threads[THREADS];
  struct condvar_worker workers[THREADS];
  for (unsigned index = 0; index < THREADS; ++index) {
    workers[index] = (struct condvar_worker){.state = &state, .id = index};
    check_pthread("pthread_create", pthread_create(&threads[index], NULL,
                                                    condvar_worker,
                                                    &workers[index]));
  }
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_join", pthread_join(threads[index], NULL));
  }
  check_pthread("pthread_cond_destroy", pthread_cond_destroy(&state.condvar));
  check_pthread("pthread_mutex_destroy", pthread_mutex_destroy(&state.mutex));
  return state.count;
}

#elif STEADY_PROFILE == PROFILE_BARRIER

enum { ROUNDS = 700 };

struct barrier_worker {
  pthread_barrier_t *barrier;
  uint64_t completed;
};

static void *barrier_worker(void *opaque) {
  struct barrier_worker *worker = opaque;
  for (unsigned round = 0; round < ROUNDS; ++round) {
    int error = pthread_barrier_wait(worker->barrier);
    if (error != 0 && error != PTHREAD_BARRIER_SERIAL_THREAD) {
      fail("pthread_barrier_wait", error);
    }
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
  uint64_t completed = 0;
  for (unsigned index = 0; index < THREADS; ++index) {
    workers[index].barrier = &barrier;
    check_pthread("pthread_create", pthread_create(&threads[index], NULL,
                                                    barrier_worker,
                                                    &workers[index]));
  }
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_join", pthread_join(threads[index], NULL));
    completed += workers[index].completed;
  }
  check_pthread("pthread_barrier_destroy", pthread_barrier_destroy(&barrier));
  return completed;
}

#elif STEADY_PROFILE == PROFILE_YIELD

enum { ROUNDS = 1000 };

static void *yield_worker(void *opaque) {
  uint64_t *completed = opaque;
  for (unsigned round = 0; round < ROUNDS; ++round) {
    if (sched_yield() != 0) {
      fail("sched_yield", errno);
    }
    *completed += 1;
  }
  return NULL;
}

static uint64_t run_profile(void) {
  pthread_t threads[THREADS];
  uint64_t completed[THREADS] = {0};
  uint64_t total = 0;
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_create", pthread_create(&threads[index], NULL,
                                                    yield_worker,
                                                    &completed[index]));
  }
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_join", pthread_join(threads[index], NULL));
    total += completed[index];
  }
  return total;
}

#elif STEADY_PROFILE == PROFILE_SIGMASK

enum { ROUNDS = 500 };

static void *sigmask_worker(void *opaque) {
  uint64_t *completed = opaque;
  sigset_t blocked;
  sigset_t previous;
  sigemptyset(&blocked);
  sigaddset(&blocked, SIGUSR1);
  for (unsigned round = 0; round < ROUNDS; ++round) {
    check_pthread("pthread_sigmask block",
                  pthread_sigmask(SIG_BLOCK, &blocked, &previous));
    check_pthread("pthread_sigmask restore",
                  pthread_sigmask(SIG_SETMASK, &previous, NULL));
    *completed += 1;
  }
  return NULL;
}

static uint64_t run_profile(void) {
  pthread_t threads[THREADS];
  uint64_t completed[THREADS] = {0};
  uint64_t total = 0;
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_create", pthread_create(&threads[index], NULL,
                                                    sigmask_worker,
                                                    &completed[index]));
  }
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_join", pthread_join(threads[index], NULL));
    total += completed[index];
  }
  return total;
}

#elif STEADY_PROFILE == PROFILE_PIPE || STEADY_PROFILE == PROFILE_SOCKETPAIR

enum { ROUNDS = 1000 };

struct echo_state {
  int input;
  int output;
};

static void *echo_worker(void *opaque) {
  struct echo_state *state = opaque;
  for (uint64_t expected = 1; expected <= ROUNDS; ++expected) {
    uint64_t value = 0;
    read_exact(state->input, &value, sizeof(value));
    if (value != expected) {
      fail("unexpected IPC token", EPROTO);
    }
    write_exact(state->output, &value, sizeof(value));
  }
  return NULL;
}

static uint64_t run_profile(void) {
  int outbound[2];
  int inbound[2];
#if STEADY_PROFILE == PROFILE_PIPE
  if (pipe(outbound) != 0 || pipe(inbound) != 0) {
    fail("pipe", errno);
  }
#else
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, outbound) != 0 ||
      socketpair(AF_UNIX, SOCK_STREAM, 0, inbound) != 0) {
    fail("socketpair", errno);
  }
#endif
  struct echo_state state = {.input = outbound[0], .output = inbound[1]};
  pthread_t thread;
  check_pthread("pthread_create",
                pthread_create(&thread, NULL, echo_worker, &state));
  uint64_t checksum = 0;
  for (uint64_t value = 1; value <= ROUNDS; ++value) {
    uint64_t echoed = 0;
    write_exact(outbound[1], &value, sizeof(value));
    read_exact(inbound[0], &echoed, sizeof(echoed));
    if (echoed != value) {
      fail("mismatched IPC token", EPROTO);
    }
    checksum += echoed;
  }
  check_pthread("pthread_join", pthread_join(thread, NULL));
  for (unsigned index = 0; index < 2; ++index) {
    close(outbound[index]);
    close(inbound[index]);
  }
  return checksum;
}

#elif STEADY_PROFILE == PROFILE_SYSCALL

enum { ROUNDS = 1000 };

static void *syscall_worker(void *opaque) {
  uint64_t *completed = opaque;
  volatile long sink = 0;
  for (unsigned round = 0; round < ROUNDS; ++round) {
    struct timespec now;
    sink ^= (long)getpid();
    sink ^= (long)getppid();
    sink ^= syscall(SYS_gettid);
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
      fail("clock_gettime", errno);
    }
    sink ^= now.tv_nsec;
    *completed += 1;
  }
  (void)sink;
  return NULL;
}

static uint64_t run_profile(void) {
  pthread_t threads[THREADS];
  uint64_t completed[THREADS] = {0};
  uint64_t total = 0;
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_create", pthread_create(&threads[index], NULL,
                                                    syscall_worker,
                                                    &completed[index]));
  }
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_join", pthread_join(threads[index], NULL));
    total += completed[index];
  }
  return total;
}

#elif STEADY_PROFILE == PROFILE_MMAP

enum { ROUNDS = 250 };

struct mmap_worker {
  unsigned id;
  uint64_t checksum;
};

static void *mmap_worker(void *opaque) {
  struct mmap_worker *worker = opaque;
  long page_size = sysconf(_SC_PAGESIZE);
  if (page_size <= 0) {
    fail("sysconf", errno);
  }
  for (unsigned round = 0; round < ROUNDS; ++round) {
    unsigned char *page = mmap(NULL, (size_t)page_size, PROT_READ | PROT_WRITE,
                               MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (page == MAP_FAILED) {
      fail("mmap", errno);
    }
    page[0] = (unsigned char)(worker->id + round);
    page[page_size - 1] = (unsigned char)(worker->id * 3 + round);
    worker->checksum += page[0] + page[page_size - 1];
    if (mprotect(page, (size_t)page_size, PROT_READ) != 0 ||
        mprotect(page, (size_t)page_size, PROT_READ | PROT_WRITE) != 0) {
      fail("mprotect", errno);
    }
    if (munmap(page, (size_t)page_size) != 0) {
      fail("munmap", errno);
    }
  }
  return NULL;
}

static uint64_t run_profile(void) {
  pthread_t threads[THREADS];
  struct mmap_worker workers[THREADS] = {0};
  uint64_t checksum = 0;
  for (unsigned index = 0; index < THREADS; ++index) {
    workers[index].id = index;
    check_pthread("pthread_create", pthread_create(&threads[index], NULL,
                                                    mmap_worker,
                                                    &workers[index]));
  }
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_join", pthread_join(threads[index], NULL));
    checksum += workers[index].checksum;
  }
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
