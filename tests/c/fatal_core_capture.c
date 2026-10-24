/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Guest for hermit-cli/tests/fatal_core_capture.rs.
 *
 *   segv          dirty 1 MiB of heap, put MARKER at its start, segfault
 *   dontdump      like segv, but first put SECRET in a page excluded from
 *                 cores with madvise(MADV_DONTDUMP)
 *   children MIB...  fork one child per argument, one at a time, that
 *                 dirties that many MiB of incompressible heap and segfaults;
 *                 exit 1
 *   child-segv-ok fork one child that segfaults; exit 0
 *   threads       two threads wait forever while a third, not the main
 *                 thread, segfaults with THREAD_MARKER in r12
 */

#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define MARKER "HERMIT-FATAL-CORE-MARKER"
#define THREAD_MARKER 0x4845524d49543132u
#define SECRET "HERMIT-DONTDUMP-SECRET"

/* Volatile so that no store to the heap is dropped as dead. */
static unsigned char* volatile heap;

static void dirty_and_segfault(size_t mib, unsigned seed) {
  size_t len = mib << 20;
  unsigned x = seed * 2654435761u + 1;
  heap = malloc(len);
  if (heap == NULL) {
    perror("malloc");
    exit(3);
  }
  for (size_t i = 0; i < len; i++) {
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    heap[i] = (unsigned char)x;
  }
  memcpy((void*)heap, MARKER, sizeof(MARKER));
  *(volatile int*)0 = 1;
}

static void fork_segfaulting_children(int count, char** mib) {
  for (int i = 0; i < count; i++) {
    int status;
    pid_t pid = fork();
    if (pid < 0) {
      perror("fork");
      exit(3);
    }
    if (pid == 0) {
      dirty_and_segfault((size_t)atoi(mib[i]), (unsigned)i + 7);
    }
    if (waitpid(pid, &status, 0) != pid) {
      perror("waitpid");
      exit(3);
    }
    printf("child %d signal %d\n", i, WIFSIGNALED(status) ? WTERMSIG(status) : 0);
  }
}

static pthread_mutex_t never_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t never = PTHREAD_COND_INITIALIZER;

static void* wait_forever(void* arg) {
  (void)arg;
  pthread_mutex_lock(&never_lock);
  for (;;) {
    pthread_cond_wait(&never, &never_lock);
  }
  return NULL;
}

/* Faults by writing to address 8 with THREAD_MARKER in r12, so the core
 * shows whose registers it kept. */
static void* segfault_with_marker(void* arg) {
  (void)arg;
  __asm__ volatile(
      "movabs %0, %%r12\n\tmovl $1, 8" ::"i"(THREAD_MARKER)
      : "r12", "memory");
  return NULL;
}

static void segfault_on_a_third_thread(void) {
  pthread_t threads[3];
  for (int i = 0; i < 2; i++) {
    if (pthread_create(&threads[i], NULL, wait_forever, NULL) != 0) {
      exit(3);
    }
  }
  if (pthread_create(&threads[2], NULL, segfault_with_marker, NULL) != 0) {
    exit(3);
  }
  pthread_join(threads[2], NULL);
  exit(4);
}

int main(int argc, char** argv) {
  const char* mode = argc > 1 ? argv[1] : "";
  if (strcmp(mode, "segv") == 0) {
    dirty_and_segfault(1, 1);
  } else if (strcmp(mode, "dontdump") == 0) {
    char* page =
        mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (page == MAP_FAILED || madvise(page, 4096, MADV_DONTDUMP) != 0) {
      perror("mmap/madvise");
      exit(3);
    }
    memcpy(page, SECRET, sizeof(SECRET));
    dirty_and_segfault(1, 1);
  } else if (strcmp(mode, "children") == 0) {
    fork_segfaulting_children(argc - 2, argv + 2);
    return 1;
  } else if (strcmp(mode, "child-segv-ok") == 0) {
    char* one[] = {"1"};
    fork_segfaulting_children(1, one);
    return 0;
  } else if (strcmp(mode, "threads") == 0) {
    segfault_on_a_third_thread();
  }
  fprintf(
      stderr,
      "usage: %s segv | dontdump | children MIB... | child-segv-ok | threads\n",
      argv[0]);
  return 2;
}
