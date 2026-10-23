/* A clear-TID join wake that overlaps another admission. In each round, the
 * main thread waits for a forked child (its wake is a run-queue admission),
 * while a second thread J joins a thread that exits (J's wake comes from the
 * exiting thread's CLONE_CHILD_CLEARTID word). Each of the two records, in
 * shared memory, the order in which it resumed. The printed order is
 * schedule-dependent but must be identical in a recording and its replay
 * (https://github.com/rrnewton/hermit/issues/4362, part 2 step 0b; Codex
 * review of https://github.com/rrnewton/hermit/pull/4453, P2).
 *
 * Every create, join and wait is checked, and each round must observe both
 * tickets, 0 and 1, so a round in which J never ran cannot print an order.
 * Built with -DFAIL_INNER_CREATE and -Wl,--wrap=pthread_create, every
 * pthread_create from a thread other than main fails with EAGAIN; the guest
 * must then exit nonzero (the Codex re-check of 4453 at 840f3e85). */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdio.h>
#include <sys/wait.h>
#include <unistd.h>

#define ROUNDS 20
#define UNWRITTEN (-1)

static atomic_int ticket;
static int joiner_place[ROUNDS];

#ifdef FAIL_INNER_CREATE
static pthread_t main_thread;
int __real_pthread_create(pthread_t*, const pthread_attr_t*, void* (*)(void*), void*);
int __wrap_pthread_create(
    pthread_t* thread,
    const pthread_attr_t* attr,
    void* (*start)(void*),
    void* arg) {
  if (!pthread_equal(pthread_self(), main_thread)) {
    return EAGAIN;
  }
  return __real_pthread_create(thread, attr, start, arg);
}
#endif

static void* exiting(void* arg) {
  int spins = (int)(long)arg;
  for (int i = 0; i < spins; ++i) {
    sched_yield();
  }
  return NULL;
}

static void* joiner(void* arg) {
  int round = (int)(long)arg;
  pthread_t thread;
  if (pthread_create(&thread, NULL, exiting, (void*)(long)(round % 4)) != 0) {
    return (void*)1;
  }
  if (pthread_join(thread, NULL) != 0) {
    return (void*)2;
  }
  joiner_place[round] = atomic_fetch_add(&ticket, 1);
  return NULL;
}

int main(void) {
#ifdef FAIL_INNER_CREATE
  main_thread = pthread_self();
#endif
  char order[ROUNDS + 1];
  for (int round = 0; round < ROUNDS; ++round) {
    atomic_store(&ticket, 0);
    joiner_place[round] = UNWRITTEN;
    pid_t child = fork();
    if (child < 0) {
      perror("fork");
      return 1;
    }
    if (child == 0) {
      for (int i = 0; i < round % 5; ++i) {
        sched_yield();
      }
      _exit(3);
    }
    pthread_t thread;
    if (pthread_create(&thread, NULL, joiner, (void*)(long)round) != 0) {
      fprintf(stderr, "round %d: creating the joiner failed\n", round);
      return 1;
    }
    int status = 0;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 3) {
      fprintf(stderr, "round %d: waitpid failed\n", round);
      return 1;
    }
    int main_place = atomic_fetch_add(&ticket, 1);
    void* joined = NULL;
    if (pthread_join(thread, &joined) != 0) {
      fprintf(stderr, "round %d: joining the joiner failed\n", round);
      return 1;
    }
    if (joined != NULL) {
      fprintf(stderr, "round %d: the joiner failed with %ld\n", round, (long)joined);
      return 1;
    }
    int theirs = joiner_place[round];
    if (theirs == UNWRITTEN || main_place == theirs || main_place + theirs != 1) {
      fprintf(stderr, "round %d: tickets main=%d joiner=%d\n", round, main_place, theirs);
      return 1;
    }
    order[round] = main_place < theirs ? 'M' : 'J';
  }
  order[ROUNDS] = '\0';
  printf("cleartid admission order %s\n", order);
  return 0;
}
