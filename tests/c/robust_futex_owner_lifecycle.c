/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

struct shared_state {
  pthread_mutex_t mutex;
  _Atomic int owner_ready;
  _Atomic int trigger;
};

static struct shared_state *state;
static const char *self_path;

static void die(const char *what) {
  perror(what);
  _exit(2);
}

static void wait_until(_Atomic int *word, int value) {
  for (unsigned i = 0; i < 10000000; ++i) {
    if (atomic_load_explicit(word, memory_order_acquire) == value) {
      return;
    }
    sched_yield();
  }
  fprintf(stderr, "timed out waiting for state %d\n", value);
  _exit(3);
}

static void wait_for_waiter(void) {
  _Atomic uint32_t *lock = (_Atomic uint32_t *)&state->mutex;
  for (unsigned i = 0; i < 10000000; ++i) {
    if (atomic_load_explicit(lock, memory_order_acquire) & FUTEX_WAITERS) {
      return;
    }
    sched_yield();
  }
  fputs("timed out waiting for FUTEX_WAITERS\n", stderr);
  _exit(4);
}

static void lock_as_owner(void) {
  int rc = pthread_mutex_lock(&state->mutex);
  if (rc != 0) {
    errno = rc;
    die("owner pthread_mutex_lock");
  }
  atomic_store_explicit(&state->owner_ready, 1, memory_order_release);
}

static void waiter_process(void) {
  int rc = pthread_mutex_lock(&state->mutex);
  if (rc != EOWNERDEAD) {
    fprintf(stderr, "waiter got %d (%s), expected EOWNERDEAD\n", rc,
            strerror(rc));
    _exit(5);
  }
  rc = pthread_mutex_consistent(&state->mutex);
  if (rc != 0) {
    errno = rc;
    die("pthread_mutex_consistent");
  }
  rc = pthread_mutex_unlock(&state->mutex);
  if (rc != 0) {
    errno = rc;
    die("pthread_mutex_unlock");
  }
  puts("PASS: waiter received EOWNERDEAD");
  fflush(stdout);
  _exit(0);
}

static pid_t spawn_waiter(void) {
  pid_t pid = fork();
  if (pid < 0) {
    die("fork waiter");
  }
  if (pid == 0) {
    waiter_process();
  }
  wait_for_waiter();
  return pid;
}

static void await_success(pid_t pid, const char *name) {
  int status;
  if (waitpid(pid, &status, 0) != pid) {
    die("waitpid");
  }
  if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    fprintf(stderr, "%s status %#x\n", name, status);
    exit(6);
  }
}

static void *owner_thread(void *unused) {
  (void)unused;
  lock_as_owner();
  for (;;) {
    pause();
  }
  return NULL;
}

static void *exec_thread(void *unused) {
  (void)unused;
  wait_until(&state->trigger, 1);
  execl(self_path, self_path, "after-exec", NULL);
  die("execl");
  return NULL;
}

static pid_t spawn_group_owner(const char *mode) {
  pid_t pid = fork();
  if (pid < 0) {
    die("fork owner");
  }
  if (pid != 0) {
    return pid;
  }

  if (strcmp(mode, "signal") == 0) {
    pthread_t thread;
    if (pthread_create(&thread, NULL, owner_thread, NULL) != 0) {
      die("pthread_create owner");
    }
    wait_until(&state->owner_ready, 1);
    wait_until(&state->trigger, 1);
    /* The signal recipient is deliberately not the robust-mutex owner. */
    raise(SIGTERM);
    _exit(9);
  }

  if (strcmp(mode, "exit-group") == 0) {
    pthread_t thread;
    if (pthread_create(&thread, NULL, owner_thread, NULL) != 0) {
      die("pthread_create owner");
    }
    wait_until(&state->owner_ready, 1);
    wait_until(&state->trigger, 1);
    syscall(SYS_exit_group, 0);
    _exit(7);
  }

  if (strcmp(mode, "de-thread") == 0) {
    lock_as_owner();
    pthread_t thread;
    if (pthread_create(&thread, NULL, exec_thread, NULL) != 0) {
      die("pthread_create exec");
    }
    for (;;) {
      pause();
    }
  }

  _exit(8);
}

int main(int argc, char **argv) {
  if (argc == 2 && strcmp(argv[1], "after-exec") == 0) {
    return 0;
  }
  if (argc != 2) {
    fprintf(stderr, "usage: %s signal|exit-group|de-thread\n", argv[0]);
    return 64;
  }
  self_path = argv[0];
  state = mmap(NULL, sizeof(*state), PROT_READ | PROT_WRITE,
               MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  if (state == MAP_FAILED) {
    die("mmap");
  }

  pthread_mutexattr_t attr;
  if (pthread_mutexattr_init(&attr) != 0) {
    die("pthread_mutexattr_init");
  }
  if (pthread_mutexattr_setpshared(&attr, PTHREAD_PROCESS_SHARED) != 0) {
    die("pthread_mutexattr_setpshared");
  }
  if (pthread_mutexattr_setrobust(&attr, PTHREAD_MUTEX_ROBUST) != 0) {
    die("pthread_mutexattr_setrobust");
  }
  if (pthread_mutex_init(&state->mutex, &attr) != 0) {
    die("pthread_mutex_init");
  }

  pid_t owner = spawn_group_owner(argv[1]);
  wait_until(&state->owner_ready, 1);
  pid_t waiter = spawn_waiter();

  atomic_store_explicit(&state->trigger, 1, memory_order_release);

  await_success(waiter, "waiter");
  int owner_status;
  if (waitpid(owner, &owner_status, 0) != owner) {
    die("waitpid owner");
  }
  puts("PASS: lifecycle completed");
  return 0;
}
