// A ppoll with a timeout must time out on time while another thread spins
// without syscalls (https://github.com/rrnewton/hermit/issues/3952).
//
// One thread busy-loops with no syscalls and no trapped instructions until told
// to stop. The main thread waits five times in ppoll on an empty pipe, each
// with a 50 ms timeout, and prints each call's result and elapsed
// CLOCK_MONOTONIC time, which is virtual under Hermit. Natively each returns 0
// after about 50 ms. Under Hermit, before the poll-deadline wake, the poller
// was backed off after its first retry and waited for a poll upgrade, up to
// 200 of the spinner's timeslices; with `--max-timeslice 5000000` each wait
// took about 1.05 s.
#define _GNU_SOURCE
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdio.h>
#include <time.h>
#include <unistd.h>

#define WAITS 5

static atomic_int started, stop;

static void *spinner(void *arg) {
  (void)arg;
  atomic_store(&started, 1);
  volatile unsigned long n = 0;
  while (!atomic_load_explicit(&stop, memory_order_relaxed)) n++;
  return NULL;
}

static long now_ns(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return ts.tv_sec * 1000000000L + ts.tv_nsec;
}

int main(void) {
  int fds[2];
  if (pipe(fds)) return 1;
  pthread_t t;
  if (pthread_create(&t, NULL, spinner, NULL)) return 1;
  while (!atomic_load(&started)) sched_yield();
  for (int i = 0; i < WAITS; i++) {
    struct pollfd p = {.fd = fds[0], .events = POLLIN};
    struct timespec timeout = {.tv_sec = 0, .tv_nsec = 50 * 1000000L};
    long start = now_ns();
    int rc = ppoll(&p, 1, &timeout, NULL);
    printf("ppoll rc=%d elapsed_ns=%ld\n", rc, now_ns() - start);
  }
  atomic_store(&stop, 1);
  pthread_join(t, NULL);
  return 0;
}
