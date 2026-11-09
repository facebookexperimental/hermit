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
 *   dumpable-secret  the same page without MADV_DONTDUMP, as a control
 *   children MIB...  fork one child per argument, one at a time, that
 *                 dirties that many MiB of incompressible heap and segfaults;
 *                 exit 1
 *   child-segv-ok fork one child that segfaults; exit 0
 *   threads       two threads wait forever while a third, not the main
 *                 thread, segfaults with THREAD_MARKER in r12
 *   parked        three threads park in raw futex, nanosleep and read calls,
 *                 thread i with PARK_REG_MARKER + i in r12 and PARK_FP_MARKER
 *                 + i in xmm7, while main joins a fifth thread that prints
 *                 every TID and then calls abort()
 */

#include <linux/futex.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define MARKER "HERMIT-FATAL-CORE-MARKER"
#define THREAD_MARKER 0x4845524d49543132u

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

static void secret_page_and_segfault(int exclude) {
  /* Decode directly into the page: a plaintext copy in the ELF's read-only
   * mapping would make the global core search depend on coredump_filter.
   * Volatile reads prevent constant folding; volatile writes retain the page. */
  static const volatile unsigned char encoded_secret[] = {
      0xed, 0xe0, 0xf7, 0xe8, 0xec, 0xf1, 0x88, 0xe1, 0xea, 0xeb, 0xf1,
      0xe1, 0xf0, 0xe8, 0xf5, 0x88, 0xf6, 0xe0, 0xe6, 0xf7, 0xe0, 0xf1,
      0xa5};
  volatile unsigned char* page =
      mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (page == MAP_FAILED ||
      (exclude && madvise((void*)page, 4096, MADV_DONTDUMP) != 0)) {
    perror("mmap/madvise");
    exit(3);
  }
  for (size_t i = 0; i < sizeof(encoded_secret); i++) {
    page[i] = encoded_secret[i] ^ 0xa5;
  }
  dirty_and_segfault(1, 1);
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

/* The main thread waits on a condition variable nothing signals while another
 * thread faults: the fault ends the process with the leader still blocked in a
 * futex wait that no thread's exit wakes. */
static void segfault_while_the_leader_waits(void) {
  pthread_t thread;
  if (pthread_create(&thread, NULL, segfault_with_marker, NULL) != 0) {
    exit(3);
  }
  wait_forever(NULL);
}

#define PARK_REG_MARKER 0x4845524d49545200u
#define PARK_FP_MARKER 0x4845524d49544600u
#define PARKERS 3

static int park_futex_word;
static int park_pipe[2];
static atomic_int parked;

/* Issues raw syscall `nr` forever with r12 = PARK_REG_MARKER + index and
 * xmm7 = PARK_FP_MARKER + index, which no syscall changes, so a core shows
 * whose registers it kept for this thread. */
__attribute__((noinline, noreturn)) static void
park_forever(long nr, long a0, long a1, long a2, long index) {
  register uint64_t r12 __asm__("r12") = PARK_REG_MARKER + (uint64_t)index;
  uint64_t fp = PARK_FP_MARKER + (uint64_t)index;
  __asm__ volatile(
      "movq %[fp], %%xmm7\n"
      "1:\n\t"
      "movq %[nr], %%rax\n\t"
      "syscall\n\t"
      "jmp 1b" ::[nr] "r"(nr),
      [fp] "r"(fp),
      "D"(a0),
      "S"(a1),
      "d"(a2),
      "r"(r12)
      : "rax", "rcx", "r11", "xmm7", "memory");
  __builtin_unreachable();
}

__attribute__((noinline, noreturn)) static void park_in_futex(void) {
  park_forever(SYS_futex, (long)&park_futex_word, FUTEX_WAIT_PRIVATE, 0, 1);
}

__attribute__((noinline, noreturn)) static void park_in_nanosleep(void) {
  static const struct timespec day = {86400, 0};
  park_forever(SYS_nanosleep, (long)&day, 0, 0, 2);
}

__attribute__((noinline, noreturn)) static void park_in_read(void) {
  static char byte;
  park_forever(SYS_read, park_pipe[0], (long)&byte, 1, 3);
}

/* main, the three parked threads, and the one that aborts. */
static pid_t park_tids[PARKERS + 2];

static void* parker(void* arg) {
  long index = (long)arg;
  park_tids[index] = (pid_t)syscall(SYS_gettid);
  atomic_fetch_add(&parked, 1);
  switch (index) {
    case 1:
      park_in_futex();
    case 2:
      park_in_nanosleep();
    default:
      park_in_read();
  }
}

static void sleep_ms(long ms) {
  struct timespec time = {0, ms * 1000000};
  nanosleep(&time, NULL);
}

/* Waits until every parker is about to block, gives them time to, prints
 * every TID and aborts. */
static void* abort_once_parked(void* arg) {
  (void)arg;
  while (atomic_load(&parked) < PARKERS) {
    sleep_ms(1);
  }
  sleep_ms(20);
  park_tids[PARKERS + 1] = (pid_t)syscall(SYS_gettid);
  printf(
      "parked main=%d futex=%d nanosleep=%d read=%d abort=%d\n",
      park_tids[0],
      park_tids[1],
      park_tids[2],
      park_tids[3],
      park_tids[4]);
  fflush(stdout);
  abort();
}

static void abort_with_threads_parked(void) {
  pthread_t threads[PARKERS + 1];
  /* Small stacks keep the whole core small, so that writing it never runs
   * out of time and falls back to a smaller tier on a loaded host. */
  pthread_attr_t small_stack;
  if (pthread_attr_init(&small_stack) != 0 ||
      pthread_attr_setstacksize(&small_stack, 256 << 10) != 0) {
    exit(3);
  }
  park_tids[0] = (pid_t)syscall(SYS_gettid);
  if (pipe(park_pipe) != 0) {
    exit(3);
  }
  for (long i = 1; i <= PARKERS; i++) {
    if (pthread_create(&threads[i - 1], &small_stack, parker, (void*)i) != 0) {
      exit(3);
    }
  }
  if (pthread_create(
          &threads[PARKERS], &small_stack, abort_once_parked, NULL) != 0) {
    exit(3);
  }
  pthread_join(threads[PARKERS], NULL);
  exit(4);
}

int main(int argc, char** argv) {
  const char* mode = argc > 1 ? argv[1] : "";
  if (strcmp(mode, "segv") == 0) {
    dirty_and_segfault(1, 1);
  } else if (strcmp(mode, "dontdump") == 0 ||
             strcmp(mode, "dumpable-secret") == 0) {
    secret_page_and_segfault(strcmp(mode, "dontdump") == 0);
  } else if (strcmp(mode, "children") == 0) {
    fork_segfaulting_children(argc - 2, argv + 2);
    return 1;
  } else if (strcmp(mode, "child-segv-ok") == 0) {
    char* one[] = {"1"};
    fork_segfaulting_children(1, one);
    return 0;
  } else if (strcmp(mode, "threads") == 0) {
    segfault_on_a_third_thread();
  } else if (strcmp(mode, "leader-waits") == 0) {
    segfault_while_the_leader_waits();
  } else if (strcmp(mode, "parked") == 0) {
    abort_with_threads_parked();
  }
  fprintf(
      stderr,
      "usage: %s segv | dontdump | dumpable-secret | children MIB... | "
      "child-segv-ok | threads | leader-waits | parked\n",
      argv[0]);
  return 2;
}
