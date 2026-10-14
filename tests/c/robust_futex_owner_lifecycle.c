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
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <time.h>
#include <stddef.h>
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
  _Atomic int unlocked;
  _Atomic int sibling_blocking;
  _Atomic int exiting;
  _Atomic int registered;
};

/* How many private robust mutexes the exit_group caller holds in the
 * blocked-sibling mode. Walking them first gives the owner's native exit time
 * to mark the word before the walk reaches the owner's list. */
#define BLOCKED_SIBLING_CALLER_LOCKS 1500

static struct shared_state* state;
static const char* self_path;

static void die(const char* what) {
  perror(what);
  _exit(2);
}

static void wait_until(_Atomic int* word, int value) {
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
  _Atomic uint32_t* lock = (_Atomic uint32_t*)&state->mutex;
  for (unsigned i = 0; i < 10000000; ++i) {
    if (atomic_load_explicit(lock, memory_order_acquire) & FUTEX_WAITERS) {
      return;
    }
    sched_yield();
  }
  fputs("timed out waiting for FUTEX_WAITERS\n", stderr);
  _exit(4);
}

static void lock_as_owner_quietly(void) {
  int rc = pthread_mutex_lock(&state->mutex);
  if (rc != 0) {
    errno = rc;
    die("owner pthread_mutex_lock");
  }
}

static void lock_as_owner(void) {
  lock_as_owner_quietly();
  atomic_store_explicit(&state->owner_ready, 1, memory_order_release);
}

static void waiter_process(void) {
  int rc = pthread_mutex_lock(&state->mutex);
  if (rc != EOWNERDEAD) {
    fprintf(
        stderr, "waiter got %d (%s), expected EOWNERDEAD\n", rc, strerror(rc));
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

/*
 * For the modes in which Linux must not wake the waiter, or not before the
 * owner unlocks: a raw FUTEX_WAIT on the owner word with a 3-second timeout,
 * which reports how it returned instead of blocking in glibc.
 */
static void raw_waiter_process(void) {
  _Atomic uint32_t* word = (_Atomic uint32_t*)&state->mutex;
  uint32_t value = atomic_load(word);
  while (!(value & FUTEX_WAITERS)) {
    if ((value & FUTEX_TID_MASK) == 0) {
      fprintf(stderr, "the mutex is not owned: %#x\n", value);
      _exit(11);
    }
    if (atomic_compare_exchange_strong(word, &value, value | FUTEX_WAITERS)) {
      value |= FUTEX_WAITERS;
    }
  }
  struct timespec timeout = {3, 0};
  long ret = syscall(SYS_futex, word, FUTEX_WAIT, value, &timeout, NULL, 0);
  const char* how = ret == 0 ? "woken" : strerrorname_np(errno);
  printf(
      "RAW: wait %s, owner unlocked %d, OWNER_DIED %d, registered %d\n",
      how,
      atomic_load(&state->unlocked),
      (atomic_load(word) & FUTEX_OWNER_DIED) != 0,
      atomic_load(&state->registered));
  fflush(stdout);
  _exit(0);
}

static pid_t spawn_waiter(int raw) {
  pid_t pid = fork();
  if (pid < 0) {
    die("fork waiter");
  }
  if (pid == 0) {
    if (raw) {
      raw_waiter_process();
    }
    waiter_process();
  }
  wait_for_waiter();
  return pid;
}

static void await_success(pid_t pid, const char* name) {
  int status;
  if (waitpid(pid, &status, 0) != pid) {
    die("waitpid");
  }
  if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    fprintf(stderr, "%s status %#x\n", name, status);
    exit(6);
  }
}

static void* owner_thread(void* unused) {
  (void)unused;
  lock_as_owner();
  for (;;) {
    pause();
  }
  return NULL;
}

/*
 * Created before the owner, so it is walked first: blocks in
 * pthread_mutex_lock on the owner's mutex, which leaves that mutex in this
 * thread's list_op_pending for as long as it waits.
 */
static void* blocked_sibling_thread(void* unused) {
  (void)unused;
  wait_until(&state->owner_ready, 1);
  atomic_store(&state->sibling_blocking, 1);
  pthread_mutex_lock(&state->mutex);
  for (;;) {
    pause();
  }
  return NULL;
}

/*
 * Owns the mutex but takes its robust list away, then registers it again only
 * once the leader has started its exit_group: Linux walks the list only if the
 * registration came first.
 */
static void* late_registration_thread(void* unused) {
  (void)unused;
  lock_as_owner_quietly();
  struct robust_list_head* head;
  size_t len;
  if (syscall(SYS_get_robust_list, 0, &head, &len) != 0) {
    die("get_robust_list");
  }
  if (syscall(SYS_set_robust_list, NULL, len) != 0) {
    die("set_robust_list NULL");
  }
  atomic_store_explicit(&state->owner_ready, 1, memory_order_release);
  while (!atomic_load(&state->exiting)) {
    sched_yield();
  }
  if (syscall(SYS_set_robust_list, head, len) != 0) {
    die("set_robust_list");
  }
  atomic_store(&state->registered, 1);
  for (;;) {
    pause();
  }
  return NULL;
}

/* The owner in the blocked-sibling mode, which says its TID. */
static void* reporting_owner_thread(void* unused) {
  (void)unused;
  printf("OWNER: tid %ld\n", (long)syscall(SYS_gettid));
  fflush(stdout);
  lock_as_owner();
  for (;;) {
    pause();
  }
  return NULL;
}

static void* exec_owner_thread(void* unused) {
  (void)unused;
  lock_as_owner();
  wait_until(&state->trigger, 1);
  execl(self_path, self_path, "after-exec", NULL);
  die("execl");
  return NULL;
}

/*
 * Locks the mutex, then rewrites its owner field to the thread group ID, as no
 * glibc path does but a hand-written lock may, and execs from a thread that is
 * not the leader. de_thread gives the caller the thread group ID before
 * futex_exec_release walks its list, so the word now matches and Linux marks it
 * FUTEX_OWNER_DIED and wakes the waiter. This pins the TID the caller's own list
 * is walked with: skipping that list would leave the waiter parked.
 */
static void* tgid_exec_owner_thread(void* unused) {
  (void)unused;
  lock_as_owner();
  _Atomic uint32_t* word = (_Atomic uint32_t*)&state->mutex;
  uint32_t value = atomic_load(word);
  while (!atomic_compare_exchange_strong(
      word, &value, (value & ~FUTEX_TID_MASK) | (uint32_t)getpid())) {
  }
  wait_until(&state->trigger, 1);
  execl(self_path, self_path, "after-exec", NULL);
  die("execl");
  return NULL;
}

static void* exec_thread(void* unused) {
  (void)unused;
  wait_until(&state->trigger, 1);
  execl(self_path, self_path, "after-exec", NULL);
  die("execl");
  return NULL;
}

static pid_t spawn_group_owner(const char* mode) {
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

  if (strcmp(mode, "blocked-sibling") == 0) {
    pthread_t sibling, owner;
    if (pthread_create(&sibling, NULL, blocked_sibling_thread, NULL) != 0) {
      die("pthread_create blocked sibling");
    }
    if (pthread_create(&owner, NULL, reporting_owner_thread, NULL) != 0) {
      die("pthread_create owner");
    }
    wait_until(&state->owner_ready, 1);
    wait_until(&state->sibling_blocking, 1);
    wait_until(&state->trigger, 1);
    pthread_mutexattr_t attr;
    if (pthread_mutexattr_init(&attr) != 0 ||
        pthread_mutexattr_setrobust(&attr, PTHREAD_MUTEX_ROBUST) != 0) {
      die("caller mutex attributes");
    }
    static pthread_mutex_t held[BLOCKED_SIBLING_CALLER_LOCKS];
    for (int i = 0; i < BLOCKED_SIBLING_CALLER_LOCKS; ++i) {
      if (pthread_mutex_init(&held[i], &attr) != 0 ||
          pthread_mutex_lock(&held[i]) != 0) {
        die("caller lock");
      }
    }
    syscall(SYS_exit_group, 0);
    _exit(7);
  }

  if (strcmp(mode, "late-registration") == 0) {
    pthread_t thread;
    if (pthread_create(&thread, NULL, late_registration_thread, NULL) != 0) {
      die("pthread_create late registration");
    }
    wait_until(&state->owner_ready, 1);
    wait_until(&state->trigger, 1);
    atomic_store(&state->exiting, 1);
    syscall(SYS_exit_group, 0);
    _exit(7);
  }

  if (strcmp(mode, "owner-exec") == 0) {
    /* The owner itself execs: Linux's futex_exec_release walks its list. */
    lock_as_owner();
    wait_until(&state->trigger, 1);
    execl(self_path, self_path, "after-exec", NULL);
    die("execl");
  }

  if (strcmp(mode, "leader-exec") == 0) {
    /* A sibling owns the mutex and the leader execs; de_thread kills it. */
    pthread_t thread;
    if (pthread_create(&thread, NULL, owner_thread, NULL) != 0) {
      die("pthread_create owner");
    }
    wait_until(&state->owner_ready, 1);
    wait_until(&state->trigger, 1);
    execl(self_path, self_path, "after-exec", NULL);
    die("execl");
  }

  if (strcmp(mode, "thread-tgid-exec") == 0) {
    pthread_t thread;
    if (pthread_create(&thread, NULL, tgid_exec_owner_thread, NULL) != 0) {
      die("pthread_create tgid exec owner");
    }
    for (;;) {
      pause();
    }
  }

  if (strcmp(mode, "thread-owner-exec") == 0) {
    /*
     * A thread that is not the leader owns the mutex and execs. de_thread
     * gives it the leader's TID before futex_exec_release walks its list, so
     * Linux leaves the word, owned under the old TID, alone: no OWNER_DIED, no
     * wake.
     */
    pthread_t thread;
    if (pthread_create(&thread, NULL, exec_owner_thread, NULL) != 0) {
      die("pthread_create exec owner");
    }
    for (;;) {
      pause();
    }
  }

  if (strcmp(mode, "failed-exec") == 0) {
    /* An exec that fails changes nothing: the owner still unlocks it later. */
    lock_as_owner();
    wait_until(&state->trigger, 1);
    execl("/nonexistent/robust_futex_owner_lifecycle", "x", NULL);
    for (int i = 0; i < 300; ++i) {
      sched_yield();
    }
    atomic_store(&state->unlocked, 1);
    if (pthread_mutex_unlock(&state->mutex) != 0) {
      die("pthread_mutex_unlock owner");
    }
    _exit(0);
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

/*
 * Two owner-death cases from the Codex re-check of
 * https://github.com/rrnewton/hermit/pull/4277, each printing how two raw
 * waiters in other processes returned and the final word:
 *
 * - shared-alias: one owner's list holds one word through two mappings of
 *   the same memfd page, the second entry looping on itself. Linux marks the
 *   word once and wakes one of the two waiters; the other times out.
 * - self-alias: the same with one mapping, so the entry is its own successor.
 *   Linux again marks the word once and wakes one waiter. Record and replay
 *   carry a single mapping, which two aliases of one memfd page they cannot.
 * - two-pending: two owners each have the same free word in their pending
 *   slot. Linux wakes once for each, so both waiters are woken, and the word
 *   stays 0.
 * - pending-marked, pending-marked-quiet: the same, with the word already
 *   marked by an earlier owner's death (FUTEX_OWNER_DIED with and without
 *   FUTEX_WAITERS), the state of woken waiters killed before they take the
 *   lock. Linux's pending rule still wakes once for each (Claude re-check of
 *   c65d9202, NF1).
 *
 * Another thread of the owners' process ends them with exit_group.
 */
struct boundary_state {
  _Atomic unsigned word;
  _Atomic int owners;
  _Atomic int waiters;
  _Atomic int fire;
};
static struct boundary_state* boundary;
static char *alias_a, *alias_b;
static unsigned* boundary_word;
static int alias_mode;

static void boundary_spin_until(_Atomic int* p, int n) {
  for (unsigned i = 0; i < 1000000; ++i) {
    if (atomic_load(p) >= n) {
      return;
    }
    sched_yield();
  }
  _exit(81);
}

static void* boundary_owner(void* unused) {
  (void)unused;
  struct robust_list_head head = {0};
  head.list.next = &head.list;
  if (alias_mode) {
    head.futex_offset = -32;
    head.list.next = (struct robust_list*)(alias_a + 32);
    head.list.next->next = (struct robust_list*)(alias_b + 32);
    atomic_store(
        (_Atomic unsigned*)boundary_word,
        FUTEX_WAITERS | (unsigned)syscall(SYS_gettid));
  } else {
    head.futex_offset = 0;
    head.list_op_pending = (struct robust_list*)boundary_word;
  }
  if (syscall(SYS_set_robust_list, &head, sizeof(head)) != 0) {
    die("set_robust_list");
  }
  atomic_fetch_add(&boundary->owners, 1);
  for (;;) {
    pause();
  }
  return NULL;
}

static int boundary_main(const char* mode) {
  int self_alias = strcmp(mode, "self-alias") == 0;
  alias_mode = self_alias || strcmp(mode, "shared-alias") == 0;
  unsigned initial = 0;
  if (strcmp(mode, "pending-marked") == 0) {
    initial = FUTEX_OWNER_DIED | FUTEX_WAITERS;
  } else if (strcmp(mode, "pending-marked-quiet") == 0) {
    initial = FUTEX_OWNER_DIED;
  }
  boundary = mmap(
      NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  if (boundary == MAP_FAILED) {
    die("boundary mmap");
  }
  boundary_word = (unsigned*)&boundary->word;
  atomic_store((_Atomic unsigned*)boundary_word, initial);
  if (alias_mode) {
    int fd = (int)syscall(SYS_memfd_create, "robust-alias", 0);
    if (fd < 0 || ftruncate(fd, 4096) != 0) {
      die("memfd");
    }
    alias_a = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    alias_b = self_alias ? alias_a
                         : mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (alias_a == MAP_FAILED || alias_b == MAP_FAILED) {
      die("alias mmap");
    }
    boundary_word = (unsigned*)alias_a;
    close(fd);
  }
  int owners = alias_mode ? 1 : 2;
  pid_t group = fork();
  if (group < 0) {
    die("fork group");
  }
  if (group == 0) {
    pthread_t threads[2];
    for (int i = 0; i < owners; ++i) {
      if (pthread_create(&threads[i], NULL, boundary_owner, NULL) != 0) {
        die("pthread_create");
      }
    }
    boundary_spin_until(&boundary->owners, owners);
    boundary_spin_until(&boundary->fire, 1);
    syscall(SYS_exit_group, 0);
    _exit(82);
  }
  boundary_spin_until(&boundary->owners, owners);
  pid_t waiters[2];
  for (int i = 0; i < 2; ++i) {
    waiters[i] = fork();
    if (waiters[i] < 0) {
      die("fork waiter");
    }
    if (waiters[i] == 0) {
      unsigned value = atomic_load((_Atomic unsigned*)boundary_word);
      atomic_fetch_add(&boundary->waiters, 1);
      struct timespec timeout = {3, 0};
      long r = syscall(
          SYS_futex, boundary_word, FUTEX_WAIT, value, &timeout, NULL, 0);
      int e = errno;
      if (r == 0) {
        _exit(0);
      }
      if (r == -1 && e == ETIMEDOUT) {
        _exit(10);
      }
      fprintf(stderr, "waiter ret=%ld errno=%d\n", r, e);
      _exit(83);
    }
  }
  boundary_spin_until(&boundary->waiters, 2);
  /* Let each published waiter enter FUTEX_WAIT; a guest sleep. */
  struct timespec settle = {0, 30000000};
  while (nanosleep(&settle, &settle) < 0 && errno == EINTR) {
  }
  atomic_store(&boundary->fire, 1);
  int woken = 0, timed_out = 0;
  for (int i = 0; i < 2; ++i) {
    int status;
    if (waitpid(waiters[i], &status, 0) != waiters[i] || !WIFEXITED(status)) {
      die("wait waiter");
    }
    if (WEXITSTATUS(status) == 0) {
      ++woken;
    } else if (WEXITSTATUS(status) == 10) {
      ++timed_out;
    } else {
      return 84;
    }
  }
  int status;
  if (waitpid(group, &status, 0) != group) {
    die("wait group");
  }
  printf(
      "BOUNDARY: mode=%s woken=%d timed_out=%d word=%#x\n",
      mode,
      woken,
      timed_out,
      *boundary_word);
  return 0;
}

/*
 * pending-owner (Claude re-check of c65d9202, NF2): the glibc state of a
 * contended robust mutex whose owner T is inside pthread_mutex_unlock, or just
 * past the acquiring CAS in pthread_mutex_lock. The word names T and sits only
 * in T's pending slot. A sibling S, created first, waits on it with it in its
 * own pending slot. A raw waiter in another process waits on it too. The
 * group's main thread holds BLOCKED_SIBLING_CALLER_LOCKS robust mutexes and
 * calls exit_group. Linux marks the word for T and wakes one waiter. Built by
 * hand with set_robust_list, as a guest cannot stop inside glibc's window.
 */
struct pending_owner_state {
  _Atomic unsigned word;
  _Atomic int owner_ready;
  _Atomic int sibling_ready;
  _Atomic int waiting;
  _Atomic int fire;
};
static struct pending_owner_state* po;

static void register_pending(struct robust_list_head* head) {
  head->list.next = &head->list;
  head->futex_offset = 0;
  head->list_op_pending = (struct robust_list*)&po->word;
  if (syscall(SYS_set_robust_list, head, sizeof(*head)) != 0) {
    die("set_robust_list");
  }
}

static void* pending_sibling(void* unused) {
  (void)unused;
  static struct robust_list_head head;
  boundary_spin_until(&po->owner_ready, 1);
  register_pending(&head);
  unsigned value = atomic_load(&po->word);
  atomic_store(&po->sibling_ready, 1);
  for (;;) {
    syscall(SYS_futex, &po->word, FUTEX_WAIT, value, NULL, NULL, 0);
  }
  return NULL;
}

static void* pending_owner(void* unused) {
  (void)unused;
  static struct robust_list_head head;
  unsigned tid = (unsigned)syscall(SYS_gettid);
  atomic_store(&po->word, FUTEX_WAITERS | tid);
  register_pending(&head);
  printf("OWNER: tid %u\n", tid);
  fflush(stdout);
  atomic_store(&po->owner_ready, 1);
  for (;;) {
    pause();
  }
  return NULL;
}

static int pending_owner_main(void) {
  po = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  if (po == MAP_FAILED) {
    die("pending-owner mmap");
  }
  pid_t group = fork();
  if (group < 0) {
    die("fork group");
  }
  if (group == 0) {
    pthread_t sibling, owner;
    if (pthread_create(&sibling, NULL, pending_sibling, NULL) != 0 ||
        pthread_create(&owner, NULL, pending_owner, NULL) != 0) {
      die("pthread_create");
    }
    boundary_spin_until(&po->sibling_ready, 1);
    boundary_spin_until(&po->fire, 1);
    pthread_mutexattr_t attr;
    if (pthread_mutexattr_init(&attr) != 0 ||
        pthread_mutexattr_setrobust(&attr, PTHREAD_MUTEX_ROBUST) != 0) {
      die("caller mutex attributes");
    }
    static pthread_mutex_t held[BLOCKED_SIBLING_CALLER_LOCKS];
    for (int i = 0; i < BLOCKED_SIBLING_CALLER_LOCKS; ++i) {
      if (pthread_mutex_init(&held[i], &attr) != 0 ||
          pthread_mutex_lock(&held[i]) != 0) {
        die("caller lock");
      }
    }
    syscall(SYS_exit_group, 0);
    _exit(82);
  }
  boundary_spin_until(&po->sibling_ready, 1);
  pid_t waiter = fork();
  if (waiter < 0) {
    die("fork waiter");
  }
  if (waiter == 0) {
    unsigned value = atomic_load(&po->word);
    atomic_store(&po->waiting, 1);
    struct timespec timeout = {60, 0};
    long r = syscall(SYS_futex, &po->word, FUTEX_WAIT, value, &timeout, NULL, 0);
    _exit(r == 0 ? 0 : 10);
  }
  boundary_spin_until(&po->waiting, 1);
  struct timespec settle = {0, 30000000};
  while (nanosleep(&settle, &settle) < 0 && errno == EINTR) {
  }
  atomic_store(&po->fire, 1);
  int status;
  if (waitpid(waiter, &status, 0) != waiter || !WIFEXITED(status)) {
    die("wait waiter");
  }
  int woken = WEXITSTATUS(status) == 0;
  if (waitpid(group, &status, 0) != group) {
    die("wait group");
  }
  printf(
      "PENDING-OWNER: waiter %s word=%#x\n",
      woken ? "woken" : "NOT-woken",
      atomic_load(&po->word));
  return 0;
}

/*
 * concurrent-exit-group (Claude re-check of 3719cd4e, F1; Claude review of
 * 65138a17, N1): thread X, created first, holds CONCURRENT_HOLDER_LOCKS
 * private robust mutexes and pauses. Threads A and B then each own one
 * process-shared robust mutex and both call exit_group. A waiter process
 * waits on each shared mutex with an 8 s timeout. Linux: both owners die and
 * both waiters get EOWNERDEAD, whichever exit_group wins. X sorts first in
 * the walk, so the winner reaches the killed filer's list only after X's long
 * one: a killed filer that did not hold its native exit would have it marked
 * by then.
 */
#define CONCURRENT_HOLDER_LOCKS 2000
struct concurrent_state {
  pthread_mutex_t m[2];
  _Atomic int ready;
  _Atomic int fire;
};
static struct concurrent_state* ce;

static void* concurrent_holder(void* unused) {
  (void)unused;
  pthread_mutexattr_t attr;
  if (pthread_mutexattr_init(&attr) != 0 ||
      pthread_mutexattr_setrobust(&attr, PTHREAD_MUTEX_ROBUST) != 0) {
    die("private mutex attributes");
  }
  pthread_mutex_t* held = calloc(CONCURRENT_HOLDER_LOCKS, sizeof(*held));
  for (int k = 0; k < CONCURRENT_HOLDER_LOCKS; ++k) {
    if (pthread_mutex_init(&held[k], &attr) != 0 ||
        pthread_mutex_lock(&held[k]) != 0) {
      die("lock private");
    }
  }
  atomic_fetch_add(&ce->ready, 1);
  for (;;) {
    pause();
  }
  return NULL;
}

static void* concurrent_exiter(void* argument) {
  int i = (int)(intptr_t)argument;
  if (pthread_mutex_lock(&ce->m[i]) != 0) {
    die("lock shared");
  }
  dprintf(STDOUT_FILENO, "%c: tid %ld\n", "AB"[i], (long)syscall(SYS_gettid));
  atomic_fetch_add(&ce->ready, 1);
  boundary_spin_until(&ce->fire, 1);
  syscall(SYS_exit_group, 0);
  _exit(82);
}

static int concurrent_exit_group_main(void) {
  ce = mmap(NULL, sizeof(*ce), PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  if (ce == MAP_FAILED) {
    die("concurrent mmap");
  }
  pthread_mutexattr_t attr;
  if (pthread_mutexattr_init(&attr) != 0 ||
      pthread_mutexattr_setpshared(&attr, PTHREAD_PROCESS_SHARED) != 0 ||
      pthread_mutexattr_setrobust(&attr, PTHREAD_MUTEX_ROBUST) != 0) {
    die("shared mutex attributes");
  }
  for (int i = 0; i < 2; ++i) {
    if (pthread_mutex_init(&ce->m[i], &attr) != 0) {
      die("pthread_mutex_init");
    }
  }
  pid_t group = fork();
  if (group < 0) {
    die("fork group");
  }
  if (group == 0) {
    pthread_t holder, threads[2];
    if (pthread_create(&holder, NULL, concurrent_holder, NULL) != 0) {
      die("pthread_create");
    }
    boundary_spin_until(&ce->ready, 1);
    for (int i = 0; i < 2; ++i) {
      if (pthread_create(&threads[i], NULL, concurrent_exiter, (void*)(intptr_t)i) != 0) {
        die("pthread_create");
      }
    }
    for (;;) {
      pause();
    }
  }
  boundary_spin_until(&ce->ready, 3);
  pid_t waiters[2];
  for (int i = 0; i < 2; ++i) {
    waiters[i] = fork();
    if (waiters[i] < 0) {
      die("fork waiter");
    }
    if (waiters[i] == 0) {
      struct timespec deadline;
      clock_gettime(CLOCK_REALTIME, &deadline);
      deadline.tv_sec += 8;
      int rc = pthread_mutex_timedlock(&ce->m[i], &deadline);
      _exit(rc == EOWNERDEAD ? 0 : rc == ETIMEDOUT ? 10 : 20);
    }
    _Atomic uint32_t* word = (_Atomic uint32_t*)&ce->m[i];
    for (unsigned k = 0; k < 10000000; ++k) {
      if (atomic_load(word) & FUTEX_WAITERS) {
        break;
      }
      sched_yield();
    }
  }
  atomic_store(&ce->fire, 1);
  const char* outcome[2];
  for (int i = 0; i < 2; ++i) {
    int status;
    if (waitpid(waiters[i], &status, 0) != waiters[i] || !WIFEXITED(status)) {
      die("wait waiter");
    }
    outcome[i] = WEXITSTATUS(status) == 0    ? "EOWNERDEAD"
        : WEXITSTATUS(status) == 10 ? "TIMEDOUT"
                                    : "ERROR";
  }
  int status;
  if (waitpid(group, &status, 0) != group) {
    die("wait group");
  }
  printf("CONCURRENT-EXIT-GROUP: A=%s B=%s\n", outcome[0], outcome[1]);
  return 0;
}

/*
 * reversed-owner (Codex re-check of 5899d5c8, P2-3,
 * https://github.com/rrnewton/hermit/issues/4302): thread A's list holds W,
 * which names thread B, and then U, which names A; B's pending slot holds W.
 * The group's main thread calls exit_group. A raw waiter waits on each of W
 * and U. Linux: A's walk skips W (another owner's) and marks U; B's walk marks
 * W from its pending slot; both waiters are woken. Each thread reports its
 * TID so the test can check which thread's walk woke which word.
 */
struct reversed_entry {
  _Atomic unsigned word;
  unsigned pad;
  struct robust_list node;
};
struct reversed_state {
  struct reversed_entry w, u;
  _Atomic int b_tid, ready, waiters, fire;
};
static struct reversed_state* rv;
#define REVERSED_OFFSET \
  ((long)offsetof(struct reversed_entry, word) - (long)offsetof(struct reversed_entry, node))

static void* reversed_b(void* unused) {
  (void)unused;
  static struct robust_list_head head;
  int tid = (int)syscall(SYS_gettid);
  atomic_store(&rv->w.word, FUTEX_WAITERS | (unsigned)tid);
  head.list.next = &head.list;
  head.futex_offset = REVERSED_OFFSET;
  head.list_op_pending = &rv->w.node;
  if (syscall(SYS_set_robust_list, &head, sizeof(head)) != 0) {
    die("set_robust_list B");
  }
  printf("B: tid %d\n", tid);
  fflush(stdout);
  atomic_store(&rv->b_tid, tid);
  atomic_fetch_add(&rv->ready, 1);
  for (;;) {
    pause();
  }
  return NULL;
}

static void* reversed_a(void* unused) {
  (void)unused;
  static struct robust_list_head head;
  boundary_spin_until(&rv->b_tid, 1);
  int tid = (int)syscall(SYS_gettid);
  atomic_store(&rv->u.word, FUTEX_WAITERS | (unsigned)tid);
  head.list.next = &rv->w.node;
  rv->w.node.next = &rv->u.node;
  rv->u.node.next = &head.list;
  head.futex_offset = REVERSED_OFFSET;
  head.list_op_pending = NULL;
  if (syscall(SYS_set_robust_list, &head, sizeof(head)) != 0) {
    die("set_robust_list A");
  }
  printf("A: tid %d\n", tid);
  fflush(stdout);
  atomic_fetch_add(&rv->ready, 1);
  for (;;) {
    pause();
  }
  return NULL;
}

static int reversed_owner_main(void) {
  rv = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  if (rv == MAP_FAILED) {
    die("reversed mmap");
  }
  pid_t group = fork();
  if (group < 0) {
    die("fork group");
  }
  if (group == 0) {
    pthread_t a, b;
    if (pthread_create(&a, NULL, reversed_a, NULL) != 0 ||
        pthread_create(&b, NULL, reversed_b, NULL) != 0) {
      die("pthread_create");
    }
    boundary_spin_until(&rv->ready, 2);
    boundary_spin_until(&rv->fire, 1);
    syscall(SYS_exit_group, 0);
    _exit(82);
  }
  boundary_spin_until(&rv->ready, 2);
  _Atomic unsigned* words[2] = {&rv->w.word, &rv->u.word};
  pid_t waiters[2];
  for (int i = 0; i < 2; ++i) {
    waiters[i] = fork();
    if (waiters[i] < 0) {
      die("fork waiter");
    }
    if (waiters[i] == 0) {
      unsigned value = atomic_load(words[i]);
      atomic_fetch_add(&rv->waiters, 1);
      struct timespec timeout = {5, 0};
      long r = syscall(SYS_futex, words[i], FUTEX_WAIT, value, &timeout, NULL, 0);
      _exit(r == 0 ? 0 : 10);
    }
  }
  boundary_spin_until(&rv->waiters, 2);
  struct timespec settle = {0, 30000000};
  while (nanosleep(&settle, &settle) < 0 && errno == EINTR) {
  }
  atomic_store(&rv->fire, 1);
  int woken[2];
  for (int i = 0; i < 2; ++i) {
    int status;
    if (waitpid(waiters[i], &status, 0) != waiters[i] || !WIFEXITED(status)) {
      die("wait waiter");
    }
    woken[i] = WEXITSTATUS(status) == 0;
  }
  int status;
  if (waitpid(group, &status, 0) != group) {
    die("wait group");
  }
  printf(
      "REVERSED-OWNER: W=%s U=%s words=%#x,%#x\n",
      woken[0] ? "woken" : "NOT-woken",
      woken[1] ? "woken" : "NOT-woken",
      atomic_load(&rv->w.word),
      atomic_load(&rv->u.word));
  return 0;
}

int main(int argc, char** argv) {
  if (argc == 2 && strcmp(argv[1], "after-exec") == 0) {
    return 0;
  }
  if (argc != 2) {
    fprintf(
        stderr,
        "usage: %s signal|exit-group|blocked-sibling|late-registration|"
        "shared-alias|self-alias|two-pending|pending-marked|pending-marked-quiet|"
        "pending-owner|concurrent-exit-group|reversed-owner|de-thread|owner-exec|"
        "leader-exec|"
        "thread-owner-exec|thread-tgid-exec|failed-exec\n",
        argv[0]);
    return 64;
  }
  if (strcmp(argv[1], "shared-alias") == 0 ||
      strcmp(argv[1], "self-alias") == 0 ||
      strcmp(argv[1], "two-pending") == 0 ||
      strcmp(argv[1], "pending-marked") == 0 ||
      strcmp(argv[1], "pending-marked-quiet") == 0) {
    return boundary_main(argv[1]);
  }
  if (strcmp(argv[1], "pending-owner") == 0) {
    return pending_owner_main();
  }
  if (strcmp(argv[1], "concurrent-exit-group") == 0) {
    return concurrent_exit_group_main();
  }
  if (strcmp(argv[1], "reversed-owner") == 0) {
    return reversed_owner_main();
  }
  self_path = argv[0];
  state = mmap(
      NULL,
      sizeof(*state),
      PROT_READ | PROT_WRITE,
      MAP_SHARED | MAP_ANONYMOUS,
      -1,
      0);
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
  int raw = strcmp(argv[1], "thread-owner-exec") == 0 ||
      strcmp(argv[1], "failed-exec") == 0 ||
      strcmp(argv[1], "late-registration") == 0;
  pid_t waiter = spawn_waiter(raw);

  atomic_store_explicit(&state->trigger, 1, memory_order_release);

  await_success(waiter, "waiter");
  int owner_status;
  if (waitpid(owner, &owner_status, 0) != owner) {
    die("waitpid owner");
  }
  puts("PASS: lifecycle completed");
  return 0;
}
