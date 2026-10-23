/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * FUTEX_REQUEUE, FUTEX_CMP_REQUEUE, FUTEX_WAKE_OP and the futex commands Linux
 * answers with an errno, checked against Linux's results
 * (https://github.com/rrnewton/hermit/issues/3958):
 *   - futex_requeue (kernel/futex/requeue.c): -EINVAL for a negative count,
 *     -EAGAIN when the CMP_REQUEUE word differs from val3, otherwise the number
 *     woken plus moved; a requeue onto the same futex moves nobody but counts;
 *     The counts differ from FUTEX_WAKE's: a count of 0 wakes nobody;
 *   - futex_wake_op (kernel/futex/waitwake.c): the encoded operation applied to
 *     uaddr2, -ENOSYS for an unknown operation (word untouched) or comparison
 *     (word already written); both of its counts follow FUTEX_WAKE's rule
 *     (`if (++ret >= nr_wake) break;`), so 0 or a negative count wakes one;
 *   - do_futex (kernel/futex/syscalls.c): -ENOSYS for FUTEX_FD, an unknown
 *     command, or FUTEX_CLOCK_REALTIME with FUTEX_WAKE; get_futex_key: -EINVAL
 *     for a misaligned word.
 * Every line printed is the same natively and under Hermit.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <limits.h>
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static uint32_t words[4] __attribute__((aligned(16)));
#define A (&words[0])
#define B (&words[1])

static long futex(
    uint32_t* uaddr,
    int op,
    uint32_t val,
    unsigned long val2,
    uint32_t* uaddr2,
    uint32_t val3) {
  return syscall(SYS_futex, uaddr, op, val, val2, uaddr2, val3);
}

static void report(const char* what, long ret) {
  if (ret < 0) {
    printf("%s: -1 %s\n", what, strerrorname_np(errno));
  } else {
    printf("%s: %ld\n", what, ret);
  }
}

static void* wait_on(void* word) {
  long ret = futex(word, FUTEX_WAIT_PRIVATE, 0, 0, NULL, 0);
  return (void*)ret;
}

/* Waiters queued on `word`, counted without waking any: a requeue onto the
 * same futex moves nobody and returns how many it counted. */
static long queued(uint32_t* word) {
  return futex(word, FUTEX_CMP_REQUEUE_PRIVATE, 0, INT_MAX, word, *word);
}

static int await_queued(uint32_t* word, long n) {
  for (int attempt = 0; attempt < 100000; attempt++) {
    if (queued(word) == n) {
      return 0;
    }
    sched_yield();
  }
  fprintf(stderr, "never saw %ld waiters queued\n", n);
  return 1;
}

/* Waiters that record the order in which they are woken: slot k holds the
 * index + 1 of the k-th waiter to finish, 0 until it is written. */
static atomic_int finished_count;
static atomic_int finished_order[4];
struct ordered_waiter {
  int index;
  uint32_t* word;
};

static void* ordered_wait(void* arg) {
  struct ordered_waiter* waiter = arg;
  futex(waiter->word, FUTEX_WAIT_PRIVATE, 0, 0, NULL, 0);
  atomic_store(&finished_order[atomic_fetch_add(&finished_count, 1)], waiter->index + 1);
  return NULL;
}

static int await_finished(int n) {
  for (int attempt = 0; attempt < 100000; attempt++) {
    if (atomic_load(&finished_order[n - 1]) != 0) {
      return 0;
    }
    sched_yield();
  }
  fprintf(stderr, "never saw %d waiters finish\n", n);
  return 1;
}

static int join_all(pthread_t* threads, int n) {
  for (int i = 0; i < n; i++) {
    void* ret;
    pthread_join(threads[i], &ret);
    if ((long)ret != 0) {
      fprintf(stderr, "waiter %d's FUTEX_WAIT returned %ld\n", i, (long)ret);
      return 1;
    }
  }
  return 0;
}

int main(void) {
  /* Errors that need no waiter. */
  report("requeue nr_requeue -1", futex(A, FUTEX_REQUEUE_PRIVATE, 1, (unsigned long)-1, B, 0));
  report("requeue nr_wake -1", futex(A, FUTEX_REQUEUE_PRIVATE, (uint32_t)-1, 1, B, 0));
  report("cmp_requeue val3 mismatch", futex(A, FUTEX_CMP_REQUEUE_PRIVATE, 1, 1, B, 1));
  report("cmp_requeue nr_requeue -1, val3 mismatch",
         futex(A, FUTEX_CMP_REQUEUE_PRIVATE, 1, (unsigned long)-1, B, 1));
  report("requeue to misaligned", futex(A, FUTEX_REQUEUE_PRIVATE, 1, 1, (uint32_t*)((char*)B + 1), 0));
  report("wake misaligned", futex((uint32_t*)((char*)A + 2), FUTEX_WAKE_PRIVATE, 1, 0, NULL, 0));
  report("futex_fd", futex(A, FUTEX_FD, 0, 0, NULL, 0));
  report("unknown command 99", futex(A, 99, 0, 0, NULL, 0));
  report("wake with clock_realtime", futex(A, FUTEX_WAKE_PRIVATE | FUTEX_CLOCK_REALTIME, 1, 0, NULL, 0));
  report("requeue with no waiters", futex(A, FUTEX_REQUEUE_PRIVATE, 1, 1, B, 0));

  /* Three waiters on A: move them all to B without waking, then wake them on B. */
  pthread_t threads[3];
  for (int i = 0; i < 3; i++) {
    pthread_create(&threads[i], NULL, wait_on, A);
  }
  if (await_queued(A, 3) != 0) {
    return 1;
  }
  report("cmp_requeue A->B wake 0 move all", futex(A, FUTEX_CMP_REQUEUE_PRIVATE, 0, INT_MAX, B, 0));
  report("wake A after the move", futex(A, FUTEX_WAKE_PRIVATE, INT_MAX, 0, NULL, 0));
  report("wake B 1", futex(B, FUTEX_WAKE_PRIVATE, 1, 0, NULL, 0));
  report("wake B all", futex(B, FUTEX_WAKE_PRIVATE, INT_MAX, 0, NULL, 0));
  if (join_all(threads, 3) != 0) {
    return 1;
  }

  /* Two waiters on A: requeue waking one and moving one, then wake the moved one. */
  for (int i = 0; i < 2; i++) {
    pthread_create(&threads[i], NULL, wait_on, A);
  }
  if (await_queued(A, 2) != 0) {
    return 1;
  }
  report("requeue A->B wake 1 move 1", futex(A, FUTEX_REQUEUE_PRIVATE, 1, 1, B, 0));
  report("wake B after requeue", futex(B, FUTEX_WAKE_PRIVATE, INT_MAX, 0, NULL, 0));
  if (join_all(threads, 2) != 0) {
    return 1;
  }

  /* FUTEX_WAKE_OP: one waiter on A, one on B (B's word is 0 while they wait). */
  pthread_create(&threads[0], NULL, wait_on, A);
  pthread_create(&threads[1], NULL, wait_on, B);
  if (await_queued(A, 1) != 0 || await_queued(B, 1) != 0) {
    return 1;
  }
  report("wake_op set B=5 if old==0",
         futex(A, FUTEX_WAKE_OP_PRIVATE, 1, 1, B,
               FUTEX_OP(FUTEX_OP_SET, 5, FUTEX_OP_CMP_EQ, 0)));
  printf("B is %u\n", *B);
  if (join_all(threads, 2) != 0) {
    return 1;
  }
  report("wake_op B+=3 if old>100",
         futex(A, FUTEX_WAKE_OP_PRIVATE, 1, 1, B,
               FUTEX_OP(FUTEX_OP_ADD, 3, FUTEX_OP_CMP_GT, 100)));
  printf("B is %u\n", *B);
  report("wake_op B|=1<<4 if old!=8",
         futex(A, FUTEX_WAKE_OP_PRIVATE, 0, 0, B,
               FUTEX_OP((FUTEX_OP_OR | FUTEX_OP_OPARG_SHIFT), 4, FUTEX_OP_CMP_NE, 8)));
  printf("B is %u\n", *B);
  report("wake_op unknown op 7", futex(A, FUTEX_WAKE_OP_PRIVATE, 1, 1, B, (7u << 28) | (1 << 12)));
  printf("B is %u\n", *B);
  report("wake_op unknown cmp 6", futex(A, FUTEX_WAKE_OP_PRIVATE, 1, 1, B, (6u << 24) | (1 << 12)));
  printf("B is %u\n", *B);
  report("wake_op uaddr2 NULL", futex(A, FUTEX_WAKE_OP_PRIVATE, 1, 1, NULL, 0));
  report("wake_op uaddr2 NULL unknown op 7",
         futex(A, FUTEX_WAKE_OP_PRIVATE, 1, 1, NULL, (7u << 28) | (1 << 12)));

  /* Counts of 0 and below: FUTEX_REQUEUE wakes and moves nobody, while each of
   * FUTEX_WAKE_OP's two counts wakes one waiter. */
  const uint32_t keep_b_zero = FUTEX_OP(FUTEX_OP_SET, 0, FUTEX_OP_CMP_EQ, 0);
  *B = 0;
  for (int i = 0; i < 2; i++) {
    pthread_create(&threads[i], NULL, wait_on, A);
  }
  if (await_queued(A, 2) != 0) {
    return 1;
  }
  report("requeue A->B wake 0 move 0", futex(A, FUTEX_REQUEUE_PRIVATE, 0, 0, B, 0));
  report("queued on A after it", queued(A));
  report("wake_op nr_wake 0", futex(A, FUTEX_WAKE_OP_PRIVATE, 0, 0, B, keep_b_zero));
  report("wake_op nr_wake -1", futex(A, FUTEX_WAKE_OP_PRIVATE, (uint32_t)-1, 0, B, keep_b_zero));
  if (join_all(threads, 2) != 0) {
    return 1;
  }
  for (int count = 0; count >= -1; count--) {
    pthread_create(&threads[0], NULL, wait_on, B);
    if (await_queued(B, 1) != 0) {
      return 1;
    }
    char what[32];
    snprintf(what, sizeof what, "wake_op nr_wake2 %d", count);
    report(what, futex(A, FUTEX_WAKE_OP_PRIVATE, 0, (unsigned long)(uint32_t)count, B, keep_b_zero));
    if (join_all(threads, 1) != 0) {
      return 1;
    }
  }

  /* Order: moved waiters join the END of the target's queue, in the order they
   * were waiting, as requeue_futex's plist_add does. W0 waits on B first, then
   * W1, W2 and W3 on A; two requeues move all three to B; single wakes on B then
   * release 0, 1, 2, 3. */
  *A = 0;
  *B = 0;
  pthread_t ordered[4];
  struct ordered_waiter args[4];
  args[0] = (struct ordered_waiter){0, B};
  pthread_create(&ordered[0], NULL, ordered_wait, &args[0]);
  if (await_queued(B, 1) != 0) {
    return 1;
  }
  for (int i = 1; i < 4; i++) {
    args[i] = (struct ordered_waiter){i, A};
    pthread_create(&ordered[i], NULL, ordered_wait, &args[i]);
    if (await_queued(A, i) != 0) {
      return 1;
    }
  }
  report("cmp_requeue A->B wake 0 move 2", futex(A, FUTEX_CMP_REQUEUE_PRIVATE, 0, 2, B, 0));
  report("requeue A->B wake 0 move 1", futex(A, FUTEX_REQUEUE_PRIVATE, 0, 1, B, 0));
  for (int k = 0; k < 4; k++) {
    futex(B, FUTEX_WAKE_PRIVATE, 1, 0, NULL, 0);
    if (await_finished(k + 1) != 0) {
      return 1;
    }
    printf("wake B 1 released waiter %d\n", atomic_load(&finished_order[k]) - 1);
  }
  for (int i = 0; i < 4; i++) {
    pthread_join(ordered[i], NULL);
  }
  return 0;
}
