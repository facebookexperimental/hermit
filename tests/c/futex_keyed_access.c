/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * How FUTEX_REQUEUE, FUTEX_CMP_REQUEUE and FUTEX_WAKE_OP key and access their
 * words, checked against Linux (https://github.com/rrnewton/hermit/pull/4020,
 * from the adversarial reviews' probes):
 *   - futex_requeue checks its counts before any address, keys both words
 *     (get_futex_key: alignment and access_ok for a private key, the backing
 *     page for a shared one), and reads uaddr only to compare it for
 *     CMP_REQUEUE;
 *   - a shared key is decided by the page backing the word (get_futex_key's
 *     GUP): a file page copied on write and then made read-only is an
 *     anonymous read-only page and EFAULT, and so is a shared file page past
 *     a truncation; the same words as private keys, and the writable or
 *     untouched neighbours, are accepted;
 *   - futex_wake_op keys both words without reading uaddr, rejects an unknown
 *     operation before touching uaddr2, and changes uaddr2 with a user-mode
 *     atomic, so a word at the end of a page works, a write-only (x86: also
 *     readable) word works, and a read-only or PROT_NONE word is EFAULT and
 *     unchanged;
 *   - NULL is a valid private key: a WAKE_OP whose first word is NULL still
 *     wakes the waiter on its second word.
 * Every line printed is the same natively and under Hermit, except that a
 * native waiter can wake spuriously and wait again on its original word
 * (__futex_wait's retry), which in about 3% of native runs leaves the waiter
 * off the NULL key in the last two blocks. Hermit's waiters do not wake
 * spuriously.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

static uint32_t words[4] __attribute__((aligned(16)));

static long futex(
    uint32_t* uaddr,
    int op,
    uint32_t val,
    unsigned long val2,
    uint32_t* uaddr2,
    uint32_t val3) {
  return syscall(SYS_futex, uaddr, op, val, val2, uaddr2, val3);
}

/* `word` is read here, after the call has been evaluated. */
static void report(const char* what, long ret, const uint32_t* word) {
  if (ret < 0) {
    printf("%s: -1 %s, word %u\n", what, strerrorname_np(errno), *word);
  } else {
    printf("%s: %ld, word %u\n", what, ret, *word);
  }
}

static const uint32_t set7 = FUTEX_OP(FUTEX_OP_SET, 7, FUTEX_OP_CMP_EQ, 0);
static const uint32_t add1 = FUTEX_OP(FUTEX_OP_ADD, 1, FUTEX_OP_CMP_EQ, 0);

/* A page with the given protection followed by a hole, holding 42 in its last
 * word. */
static uint32_t* last_word_of_page(long pagesize, int prot) {
  char* page = mmap(
      NULL, 2 * pagesize, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  munmap(page + pagesize, pagesize);
  uint32_t* word = (uint32_t*)(page + pagesize - sizeof(uint32_t));
  *word = 42;
  mprotect(page, pagesize, prot);
  return word;
}

static long thread_result;
static int thread_errno;
static void* wait_on_b(void* unused) {
  (void)unused;
  struct timespec timeout = {1, 0};
  thread_result = futex(&words[1], FUTEX_WAIT_PRIVATE, 0, (unsigned long)&timeout, NULL, 0);
  thread_errno = errno;
  return NULL;
}

static long queued(uint32_t* word) {
  return futex(word, FUTEX_CMP_REQUEUE_PRIVATE, 0, 0x7fffffff, word, *word);
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  long pagesize = sysconf(_SC_PAGESIZE);
  uint32_t* B = &words[1];

  /* An unmapped page and a PROT_NONE page next to each other. */
  char* pair = mmap(
      NULL, 2 * pagesize, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  uint32_t* none = (uint32_t*)pair;
  uint32_t* hole = (uint32_t*)(pair + pagesize);
  munmap(hole, pagesize);
  uint32_t* outside = (uint32_t*)(UINTPTR_MAX & ~(uintptr_t)3);

  report("requeue, unmapped private source, counts 0",
         futex(hole, FUTEX_REQUEUE_PRIVATE, 0, 0, B, 0), B);
  report("requeue, negative nr_wake, unmapped source",
         futex(hole, FUTEX_REQUEUE_PRIVATE, (uint32_t)-1, 0, B, 0), B);
  report("cmp_requeue, negative nr_requeue, unmapped source",
         futex(hole, FUTEX_CMP_REQUEUE_PRIVATE, 0, (uint32_t)-1, B, 0), B);
  report("requeue, target at the top of the address space",
         futex(&words[0], FUTEX_REQUEUE_PRIVATE, 0, 0, outside, 0), B);
  report("requeue, target in the kernel half",
         futex(&words[0], FUTEX_REQUEUE_PRIVATE, 0, 0, (uint32_t*)0xffff800000000000UL, 0), B);
  /* The limit is USER_PTR_MAX, which depends on the paging mode; these two
   * lie on the same side of it under 4- and 5-level paging. */
  report("requeue, private target in the low half above 2^62",
         futex(&words[0], FUTEX_REQUEUE_PRIVATE, 0, 0, (uint32_t*)0x4000000000000000UL, 0), B);
  report("requeue, private target just below the 4-level user limit",
         futex(&words[0], FUTEX_REQUEUE_PRIVATE, 0, 0, (uint32_t*)0x7fffffffeffcUL, 0), B);

  /* Shared keys need their backing page: a read-only anonymous page fails
   * even for reading, a read-only shmem page is fine for reading, and
   * WAKE_OP's second key is taken for writing, before its operation is
   * decoded. */
  uint32_t* ro_anon = mmap(NULL, pagesize, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  uint32_t* ro_shmem = mmap(NULL, pagesize, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  *ro_anon = 17;
  *ro_shmem = 17;
  mprotect(ro_anon, pagesize, PROT_READ);
  mprotect(ro_shmem, pagesize, PROT_READ);
  report("shared requeue, read-only anonymous target",
         futex(&words[0], FUTEX_REQUEUE, 0, 0, ro_anon, 0), ro_anon);
  report("shared requeue, read-only shmem target",
         futex(&words[0], FUTEX_REQUEUE, 0, 0, ro_shmem, 0), ro_shmem);
  report("shared wake_op, read-only shmem word, unknown op",
         futex(&words[0], FUTEX_WAKE_OP, 0, 0, ro_shmem, (7u << 28) | (1 << 12)), ro_shmem);
  report("private wake_op, read-only word, unknown op",
         futex(&words[0], FUTEX_WAKE_OP_PRIVATE, 0, 0, ro_anon, (7u << 28) | (1 << 12)), ro_anon);

  /* The backing page itself decides, not the mapping's permissions or file:
   * a file page copied on write and made read-only is anonymous, and a page
   * past the end of a truncated file is gone. A private key needs neither. */
  int fd = memfd_create("futex-backing-page", 0);
  ftruncate(fd, pagesize);
  uint32_t* file_rw = mmap(NULL, pagesize, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  *file_rw = 42;
  uint32_t* file_ro = mmap(NULL, pagesize, PROT_READ, MAP_PRIVATE, fd, 0);
  report("shared requeue, untouched read-only private file page",
         futex(&words[0], FUTEX_REQUEUE, 0, 0, file_ro, 0), file_ro);
  uint32_t* cow = mmap(NULL, pagesize, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
  *cow = 43;
  report("shared requeue, writable copied-on-write page",
         futex(&words[0], FUTEX_REQUEUE, 0, 0, cow, 0), cow);
  mprotect(cow, pagesize, PROT_READ);
  report("shared requeue, copied-on-write page made read-only",
         futex(&words[0], FUTEX_REQUEUE, 0, 0, cow, 0), cow);
  report("private requeue, copied-on-write page made read-only",
         futex(&words[0], FUTEX_REQUEUE_PRIVATE, 0, 0, cow, 0), cow);
  report("shared requeue, writable shared file page",
         futex(&words[0], FUTEX_REQUEUE, 0, 0, file_rw, 0), file_rw);
  ftruncate(fd, 0);
  report("shared requeue, shared file page past a truncation",
         futex(&words[0], FUTEX_REQUEUE, 0, 0, file_rw, 0), B);
  report("private requeue, shared file page past a truncation",
         futex(&words[0], FUTEX_REQUEUE_PRIVATE, 0, 0, file_rw, 0), B);
  report("shared requeue, PROT_NONE target",
         futex(&words[0], FUTEX_REQUEUE, 0, 0, none, 0), B);
  report("cmp_requeue, PROT_NONE source",
         futex(none, FUTEX_CMP_REQUEUE_PRIVATE, 0, 0, B, 0), B);
  report("wake_op, unmapped private first word, set B=7",
         futex(hole, FUTEX_WAKE_OP_PRIVATE, 0, 0, B, set7), B);
  report("wake_op, NULL second word, unknown op",
         futex(&words[0], FUTEX_WAKE_OP_PRIVATE, 0, 0, NULL, (7u << 28) | (1 << 12)), B);

  struct {
    const char* what;
    int prot;
  } pages[] = {
      {"wake_op +1, word at the end of a page before a hole", PROT_READ | PROT_WRITE},
      {"wake_op +1, write-only page", PROT_WRITE},
      {"wake_op +1, read-only page", PROT_READ},
      {"wake_op +1, PROT_NONE page", PROT_NONE},
  };
  for (unsigned i = 0; i < sizeof pages / sizeof pages[0]; i++) {
    uint32_t* word = last_word_of_page(pagesize, pages[i].prot);
    long ret = futex(&words[0], FUTEX_WAKE_OP_PRIVATE, 0, 0, word, add1);
    int err = errno;
    mprotect((char*)word - pagesize + sizeof(uint32_t), pagesize, PROT_READ);
    errno = err;
    report(pages[i].what, ret, word);
  }

  /* NULL first private key: the waiter on B must still be woken. */
  *B = 0;
  pthread_t thread;
  pthread_create(&thread, NULL, wait_on_b, NULL);
  while (queued(B) != 1) {
    sched_yield();
  }
  report("wake_op, NULL first word, wake 1 on B",
         futex(NULL, FUTEX_WAKE_OP_PRIVATE, 0, 1, B, FUTEX_OP(FUTEX_OP_SET, 0, FUTEX_OP_CMP_EQ, 0)),
         B);
  pthread_join(thread, NULL);
  printf("waiter on B: %s\n", thread_result == 0 ? "woken" : strerrorname_np(thread_errno));

  /* A waiter requeued onto the NULL private key is woken by FUTEX_WAKE on it;
   * one requeued onto NULL and back onto B is woken on B. */
  for (int back = 0; back <= 1; back++) {
    *B = 0;
    pthread_create(&thread, NULL, wait_on_b, NULL);
    while (queued(B) != 1) {
      sched_yield();
    }
    report("requeue B->NULL", futex(B, FUTEX_REQUEUE_PRIVATE, 0, 1, NULL, 0), B);
    if (back) {
      report("requeue NULL->B", futex(NULL, FUTEX_REQUEUE_PRIVATE, 0, 1, B, 0), B);
      report("wake B 1", futex(B, FUTEX_WAKE_PRIVATE, 1, 0, NULL, 0), B);
    } else {
      report("wake NULL 1", futex(NULL, FUTEX_WAKE_PRIVATE, 1, 0, NULL, 0), B);
    }
    pthread_join(thread, NULL);
    printf("waiter: %s\n", thread_result == 0 ? "woken" : strerrorname_np(thread_errno));
  }
  return 0;
}
