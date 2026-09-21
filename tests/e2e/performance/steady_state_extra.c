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
#include <linux/futex.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <semaphore.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define PROFILE_SEMAPHORE 1
#define PROFILE_FUTEX 2
#define PROFILE_EVENTFD 3
#define PROFILE_POLL_PIPE 4
#define PROFILE_EPOLL_EVENTFD 5
#define PROFILE_PIPE_CHURN 6
#define PROFILE_THREAD_CHURN 7
#define PROFILE_FORK_WAIT 8
#define PROFILE_TRYLOCK 9
#define PROFILE_NANOSLEEP 10

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

#if STEADY_PROFILE != PROFILE_FORK_WAIT
static void check_pthread(const char *operation, int error) {
  if (error != 0) {
    fail(operation, error);
  }
}
#endif

#if STEADY_PROFILE == PROFILE_EVENTFD || STEADY_PROFILE == PROFILE_POLL_PIPE || \
    STEADY_PROFILE == PROFILE_EPOLL_EVENTFD ||                            \
    STEADY_PROFILE == PROFILE_PIPE_CHURN
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

#if STEADY_PROFILE == PROFILE_SEMAPHORE

enum { ROUNDS = 700 };

struct semaphore_worker {
  sem_t *mine;
  sem_t *next;
  uint64_t completed;
};

static void *semaphore_worker(void *opaque) {
  struct semaphore_worker *worker = opaque;
  for (unsigned round = 0; round < ROUNDS; ++round) {
    while (sem_wait(worker->mine) != 0) {
      if (errno != EINTR) {
        fail("sem_wait", errno);
      }
    }
    worker->completed += 1;
    if (sem_post(worker->next) != 0) {
      fail("sem_post", errno);
    }
  }
  return NULL;
}

static uint64_t run_profile(void) {
  sem_t semaphores[2];
  if (sem_init(&semaphores[0], 0, 1) != 0 ||
      sem_init(&semaphores[1], 0, 0) != 0) {
    fail("sem_init", errno);
  }
  struct semaphore_worker workers[2] = {
      {.mine = &semaphores[0], .next = &semaphores[1], .completed = 0},
      {.mine = &semaphores[1], .next = &semaphores[0], .completed = 0},
  };
  pthread_t threads[2];
  for (unsigned index = 0; index < 2; ++index) {
    check_pthread("pthread_create", pthread_create(&threads[index], NULL,
                                                    semaphore_worker,
                                                    &workers[index]));
  }
  for (unsigned index = 0; index < 2; ++index) {
    check_pthread("pthread_join", pthread_join(threads[index], NULL));
  }
  for (unsigned index = 0; index < 2; ++index) {
    if (sem_destroy(&semaphores[index]) != 0) {
      fail("sem_destroy", errno);
    }
  }
  return workers[0].completed + workers[1].completed;
}

#elif STEADY_PROFILE == PROFILE_FUTEX

enum { ROUNDS = 700 };

struct futex_worker {
  atomic_int *turn;
  int id;
  uint64_t completed;
};

static void futex_wait_for_turn(atomic_int *turn, int id) {
  while (atomic_load(turn) != id) {
    int observed = atomic_load(turn);
    long result = syscall(SYS_futex, turn, FUTEX_WAIT_PRIVATE, observed, NULL,
                          NULL, 0);
    if (result != 0 && errno != EAGAIN && errno != EINTR) {
      fail("futex wait", errno);
    }
  }
}

static void *futex_worker(void *opaque) {
  struct futex_worker *worker = opaque;
  for (unsigned round = 0; round < ROUNDS; ++round) {
    futex_wait_for_turn(worker->turn, worker->id);
    worker->completed += 1;
    atomic_store(worker->turn, 1 - worker->id);
    if (syscall(SYS_futex, worker->turn, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0) <
        0) {
      fail("futex wake", errno);
    }
  }
  return NULL;
}

static uint64_t run_profile(void) {
  atomic_int turn = 0;
  struct futex_worker workers[2] = {
      {.turn = &turn, .id = 0, .completed = 0},
      {.turn = &turn, .id = 1, .completed = 0},
  };
  pthread_t threads[2];
  for (unsigned index = 0; index < 2; ++index) {
    check_pthread("pthread_create", pthread_create(&threads[index], NULL,
                                                    futex_worker,
                                                    &workers[index]));
  }
  for (unsigned index = 0; index < 2; ++index) {
    check_pthread("pthread_join", pthread_join(threads[index], NULL));
  }
  return workers[0].completed + workers[1].completed;
}

#elif STEADY_PROFILE == PROFILE_EVENTFD

enum { ROUNDS = 700 };

struct eventfd_worker {
  int mine;
  int next;
  uint64_t completed;
};

static void *eventfd_worker(void *opaque) {
  struct eventfd_worker *worker = opaque;
  for (unsigned round = 0; round < ROUNDS; ++round) {
    uint64_t token = 0;
    read_exact(worker->mine, &token, sizeof(token));
    worker->completed += token;
    token = 1;
    write_exact(worker->next, &token, sizeof(token));
  }
  return NULL;
}

static uint64_t run_profile(void) {
  int events[2] = {eventfd(1, EFD_CLOEXEC), eventfd(0, EFD_CLOEXEC)};
  if (events[0] < 0 || events[1] < 0) {
    fail("eventfd", errno);
  }
  struct eventfd_worker workers[2] = {
      {.mine = events[0], .next = events[1], .completed = 0},
      {.mine = events[1], .next = events[0], .completed = 0},
  };
  pthread_t threads[2];
  for (unsigned index = 0; index < 2; ++index) {
    check_pthread("pthread_create", pthread_create(&threads[index], NULL,
                                                    eventfd_worker,
                                                    &workers[index]));
  }
  for (unsigned index = 0; index < 2; ++index) {
    check_pthread("pthread_join", pthread_join(threads[index], NULL));
  }
  for (unsigned index = 0; index < 2; ++index) {
    if (close(events[index]) != 0) {
      fail("close eventfd", errno);
    }
  }
  return workers[0].completed + workers[1].completed;
}

#elif STEADY_PROFILE == PROFILE_POLL_PIPE

enum { ROUNDS = 600 };

struct poll_worker {
  int input;
  int output;
  uint64_t completed;
};

static void *poll_worker(void *opaque) {
  struct poll_worker *worker = opaque;
  struct pollfd descriptor = {.fd = worker->input, .events = POLLIN};
  for (unsigned round = 0; round < ROUNDS; ++round) {
    int ready;
    do {
      ready = poll(&descriptor, 1, -1);
    } while (ready < 0 && errno == EINTR);
    if (ready != 1 || (descriptor.revents & POLLIN) == 0) {
      fail("poll", ready < 0 ? errno : EIO);
    }
    unsigned char token;
    read_exact(worker->input, &token, sizeof(token));
    worker->completed += token;
    write_exact(worker->output, &token, sizeof(token));
  }
  return NULL;
}

static uint64_t run_profile(void) {
  int pipes[2][2];
  if (pipe2(pipes[0], O_CLOEXEC) != 0 || pipe2(pipes[1], O_CLOEXEC) != 0) {
    fail("pipe2", errno);
  }
  struct poll_worker workers[2] = {
      {.input = pipes[0][0], .output = pipes[1][1], .completed = 0},
      {.input = pipes[1][0], .output = pipes[0][1], .completed = 0},
  };
  unsigned char token = 1;
  write_exact(pipes[0][1], &token, sizeof(token));
  pthread_t threads[2];
  for (unsigned index = 0; index < 2; ++index) {
    check_pthread("pthread_create", pthread_create(&threads[index], NULL,
                                                    poll_worker,
                                                    &workers[index]));
  }
  for (unsigned index = 0; index < 2; ++index) {
    check_pthread("pthread_join", pthread_join(threads[index], NULL));
  }
  for (unsigned index = 0; index < 2; ++index) {
    if (close(pipes[index][0]) != 0 || close(pipes[index][1]) != 0) {
      fail("close pipe", errno);
    }
  }
  return workers[0].completed + workers[1].completed;
}

#elif STEADY_PROFILE == PROFILE_EPOLL_EVENTFD

enum { ROUNDS = 500 };

struct epoll_worker {
  int epoll_fd;
  int mine;
  int next;
  uint64_t completed;
};

static void *epoll_worker(void *opaque) {
  struct epoll_worker *worker = opaque;
  for (unsigned round = 0; round < ROUNDS; ++round) {
    struct epoll_event event;
    int ready;
    do {
      ready = epoll_wait(worker->epoll_fd, &event, 1, -1);
    } while (ready < 0 && errno == EINTR);
    if (ready != 1 || event.data.fd != worker->mine) {
      fail("epoll_wait", ready < 0 ? errno : EIO);
    }
    uint64_t token = 0;
    read_exact(worker->mine, &token, sizeof(token));
    worker->completed += token;
    token = 1;
    write_exact(worker->next, &token, sizeof(token));
  }
  return NULL;
}

static uint64_t run_profile(void) {
  int events[2] = {eventfd(1, EFD_CLOEXEC), eventfd(0, EFD_CLOEXEC)};
  int epolls[2] = {epoll_create1(EPOLL_CLOEXEC), epoll_create1(EPOLL_CLOEXEC)};
  if (events[0] < 0 || events[1] < 0 || epolls[0] < 0 || epolls[1] < 0) {
    fail("epoll setup", errno);
  }
  for (unsigned index = 0; index < 2; ++index) {
    struct epoll_event event = {.events = EPOLLIN, .data.fd = events[index]};
    if (epoll_ctl(epolls[index], EPOLL_CTL_ADD, events[index], &event) != 0) {
      fail("epoll_ctl", errno);
    }
  }
  struct epoll_worker workers[2] = {
      {.epoll_fd = epolls[0], .mine = events[0], .next = events[1]},
      {.epoll_fd = epolls[1], .mine = events[1], .next = events[0]},
  };
  pthread_t threads[2];
  for (unsigned index = 0; index < 2; ++index) {
    check_pthread("pthread_create", pthread_create(&threads[index], NULL,
                                                    epoll_worker,
                                                    &workers[index]));
  }
  for (unsigned index = 0; index < 2; ++index) {
    check_pthread("pthread_join", pthread_join(threads[index], NULL));
  }
  for (unsigned index = 0; index < 2; ++index) {
    if (close(epolls[index]) != 0 || close(events[index]) != 0) {
      fail("close epoll", errno);
    }
  }
  return workers[0].completed + workers[1].completed;
}

#elif STEADY_PROFILE == PROFILE_PIPE_CHURN

enum { THREADS = 4, ROUNDS = 300 };

struct pipe_churn_worker {
  unsigned id;
  uint64_t checksum;
};

static void *pipe_churn_worker(void *opaque) {
  struct pipe_churn_worker *worker = opaque;
  for (unsigned round = 0; round < ROUNDS; ++round) {
    int descriptors[2];
    if (pipe2(descriptors, O_CLOEXEC) != 0) {
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
  struct pipe_churn_worker workers[THREADS] = {0};
  uint64_t checksum = 0;
  for (unsigned index = 0; index < THREADS; ++index) {
    workers[index].id = index;
    check_pthread("pthread_create", pthread_create(&threads[index], NULL,
                                                    pipe_churn_worker,
                                                    &workers[index]));
  }
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_join", pthread_join(threads[index], NULL));
    checksum += workers[index].checksum;
  }
  return checksum;
}

#elif STEADY_PROFILE == PROFILE_THREAD_CHURN

enum { ROUNDS = 250 };

static void *thread_churn_worker(void *opaque) { return opaque; }

static uint64_t run_profile(void) {
  uint64_t checksum = 0;
  for (uintptr_t round = 1; round <= ROUNDS; ++round) {
    pthread_t thread;
    void *result = NULL;
    check_pthread("pthread_create",
                  pthread_create(&thread, NULL, thread_churn_worker,
                                 (void *)round));
    check_pthread("pthread_join", pthread_join(thread, &result));
    checksum += (uintptr_t)result;
  }
  return checksum;
}

#elif STEADY_PROFILE == PROFILE_FORK_WAIT

enum { ROUNDS = 450 };

static uint64_t run_profile(void) {
  uint64_t checksum = 0;
  for (unsigned round = 0; round < ROUNDS; ++round) {
    pid_t child = fork();
    if (child < 0) {
      fail("fork", errno);
    }
    if (child == 0) {
      _exit((int)(round % 64));
    }
    int status = 0;
    if (waitpid(child, &status, 0) != child) {
      fail("waitpid", errno);
    }
    if (!WIFEXITED(status)) {
      fail("child status", ECHILD);
    }
    checksum += (uint64_t)WEXITSTATUS(status);
  }
  return checksum;
}

#elif STEADY_PROFILE == PROFILE_TRYLOCK

enum { THREADS = 4, ROUNDS = 700 };

struct trylock_state {
  pthread_mutex_t mutex;
  uint64_t count;
};

static void *trylock_worker(void *opaque) {
  struct trylock_state *state = opaque;
  for (unsigned round = 0; round < ROUNDS; ++round) {
    int error;
    while ((error = pthread_mutex_trylock(&state->mutex)) == EBUSY) {
      if (sched_yield() != 0) {
        fail("sched_yield", errno);
      }
    }
    check_pthread("pthread_mutex_trylock", error);
    state->count += 1;
    if (sched_yield() != 0) {
      fail("sched_yield", errno);
    }
    check_pthread("pthread_mutex_unlock", pthread_mutex_unlock(&state->mutex));
  }
  return NULL;
}

static uint64_t run_profile(void) {
  struct trylock_state state = {
      .mutex = PTHREAD_MUTEX_INITIALIZER,
      .count = 0,
  };
  pthread_t threads[THREADS];
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_create", pthread_create(&threads[index], NULL,
                                                    trylock_worker, &state));
  }
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_join", pthread_join(threads[index], NULL));
  }
  check_pthread("pthread_mutex_destroy", pthread_mutex_destroy(&state.mutex));
  return state.count;
}

#elif STEADY_PROFILE == PROFILE_NANOSLEEP

enum { THREADS = 4, ROUNDS = 500 };

static void *nanosleep_worker(void *opaque) {
  uint64_t *completed = opaque;
  const struct timespec duration = {.tv_sec = 0, .tv_nsec = 1000};
  for (unsigned round = 0; round < ROUNDS; ++round) {
    struct timespec remaining = duration;
    while (nanosleep(&remaining, &remaining) != 0) {
      if (errno != EINTR) {
        fail("nanosleep", errno);
      }
    }
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
  uint64_t checksum = 0;
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_create", pthread_create(&threads[index], NULL,
                                                    nanosleep_worker,
                                                    &completed[index]));
  }
  for (unsigned index = 0; index < THREADS; ++index) {
    check_pthread("pthread_join", pthread_join(threads[index], NULL));
    checksum += completed[index];
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
