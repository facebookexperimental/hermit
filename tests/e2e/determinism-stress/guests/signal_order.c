/* @lint-ignore-every LICENSELINT */

#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>

static pthread_t main_thread;
static pthread_barrier_t start;
static volatile sig_atomic_t delivered;
static volatile sig_atomic_t order[2];

static void handler(int signal_number) {
  sig_atomic_t position = delivered++;
  if (position < 2) {
    order[position] = signal_number;
  }
}

static void *sender(void *opaque) {
  int signal_number = *(int *)opaque;
  int barrier_result = pthread_barrier_wait(&start);
  if (barrier_result != 0 && barrier_result != PTHREAD_BARRIER_SERIAL_THREAD) {
    return (void *)1;
  }
  return (void *)(long)pthread_kill(main_thread, signal_number);
}

int main(void) {
  main_thread = pthread_self();
  struct sigaction action = {.sa_handler = handler};
  sigemptyset(&action.sa_mask);
  sigaddset(&action.sa_mask, SIGUSR1);
  sigaddset(&action.sa_mask, SIGUSR2);
  if (sigaction(SIGUSR1, &action, NULL) != 0 ||
      sigaction(SIGUSR2, &action, NULL) != 0 ||
      pthread_barrier_init(&start, NULL, 3) != 0) {
    return 1;
  }

  int signals[2] = {SIGUSR1, SIGUSR2};
  pthread_t threads[2];
  for (int index = 0; index < 2; index++) {
    if (pthread_create(&threads[index], NULL, sender, &signals[index]) != 0) {
      return 2;
    }
  }
  int barrier_result = pthread_barrier_wait(&start);
  if (barrier_result != 0 && barrier_result != PTHREAD_BARRIER_SERIAL_THREAD) {
    return 3;
  }

  while (delivered < 2) {
    sched_yield();
  }
  for (int index = 0; index < 2; index++) {
    void *result = NULL;
    if (pthread_join(threads[index], &result) != 0 || result != NULL) {
      return 4;
    }
  }

  printf("signal-order=%s,%s\n", order[0] == SIGUSR1 ? "USR1" : "USR2",
         order[1] == SIGUSR1 ? "USR1" : "USR2");
  return pthread_barrier_destroy(&start) == 0 ? 0 : 5;
}
