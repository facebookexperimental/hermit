// A 100 ms nanosleep that a 50 ms one-shot ITIMER_REAL interrupts, while a
// sibling thread that blocks SIGALRM computes without syscalls for longer
// than both. On Linux the alarm arrives before the sleep's timer fires, so
// nanosleep returns -1 EINTR with about 50 ms left in rem, and the SIGALRM
// handler has run (do_nanosleep in kernel/time/hrtimer.c). Under Hermit the
// sibling's slice carries virtual time past the alarm's due time and the
// sleep's deadline before the alarm pops.
//
// Usage: sleep_timer_before_deadline [ITERATIONS]; the sibling's loop length,
// 200000000 by default.
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/time.h>
#include <time.h>

static volatile sig_atomic_t handled;
static volatile unsigned long sink;

static void on_alarm(int sig) {
  (void)sig;
  handled++;
}

static void* spin(void* arg) {
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGALRM);
  pthread_sigmask(SIG_BLOCK, &set, NULL);
  unsigned long n = (unsigned long)arg, x = 1;
  for (unsigned long i = 0; i < n; i++) {
    x = x * 6364136223846793005UL + 1442695040888963407UL;
    if (x & 1) {
      sink++;
    }
  }
  return NULL;
}

int main(int argc, char** argv) {
  unsigned long iterations = argc > 1 ? strtoul(argv[1], NULL, 0) : 200000000UL;
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_handler = on_alarm;
  if (sigaction(SIGALRM, &sa, NULL) != 0) {
    perror("sigaction");
    return 2;
  }
  pthread_t thread;
  if (pthread_create(&thread, NULL, spin, (void*)iterations) != 0) {
    fprintf(stderr, "pthread_create failed\n");
    return 2;
  }
  struct itimerval once = {{0, 0}, {0, 50 * 1000}};
  if (setitimer(ITIMER_REAL, &once, NULL) != 0) {
    perror("setitimer");
    return 2;
  }
  struct timespec req = {0, 100 * 1000 * 1000}, rem = {0, 0};
  int rc = nanosleep(&req, &rem);
  int err = errno;
  long rem_ns = rem.tv_sec * 1000000000L + rem.tv_nsec;
  pthread_join(thread, NULL);
  printf(
      "nanosleep: %s; rem between 40 and 60 ms: %s; handled=%d\n",
      rc == 0 ? "rc=0" : (err == EINTR ? "rc=-1 EINTR" : "rc=-1 other"),
      rem_ns >= 40000000L && rem_ns <= 60000000L ? "yes" : "no",
      (int)handled);
  return 0;
}
