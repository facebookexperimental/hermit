/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A PI futex word whose owner ends while the word still names it, and a
 * FUTEX_LOCK_PI that nothing orders after that end but the schedule.
 *
 * 1. The owner thread leaves by a raw exit; main spins on a flag the owner
 *    sets just before it, with no pthread_join and no syscall in between.
 * 2. The owner is a child process holding a process-shared word, and main
 *    calls FUTEX_LOCK_PI right after its kill(SIGKILL) returns.
 * 3. As 2, with a waiter thread already queued on the word when the SIGKILL
 *    comes.
 * 4. As 3, with the SIGKILL sent to the owner's process group by another
 *    member of it, which sets a flag that tells the owner to unlock just
 *    before it sends. Until the kill takes effect the owner is alive, so its
 *    FUTEX_UNLOCK_PI either succeeds, handing the waiter the lock, or never
 *    runs; it never fails.
 *
 * In 1 and 2, Linux answers by which came first. A locker that comes after
 * the owner is dead gets ESRCH, leaving FUTEX_WAITERS in the word
 * (attach_to_pi_owner and handle_exit_race, kernel/futex/pi.c). A locker that
 * comes first queues behind the owner, whose exit hands it the lock
 * (exit_pi_state_list), so it gets 0 and writes its TID with
 * FUTEX_OWNER_DIED. Natively either can happen; under Hermit the committed
 * order decides, so every run prints the same lines. In 3 and 4 the queued
 * waiter is handed the lock in every run.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static uint32_t main_tid;
static uint32_t owner_tid;
static uint32_t waiter_tid;

static uint32_t gettid_u32(void) {
  return (uint32_t)syscall(SYS_gettid);
}

static long futex(uint32_t* word, int op) {
  return syscall(SYS_futex, word, op, 0, NULL, NULL, 0);
}

static const char* holder(uint32_t value) {
  uint32_t tid = value & FUTEX_TID_MASK;
  if (tid == 0) {
    return "0";
  }
  if (tid == main_tid) {
    return "main";
  }
  if (tid == __atomic_load_n(&owner_tid, __ATOMIC_SEQ_CST)) {
    return "owner";
  }
  return tid == __atomic_load_n(&waiter_tid, __ATOMIC_SEQ_CST) ? "waiter" : "unknown";
}

static void report(const char* what, long ret, int error, const uint32_t* word) {
  uint32_t value = __atomic_load_n(word, __ATOMIC_SEQ_CST);
  printf(
      "%s: %s%s, word %s%s%s\n",
      what,
      ret < 0 ? "-1 " : "0",
      ret < 0 ? strerrorname_np(error) : "",
      holder(value),
      value & FUTEX_OWNER_DIED ? "|OWNER_DIED" : "",
      value & FUTEX_WAITERS ? "|WAITERS" : "");
  fflush(stdout);
}

/* 1. The owner thread leaves by a raw exit. */

static uint32_t thread_word;
static int about_to_exit;

static void* own_and_exit_raw(void* arg) {
  (void)arg;
  uint32_t tid = gettid_u32();
  __atomic_store_n(&owner_tid, tid, __ATOMIC_SEQ_CST);
  __atomic_store_n(&thread_word, tid, __ATOMIC_SEQ_CST);
  __atomic_store_n(&about_to_exit, 1, __ATOMIC_SEQ_CST);
  /* Not pthread_exit: no unwinding and no robust list entry for the word. */
  syscall(SYS_exit, 0);
  return NULL;
}

static void raw_exit(void) {
  pthread_t owner;
  if (pthread_create(&owner, NULL, own_and_exit_raw, NULL) != 0) {
    perror("pthread_create");
    exit(1);
  }
  while (!__atomic_load_n(&about_to_exit, __ATOMIC_SEQ_CST)) {
  }
  long ret = futex(&thread_word, FUTEX_LOCK_PI_PRIVATE);
  report("lock_pi on a word whose owner left by a raw exit", ret, errno, &thread_word);
}

/* 2 and 3. The owner is a child process that a SIGKILL ends. */

static uint32_t* shared_word(void) {
  void* word = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  if (word == MAP_FAILED) {
    perror("mmap");
    exit(1);
  }
  return word;
}

/* A child process that takes `word` and waits to be killed. */
static pid_t child_owning(uint32_t* word) {
  fflush(stdout);
  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    exit(1);
  }
  if (child == 0) {
    if (futex(word, FUTEX_TRYLOCK_PI) != 0) {
      _exit(1);
    }
    for (;;) {
      pause();
    }
  }
  __atomic_store_n(&owner_tid, (uint32_t)child, __ATOMIC_SEQ_CST);
  while (__atomic_load_n(word, __ATOMIC_SEQ_CST) == 0) {
    sched_yield();
  }
  return child;
}

static void reap(pid_t child) {
  int status;
  if (waitpid(child, &status, 0) != child || !WIFSIGNALED(status) || WTERMSIG(status) != SIGKILL) {
    printf("the owner did not die of SIGKILL\n");
    exit(1);
  }
}

struct waiter {
  uint32_t* word;
  long ret;
  int error;
};

static void* lock_shared(void* arg) {
  struct waiter* waiter = arg;
  __atomic_store_n(&waiter_tid, gettid_u32(), __ATOMIC_SEQ_CST);
  waiter->ret = futex(waiter->word, FUTEX_LOCK_PI);
  waiter->error = errno;
  return NULL;
}

static void sigkill_then_lock(void) {
  uint32_t* word = shared_word();
  pid_t owner = child_owning(word);
  kill(owner, SIGKILL);
  long ret = futex(word, FUTEX_LOCK_PI);
  report("lock_pi right after a SIGKILL ended the owner's process", ret, errno, word);
  reap(owner);
}

/* Start a thread that queues on `word` with FUTEX_LOCK_PI, and wait until it has. */
static pthread_t queue_a_waiter(struct waiter* waiter) {
  pthread_t thread;
  if (pthread_create(&thread, NULL, lock_shared, waiter) != 0) {
    perror("pthread_create");
    exit(1);
  }
  while (!(__atomic_load_n(waiter->word, __ATOMIC_SEQ_CST) & FUTEX_WAITERS)) {
    sched_yield();
  }
  /* Let the waiter finish queueing in the kernel. */
  for (int i = 0; i < 10; i++) {
    sched_yield();
  }
  usleep(50000);
  return thread;
}

static void sigkill_with_a_waiter(void) {
  uint32_t* word = shared_word();
  pid_t owner = child_owning(word);
  struct waiter waiter = {.word = word};
  pthread_t thread = queue_a_waiter(&waiter);
  kill(owner, SIGKILL);
  pthread_join(thread, NULL);
  report("the waiter queued when a SIGKILL ended the owner's process", waiter.ret, waiter.error, word);
  reap(owner);
}

/* Shared between main, the sender and the owner, after the word. */
struct group_kill {
  int go;
  int unlock_now;
  long unlock_ret;
  int unlock_errno;
};

static void group_sigkill_from_a_member(void) {
  uint32_t* word = shared_word();
  struct group_kill* control = (struct group_kill*)(word + 16);
  control->unlock_ret = -2;
  fflush(stdout);
  pid_t sender = fork();
  if (sender < 0) {
    perror("fork");
    exit(1);
  }
  if (sender == 0) {
    setpgid(0, 0);
    while (!__atomic_load_n(&control->go, __ATOMIC_SEQ_CST)) {
      sched_yield();
    }
    __atomic_store_n(&control->unlock_now, 1, __ATOMIC_SEQ_CST);
    kill(-getpid(), SIGKILL);
    _exit(1);
  }
  setpgid(sender, sender);
  pid_t owner = fork();
  if (owner < 0) {
    perror("fork");
    exit(1);
  }
  if (owner == 0) {
    if (setpgid(0, sender) != 0 || futex(word, FUTEX_TRYLOCK_PI) != 0) {
      _exit(1);
    }
    while (!__atomic_load_n(&control->unlock_now, __ATOMIC_SEQ_CST)) {
      sched_yield();
    }
    long ret = futex(word, FUTEX_UNLOCK_PI);
    control->unlock_errno = errno;
    __atomic_store_n(&control->unlock_ret, ret, __ATOMIC_SEQ_CST);
    for (;;) {
      pause();
    }
  }
  __atomic_store_n(&owner_tid, (uint32_t)owner, __ATOMIC_SEQ_CST);
  while (__atomic_load_n(word, __ATOMIC_SEQ_CST) == 0) {
    sched_yield();
  }
  struct waiter waiter = {.word = word};
  pthread_t thread = queue_a_waiter(&waiter);
  __atomic_store_n(&control->go, 1, __ATOMIC_SEQ_CST);
  pthread_join(thread, NULL);
  report(
      "the waiter queued when a member's group SIGKILL ended the owner's process",
      waiter.ret,
      waiter.error,
      word);
  long unlocked = __atomic_load_n(&control->unlock_ret, __ATOMIC_SEQ_CST);
  if (unlocked == -2) {
    printf("the owner's unlock_pi before the kill: not reached\n");
  } else if (unlocked < 0) {
    printf("the owner's unlock_pi before the kill: -1 %s\n", strerrorname_np(control->unlock_errno));
  } else {
    printf("the owner's unlock_pi before the kill: %ld\n", unlocked);
  }
  fflush(stdout);
  reap(sender);
  reap(owner);
}

int main(void) {
  main_tid = gettid_u32();
  raw_exit();
  sigkill_then_lock();
  sigkill_with_a_waiter();
  group_sigkill_from_a_member();
  return 0;
}
