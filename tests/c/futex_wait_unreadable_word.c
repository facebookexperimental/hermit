/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * FUTEX_WAIT and FUTEX_WAIT_BITSET on a word they cannot read
 * (https://github.com/rrnewton/hermit/issues/4030). Linux's futex_wait_setup
 * reads the word with user-mode permissions and fails with EFAULT for an
 * unmapped or PROT_NONE word, private or shared; a readable word that differs
 * from the expected value is EAGAIN. The timeout and the bitset are checked
 * first, without touching the word: an invalid timeout or a zero bitset is
 * EINVAL even on an unreadable word. Every call carries a timeout, so a
 * wait that wrongly blocks still returns. Every line is the same natively and
 * under Hermit.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static void report_timeout(const char* what, uint32_t* word, int op, uint32_t bitset,
                           struct timespec timeout) {
  long ret = syscall(SYS_futex, word, op, 1, &timeout, NULL, bitset);
  if (ret == 0) {
    printf("%s: 0\n", what);
  } else {
    printf("%s: %ld %s\n", what, ret, strerrorname_np(errno));
  }
}

static void report(const char* what, uint32_t* word, int op, uint32_t bitset) {
  struct timespec timeout = {0, 10 * 1000 * 1000};
  report_timeout(what, word, op, bitset, timeout);
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  long pagesize = sysconf(_SC_PAGESIZE);
  char* pages = mmap(NULL, 2 * pagesize, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  uint32_t* readable = (uint32_t*)pages;
  uint32_t* hidden = (uint32_t*)(pages + pagesize);
  *readable = 7;
  *hidden = 7;
  mprotect(hidden, pagesize, PROT_NONE);
  uint32_t* gone = mmap(NULL, pagesize, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  munmap(gone, pagesize);

  report("wait, readable word 7, expecting 1", readable, FUTEX_WAIT_PRIVATE, 0);
  report("wait, unmapped private word", gone, FUTEX_WAIT_PRIVATE, 0);
  report("wait, PROT_NONE private word", hidden, FUTEX_WAIT_PRIVATE, 0);
  report("wait, unmapped shared word", gone, FUTEX_WAIT, 0);
  report("wait_bitset, unmapped private word", gone, FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG,
         FUTEX_BITSET_MATCH_ANY);
  report("wait_bitset, PROT_NONE private word", hidden, FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG,
         FUTEX_BITSET_MATCH_ANY);
  /* The timeout and the bitset are checked before the word is touched. */
  struct timespec bad_timeout = {0, 1000 * 1000 * 1000};
  report("wait_bitset, mask 0, PROT_NONE private word", hidden,
         FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG, 0);
  report_timeout("wait, tv_nsec 1e9, PROT_NONE private word", hidden, FUTEX_WAIT_PRIVATE, 0,
                 bad_timeout);
  report_timeout("wait, tv_nsec 1e9, readable word 7, expecting 1", readable,
                 FUTEX_WAIT_PRIVATE, 0, bad_timeout);
  return 0;
}
