/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <semaphore.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#define THREAD_COUNT 4
#define BARRIER_ROUNDS 8

static void fail_errno(const char *operation) {
  perror(operation);
  exit(1);
}

static void check_pthread(int result, const char *operation) {
  if (result != 0) {
    errno = result;
    fail_errno(operation);
  }
}

static void wait_semaphore(sem_t *semaphore) {
  while (sem_wait(semaphore) != 0) {
    if (errno != EINTR) {
      fail_errno("sem_wait");
    }
  }
}

struct barrier_args {
  pthread_barrier_t *barrier;
  int id;
  int *serial_threads;
};

static void *barrier_worker(void *opaque) {
  struct barrier_args *args = opaque;
  for (int round = 0; round < BARRIER_ROUNDS; round++) {
    int result = pthread_barrier_wait(args->barrier);
    if (result == PTHREAD_BARRIER_SERIAL_THREAD) {
      args->serial_threads[round] = args->id;
    } else {
      check_pthread(result, "pthread_barrier_wait");
    }
    sched_yield();
  }
  return NULL;
}

static void barrier_pattern(void) {
  pthread_barrier_t barrier;
  check_pthread(pthread_barrier_init(&barrier, NULL, THREAD_COUNT),
                "pthread_barrier_init");
  pthread_t threads[THREAD_COUNT];
  struct barrier_args args[THREAD_COUNT];
  int serial_threads[BARRIER_ROUNDS];
  for (int round = 0; round < BARRIER_ROUNDS; round++) {
    serial_threads[round] = -1;
  }
  for (int id = 0; id < THREAD_COUNT; id++) {
    args[id] = (struct barrier_args){&barrier, id, serial_threads};
    check_pthread(pthread_create(&threads[id], NULL, barrier_worker, &args[id]),
                  "pthread_create(barrier)");
  }
  for (int id = 0; id < THREAD_COUNT; id++) {
    check_pthread(pthread_join(threads[id], NULL), "pthread_join(barrier)");
  }
  printf("barrier:");
  for (int round = 0; round < BARRIER_ROUNDS; round++) {
    if (serial_threads[round] < 0) {
      fprintf(stderr, "barrier round %d did not select a serial thread\n", round);
      exit(1);
    }
    printf("%s%d", round == 0 ? "" : ",", serial_threads[round]);
  }
  printf("\n");
  check_pthread(pthread_barrier_destroy(&barrier),
                "pthread_barrier_destroy");
}

struct condvar_state {
  pthread_mutex_t mutex;
  pthread_cond_t work;
  pthread_cond_t ready_changed;
  pthread_cond_t completed_changed;
  int ready;
  int tickets;
  int completed;
  int order[THREAD_COUNT];
};

struct condvar_args {
  struct condvar_state *state;
  int id;
};

static void *condvar_worker(void *opaque) {
  struct condvar_args *args = opaque;
  struct condvar_state *state = args->state;
  check_pthread(pthread_mutex_lock(&state->mutex),
                "pthread_mutex_lock(condvar)");
  state->ready++;
  check_pthread(pthread_cond_signal(&state->ready_changed),
                "pthread_cond_signal(ready)");
  while (state->tickets == 0) {
    check_pthread(pthread_cond_wait(&state->work, &state->mutex),
                  "pthread_cond_wait(work)");
  }
  state->tickets--;
  state->order[state->completed++] = args->id;
  check_pthread(pthread_cond_signal(&state->completed_changed),
                "pthread_cond_signal(completed)");
  check_pthread(pthread_mutex_unlock(&state->mutex),
                "pthread_mutex_unlock(condvar)");
  return NULL;
}

static void condvar_pattern(void) {
  struct condvar_state state = {
      .mutex = PTHREAD_MUTEX_INITIALIZER,
      .work = PTHREAD_COND_INITIALIZER,
      .ready_changed = PTHREAD_COND_INITIALIZER,
      .completed_changed = PTHREAD_COND_INITIALIZER,
  };
  pthread_t threads[THREAD_COUNT];
  struct condvar_args args[THREAD_COUNT];
  for (int id = 0; id < THREAD_COUNT; id++) {
    args[id] = (struct condvar_args){&state, id};
    check_pthread(pthread_create(&threads[id], NULL, condvar_worker, &args[id]),
                  "pthread_create(condvar)");
  }

  check_pthread(pthread_mutex_lock(&state.mutex),
                "pthread_mutex_lock(condvar main)");
  while (state.ready < THREAD_COUNT) {
    check_pthread(pthread_cond_wait(&state.ready_changed, &state.mutex),
                  "pthread_cond_wait(ready)");
  }
  for (int signal = 0; signal < 2; signal++) {
    state.tickets++;
    check_pthread(pthread_cond_signal(&state.work),
                  "pthread_cond_signal(work)");
    while (state.completed <= signal) {
      check_pthread(pthread_cond_wait(&state.completed_changed, &state.mutex),
                    "pthread_cond_wait(completed)");
    }
  }
  state.tickets += THREAD_COUNT - 2;
  check_pthread(pthread_cond_broadcast(&state.work),
                "pthread_cond_broadcast(work)");
  while (state.completed < THREAD_COUNT) {
    check_pthread(pthread_cond_wait(&state.completed_changed, &state.mutex),
                  "pthread_cond_wait(completed broadcast)");
  }
  check_pthread(pthread_mutex_unlock(&state.mutex),
                "pthread_mutex_unlock(condvar main)");

  for (int id = 0; id < THREAD_COUNT; id++) {
    check_pthread(pthread_join(threads[id], NULL), "pthread_join(condvar)");
  }
  printf("condvar:");
  for (int position = 0; position < THREAD_COUNT; position++) {
    printf("%s%d", position == 0 ? "" : ",", state.order[position]);
  }
  printf("\n");
  check_pthread(pthread_cond_destroy(&state.completed_changed),
                "pthread_cond_destroy(completed)");
  check_pthread(pthread_cond_destroy(&state.ready_changed),
                "pthread_cond_destroy(ready)");
  check_pthread(pthread_cond_destroy(&state.work), "pthread_cond_destroy(work)");
  check_pthread(pthread_mutex_destroy(&state.mutex),
                "pthread_mutex_destroy(condvar)");
}

struct rwlock_state {
  pthread_rwlock_t lock;
  pthread_mutex_t record_mutex;
  pthread_barrier_t start;
  int value;
  int count;
  char types[THREAD_COUNT];
  int ids[THREAD_COUNT];
  int observed[THREAD_COUNT];
};

struct rwlock_args {
  struct rwlock_state *state;
  int id;
  int writer;
};

static void *rwlock_worker(void *opaque) {
  struct rwlock_args *args = opaque;
  struct rwlock_state *state = args->state;
  int result = pthread_barrier_wait(&state->start);
  if (result != 0 && result != PTHREAD_BARRIER_SERIAL_THREAD) {
    check_pthread(result, "pthread_barrier_wait(rwlock)");
  }

  int observed;
  if (args->writer) {
    check_pthread(pthread_rwlock_wrlock(&state->lock),
                  "pthread_rwlock_wrlock");
    observed = ++state->value;
  } else {
    check_pthread(pthread_rwlock_rdlock(&state->lock),
                  "pthread_rwlock_rdlock");
    observed = state->value;
  }
  check_pthread(pthread_mutex_lock(&state->record_mutex),
                "pthread_mutex_lock(rwlock record)");
  int position = state->count++;
  state->types[position] = args->writer ? 'W' : 'R';
  state->ids[position] = args->id;
  state->observed[position] = observed;
  check_pthread(pthread_mutex_unlock(&state->record_mutex),
                "pthread_mutex_unlock(rwlock record)");
  sched_yield();
  if (args->writer) {
    check_pthread(pthread_rwlock_unlock(&state->lock),
                  "pthread_rwlock_unlock(writer)");
  } else {
    check_pthread(pthread_rwlock_unlock(&state->lock),
                  "pthread_rwlock_unlock(reader)");
  }
  return NULL;
}

static void rwlock_pattern(void) {
  struct rwlock_state state = {
      .lock = PTHREAD_RWLOCK_INITIALIZER,
      .record_mutex = PTHREAD_MUTEX_INITIALIZER,
  };
  check_pthread(pthread_barrier_init(&state.start, NULL, THREAD_COUNT + 1),
                "pthread_barrier_init(rwlock)");
  const int writer[THREAD_COUNT] = {0, 1, 0, 1};
  pthread_t threads[THREAD_COUNT];
  struct rwlock_args args[THREAD_COUNT];
  for (int id = 0; id < THREAD_COUNT; id++) {
    args[id] = (struct rwlock_args){&state, id, writer[id]};
    check_pthread(pthread_create(&threads[id], NULL, rwlock_worker, &args[id]),
                  "pthread_create(rwlock)");
  }
  int result = pthread_barrier_wait(&state.start);
  if (result != 0 && result != PTHREAD_BARRIER_SERIAL_THREAD) {
    check_pthread(result, "pthread_barrier_wait(rwlock main)");
  }
  for (int id = 0; id < THREAD_COUNT; id++) {
    check_pthread(pthread_join(threads[id], NULL), "pthread_join(rwlock)");
  }
  if (state.count != THREAD_COUNT || state.value != 2) {
    fprintf(stderr, "rwlock state mismatch: count=%d value=%d\n", state.count,
            state.value);
    exit(1);
  }
  printf("rwlock:");
  for (int position = 0; position < THREAD_COUNT; position++) {
    printf("%s%c%d=%d", position == 0 ? "" : ",", state.types[position],
           state.ids[position], state.observed[position]);
  }
  printf("\n");
  check_pthread(pthread_barrier_destroy(&state.start),
                "pthread_barrier_destroy(rwlock)");
  check_pthread(pthread_mutex_destroy(&state.record_mutex),
                "pthread_mutex_destroy(rwlock)");
  check_pthread(pthread_rwlock_destroy(&state.lock),
                "pthread_rwlock_destroy");
}

struct semaphore_state {
  sem_t ready;
  sem_t gate;
  sem_t completed;
  pthread_mutex_t record_mutex;
  int count;
  int order[THREAD_COUNT];
};

struct semaphore_args {
  struct semaphore_state *state;
  int id;
};

static void *semaphore_worker(void *opaque) {
  struct semaphore_args *args = opaque;
  struct semaphore_state *state = args->state;
  if (sem_post(&state->ready) != 0) {
    fail_errno("sem_post(ready)");
  }
  wait_semaphore(&state->gate);
  check_pthread(pthread_mutex_lock(&state->record_mutex),
                "pthread_mutex_lock(semaphore record)");
  state->order[state->count++] = args->id;
  check_pthread(pthread_mutex_unlock(&state->record_mutex),
                "pthread_mutex_unlock(semaphore record)");
  if (sem_post(&state->completed) != 0) {
    fail_errno("sem_post(completed)");
  }
  return NULL;
}

static void semaphore_pattern(void) {
  struct semaphore_state state = {
      .record_mutex = PTHREAD_MUTEX_INITIALIZER,
  };
  if (sem_init(&state.ready, 0, 0) != 0 || sem_init(&state.gate, 0, 0) != 0 ||
      sem_init(&state.completed, 0, 0) != 0) {
    fail_errno("sem_init");
  }
  pthread_t threads[THREAD_COUNT];
  struct semaphore_args args[THREAD_COUNT];
  for (int id = 0; id < THREAD_COUNT; id++) {
    args[id] = (struct semaphore_args){&state, id};
    check_pthread(
        pthread_create(&threads[id], NULL, semaphore_worker, &args[id]),
        "pthread_create(semaphore)");
  }
  for (int id = 0; id < THREAD_COUNT; id++) {
    wait_semaphore(&state.ready);
  }
  for (int id = 0; id < THREAD_COUNT; id++) {
    if (sem_post(&state.gate) != 0) {
      fail_errno("sem_post(gate)");
    }
    wait_semaphore(&state.completed);
  }
  for (int id = 0; id < THREAD_COUNT; id++) {
    check_pthread(pthread_join(threads[id], NULL), "pthread_join(semaphore)");
  }
  printf("semaphore:");
  for (int position = 0; position < THREAD_COUNT; position++) {
    printf("%s%d", position == 0 ? "" : ",", state.order[position]);
  }
  printf("\n");
  if (sem_destroy(&state.completed) != 0 || sem_destroy(&state.gate) != 0 ||
      sem_destroy(&state.ready) != 0) {
    fail_errno("sem_destroy");
  }
  check_pthread(pthread_mutex_destroy(&state.record_mutex),
                "pthread_mutex_destroy(semaphore)");
}

struct cancellation_state {
  pthread_mutex_t mutex;
  pthread_cond_t ready_changed;
  pthread_cond_t proceed_changed;
  int ready;
  int proceed;
  int cleanup_ran;
};

static void cancellation_cleanup(void *opaque) {
  struct cancellation_state *state = opaque;
  state->cleanup_ran = 1;
}

static void *cancellation_worker(void *opaque) {
  struct cancellation_state *state = opaque;
  int previous_state;
  check_pthread(pthread_setcancelstate(PTHREAD_CANCEL_DISABLE, &previous_state),
                "pthread_setcancelstate(disable)");
  check_pthread(pthread_mutex_lock(&state->mutex),
                "pthread_mutex_lock(cancellation)");
  state->ready = 1;
  check_pthread(pthread_cond_signal(&state->ready_changed),
                "pthread_cond_signal(cancellation ready)");
  while (!state->proceed) {
    check_pthread(pthread_cond_wait(&state->proceed_changed, &state->mutex),
                  "pthread_cond_wait(cancellation proceed)");
  }
  check_pthread(pthread_mutex_unlock(&state->mutex),
                "pthread_mutex_unlock(cancellation)");

  pthread_cleanup_push(cancellation_cleanup, state);
  check_pthread(pthread_setcancelstate(previous_state, NULL),
                "pthread_setcancelstate(enable)");
  pthread_testcancel();
  pthread_cleanup_pop(0);
  return NULL;
}

static void cancellation_pattern(void) {
  struct cancellation_state state = {
      .mutex = PTHREAD_MUTEX_INITIALIZER,
      .ready_changed = PTHREAD_COND_INITIALIZER,
      .proceed_changed = PTHREAD_COND_INITIALIZER,
  };
  pthread_t thread;
  check_pthread(pthread_create(&thread, NULL, cancellation_worker, &state),
                "pthread_create(cancellation)");
  check_pthread(pthread_mutex_lock(&state.mutex),
                "pthread_mutex_lock(cancellation main)");
  while (!state.ready) {
    check_pthread(pthread_cond_wait(&state.ready_changed, &state.mutex),
                  "pthread_cond_wait(cancellation ready)");
  }
  check_pthread(pthread_cancel(thread), "pthread_cancel");
  state.proceed = 1;
  check_pthread(pthread_cond_signal(&state.proceed_changed),
                "pthread_cond_signal(cancellation proceed)");
  check_pthread(pthread_mutex_unlock(&state.mutex),
                "pthread_mutex_unlock(cancellation main)");
  void *result = NULL;
  check_pthread(pthread_join(thread, &result), "pthread_join(cancellation)");
  if (result != PTHREAD_CANCELED || !state.cleanup_ran) {
    fprintf(stderr, "cancellation mismatch: result=%p cleanup=%d\n", result,
            state.cleanup_ran);
    exit(1);
  }
  printf("cancellation:result=canceled,cleanup=%d\n", state.cleanup_ran);
  check_pthread(pthread_cond_destroy(&state.proceed_changed),
                "pthread_cond_destroy(cancellation proceed)");
  check_pthread(pthread_cond_destroy(&state.ready_changed),
                "pthread_cond_destroy(cancellation ready)");
  check_pthread(pthread_mutex_destroy(&state.mutex),
                "pthread_mutex_destroy(cancellation)");
}

static _Thread_local int direct_tls;

struct tls_result {
  int direct;
  uintptr_t key;
};

static void write_exact(int fd, const void *buffer, size_t length) {
  const uint8_t *cursor = buffer;
  while (length > 0) {
    ssize_t written = write(fd, cursor, length);
    if (written < 0 && errno == EINTR) {
      continue;
    }
    if (written <= 0) {
      _exit(90);
    }
    cursor += written;
    length -= (size_t)written;
  }
}

static void read_exact(int fd, void *buffer, size_t length) {
  uint8_t *cursor = buffer;
  while (length > 0) {
    ssize_t count = read(fd, cursor, length);
    if (count < 0 && errno == EINTR) {
      continue;
    }
    if (count <= 0) {
      fail_errno("read(tls child result)");
    }
    cursor += count;
    length -= (size_t)count;
  }
}

static void tls_fork_pattern(void) {
  pthread_key_t key;
  check_pthread(pthread_key_create(&key, NULL), "pthread_key_create");
  direct_tls = 41;
  check_pthread(pthread_setspecific(key, (void *)(uintptr_t)0x1234),
                "pthread_setspecific");
  int pipefds[2];
  if (pipe(pipefds) != 0) {
    fail_errno("pipe(tls fork)");
  }
  pid_t child = fork();
  if (child < 0) {
    fail_errno("fork(tls)");
  }
  if (child == 0) {
    close(pipefds[0]);
    struct tls_result result = {
        .direct = direct_tls,
        .key = (uintptr_t)pthread_getspecific(key),
    };
    write_exact(pipefds[1], &result, sizeof(result));
    close(pipefds[1]);
    _exit(0);
  }

  close(pipefds[1]);
  int status;
  while (waitpid(child, &status, 0) < 0) {
    if (errno != EINTR) {
      fail_errno("waitpid(tls)");
    }
  }
  struct tls_result result;
  read_exact(pipefds[0], &result, sizeof(result));
  close(pipefds[0]);
  if (!WIFEXITED(status) || WEXITSTATUS(status) != 0 || result.direct != 41 ||
      result.key != 0x1234) {
    fprintf(stderr,
            "TLS fork mismatch: status=%d direct=%d key=%#lx\n", status,
            result.direct, (unsigned long)result.key);
    exit(1);
  }
  printf("tls-fork:direct=%d,key=%#lx\n", result.direct,
         (unsigned long)result.key);
  check_pthread(pthread_key_delete(key), "pthread_key_delete");
}

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s PATTERN\n", argv[0]);
    return 2;
  }
  if (strcmp(argv[1], "barrier") == 0) {
    barrier_pattern();
  } else if (strcmp(argv[1], "condvar") == 0) {
    condvar_pattern();
  } else if (strcmp(argv[1], "rwlock") == 0) {
    rwlock_pattern();
  } else if (strcmp(argv[1], "semaphore") == 0) {
    semaphore_pattern();
  } else if (strcmp(argv[1], "cancellation") == 0) {
    cancellation_pattern();
  } else if (strcmp(argv[1], "tls-fork") == 0) {
    tls_fork_pattern();
  } else {
    fprintf(stderr, "unknown pattern: %s\n", argv[1]);
    return 2;
  }
  return 0;
}
