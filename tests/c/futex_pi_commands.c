/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * The priority-inheritance futex commands FUTEX_LOCK_PI, FUTEX_LOCK_PI2,
 * FUTEX_TRYLOCK_PI and FUTEX_UNLOCK_PI, called directly: their results, their
 * errors in Linux's order, and the futex word each leaves. A word is printed
 * with the owner TID replaced by the name of the thread it belongs to, so the
 * output is the same natively and under Hermit.
 *
 * Threads wait for each other by watching the word: FUTEX_LOCK_PI sets
 * FUTEX_WAITERS before its caller sleeps, so a set bit says the waiter is on
 * its way to the kernel's queue; a short sleep after it lets the waiter finish
 * queueing. A waiter that gets the lock keeps it until main has printed the
 * word, so every line is the same in every run. The last case execs this
 * program again to reap a child.
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
#include <time.h>
#include <unistd.h>

#ifndef FUTEX_LOCK_PI2
#define FUTEX_LOCK_PI2 13
#endif

static struct {
  const char* name;
  uint32_t tid;
} names[16];
static int named;

static void name_thread(const char* name, uint32_t tid) {
  names[named].name = name;
  names[named].tid = tid;
  named++;
}

static uint32_t self(void) {
  return (uint32_t)syscall(SYS_gettid);
}

static long futex(uint32_t* word, int op, const struct timespec* timeout) {
  return syscall(SYS_futex, word, op, 0, timeout, NULL, 0);
}

static void describe(uint32_t value, char* out, size_t size) {
  uint32_t tid = value & FUTEX_TID_MASK;
  const char* owner = NULL;
  for (int i = 0; i < named; i++) {
    if (names[i].tid == tid) {
      owner = names[i].name;
    }
  }
  char tid_text[16];
  if (tid == 0) {
    snprintf(tid_text, sizeof(tid_text), "0");
  } else if (owner == NULL) {
    snprintf(tid_text, sizeof(tid_text), "unknown");
  }
  snprintf(
      out,
      size,
      "%s%s%s",
      owner != NULL ? owner : tid_text,
      value & FUTEX_OWNER_DIED ? "|OWNER_DIED" : "",
      value & FUTEX_WAITERS ? "|WAITERS" : "");
}

static void report(const char* what, long ret, int error, const uint32_t* word) {
  char result[32];
  if (ret < 0) {
    snprintf(result, sizeof(result), "-1 %s", strerrorname_np(error));
  } else {
    snprintf(result, sizeof(result), "%ld", ret);
  }
  if (word != NULL) {
    char text[64];
    describe(__atomic_load_n(word, __ATOMIC_SEQ_CST), text, sizeof(text));
    printf("%s: %s, word %s\n", what, result, text);
  } else {
    printf("%s: %s\n", what, result);
  }
}

#define CALL(what, expression, word)  \
  do {                                \
    long ret_ = (expression);         \
    report(what, ret_, errno, word);  \
  } while (0)

static void wait_for_waiters(const uint32_t* word) {
  while (!(__atomic_load_n(word, __ATOMIC_SEQ_CST) & FUTEX_WAITERS)) {
    sched_yield();
  }
}

static void wait_for_flag(const int* flag) {
  while (!__atomic_load_n(flag, __ATOMIC_SEQ_CST)) {
    sched_yield();
  }
}

/* Let a thread that set FUTEX_WAITERS finish queueing in the kernel. */
static void let_it_queue(void) {
  for (int i = 0; i < 10; i++) {
    sched_yield();
  }
  usleep(50000);
}

static void set_flag(int* flag) {
  __atomic_store_n(flag, 1, __ATOMIC_SEQ_CST);
}

static struct timespec after_ms(clockid_t clock, long ms) {
  struct timespec now;
  clock_gettime(clock, &now);
  now.tv_nsec += ms * 1000000;
  now.tv_sec += now.tv_nsec / 1000000000;
  now.tv_nsec %= 1000000000;
  return now;
}

/* A thread that takes a word and holds it until told to release it. */
struct holder {
  uint32_t* word;
  uint32_t tid;
  int holding;
  int release;
  long unlock_ret;
  int unlock_errno;
};

static void* hold(void* arg) {
  struct holder* holder = arg;
  holder->tid = self();
  if (futex(holder->word, FUTEX_TRYLOCK_PI_PRIVATE, NULL) != 0) {
    perror("holder trylock");
    exit(1);
  }
  set_flag(&holder->holding);
  wait_for_flag(&holder->release);
  holder->unlock_ret = futex(holder->word, FUTEX_UNLOCK_PI_PRIVATE, NULL);
  holder->unlock_errno = errno;
  return NULL;
}

static void start_holder(pthread_t* thread, struct holder* holder, const char* name) {
  pthread_create(thread, NULL, hold, holder);
  wait_for_flag(&holder->holding);
  name_thread(name, holder->tid);
}

/* A thread that blocks in FUTEX_LOCK_PI, records what it got, and releases
 * the lock when main says so. */
struct waiter {
  uint32_t* word;
  uint32_t* second;
  const struct timespec* timeout;
  uint32_t tid;
  int started;
  int go;
  long ret;
  int error;
  uint32_t word_at_return;
  long unlock_ret;
  uint32_t word_after_unlock;
  long second_unlock_ret;
};

static void* lock_and_release(void* arg) {
  struct waiter* waiter = arg;
  waiter->tid = self();
  if (waiter->second != NULL &&
      futex(waiter->second, FUTEX_TRYLOCK_PI_PRIVATE, NULL) != 0) {
    perror("waiter trylock");
    exit(1);
  }
  set_flag(&waiter->started);
  waiter->ret = futex(waiter->word, FUTEX_LOCK_PI_PRIVATE, waiter->timeout);
  waiter->error = errno;
  waiter->word_at_return = __atomic_load_n(waiter->word, __ATOMIC_SEQ_CST);
  wait_for_flag(&waiter->go);
  waiter->unlock_ret = futex(waiter->word, FUTEX_UNLOCK_PI_PRIVATE, NULL);
  waiter->word_after_unlock = __atomic_load_n(waiter->word, __ATOMIC_SEQ_CST);
  if (waiter->second != NULL) {
    waiter->second_unlock_ret =
        futex(waiter->second, FUTEX_UNLOCK_PI_PRIVATE, NULL);
  }
  return NULL;
}

static void start_waiter(pthread_t* thread, struct waiter* waiter, const char* name) {
  pthread_create(thread, NULL, lock_and_release, waiter);
  wait_for_flag(&waiter->started);
  name_thread(name, waiter->tid);
}

static void finish_waiter(pthread_t thread, struct waiter* waiter) {
  set_flag(&waiter->go);
  pthread_join(thread, NULL);
}

static void report_waiter(const char* what, const struct waiter* waiter) {
  char at_return[64];
  char after_unlock[64];
  describe(waiter->word_at_return, at_return, sizeof(at_return));
  describe(waiter->word_after_unlock, after_unlock, sizeof(after_unlock));
  printf(
      "%s: lock_pi %ld%s%s, word %s; its unlock_pi %ld, word %s\n",
      what,
      waiter->ret,
      waiter->ret < 0 ? " " : "",
      waiter->ret < 0 ? strerrorname_np(waiter->error) : "",
      at_return,
      waiter->unlock_ret,
      after_unlock);
}

/* An owner that takes a word, waits for a waiter to queue, and exits. */
static void* own_and_exit(void* arg) {
  struct holder* holder = arg;
  holder->tid = self();
  if (futex(holder->word, FUTEX_TRYLOCK_PI_PRIVATE, NULL) != 0) {
    perror("owner trylock");
    exit(1);
  }
  set_flag(&holder->holding);
  wait_for_waiters(holder->word);
  let_it_queue();
  return NULL;
}

/* A thread that tries to take a word once each way and records the answers. */
struct prober {
  uint32_t* word;
  long trylock_ret;
  int trylock_errno;
  long lock_ret;
  int lock_errno;
};

static void* probe(void* arg) {
  struct prober* prober = arg;
  prober->trylock_ret = futex(prober->word, FUTEX_TRYLOCK_PI_PRIVATE, NULL);
  prober->trylock_errno = errno;
  prober->lock_ret = futex(prober->word, FUTEX_LOCK_PI_PRIVATE, NULL);
  prober->lock_errno = errno;
  return NULL;
}

/* A thread parked in a plain FUTEX_WAIT on a word. */
struct sleeper {
  uint32_t* word;
  uint32_t expected;
  long ret;
};

static void* plain_wait(void* arg) {
  struct sleeper* sleeper = arg;
  sleeper->ret = syscall(
      SYS_futex, sleeper->word, FUTEX_WAIT_PRIVATE, sleeper->expected, NULL, NULL, 0);
  return NULL;
}

static volatile sig_atomic_t caught;

static void count_signal(int signo) {
  (void)signo;
  caught++;
}

static void catch_sigusr1(int flags) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = count_signal;
  action.sa_flags = flags;
  sigemptyset(&action.sa_mask);
  sigaction(SIGUSR1, &action, NULL);
}

/* Interrupt a waiter blocked in FUTEX_LOCK_PI with a caught signal and let its
 * restarted call queue again. */
static void interrupt(pthread_t thread, struct waiter* waiter) {
  wait_for_waiters(waiter->word);
  let_it_queue();
  int before = caught;
  pthread_kill(thread, SIGUSR1);
  while (caught == before) {
    sched_yield();
  }
  let_it_queue();
}

/* The child of an owner that execs: it waits for the word and reports. */
static void wait_for_execing_owner(uint32_t* word) {
  name_thread("exec child", self());
  long ret = futex(word, FUTEX_LOCK_PI, NULL);
  char at_return[64];
  describe(__atomic_load_n(word, __ATOMIC_SEQ_CST), at_return, sizeof(at_return));
  long unlock_ret = futex(word, FUTEX_UNLOCK_PI, NULL);
  char after_unlock[64];
  describe(__atomic_load_n(word, __ATOMIC_SEQ_CST), after_unlock, sizeof(after_unlock));
  printf(
      "the child when its owner exec'd: lock_pi %ld, word %s; its unlock_pi %ld, word %s\n",
      ret,
      at_return,
      unlock_ret,
      after_unlock);
  fflush(stdout);
  _exit(0);
}

static void* note_tid(void* arg) {
  *(uint32_t*)arg = self();
  return NULL;
}

int main(int argc, char** argv) {
  if (argc == 3 && strcmp(argv[1], "reap") == 0) {
    int status;
    waitpid(atoi(argv[2]), &status, 0);
    return WIFEXITED(status) ? WEXITSTATUS(status) : 1;
  }
  static uint32_t word;
  name_thread("main", self());

  CALL("trylock_pi, unowned word", futex(&word, FUTEX_TRYLOCK_PI_PRIVATE, NULL), &word);
  CALL("lock_pi, word the caller owns", futex(&word, FUTEX_LOCK_PI_PRIVATE, NULL), &word);
  CALL("trylock_pi, word the caller owns", futex(&word, FUTEX_TRYLOCK_PI_PRIVATE, NULL), &word);
  CALL("unlock_pi, no waiter", futex(&word, FUTEX_UNLOCK_PI_PRIVATE, NULL), &word);
  CALL("unlock_pi, word 0", futex(&word, FUTEX_UNLOCK_PI_PRIVATE, NULL), &word);
  CALL("lock_pi, unowned word", futex(&word, FUTEX_LOCK_PI_PRIVATE, NULL), &word);
  CALL("unlock_pi again", futex(&word, FUTEX_UNLOCK_PI_PRIVATE, NULL), &word);

  /* The argument checks, in Linux's order. */
  struct timespec bad = {0, 1000000000};
  CALL("lock_pi, tv_nsec 1e9", futex(&word, FUTEX_LOCK_PI_PRIVATE, &bad), &word);
  CALL("lock_pi, unmapped timeout", futex(&word, FUTEX_LOCK_PI_PRIVATE, (void*)8), &word);
  CALL("trylock_pi ignores its timeout argument",
       futex(&word, FUTEX_TRYLOCK_PI_PRIVATE, (void*)8), &word);
  CALL("unlock_pi", futex(&word, FUTEX_UNLOCK_PI_PRIVATE, NULL), &word);
  CALL("lock_pi with FUTEX_CLOCK_REALTIME",
       futex(&word, FUTEX_LOCK_PI_PRIVATE | FUTEX_CLOCK_REALTIME, NULL), &word);
  CALL("lock_pi, NULL word", futex(NULL, FUTEX_LOCK_PI_PRIVATE, NULL), NULL);
  static uint32_t pair[2];
  CALL("lock_pi, misaligned word",
       futex((uint32_t*)((char*)pair + 1), FUTEX_LOCK_PI_PRIVATE, NULL), NULL);
  uint32_t me = self();
  memcpy((char*)pair + 1, &me, sizeof(me));
  CALL("unlock_pi, misaligned word naming the caller",
       futex((uint32_t*)((char*)pair + 1), FUTEX_UNLOCK_PI_PRIVATE, NULL), NULL);
  pair[0] = pair[1] = 0;
  CALL("unlock_pi, misaligned word naming nobody",
       futex((uint32_t*)((char*)pair + 1), FUTEX_UNLOCK_PI_PRIVATE, NULL), NULL);
  uint32_t* readonly =
      mmap(NULL, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  CALL("lock_pi, read-only word", futex(readonly, FUTEX_LOCK_PI_PRIVATE, NULL), readonly);
  CALL("shared lock_pi, read-only anonymous word", futex(readonly, FUTEX_LOCK_PI, NULL), readonly);
  CALL("unlock_pi, read-only word naming nobody", futex(readonly, FUTEX_UNLOCK_PI_PRIVATE, NULL), readonly);

  /* A word another live thread holds. */
  static uint32_t held;
  struct holder holder = {.word = &held};
  pthread_t holder_thread;
  start_holder(&holder_thread, &holder, "holder");
  CALL("unlock_pi, word another thread holds", futex(&held, FUTEX_UNLOCK_PI_PRIVATE, NULL), &held);
  CALL("trylock_pi, word another thread holds", futex(&held, FUTEX_TRYLOCK_PI_PRIVATE, NULL), &held);
  struct timespec deadline = after_ms(CLOCK_REALTIME, 20);
  CALL("lock_pi, CLOCK_REALTIME deadline 20 ms on",
       futex(&held, FUTEX_LOCK_PI_PRIVATE, &deadline), &held);
  deadline = after_ms(CLOCK_MONOTONIC, 20);
  CALL("lock_pi2, CLOCK_MONOTONIC deadline 20 ms on",
       futex(&held, FUTEX_LOCK_PI2 | FUTEX_PRIVATE_FLAG, &deadline), &held);
  deadline = after_ms(CLOCK_REALTIME, 20);
  CALL("lock_pi2 with FUTEX_CLOCK_REALTIME, deadline 20 ms on",
       futex(&held, FUTEX_LOCK_PI2 | FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME, &deadline),
       &held);
  set_flag(&holder.release);
  pthread_join(holder_thread, NULL);
  report("the holder's unlock_pi with no waiter left", holder.unlock_ret, holder.unlock_errno, &held);

  /* A word naming a thread that has exited. */
  static uint32_t orphan;
  uint32_t dead = 0;
  pthread_t dead_thread;
  pthread_create(&dead_thread, NULL, note_tid, &dead);
  pthread_join(dead_thread, NULL);
  name_thread("exited", dead);
  orphan = dead;
  CALL("lock_pi, word naming an exited thread", futex(&orphan, FUTEX_LOCK_PI_PRIVATE, NULL), &orphan);
  orphan = dead;
  CALL("trylock_pi, word naming an exited thread", futex(&orphan, FUTEX_TRYLOCK_PI_PRIVATE, NULL), &orphan);
  orphan = FUTEX_OWNER_DIED;
  CALL("trylock_pi, word marked OWNER_DIED", futex(&orphan, FUTEX_TRYLOCK_PI_PRIVATE, NULL), &orphan);
  CALL("unlock_pi, word marked OWNER_DIED", futex(&orphan, FUTEX_UNLOCK_PI_PRIVATE, NULL), &orphan);

  /* A waiter queued behind main: plain wakes refuse it, unlock_pi hands over. */
  static uint32_t handed;
  futex(&handed, FUTEX_TRYLOCK_PI_PRIVATE, NULL);
  struct waiter waiter = {.word = &handed};
  pthread_t waiter_thread;
  start_waiter(&waiter_thread, &waiter, "waiter");
  wait_for_waiters(&handed);
  let_it_queue();
  CALL("word with a queued waiter", 0, &handed);
  CALL("futex_wake, word with a PI waiter",
       syscall(SYS_futex, &handed, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0), &handed);
  static uint32_t elsewhere;
  CALL("futex_requeue 0 and 0, word with a PI waiter",
       syscall(SYS_futex, &handed, FUTEX_REQUEUE_PRIVATE, 0, (void*)0, &elsewhere, 0), &handed);
  CALL("futex_requeue 0 and 1, word with a PI waiter",
       syscall(SYS_futex, &handed, FUTEX_REQUEUE_PRIVATE, 0, (void*)1, &elsewhere, 0), &handed);
  CALL("unlock_pi, waiter queued", futex(&handed, FUTEX_UNLOCK_PI_PRIVATE, NULL), &handed);
  finish_waiter(waiter_thread, &waiter);
  report_waiter("the waiter", &waiter);

  /* Two waiters behind main are handed the word in the order they queued. */
  static uint32_t queue;
  futex(&queue, FUTEX_TRYLOCK_PI_PRIVATE, NULL);
  struct waiter first = {.word = &queue};
  struct waiter second = {.word = &queue};
  pthread_t first_thread;
  pthread_t second_thread;
  start_waiter(&first_thread, &first, "first");
  wait_for_waiters(&queue);
  let_it_queue();
  start_waiter(&second_thread, &second, "second");
  let_it_queue();
  CALL("unlock_pi, two waiters queued", futex(&queue, FUTEX_UNLOCK_PI_PRIVATE, NULL), &queue);
  finish_waiter(first_thread, &first);
  finish_waiter(second_thread, &second);
  report_waiter("the first waiter", &first);
  report_waiter("the second waiter", &second);

  /* A plain waiter queued first: the PI commands find no PI state. */
  static uint32_t plain_first;
  futex(&plain_first, FUTEX_TRYLOCK_PI_PRIVATE, NULL);
  struct sleeper sleeper = {.word = &plain_first, .expected = plain_first};
  pthread_t sleeper_thread;
  pthread_create(&sleeper_thread, NULL, plain_wait, &sleeper);
  let_it_queue();
  struct prober prober = {.word = &plain_first};
  pthread_t prober_thread;
  pthread_create(&prober_thread, NULL, probe, &prober);
  pthread_join(prober_thread, NULL);
  report("another thread's trylock_pi, plain waiter queued first",
         prober.trylock_ret, prober.trylock_errno, &plain_first);
  report("its lock_pi", prober.lock_ret, prober.lock_errno, &plain_first);
  CALL("unlock_pi, plain waiter queued first",
       futex(&plain_first, FUTEX_UNLOCK_PI_PRIVATE, NULL), &plain_first);
  CALL("futex_wake of the plain waiter",
       syscall(SYS_futex, &plain_first, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0), &plain_first);
  pthread_join(sleeper_thread, NULL);
  CALL("unlock_pi once it left", futex(&plain_first, FUTEX_UNLOCK_PI_PRIVATE, NULL), &plain_first);

  /* A caught signal interrupts a blocked lock_pi, which restarts whatever the
   * handler's SA_RESTART, and keeps its absolute deadline. */
  static uint32_t signalled;
  for (int restart = 1; restart >= 0; restart--) {
    catch_sigusr1(restart ? SA_RESTART : 0);
    futex(&signalled, FUTEX_TRYLOCK_PI_PRIVATE, NULL);
    struct waiter interrupted = {.word = &signalled};
    pthread_t interrupted_thread;
    start_waiter(&interrupted_thread, &interrupted, restart ? "restarted" : "resumed");
    interrupt(interrupted_thread, &interrupted);
    CALL(restart ? "unlock_pi, waiter signalled with SA_RESTART"
                 : "unlock_pi, waiter signalled without SA_RESTART",
         futex(&signalled, FUTEX_UNLOCK_PI_PRIVATE, NULL), &signalled);
    finish_waiter(interrupted_thread, &interrupted);
    report_waiter("the signalled waiter", &interrupted);
  }
  futex(&signalled, FUTEX_TRYLOCK_PI_PRIVATE, NULL);
  struct timespec signalled_deadline = after_ms(CLOCK_REALTIME, 300);
  struct waiter timed = {.word = &signalled, .timeout = &signalled_deadline};
  pthread_t timed_thread;
  start_waiter(&timed_thread, &timed, "timed");
  usleep(150000);
  interrupt(timed_thread, &timed);
  finish_waiter(timed_thread, &timed);
  /* A restart with a fresh 300 ms would end about 200 ms after the deadline. */
  struct timespec ended;
  clock_gettime(CLOCK_REALTIME, &ended);
  long late_ms = (ended.tv_sec - signalled_deadline.tv_sec) * 1000 +
      (ended.tv_nsec - signalled_deadline.tv_nsec) / 1000000;
  printf(
      "the waiter signalled 200 ms into a 300 ms deadline: lock_pi %ld %s, ended by the deadline: %s, "
      "handler ran %d times in all\n",
      timed.ret,
      timed.ret < 0 ? strerrorname_np(timed.error) : "",
      late_ms < 100 ? "yes" : "no",
      (int)caught);
  CALL("unlock_pi, timed waiter gone", futex(&signalled, FUTEX_UNLOCK_PI_PRIVATE, NULL), &signalled);
  signal(SIGUSR1, SIG_DFL);

  /* A lock that would close a cycle of PI waits is a deadlock. */
  static uint32_t a;
  static uint32_t b;
  futex(&a, FUTEX_TRYLOCK_PI_PRIVATE, NULL);
  struct waiter cycle = {.word = &a, .second = &b};
  pthread_t cycle_thread;
  start_waiter(&cycle_thread, &cycle, "cycle");
  wait_for_waiters(&a);
  let_it_queue();
  CALL("lock_pi on a word whose owner waits for the caller's",
       futex(&b, FUTEX_LOCK_PI_PRIVATE, NULL), &b);
  CALL("unlock_pi, cycle waiter queued", futex(&a, FUTEX_UNLOCK_PI_PRIVATE, NULL), &a);
  finish_waiter(cycle_thread, &cycle);
  report_waiter("the cycle waiter", &cycle);
  report("its unlock_pi of the word main waited on", cycle.second_unlock_ret, 0, &b);

  /* The owner exits holding a non-robust word with a waiter queued. */
  static uint32_t abandoned;
  struct holder owner = {.word = &abandoned};
  pthread_t owner_thread;
  pthread_create(&owner_thread, NULL, own_and_exit, &owner);
  wait_for_flag(&owner.holding);
  name_thread("owner", owner.tid);
  struct waiter heir = {.word = &abandoned};
  pthread_t heir_thread;
  start_waiter(&heir_thread, &heir, "heir");
  pthread_join(owner_thread, NULL);
  finish_waiter(heir_thread, &heir);
  report_waiter("the waiter when its owner exited", &heir);

  /* A process-shared word handed to a child process. */
  uint32_t* shared =
      mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  futex(shared, FUTEX_TRYLOCK_PI, NULL);
  uint32_t* child_report = shared + 1;
  int* child_go = (int*)(shared + 4);
  pid_t child = fork();
  if (child == 0) {
    long ret = futex(shared, FUTEX_LOCK_PI, NULL);
    child_report[0] = (uint32_t)ret;
    child_report[1] = __atomic_load_n(shared, __ATOMIC_SEQ_CST);
    wait_for_flag(child_go);
    child_report[2] = (uint32_t)futex(shared, FUTEX_UNLOCK_PI, NULL);
    _exit(0);
  }
  name_thread("child", (uint32_t)child);
  wait_for_waiters(shared);
  let_it_queue();
  CALL("shared unlock_pi, child queued", futex(shared, FUTEX_UNLOCK_PI, NULL), shared);
  set_flag(child_go);
  int status;
  waitpid(child, &status, 0);
  char text[64];
  describe(child_report[1], text, sizeof(text));
  printf(
      "the child: lock_pi %d, word %s; its unlock_pi %d, word %s\n",
      (int)child_report[0],
      text,
      (int)child_report[2],
      *shared == 0 ? "0" : "not 0");

  /* The owner execs holding a process-shared word a child process waits on:
   * the exec hands the word over as an exit does. The new image reaps the
   * child, whose line comes last. */
  uint32_t* exec_word =
      mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  futex(exec_word, FUTEX_TRYLOCK_PI, NULL);
  fflush(stdout);
  pid_t exec_child = fork();
  if (exec_child == 0) {
    wait_for_execing_owner(exec_word);
  }
  wait_for_waiters(exec_word);
  let_it_queue();
  char child_pid[16];
  snprintf(child_pid, sizeof(child_pid), "%d", (int)exec_child);
  execl("/proc/self/exe", argv[0], "reap", child_pid, (char*)NULL);
  perror("execl");
  return 1;
}
