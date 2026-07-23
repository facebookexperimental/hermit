#define _GNU_SOURCE

#include <errno.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

enum {
  THREADS = 8,
  ITERATIONS = 10000,
};

struct thread_state {
  pthread_barrier_t barrier;
  pthread_mutex_t mutex;
  long counter;
};

struct process_state {
  pthread_mutex_t mutex;
  pthread_cond_t condition;
  int child_ready;
  int release_child;
};

static void check_pthread(int result, const char *operation) {
  if (result != 0) {
    errno = result;
    perror(operation);
    exit(1);
  }
}

static void *thread_worker(void *opaque) {
  struct thread_state *state = opaque;
  int barrier_result = pthread_barrier_wait(&state->barrier);
  if (barrier_result != 0 && barrier_result != PTHREAD_BARRIER_SERIAL_THREAD) {
    check_pthread(barrier_result, "pthread_barrier_wait");
  }

  for (int i = 0; i < ITERATIONS; ++i) {
    check_pthread(pthread_mutex_lock(&state->mutex), "pthread_mutex_lock");
    ++state->counter;
    check_pthread(pthread_mutex_unlock(&state->mutex), "pthread_mutex_unlock");
  }
  return NULL;
}

static void verify_thread_futexes(void) {
  struct thread_state state = {0};
  pthread_t threads[THREADS];

  check_pthread(pthread_barrier_init(&state.barrier, NULL, THREADS + 1),
                "pthread_barrier_init");
  check_pthread(pthread_mutex_init(&state.mutex, NULL), "pthread_mutex_init");

  for (int i = 0; i < THREADS; ++i) {
    check_pthread(pthread_create(&threads[i], NULL, thread_worker, &state),
                  "pthread_create");
  }

  int barrier_result = pthread_barrier_wait(&state.barrier);
  if (barrier_result != 0 && barrier_result != PTHREAD_BARRIER_SERIAL_THREAD) {
    check_pthread(barrier_result, "pthread_barrier_wait");
  }
  for (int i = 0; i < THREADS; ++i) {
    check_pthread(pthread_join(threads[i], NULL), "pthread_join");
  }

  long expected = (long)THREADS * ITERATIONS;
  if (state.counter != expected) {
    fprintf(stderr, "thread counter: expected %ld, got %ld\n", expected,
            state.counter);
    exit(1);
  }
  check_pthread(pthread_mutex_destroy(&state.mutex), "pthread_mutex_destroy");
  check_pthread(pthread_barrier_destroy(&state.barrier),
                "pthread_barrier_destroy");
}

static void verify_process_shared_futexes(void) {
  struct process_state *state = mmap(NULL, sizeof(*state), PROT_READ | PROT_WRITE,
                                     MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  if (state == MAP_FAILED) {
    perror("mmap");
    exit(1);
  }

  pthread_mutexattr_t mutex_attributes;
  pthread_condattr_t condition_attributes;
  check_pthread(pthread_mutexattr_init(&mutex_attributes),
                "pthread_mutexattr_init");
  check_pthread(pthread_mutexattr_setpshared(&mutex_attributes,
                                             PTHREAD_PROCESS_SHARED),
                "pthread_mutexattr_setpshared");
  check_pthread(pthread_condattr_init(&condition_attributes),
                "pthread_condattr_init");
  check_pthread(pthread_condattr_setpshared(&condition_attributes,
                                            PTHREAD_PROCESS_SHARED),
                "pthread_condattr_setpshared");
  check_pthread(pthread_mutex_init(&state->mutex, &mutex_attributes),
                "pthread_mutex_init");
  check_pthread(pthread_cond_init(&state->condition, &condition_attributes),
                "pthread_cond_init");

  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    exit(1);
  }
  if (child == 0) {
    check_pthread(pthread_mutex_lock(&state->mutex), "child mutex lock");
    state->child_ready = 1;
    check_pthread(pthread_cond_broadcast(&state->condition),
                  "child condition broadcast");
    while (!state->release_child) {
      check_pthread(pthread_cond_wait(&state->condition, &state->mutex),
                    "child condition wait");
    }
    check_pthread(pthread_mutex_unlock(&state->mutex), "child mutex unlock");
    _exit(0);
  }

  check_pthread(pthread_mutex_lock(&state->mutex), "parent mutex lock");
  while (!state->child_ready) {
    check_pthread(pthread_cond_wait(&state->condition, &state->mutex),
                  "parent condition wait");
  }
  state->release_child = 1;
  check_pthread(pthread_cond_broadcast(&state->condition),
                "parent condition broadcast");
  check_pthread(pthread_mutex_unlock(&state->mutex), "parent mutex unlock");

  int status = 0;
  if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    fprintf(stderr, "process-shared child failed: status=%d\n", status);
    exit(1);
  }

  check_pthread(pthread_cond_destroy(&state->condition), "pthread_cond_destroy");
  check_pthread(pthread_mutex_destroy(&state->mutex), "pthread_mutex_destroy");
  check_pthread(pthread_condattr_destroy(&condition_attributes),
                "pthread_condattr_destroy");
  check_pthread(pthread_mutexattr_destroy(&mutex_attributes),
                "pthread_mutexattr_destroy");
  if (munmap(state, sizeof(*state)) != 0) {
    perror("munmap");
    exit(1);
  }
}

int main(void) {
  verify_thread_futexes();
  verify_process_shared_futexes();
  puts("SHARED_FUTEX_PTHREAD_OK threads=8 process_shared=1");
  return 0;
}
